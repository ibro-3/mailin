use crate::account::AccountId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

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

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("internal store lock is poisoned")]
    LockPoisoned,
}

pub type Result<T> = std::result::Result<T, Error>;
