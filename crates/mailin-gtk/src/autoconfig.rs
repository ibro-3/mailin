//! Server host and port suggestions for the account wizard.
//!
//! This is deliberately not a full autoconfig implementation (no DNS SRV,
//! autodiscover or Thunderbird style lookups). It covers the common providers
//! and otherwise guesses the conventional `imap.<domain>` and
//! `smtp.<domain>` hosts, which the user can correct in the wizard.

use mailin_core::Security;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
}

impl Suggestion {
    fn new(
        imap_host: impl Into<String>,
        imap_port: u16,
        imap_security: Security,
        smtp_host: impl Into<String>,
        smtp_port: u16,
        smtp_security: Security,
    ) -> Self {
        Self {
            imap_host: imap_host.into(),
            imap_port,
            imap_security,
            smtp_host: smtp_host.into(),
            smtp_port,
            smtp_security,
        }
    }
}

/// Suggest server settings based on the address domain. Returns `None` when
/// the input does not look like an email address.
pub fn suggest(email: &str) -> Option<Suggestion> {
    let (_, domain) = email.rsplit_once('@')?;
    let domain = domain.trim().to_ascii_lowercase();
    if domain.is_empty() {
        return None;
    }

    let known = match domain.as_str() {
        "gmail.com" | "googlemail.com" => Suggestion::new(
            "imap.gmail.com",
            993,
            Security::Tls,
            "smtp.gmail.com",
            465,
            Security::Tls,
        ),
        "outlook.com" | "hotmail.com" | "live.com" | "outlook.de" => Suggestion::new(
            "outlook.office365.com",
            993,
            Security::Tls,
            "smtp.office365.com",
            587,
            Security::StartTls,
        ),
        "yahoo.com" | "yahoo.co.uk" | "ymail.com" => Suggestion::new(
            "imap.mail.yahoo.com",
            993,
            Security::Tls,
            "smtp.mail.yahoo.com",
            465,
            Security::Tls,
        ),
        "icloud.com" | "me.com" | "mac.com" => Suggestion::new(
            "imap.mail.me.com",
            993,
            Security::Tls,
            "smtp.mail.me.com",
            587,
            Security::StartTls,
        ),
        "fastmail.com" | "fastmail.fm" => Suggestion::new(
            "imap.fastmail.com",
            993,
            Security::Tls,
            "smtp.fastmail.com",
            465,
            Security::Tls,
        ),
        "gmx.net" | "gmx.de" | "gmx.com" => Suggestion::new(
            "imap.gmx.net",
            993,
            Security::Tls,
            "mail.gmx.net",
            587,
            Security::StartTls,
        ),
        "proton.me" | "protonmail.com" => {
            // Proton exposes IMAP only through Proton Bridge on localhost.
            return None;
        }
        _ => Suggestion::new(
            format!("imap.{domain}"),
            993,
            Security::Tls,
            format!("smtp.{domain}"),
            465,
            Security::Tls,
        ),
    };

    Some(known)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_providers_are_mapped() {
        let gmail = suggest("ada@gmail.com").expect("gmail");
        assert_eq!(gmail.imap_host, "imap.gmail.com");
        assert_eq!(gmail.smtp_security, Security::Tls);

        let outlook = suggest("ada@outlook.com").expect("outlook");
        assert_eq!(outlook.smtp_port, 587);
        assert_eq!(outlook.smtp_security, Security::StartTls);
    }

    #[test]
    fn unknown_domains_get_conventional_hosts() {
        let suggestion = suggest("ada@example.com").expect("suggestion");
        assert_eq!(suggestion.imap_host, "imap.example.com");
        assert_eq!(suggestion.smtp_host, "smtp.example.com");
    }

    #[test]
    fn invalid_addresses_and_proton_bridge_are_rejected() {
        assert!(suggest("nope").is_none());
        assert!(suggest("ada@").is_none());
        assert!(suggest("ada@proton.me").is_none());
    }
}
