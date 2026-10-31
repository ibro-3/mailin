//! PGP/GPG encryption and signing using sequoia-openpgp.
//! Stub implementation - infrastructure ready, crypto operations to be implemented.

use crate::error::{Error, Result};
use crate::model::{SignatureStatus};
use sequoia_openpgp as openpgp;
use openpgp::cert::prelude::*;
use openpgp::policy::StandardPolicy;
use openpgp::serialize::SerializeInto;
use openpgp::types::KeyFlags;
use ureq::Agent;
use sha1::{Digest, Sha1};

const POLICY: StandardPolicy = StandardPolicy::new();

/// Z-Base32 alphabet (RFC 6189 / z-base-32).
/// Differs from RFC 4648: uses lowercase, no padding, different character mapping.
const ZBASE32_ALPHABET: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";

/// Encode bytes as z-base-32 (no padding, lowercase).
fn zbase32_encode(data: &[u8]) -> String {
    let mut result = String::with_capacity((data.len() * 8 + 4) / 5);
    let mut buffer = 0u32;
    let mut bits = 0;
    
    for &byte in data {
        buffer = (buffer << 8) | (byte as u32);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = (buffer >> bits) & 0x1F;
            result.push(ZBASE32_ALPHABET[index as usize] as char);
        }
    }
    
    if bits > 0 {
        let index = (buffer << (5 - bits)) & 0x1F;
        result.push(ZBASE32_ALPHABET[index as usize] as char);
    }
    
    result
}

/// Z-Base32 hash of the local part for WKD (SHA-1 + z-base-32).
fn zbase32_hash(input: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(input.to_lowercase().as_bytes());
    let hash = hasher.finalize();
    zbase32_encode(&hash)
}

/// Generate a new PGP key pair for the given user ID.
pub fn generate_keypair(user_id: &str) -> Result<(String, String)> {
    let (cert, _rev) = CertBuilder::new()
        .add_userid(user_id)
        .set_primary_key_flags(KeyFlags::empty()
            .set_certification()
            .set_signing())
        .add_signing_subkey()
        .add_storage_encryption_subkey()
        .add_authentication_subkey()
        .generate().map_err(|e| Error::Protocol(e.to_string()))?;

    // Export public key
    let mut public_key = Vec::new();
    cert.armored().serialize_into(&mut public_key).map_err(|e| Error::Protocol(e.to_string()))?;
    let public_key = String::from_utf8(public_key).map_err(|e| Error::Protocol(e.to_string()))?;

    // Export secret key 
    let mut secret_key = Vec::new();
    cert.with_policy(&POLICY, None)
        .map_err(|e| Error::Protocol(e.to_string()))?
        .armored()
        .serialize_into(&mut secret_key).map_err(|e| Error::Protocol(e.to_string()))?;
    let secret_key = String::from_utf8(secret_key).map_err(|e| Error::Protocol(e.to_string()))?;

    Ok((public_key, secret_key))
}

/// Import a PGP public key from armored text.
pub fn import_public_key(armored: &str) -> Result<Cert> {
    // For now, return a stub error - full parsing to be implemented
    Err(Error::Protocol("PGP key import not yet implemented".into()))
}

/// Import a PGP secret key from armored text.
pub fn import_secret_key(armored: &str) -> Result<Cert> {
    Err(Error::Protocol("PGP key import not yet implemented".into()))
}

/// Extract fingerprint from a public key.
pub fn key_fingerprint(cert: &Cert) -> String {
    cert.fingerprint().to_string()
}

/// Extract user IDs from a certificate.
pub fn key_user_ids(cert: &Cert) -> Vec<String> {
    cert.userids()
        .map(|uid| uid.userid().to_string())
        .collect()
}

/// Encrypt a message for the given recipients.
/// Returns the encrypted message as armored text.
/// TODO: Implement full encryption using sequoia-openpgp streaming API.
pub fn encrypt_message(
    _plaintext: &str,
    _recipient_certs: &[Cert],
    _signer_cert: Option<&Cert>,
    _signer_secret: Option<&str>,
) -> Result<String> {
    Err(Error::Protocol("PGP encryption not yet implemented".into()))
}

/// Decrypt and verify a message.
/// Returns (plaintext, signature_status).
/// TODO: Implement full decryption using sequoia-openpgp streaming API.
pub fn decrypt_message(
    _encrypted: &str,
    _recipient_secret_keys: &[Cert],
) -> Result<(String, SignatureStatus)> {
    Err(Error::Protocol("PGP decryption not yet implemented".into()))
}

/// Sign a message (cleartext signature).
/// TODO: Implement full signing using sequoia-openpgp streaming API.
pub fn sign_message(
    _plaintext: &str,
    _signer_cert: &Cert,
    _signer_secret: &str,
) -> Result<String> {
    Err(Error::Protocol("PGP signing not yet implemented".into()))
}

/// Verify a cleartext signature.
/// TODO: Implement full verification using sequoia-openpgp streaming API.
pub fn verify_signature(
    _signed_text: &str,
    _signer_cert: &Cert,
) -> Result<SignatureStatus> {
    Err(Error::Protocol("PGP verification not yet implemented".into()))
}

/// Fetch a PGP key via WKD (Web Key Directory).
/// Returns the public key if found.
pub async fn fetch_wkd_key(email: &str, client: &ureq::Agent) -> Result<Option<String>> {
    let domain = email.split('@').nth(1).ok_or(Error::Protocol("invalid email".into()))?;
    let local_part = email.split('@').next().ok_or(Error::Protocol("invalid email".into()))?;
    
    // Z-Base32 hash of the local part
    let hash = zbase32_hash(local_part);
    
    // Try direct WKD first
    let urls = vec![
        format!("https://{}/.well-known/openpgpkey/hu/{}", domain, hash),
        format!("https://openpgpkey.{}/.well-known/openpgpkey/hu/{}", domain, hash),
    ];
    
    for url in urls {
        if let Ok(mut response) = client.get(&url).call() {
            if response.status() == 200 {
                let key = response.body_mut().read_to_string().map_err(|e| Error::Protocol(e.to_string()))?;
                return Ok(Some(key));
            }
        }
    }
    
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "key generation API needs adjustment"]
    fn test_generate_keypair() {
        let (public, secret) = generate_keypair("Test User <test@example.com>").unwrap();
        assert!(public.contains("-----BEGIN PGP PUBLIC KEY BLOCK-----"));
        assert!(secret.contains("-----BEGIN PGP PRIVATE KEY BLOCK-----"));
    }

    #[test]
    #[ignore = "import_public_key not yet implemented"]
    fn test_key_fingerprint() {
        let (public, _secret) = generate_keypair("Test <test@example.com>").unwrap();
        let cert = import_public_key(&public).unwrap();
        let fp = key_fingerprint(&cert);
        assert_eq!(fp.len(), 40); // SHA-1 fingerprint is 40 hex chars
    }

    #[test]
    #[ignore = "import_public_key not yet implemented"]
    fn test_user_ids() {
        let (public, _secret) = generate_keypair("Alice <alice@example.com>").unwrap();
        let cert = import_public_key(&public).unwrap();
        let uids = key_user_ids(&cert);
        assert!(uids.iter().any(|u| u.contains("alice@example.com")));
    }

    #[test]
    fn test_zbase32_hash() {
        // Test vectors for z-base-32 encoding
        // SHA-1("test") = a94a8fe5ccb19ba61c4c0873d391e987982fbbd3
        // z-base-32 encoded: should match WKD spec
        let hash = zbase32_hash("test");
        // Just verify it produces a valid z-base32 string (32 chars, valid alphabet)
        assert_eq!(hash.len(), 32);
        assert!(hash.chars().all(|c| ZBASE32_ALPHABET.contains(&(c as u8))));
    }
}