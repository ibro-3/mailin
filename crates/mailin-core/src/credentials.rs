use std::collections::HashMap;
#[cfg(feature = "keyring")]
use std::fmt;
use std::sync::Mutex;

use crate::account::AccountId;
use crate::error::{Error, Result};
#[cfg(any(feature = "keyring", test))]
use secrecy::ExposeSecret;
use secrecy::SecretString;

/// Where account passwords live.
///
/// Both front-ends use the platform keyring, but the trait keeps the core
/// testable without a running Secret Service or Credential Manager.
/// Passwords are handled as `SecretString` to prevent accidental logging
/// and to zeroize memory on drop.
pub trait CredentialsStore: Send + Sync {
    fn set(&self, account: &AccountId, secret: &SecretString) -> Result<()>;
    fn get(&self, account: &AccountId) -> Result<Option<SecretString>>;
    fn delete(&self, account: &AccountId) -> Result<()>;

    /// Store the armored secret key identified by `fingerprint`.
    fn set_pgp_secret(
        &self,
        account: &AccountId,
        fingerprint: &str,
        secret: &SecretString,
    ) -> Result<()>;

    /// Load the armored secret key identified by `fingerprint`.
    fn get_pgp_secret(
        &self,
        account: &AccountId,
        fingerprint: &str,
    ) -> Result<Option<SecretString>>;

    /// Remove the armored secret key identified by `fingerprint`.
    fn delete_pgp_secret(&self, account: &AccountId, fingerprint: &str) -> Result<()>;

    /// Store the CalDAV password for an account.
    fn set_caldav_password(&self, account: &AccountId, secret: &SecretString) -> Result<()>;

    /// Load the CalDAV password for an account.
    fn get_caldav_password(&self, account: &AccountId) -> Result<Option<SecretString>>;

    /// Remove the CalDAV password for an account.
    fn delete_caldav_password(&self, account: &AccountId) -> Result<()>;
}

/// In-memory store for tests and for `--self-test` runs.
#[derive(Debug, Default)]
pub struct MemoryCredentials {
    secrets: Mutex<HashMap<AccountId, SecretString>>,
    pgp_secrets: Mutex<HashMap<(AccountId, String), SecretString>>,
    caldav_secrets: Mutex<HashMap<AccountId, SecretString>>,
}

impl MemoryCredentials {
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self) -> Result<std::sync::MutexGuard<'_, HashMap<AccountId, SecretString>>> {
        self.secrets.lock().map_err(|_| Error::LockPoisoned)
    }

    fn pgp_guard(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<(AccountId, String), SecretString>>> {
        self.pgp_secrets.lock().map_err(|_| Error::LockPoisoned)
    }
}

impl CredentialsStore for MemoryCredentials {
    fn set(&self, account: &AccountId, secret: &SecretString) -> Result<()> {
        self.guard()?.insert(*account, secret.clone());
        Ok(())
    }

    fn get(&self, account: &AccountId) -> Result<Option<SecretString>> {
        Ok(self.guard()?.get(account).cloned())
    }

    fn delete(&self, account: &AccountId) -> Result<()> {
        self.guard()?.remove(account);
        Ok(())
    }

    fn set_pgp_secret(
        &self,
        account: &AccountId,
        fingerprint: &str,
        secret: &SecretString,
    ) -> Result<()> {
        self.pgp_guard()?
            .insert((*account, fingerprint.to_owned()), secret.clone());
        Ok(())
    }

    fn get_pgp_secret(
        &self,
        account: &AccountId,
        fingerprint: &str,
    ) -> Result<Option<SecretString>> {
        Ok(self
            .pgp_guard()?
            .get(&(*account, fingerprint.to_owned()))
            .cloned())
    }

    fn delete_pgp_secret(&self, account: &AccountId, fingerprint: &str) -> Result<()> {
        self.pgp_guard()?
            .remove(&(*account, fingerprint.to_owned()));
        Ok(())
    }

    fn set_caldav_password(&self, account: &AccountId, secret: &SecretString) -> Result<()> {
        self.caldav_secrets
            .lock()
            .map_err(|_| Error::LockPoisoned)?
            .insert(*account, secret.clone());
        Ok(())
    }

    fn get_caldav_password(&self, account: &AccountId) -> Result<Option<SecretString>> {
        Ok(self
            .caldav_secrets
            .lock()
            .map_err(|_| Error::LockPoisoned)?
            .get(account)
            .cloned())
    }

    fn delete_caldav_password(&self, account: &AccountId) -> Result<()> {
        self.caldav_secrets
            .lock()
            .map_err(|_| Error::LockPoisoned)?
            .remove(account);
        Ok(())
    }
}

/// OS keyring backed store: Secret Service (GNOME Keyring, KWallet) on Linux
/// and Credential Manager on Windows.
#[cfg(feature = "keyring")]
pub struct KeyringCredentials {
    service: String,
}

#[cfg(feature = "keyring")]
impl KeyringCredentials {
    pub fn new() -> Self {
        Self {
            service: "dev.mailin.Mailin".to_owned(),
        }
    }

    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, account: &AccountId) -> Result<keyring::Entry> {
        self.entry_named(&account.to_string())
    }

    fn entry_pgp(&self, account: &AccountId, fingerprint: &str) -> Result<keyring::Entry> {
        self.entry_named(&format!("{account}:pgp:{fingerprint}"))
    }

    fn entry_named(&self, user: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, user).map_err(|e| Error::Credentials(e.to_string()))
    }
}

#[cfg(feature = "keyring")]
impl Default for KeyringCredentials {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "keyring")]
impl fmt::Debug for KeyringCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyringCredentials")
            .field("service", &self.service)
            .finish()
    }
}

#[cfg(feature = "keyring")]
impl CredentialsStore for KeyringCredentials {
    fn set(&self, account: &AccountId, secret: &SecretString) -> Result<()> {
        self.entry(account)?
            .set_password(secret.expose_secret())
            .map_err(|e| Error::Credentials(e.to_string()))
    }

    fn get(&self, account: &AccountId) -> Result<Option<SecretString>> {
        match self.entry(account)?.get_password() {
            Ok(secret) => Ok(Some(SecretString::new(secret.into()))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::Credentials(e.to_string())),
        }
    }

    fn delete(&self, account: &AccountId) -> Result<()> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::Credentials(e.to_string())),
        }
    }

    fn set_pgp_secret(
        &self,
        account: &AccountId,
        fingerprint: &str,
        secret: &SecretString,
    ) -> Result<()> {
        self.entry_pgp(account, fingerprint)?
            .set_password(secret.expose_secret())
            .map_err(|e| Error::Credentials(e.to_string()))
    }

    fn get_pgp_secret(
        &self,
        account: &AccountId,
        fingerprint: &str,
    ) -> Result<Option<SecretString>> {
        match self.entry_pgp(account, fingerprint)?.get_password() {
            Ok(secret) => Ok(Some(SecretString::new(secret.into()))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::Credentials(e.to_string())),
        }
    }

    fn delete_pgp_secret(&self, account: &AccountId, fingerprint: &str) -> Result<()> {
        match self.entry_pgp(account, fingerprint)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::Credentials(e.to_string())),
        }
    }

    fn set_caldav_password(&self, account: &AccountId, secret: &SecretString) -> Result<()> {
        self.entry_caldav(account)?
            .set_password(secret.expose_secret())
            .map_err(|e| Error::Credentials(e.to_string()))
    }

    fn get_caldav_password(&self, account: &AccountId) -> Result<Option<SecretString>> {
        match self.entry_caldav(account)?.get_password() {
            Ok(secret) => Ok(Some(SecretString::new(secret.into()))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::Credentials(e.to_string())),
        }
    }

    fn delete_caldav_password(&self, account: &AccountId) -> Result<()> {
        match self.entry_caldav(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::Credentials(e.to_string())),
        }
    }
}

#[cfg(feature = "keyring")]
impl KeyringCredentials {
    fn entry_caldav(&self, account: &AccountId) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, &format!("{}:caldav", account))
            .map_err(|e| Error::Credentials(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;

    #[test]
    fn memory_store_round_trips() {
        let store = MemoryCredentials::new();
        let account = AccountId::new();
        assert!(store.get(&account).expect("get").is_none());
        store
            .set(&account, &SecretString::new("hunter2".into()))
            .expect("set");
        let secret = store.get(&account).expect("get").expect("secret");
        assert_eq!(secret.expose_secret(), "hunter2");
        store
            .set(&account, &SecretString::new("rotated".into()))
            .expect("set");
        let secret = store.get(&account).expect("get").expect("secret");
        assert_eq!(secret.expose_secret(), "rotated");
        store.delete(&account).expect("delete");
        assert!(store.get(&account).expect("get").is_none());
        store.delete(&account).expect("delete is idempotent");
    }

    #[test]
    fn accounts_are_isolated() {
        let store = MemoryCredentials::new();
        let a = AccountId::new();
        let b = AccountId::new();
        store
            .set(&a, &SecretString::new("a-secret".into()))
            .expect("set");
        assert!(store.get(&b).expect("get").is_none());
    }

    #[test]
    fn pgp_secrets_round_trip_per_account_and_fingerprint() {
        let store = MemoryCredentials::new();
        let a = AccountId::new();
        let b = AccountId::new();
        let fp = "AAAA1111BBBB2222CCCC3333DDDD4444EEEE5555";
        assert!(store.get_pgp_secret(&a, fp).expect("get").is_none());
        store
            .set_pgp_secret(&a, fp, &SecretString::new("secret-key".into()))
            .expect("set");
        assert_eq!(
            store
                .get_pgp_secret(&a, fp)
                .expect("get")
                .expect("secret")
                .expose_secret(),
            "secret-key"
        );
        // Another account cannot see the key.
        assert!(store.get_pgp_secret(&b, fp).expect("get").is_none());
        store.delete_pgp_secret(&a, fp).expect("delete");
        assert!(store.get_pgp_secret(&a, fp).expect("get").is_none());
        store.delete_pgp_secret(&a, fp).expect("delete idempotent");
    }
}
