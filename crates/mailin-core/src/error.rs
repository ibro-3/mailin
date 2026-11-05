use crate::account::AccountId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("could not {action}: {reason}")]
    ActionFailed { action: String, reason: String },

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid account: {0}")]
    InvalidAccount(String),

    #[error("invalid account id: {0}")]
    InvalidAccountId(String),

    #[error("account not found: {0}")]
    AccountNotFound(AccountId),

    #[error("folder not found: {0}")]
    FolderNotFound(i64),

    #[error("credentials error: {0}")]
    Credentials(String),

    #[error("IMAP error: {0}")]
    Imap(String),

    #[error("SMTP error: {0}")]
    Smtp(String),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("OAuth2 error: {0}")]
    OAuth(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("plugin error: {0}")]
    Plugin(String),

    #[error("internal store lock is poisoned")]
    LockPoisoned,
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<mlua::Error> for Error {
    fn from(e: mlua::Error) -> Self {
        Error::Plugin(e.to_string())
    }
}

/// Broad reason an operation failed, phrase-matched from the underlying
/// error text.
enum Reason {
    Auth,
    NotFound,
    Network,
    Timeout,
    Certificate,
    Other,
}

fn reason_key(reason: &str) -> Reason {
    let lower = reason.to_lowercase();
    if lower.contains("authenticat")
        || lower.contains("login")
        || lower.contains("password")
        || lower.contains("credential")
        || lower.contains("auth")
        || lower.contains("not authorized")
    {
        return Reason::Auth;
    }
    if lower.contains("not exist")
        || lower.contains("does not exist")
        || lower.contains("no such")
        || lower.contains("not found")
        || lower.contains("missing")
    {
        return Reason::NotFound;
    }
    if lower.contains("timeout") || lower.contains("timed out") {
        return Reason::Timeout;
    }
    if lower.contains("certificate")
        || lower.contains("tls")
        || lower.contains("ssl")
        || lower.contains("untrusted")
    {
        return Reason::Certificate;
    }
    if lower.contains("connection")
        || lower.contains("refused")
        || lower.contains("reset")
        || lower.contains("dns")
        || lower.contains("name or service")
        || lower.contains("network unreachable")
        || lower.contains("no route")
        || lower.contains("unreachable")
        || lower.contains("offline")
    {
        return Reason::Network;
    }
    Reason::Other
}

/// First line of a possibly multi-line error, trimmed and capped so banners
/// stay readable.
fn reason_headline(reason: &str) -> String {
    let headline = reason.lines().next().unwrap_or(reason).trim();
    if headline.chars().count() > 140 {
        return headline
            .chars()
            .take(139)
            .collect::<String>()
            .trim_end()
            .to_owned();
    }
    headline.to_owned()
}

fn friendly_action(action: &str, reason: &str) -> String {
    let headline = format!("Couldn't {action}.");
    match reason_key(reason) {
        Reason::Auth => {
            format!("{headline} The server rejected the sign-in — check the account credentials.")
        }
        Reason::NotFound => format!("{headline} A required item is missing on the server."),
        Reason::Network => format!("{headline} Check your internet connection."),
        Reason::Timeout => format!("{headline} The server took too long to respond."),
        Reason::Certificate => {
            format!("{headline} The server's security certificate could not be verified.")
        }
        Reason::Other => format!("{headline} {}", reason_headline(reason)),
    }
}

impl Error {
    /// User-facing text for the error. The technical details go to the
    /// log (the engine logs every failure before publishing it); what the
    /// user reads should say what happened in plain words.
    pub fn friendly(&self) -> String {
        match self {
            Error::ActionFailed { action, reason } => friendly_action(action, reason),
            Error::Database(_) => {
                "The local mail database failed. Restart Mailin and try again.".into()
            }
            Error::Io(io) => match io.kind() {
                std::io::ErrorKind::ConnectionRefused => {
                    "The server refused the connection.".into()
                }
                std::io::ErrorKind::ConnectionReset => {
                    "The connection was reset by the server.".into()
                }
                std::io::ErrorKind::NotConnected | std::io::ErrorKind::UnexpectedEof => {
                    "The connection dropped before the operation finished.".into()
                }
                std::io::ErrorKind::TimedOut => {
                    "The server took too long to respond. Check your connection.".into()
                }
                std::io::ErrorKind::NetworkUnreachable | std::io::ErrorKind::HostUnreachable => {
                    "Check your internet connection.".into()
                }
                _ => format!(
                    "A connection error occurred: {}.",
                    reason_headline(&io.to_string())
                ),
            },
            Error::Json(_) => "The local data could not be read.".into(),
            Error::InvalidAccount(_) | Error::InvalidAccountId(_) => {
                "The account settings are invalid.".into()
            }
            Error::AccountNotFound(_) => "The account no longer exists.".into(),
            Error::FolderNotFound(_) => "The folder does not exist on the server.".into(),
            Error::Credentials(_) => "The stored credentials are missing or invalid.".into(),
            Error::Imap(text) => match reason_key(text) {
                Reason::Auth => {
                    "The server rejected the sign-in — check the account credentials.".into()
                }
                Reason::NotFound => "A folder or mailbox on the server is missing.".into(),
                Reason::Network => "Check your internet connection.".into(),
                Reason::Timeout => "The server took too long to respond.".into(),
                Reason::Certificate => {
                    "The server's security certificate could not be verified.".into()
                }
                Reason::Other => format!("Mail couldn't be synced: {}", reason_headline(text)),
            },
            Error::Smtp(text) => match reason_key(text) {
                Reason::Auth => {
                    "The outgoing server rejected the sign-in — check the account credentials."
                        .into()
                }
                Reason::Network => "Check your internet connection.".into(),
                Reason::Timeout => "The outgoing server took too long to respond.".into(),
                _ => format!("The message could not be sent: {}", reason_headline(text)),
            },
            Error::Tls(_) => "The server's security certificate could not be verified.".into(),
            Error::OAuth(_) => {
                "Sign-in expired or was rejected. Connect again in Account Settings.".into()
            }
            Error::Protocol(_) => {
                "The server sent something unexpected. Check the account settings.".into()
            }
            Error::Plugin(_) => "A plugin failed.".into(),
            Error::Config(_) => "The account configuration is invalid.".into(),
            Error::LockPoisoned => {
                "The application is in an inconsistent state. Restart Mailin.".into()
            }
        }
    }
}
