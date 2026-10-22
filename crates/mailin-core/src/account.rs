use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

/// Stable identifier of an account.
///
/// Accounts are stored as JSON in the local database, so the identifier is a
/// random UUID rather than a database row id; that keeps it stable across
/// account renames and folder pruning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(Uuid);

impl AccountId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(value: &str) -> Result<Self> {
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|_| Error::InvalidAccountId(value.to_owned()))
    }
}

impl Default for AccountId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for AccountId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// How the client connects to an IMAP or SMTP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Security {
    /// Implicit TLS, usually port 993 (IMAP) or 465 (SMTP).
    #[default]
    Tls,
    /// Upgrade a plain connection with `STARTTLS`.
    StartTls,
    /// Unencrypted. Only meant for local testing.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub security: Security,
    pub username: String,
}

impl ServerSettings {
    pub fn imap(host: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 993,
            security: Security::Tls,
            username: username.into(),
        }
    }

    pub fn imap_starttls(host: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 143,
            security: Security::StartTls,
            username: username.into(),
        }
    }

    pub fn smtp(host: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 465,
            security: Security::Tls,
            username: username.into(),
        }
    }

    pub fn smtp_starttls(host: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 587,
            security: Security::StartTls,
            username: username.into(),
        }
    }
}

/// Per-account synchronization preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountSyncSettings {
    /// Full IMAP paths to sync. Empty means every folder the server offers,
    /// except those marked `\Noselect`.
    pub folder_allowlist: Vec<String>,
    /// Idle watchers fall back to polling with this interval.
    pub poll_interval_secs: u64,
    /// Keep the connection and refresh folders in the background.
    pub background_sync: bool,
}

impl Default for AccountSyncSettings {
    fn default() -> Self {
        Self {
            folder_allowlist: Vec::new(),
            poll_interval_secs: 60,
            background_sync: true,
        }
    }
}

/// A configured mail account. Passwords are stored separately in the keyring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub display_name: String,
    pub email: String,
    pub imap: ServerSettings,
    pub smtp: ServerSettings,
    #[serde(default)]
    pub sync: AccountSyncSettings,
}

impl Account {
    pub fn new(
        display_name: impl Into<String>,
        email: impl Into<String>,
        imap: ServerSettings,
        smtp: ServerSettings,
    ) -> Self {
        Self {
            id: AccountId::new(),
            display_name: display_name.into(),
            email: email.into(),
            imap,
            smtp,
            sync: AccountSyncSettings::default(),
        }
    }

    /// Local part of the address, used to prefill the IMAP/SMTP username.
    pub fn local_part(&self) -> &str {
        self.email
            .split_once('@')
            .map_or(self.email.as_str(), |(l, _)| l)
    }

    pub fn domain(&self) -> Option<&str> {
        let (_, domain) = self.email.rsplit_once('@')?;
        (!domain.is_empty()).then_some(domain)
    }

    pub fn validate(&self) -> Result<()> {
        let (local, domain) = self
            .email
            .split_once('@')
            .ok_or_else(|| Error::InvalidAccount("email address must contain '@'".to_owned()))?;
        if local.is_empty() || domain.is_empty() || self.email.contains(char::is_whitespace) {
            return Err(Error::InvalidAccount(format!(
                "invalid email address: {}",
                self.email
            )));
        }
        for (label, server) in [("IMAP", &self.imap), ("SMTP", &self.smtp)] {
            if server.host.trim().is_empty() {
                return Err(Error::InvalidAccount(format!("{label} host is empty")));
            }
            if server.host.contains(char::is_whitespace) {
                return Err(Error::InvalidAccount(format!(
                    "{label} host contains whitespace: {}",
                    server.host
                )));
            }
            if server.port == 0 {
                return Err(Error::InvalidAccount(format!("{label} port is zero")));
            }
            if server.username.trim().is_empty() {
                return Err(Error::InvalidAccount(format!("{label} username is empty")));
            }
        }
        if self.sync.poll_interval_secs < 15 {
            return Err(Error::InvalidAccount(
                "poll interval must be at least 15 seconds".to_owned(),
            ));
        }
        if self.display_name.trim().is_empty() {
            return Err(Error::InvalidAccount("display name is empty".to_owned()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account {
        Account::new(
            "Ada",
            "ada@example.com",
            ServerSettings::imap("imap.example.com", "ada@example.com"),
            ServerSettings::smtp_starttls("smtp.example.com", "ada@example.com"),
        )
    }

    #[test]
    fn valid_account_passes_validation() {
        let acc = account();
        acc.validate().expect("valid account");
        assert_eq!(acc.local_part(), "ada");
        assert_eq!(acc.domain(), Some("example.com"));
    }

    #[test]
    fn invalid_accounts_are_rejected() {
        let mut acc = account();
        acc.email = "no-at-sign".to_owned();
        assert!(acc.validate().is_err());

        let mut acc = account();
        acc.imap.host = "  ".to_owned();
        assert!(acc.validate().is_err());

        let mut acc = account();
        acc.smtp.port = 0;
        assert!(acc.validate().is_err());

        let mut acc = account();
        acc.sync.poll_interval_secs = 5;
        assert!(acc.validate().is_err());
    }

    #[test]
    fn account_round_trips_through_json() {
        let acc = account();
        let json = serde_json::to_string(&acc).expect("serialize");
        let back: Account = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(acc, back);
    }

    #[test]
    fn security_kebab_case() {
        let json = serde_json::to_string(&Security::StartTls).expect("serialize");
        assert_eq!(json, "\"start-tls\"");
    }

    #[test]
    fn account_id_parses_and_formats() {
        let id = AccountId::new();
        assert_eq!(AccountId::parse(&id.to_string()).expect("parse"), id);
        assert!(AccountId::parse("nope").is_err());
    }
}
