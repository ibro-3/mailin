//! PGP/GPG encryption and signing using sequoia-openpgp.

use crate::error::{Error, Result};
use crate::model::SignatureStatus;
use sequoia_openpgp as openpgp;
use openpgp::cert::prelude::*;
use openpgp::parse::{stream::*, Parse};
use openpgp::policy::StandardPolicy;
use openpgp::serialize::stream::{Encryptor2, LiteralWriter, Message, Recipient, Signer};
use openpgp::serialize::SerializeInto;
use openpgp::types::KeyFlags;
use openpgp::types::SymmetricAlgorithm;
use openpgp::KeyHandle;
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use ureq::Agent;

static POLICY: StandardPolicy = StandardPolicy::new();

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
/// Returns `(public_key_armored, secret_key_armored)`.
pub fn generate_keypair(user_id: &str) -> Result<(String, String)> {
    let (cert, _rev) = CertBuilder::new()
        .add_userid(user_id)
        .set_primary_key_flags(KeyFlags::empty().set_certification().set_signing())
        .add_signing_subkey()
        .add_storage_encryption_subkey()
        .add_authentication_subkey()
        .generate()
        .map_err(|e| Error::Protocol(e.to_string()))?;

    let public_key = cert
        .armored()
        .to_vec()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    let public_key = String::from_utf8(public_key).map_err(|e| Error::Protocol(e.to_string()))?;

    let secret_key = cert
        .as_tsk()
        .armored()
        .to_vec()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    let secret_key = String::from_utf8(secret_key).map_err(|e| Error::Protocol(e.to_string()))?;

    Ok((public_key, secret_key))
}

/// Import a PGP public key from armored text.
pub fn import_public_key(armored: &str) -> Result<Cert> {
    armored
        .parse()
        .map_err(|e| Error::Protocol(format!("Failed to parse PGP public key: {e}")))
}

/// Import a PGP secret key from armored text.
pub fn import_secret_key(armored: &str) -> Result<Cert> {
    armored
        .parse()
        .map_err(|e| Error::Protocol(format!("Failed to parse PGP secret key: {e}")))
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

/// Export the public part of a certificate as armored text.
pub fn export_public_key(cert: &Cert) -> Result<String> {
    let bytes = cert
        .armored()
        .to_vec()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    String::from_utf8(bytes).map_err(|e| Error::Protocol(e.to_string()))
}

/// Export a certificate including its secret key material as armored text.
pub fn export_secret_key(cert: &Cert) -> Result<String> {
    let bytes = cert
        .as_tsk()
        .armored()
        .to_vec()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    String::from_utf8(bytes).map_err(|e| Error::Protocol(e.to_string()))
}

/// True when any user ID of `cert` mentions `address` (case-insensitive).
pub fn matches_address(cert: &Cert, address: &str) -> bool {
    let address = address.trim().to_lowercase();
    key_user_ids(cert)
        .iter()
        .any(|uid| uid.to_lowercase().contains(&address))
}

/// Build a signing key pair from a certificate containing secret key material.
fn signing_keypair(cert: &Cert) -> Result<openpgp::crypto::KeyPair> {
    cert.keys()
        .secret()
        .with_policy(&POLICY, None)
        .supported()
        .alive()
        .revoked(false)
        .for_signing()
        .next()
        .ok_or_else(|| Error::Protocol("No usable signing key found".into()))?
        .key()
        .clone()
        .into_keypair()
        .map_err(|e| Error::Protocol(e.to_string()))
}

/// Encrypt a message for the given recipients.
/// Returns the encrypted message as armored text.
///
/// If `signer_cert`/`signer_secret` are provided, the plaintext is signed
/// before encryption (sign-then-encrypt).
pub fn encrypt_message(
    plaintext: &str,
    recipient_certs: &[Cert],
    signer_cert: Option<&Cert>,
    signer_secret: Option<&str>,
) -> Result<String> {
    if recipient_certs.is_empty() {
        return Err(Error::Protocol(
            "No recipient certificates provided".into(),
        ));
    }

    // Collect all valid encryption keys from all recipients.
    let mut recipients: Vec<Recipient> = Vec::new();
    for cert in recipient_certs {
        let keys = cert
            .keys()
            .with_policy(&POLICY, None)
            .supported()
            .alive()
            .revoked(false)
            .for_storage_encryption();
        recipients.extend(keys.map(Recipient::from));
    }

    if recipients.is_empty() {
        return Err(Error::Protocol(
            "No valid encryption keys found for any recipient".into(),
        ));
    }

    let signer = match (signer_cert, signer_secret) {
        (Some(cert), secret) => {
            let cert = match secret {
                Some(secret) if !secret.trim().is_empty() => import_secret_key(secret)?,
                _ => cert.clone(),
            };
            Some(signing_keypair(&cert)?)
        }
        _ => None,
    };

    let mut sink = Vec::new();
    let message = Message::new(&mut sink);
    let message = match signer {
        Some(keypair) => Signer::new(message, keypair)
            .creation_time(SystemTime::now())
            .build()
            .map_err(|e| Error::Protocol(e.to_string()))?,
        None => message,
    };

    let encrypted = Encryptor2::for_recipients(message, recipients)
        .build()
        .map_err(|e| Error::Protocol(e.to_string()))?;

    let mut writer = LiteralWriter::new(encrypted)
        .build()
        .map_err(|e| Error::Protocol(e.to_string()))?;

    writer
        .write_all(plaintext.as_bytes())
        .map_err(|e| Error::Protocol(e.to_string()))?;
    writer
        .finalize()
        .map_err(|e| Error::Protocol(e.to_string()))?;

    armor_message(&sink)
}

/// Armor a serialized OpenPGP message (kind `Message`).
fn armor_message(packets: &[u8]) -> Result<String> {
    let mut armored = Vec::new();
    let mut writer = openpgp::armor::Writer::new(&mut armored, openpgp::armor::Kind::Message)
        .map_err(|e| Error::Protocol(e.to_string()))?;
    writer
        .write_all(packets)
        .map_err(|e| Error::Protocol(e.to_string()))?;
    writer
        .finalize()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    String::from_utf8(armored).map_err(|e| Error::Protocol(e.to_string()))
}

/// Translate the verification results recorded by a helper into a status.
fn verdict(good: usize, bad: usize, unknown: usize) -> SignatureStatus {
    if bad > 0 {
        SignatureStatus::Invalid
    } else if good > 0 {
        SignatureStatus::Valid
    } else if unknown > 0 {
        SignatureStatus::UnknownKey
    } else {
        // No signature present.
        SignatureStatus::UnknownKey
    }
}

/// Shared helper that decrypts with the given secret keys and records the
/// verification outcome while the message is being parsed.
struct DecryptHelper {
    keys: Vec<Cert>,
    verdict: Arc<Mutex<(usize, usize, usize)>>, // (good, bad, unknown)
}

impl DecryptionHelper for DecryptHelper {
    fn decrypt<D>(
        &mut self,
        pkesks: &[openpgp::packet::PKESK],
        _skesks: &[openpgp::packet::SKESK],
        _sym_algo: Option<SymmetricAlgorithm>,
        mut decrypt: D,
    ) -> openpgp::Result<Option<openpgp::Fingerprint>>
    where
        D: FnMut(SymmetricAlgorithm, &openpgp::crypto::SessionKey) -> bool,
    {
        for cert in &self.keys {
            for pkesk in pkesks {
                for ka in cert
                    .keys()
                    .secret()
                    .with_policy(&POLICY, None)
                    .supported()
                    .alive()
                    .revoked(false)
                {
                    let Ok(key) = ka.key().clone().parts_into_secret() else {
                        continue;
                    };
                    let Ok(mut keypair) = key.into_keypair() else {
                        continue;
                    };
                    if let Some((algo, session_key)) = pkesk.decrypt(&mut keypair, None) {
                        if decrypt(algo, &session_key) {
                            return Ok(Some(ka.fingerprint()));
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}

impl VerificationHelper for DecryptHelper {
    fn get_certs(&mut self, ids: &[KeyHandle]) -> openpgp::Result<Vec<Cert>> {
        Ok(self
            .keys
            .iter()
            .filter(|cert| ids.iter().any(|id| cert.fingerprint().aliases(id)))
            .cloned()
            .collect())
    }

    fn check(&mut self, structure: MessageStructure) -> openpgp::Result<()> {
        let mut counts = self.verdict.lock().unwrap();
        for layer in structure.iter() {
            if let MessageLayer::SignatureGroup { results } = layer {
                for result in results {
                    match result {
                        Ok(_) => counts.0 += 1,
                        Err(VerificationError::MissingKey { .. })
                        | Err(VerificationError::UnboundKey { .. }) => counts.2 += 1,
                        Err(_) => counts.1 += 1,
                    }
                }
            }
        }
        Ok(())
    }
}

/// Decrypt (and, when signed, verify) a message.
/// Returns `(plaintext, signature_status)`.
pub fn decrypt_message(
    encrypted: &str,
    recipient_secret_keys: &[Cert],
) -> Result<(String, SignatureStatus)> {
    let counts = Arc::new(Mutex::new((0usize, 0usize, 0usize)));
    let helper = DecryptHelper {
        keys: recipient_secret_keys.to_vec(),
        verdict: Arc::clone(&counts),
    };

    let mut decryptor = DecryptorBuilder::from_bytes(encrypted.as_bytes())
        .map_err(|e| Error::Protocol(e.to_string()))?
        .with_policy(&POLICY, None, helper)
        .map_err(|e| Error::Protocol(e.to_string()))?;

    let mut plaintext = String::new();
    decryptor
        .read_to_string(&mut plaintext)
        .map_err(|e| Error::Protocol(e.to_string()))?;

    let (good, bad, unknown) = *counts.lock().unwrap();
    Ok((plaintext, verdict(good, bad, unknown)))
}

/// Create a detached signature (armored) over `data` using `signer_cert`.
pub fn sign_detached(data: &[u8], signer_cert: &Cert, signer_secret: &str) -> Result<String> {
    let secret_cert = if signer_secret.trim().is_empty() {
        signer_cert.clone()
    } else {
        import_secret_key(signer_secret)?
    };
    let keypair = signing_keypair(&secret_cert)?;

    let mut sink = Vec::new();
    let message = Message::new(&mut sink);
    let mut signer = Signer::new(message, keypair)
        .detached()
        .hash_algo(openpgp::types::HashAlgorithm::SHA256)
        .map_err(|e| Error::Protocol(e.to_string()))?
        .creation_time(SystemTime::now())
        .build()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    signer
        .write_all(data)
        .map_err(|e| Error::Protocol(e.to_string()))?;
    signer
        .finalize()
        .map_err(|e| Error::Protocol(e.to_string()))?;

    let mut armored = Vec::new();
    let mut writer = openpgp::armor::Writer::new(&mut armored, openpgp::armor::Kind::Signature)
        .map_err(|e| Error::Protocol(e.to_string()))?;
    writer
        .write_all(&sink)
        .map_err(|e| Error::Protocol(e.to_string()))?;
    writer
        .finalize()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    String::from_utf8(armored).map_err(|e| Error::Protocol(e.to_string()))
}

/// Verify a detached signature over `data` against `signer_cert`.
pub fn verify_detached(
    data: &[u8],
    armored_signature: &str,
    signer_cert: &Cert,
) -> Result<SignatureStatus> {
    let counts = Arc::new(Mutex::new((0usize, 0usize, 0usize)));
    let helper = VerifyHelper {
        cert: signer_cert.clone(),
        verdict: Arc::clone(&counts),
    };

    let mut verifier = DetachedVerifierBuilder::from_bytes(armored_signature.as_bytes())
        .map_err(|e| Error::Protocol(e.to_string()))?
        .with_policy(&POLICY, None, helper)
        .map_err(|e| Error::Protocol(e.to_string()))?;
    verifier
        .verify_bytes(data)
        .map_err(|e| Error::Protocol(e.to_string()))?;

    let (good, bad, unknown) = *counts.lock().unwrap();
    Ok(verdict(good, bad, unknown))
}

/// Sign a message as a cleartext signature (RFC 4880 §7).
///
/// `signer_secret`, when non-empty, is an armored secret key; otherwise the
/// secret key material is expected to be present in `signer_cert`.
pub fn sign_message(plaintext: &str, signer_cert: &Cert, signer_secret: &str) -> Result<String> {
    let secret_cert = if signer_secret.trim().is_empty() {
        signer_cert.clone()
    } else {
        import_secret_key(signer_secret)?
    };
    let keypair = signing_keypair(&secret_cert)?;

    let mut sink = Vec::new();
    let message = Message::new(&mut sink);
    // Cleartext mode implies ASCII armor: no LiteralWriter/Armorer on the stack.
    let mut signer = Signer::new(message, keypair)
        .cleartext()
        .creation_time(SystemTime::now())
        .build()
        .map_err(|e| Error::Protocol(e.to_string()))?;

    signer
        .write_all(plaintext.as_bytes())
        .map_err(|e| Error::Protocol(e.to_string()))?;
    signer
        .finalize()
        .map_err(|e| Error::Protocol(e.to_string()))?;

    String::from_utf8(sink).map_err(|e| Error::Protocol(e.to_string()))
}

/// Verifier helper that trusts a single certificate.
struct VerifyHelper {
    cert: Cert,
    verdict: Arc<Mutex<(usize, usize, usize)>>,
}

impl VerificationHelper for VerifyHelper {
    fn get_certs(&mut self, _ids: &[KeyHandle]) -> openpgp::Result<Vec<Cert>> {
        Ok(vec![self.cert.clone()])
    }

    fn check(&mut self, structure: MessageStructure) -> openpgp::Result<()> {
        let mut counts = self.verdict.lock().unwrap();
        for layer in structure.iter() {
            if let MessageLayer::SignatureGroup { results } = layer {
                for result in results {
                    match result {
                        Ok(_) => counts.0 += 1,
                        Err(VerificationError::MissingKey { .. })
                        | Err(VerificationError::UnboundKey { .. }) => counts.2 += 1,
                        Err(_) => counts.1 += 1,
                    }
                }
            }
        }
        Ok(())
    }
}

/// Verify a cleartext signature against `signer_cert`.
pub fn verify_signature(signed_text: &str, signer_cert: &Cert) -> Result<SignatureStatus> {
    let counts = Arc::new(Mutex::new((0usize, 0usize, 0usize)));
    let helper = VerifyHelper {
        cert: signer_cert.clone(),
        verdict: Arc::clone(&counts),
    };

    let mut verifier = VerifierBuilder::from_bytes(signed_text.as_bytes())
        .map_err(|e| Error::Protocol(e.to_string()))?
        .with_policy(&POLICY, None, helper)
        .map_err(|e| Error::Protocol(e.to_string()))?;

    let mut output = Vec::new();
    verifier
        .read_to_end(&mut output)
        .map_err(|e| Error::Protocol(e.to_string()))?;

    let (good, bad, unknown) = *counts.lock().unwrap();
    Ok(verdict(good, bad, unknown))
}

/// Fetch a PGP key via WKD (Web Key Directory).
/// Returns the armored public key if found.
pub fn fetch_wkd_key(email: &str, client: &Agent) -> Result<Option<String>> {
    let (local_part, domain) = email
        .split_once('@')
        .ok_or_else(|| Error::Protocol("invalid email".into()))?;

    let hash = zbase32_hash(local_part);

    // Try the advanced (subdomain) method first, then the direct method.
    let urls = [
        format!("https://openpgpkey.{domain}/.well-known/openpgpkey/{domain}/hu/{hash}"),
        format!("https://{domain}/.well-known/openpgpkey/hu/{hash}"),
    ];

    for url in urls {
        let Ok(mut response) = client.get(&url).call() else {
            continue;
        };
        if response.status() == 200 {
            let key = response
                .body_mut()
                .read_to_string()
                .map_err(|e| Error::Protocol(e.to_string()))?;
            return Ok(Some(key));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_keypair() {
        let (public, secret) = generate_keypair("Test User <test@example.com>").unwrap();
        assert!(public.contains("-----BEGIN PGP PUBLIC KEY BLOCK-----"));
        assert!(secret.contains("-----BEGIN PGP PRIVATE KEY BLOCK-----"));
    }

    #[test]
    fn test_import_public_key() {
        let (public, _secret) = generate_keypair("Test <test@example.com>").unwrap();
        let cert = import_public_key(&public).unwrap();
        assert_eq!(key_fingerprint(&cert).len(), 40);
    }

    #[test]
    fn test_import_secret_key() {
        let (_public, secret) = generate_keypair("Test <test@example.com>").unwrap();
        let cert = import_secret_key(&secret).unwrap();
        assert_eq!(key_fingerprint(&cert).len(), 40);
        assert!(cert.is_tsk());
    }

    #[test]
    fn test_key_fingerprint() {
        let (public, _secret) = generate_keypair("Test <test@example.com>").unwrap();
        let cert = import_public_key(&public).unwrap();
        assert_eq!(key_fingerprint(&cert).len(), 40);
    }

    #[test]
    fn test_user_ids() {
        let (public, _secret) = generate_keypair("Alice <alice@example.com>").unwrap();
        let cert = import_public_key(&public).unwrap();
        let uids = key_user_ids(&cert);
        assert!(uids.iter().any(|u| u.contains("alice@example.com")));
    }

    #[test]
    fn test_zbase32_hash() {
        // SHA-1 output is 20 bytes => exactly 32 z-base-32 characters.
        let hash = zbase32_hash("test");
        assert_eq!(hash.len(), 32);
        assert!(hash.chars().all(|c| ZBASE32_ALPHABET.contains(&(c as u8))));
    }

    #[test]
    fn test_zbase32_rfc6189_vectors() {
        // Reference vectors for z-base-32.
        assert_eq!(zbase32_encode(b"foo"), "c3zs6");
        assert_eq!(zbase32_encode(b"foob"), "c3zs6ao");
        assert_eq!(zbase32_encode(b"fooba"), "c3zs6aub");
        assert_eq!(zbase32_encode(b"foobar"), "c3zs6aubqe");
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let (public, secret) = generate_keypair("Test <test@example.com>").unwrap();
        let cert = import_public_key(&public).unwrap();
        let secret_cert = import_secret_key(&secret).unwrap();

        let plaintext = "Hello, World!";
        let encrypted = encrypt_message(plaintext, &[cert], None, None).unwrap();
        assert!(encrypted.contains("-----BEGIN PGP MESSAGE-----"));
        let (decrypted, _status) = decrypt_message(&encrypted, &[secret_cert]).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_sign_verify_roundtrip() {
        let (public, secret) = generate_keypair("Test <test@example.com>").unwrap();
        let cert = import_public_key(&public).unwrap();
        let secret_cert = import_secret_key(&secret).unwrap();

        let plaintext = "Hello, World!";
        let signed = sign_message(plaintext, &secret_cert, &secret).unwrap();
        assert!(signed.contains("-----BEGIN PGP SIGNED MESSAGE-----"));
        let status = verify_signature(&signed, &cert).unwrap();
        assert_eq!(status, SignatureStatus::Valid);
    }

    #[test]
    fn test_encrypt_sign_decrypt_verify_roundtrip() {
        let (public, secret) = generate_keypair("Test <test@example.com>").unwrap();
        let cert = import_public_key(&public).unwrap();
        let secret_cert = import_secret_key(&secret).unwrap();

        let plaintext = "Signed and sealed.";
        let encrypted =
            encrypt_message(plaintext, &[cert.clone()], Some(&secret_cert), Some(&secret))
                .unwrap();
        let (decrypted, status) = decrypt_message(&encrypted, &[secret_cert]).unwrap();
        assert_eq!(decrypted, plaintext);
        assert_eq!(status, SignatureStatus::Valid);
    }

    #[test]
    fn test_verify_tampered_signature_is_invalid() {
        let (public, secret) = generate_keypair("Test <test@example.com>").unwrap();
        let cert = import_public_key(&public).unwrap();
        let secret_cert = import_secret_key(&secret).unwrap();

        let signed = sign_message("Hello, World!", &secret_cert, &secret).unwrap();
        let tampered = signed.replace("Hello, World!", "Hello, Wrold!");
        assert_eq!(
            verify_signature(&tampered, &cert).unwrap(),
            SignatureStatus::Invalid
        );
    }

    #[test]
    fn test_detached_sign_and_verify() {
        let (public, secret) = generate_keypair("Test <test@example.com>").unwrap();
        let cert = import_public_key(&public).unwrap();
        let secret_cert = import_secret_key(&secret).unwrap();

        let payload = b"the quick brown fox";
        let signature = sign_detached(payload, &secret_cert, &secret).unwrap();
        assert!(signature.contains("-----BEGIN PGP SIGNATURE-----"));
        assert_eq!(
            verify_detached(payload, &signature, &cert).unwrap(),
            SignatureStatus::Valid
        );
        assert_eq!(
            verify_detached(b"the quick brown fix", &signature, &cert).unwrap(),
            SignatureStatus::Invalid
        );
    }

    #[test]
    fn test_decrypt_wrong_key_reports_error() {
        let (public, _secret) = generate_keypair("A <a@example.com>").unwrap();
        let (_other_public, other_secret) = generate_keypair("B <b@example.com>").unwrap();
        let cert = import_public_key(&public).unwrap();
        let wrong = import_secret_key(&other_secret).unwrap();

        let encrypted = encrypt_message("secret", &[cert], None, None).unwrap();
        assert!(decrypt_message(&encrypted, &[wrong]).is_err());
    }
}
