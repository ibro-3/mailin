//! Core of the Mailin desktop mail client.
//!
//! The crate is UI agnostic and is shared by the GTK 4 (Linux) and WinUI 3
//! (Windows) front-ends. It owns accounts, credentials, the local SQLite cache
//! and, in later milestones, IMAP/SMTP synchronization.
//!
//! Two rules keep both front-ends simple:
//!
//! * everything is synchronous and cheap to call from a UI thread except
//!   network work, which lives behind the [`Mailin`] façade;
//! * accounts live in the local database, application preferences live in
//!   `config.toml`. Passwords never touch either: they are kept in the OS
//!   keyring through [`CredentialsStore`].

pub mod account;
pub mod client;
pub mod compose;
pub mod config;
pub mod credentials;
pub mod engine;
pub mod error;
mod imap;
pub mod model;
mod smtp;
pub mod store;
mod sync;
mod tls;

pub use account::{Account, AccountId, AccountSyncSettings, Security, ServerSettings};
pub use client::{Event, Mailin};
pub use compose::{ComposeAttachment, OutgoingMessage};
pub use config::{Config, Paths, SyncPreferences, Theme, UiPreferences};
pub use credentials::{CredentialsStore, MemoryCredentials};
pub use engine::Engine;
pub use error::{Error, Result};
pub use model::{
    AttachmentData, AttachmentMeta, Folder, FolderId, FolderRole, FolderSyncState, MessageHeader,
    MessageId, NewMessage, OutboxEntry, OutboxState, OutgoingEnvelope, UpsertedMessage,
};
pub use store::{FolderUpsert, Store};

#[cfg(feature = "keyring")]
pub use credentials::KeyringCredentials;

/// Current Unix timestamp in seconds.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}
