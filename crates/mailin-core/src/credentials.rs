use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use crate::account::AccountId;
use crate::error::{Error, Result};
use secrecy::{ExposeSecret, SecretString};

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
}

/// In-memory store for tests and for `--self-test` runs.
#[derive(Debug, Default)]
pub struct MemoryCredentials {
    secrets: Mutex<HashMap<AccountId, SecretString>>,
}

impl MemoryCredentials {
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self) -> Result<std::sync::MutexGuard<'_, HashMap<AccountId, SecretString>>> {
        self.secrets.lock().map_err(|_| Error::LockPoisoned)
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
        keyring::Entry::new(&self.service, &account.to_string())
            .map_err(|e| Error::Credentials(e.to_string()))
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
}
