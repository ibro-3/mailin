use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::account::AccountId;

/// Local row identifier of a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FolderId(pub i64);

impl fmt::Display for FolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Local row identifier of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(pub i64);

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Well known folder roles, used by the UIs to pin Inbox, Sent, Drafts and so
/// on without hard coding server specific names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FolderRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    #[default]
    Other,
}

impl FolderRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::Archive => "archive",
            Self::Other => "other",
        }
    }

    /// Map the IMAP `SPECIAL-USE` attributes (RFC 6154) plus the common
    /// non-standard names to a role.
    pub fn from_imap(name: &str, attributes: &[String]) -> Self {
        let has = |needle: &str| attributes.iter().any(|a| a.eq_ignore_ascii_case(needle));

        if has("\\Inbox") || name.eq_ignore_ascii_case("INBOX") {
            return Self::Inbox;
        }
        if has("\\Sent") || has("\\SentMail") {
            return Self::Sent;
        }
        if has("\\Drafts") {
            return Self::Drafts;
        }
        if has("\\Trash") {
            return Self::Trash;
        }
        if has("\\Junk") || has("\\Spam") {
            return Self::Junk;
        }
        if has("\\Archive") || has("\\All") || has("\\AllMail") {
            return Self::Archive;
        }

        let leaf = name.rsplit(['/', '.']).next().unwrap_or(name);
        let leaf = leaf.to_ascii_lowercase();
        match leaf.as_str() {
            "sent" | "sent items" | "sent messages" | "sent mail" => Self::Sent,
            "drafts" | "draft" => Self::Drafts,
            "trash" | "deleted" | "deleted items" | "deleted messages" => Self::Trash,
            "junk" | "spam" | "junk e-mail" | "bulk mail" => Self::Junk,
            "archive" | "all mail" | "all" => Self::Archive,
            _ => Self::Other,
        }
    }

    /// Sort rank used when listing folders (Inbox first).
    pub const fn rank(self) -> u8 {
        match self {
            Self::Inbox => 0,
            Self::Drafts => 1,
            Self::Sent => 2,
            Self::Archive => 3,
            Self::Junk => 4,
            Self::Trash => 5,
            Self::Other => 6,
        }
    }
}

impl FromStr for FolderRole {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, ()> {
        Ok(match value {
            "inbox" => Self::Inbox,
            "sent" => Self::Sent,
            "drafts" => Self::Drafts,
            "trash" => Self::Trash,
            "junk" => Self::Junk,
            "archive" => Self::Archive,
            "other" => Self::Other,
            _ => return Err(()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: FolderId,
    pub account_id: AccountId,
    /// Full IMAP path, for example `INBOX` or `Archive/2025`.
    pub name: String,
    pub delimiter: String,
    pub role: FolderRole,
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub highest_modseq: Option<u64>,
    pub last_synced_at: Option<i64>,
    pub message_count: u32,
    pub unread_count: u32,
}

/// Cursor persisted between sync runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderSyncState {
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub highest_modseq: Option<u64>,
    pub last_synced_at: Option<i64>,
}

impl Folder {
    pub fn sync_state(&self) -> FolderSyncState {
        FolderSyncState {
            uid_validity: self.uid_validity,
            uid_next: self.uid_next,
            highest_modseq: self.highest_modseq,
            last_synced_at: self.last_synced_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageHeader {
    pub id: MessageId,
    pub folder_id: FolderId,
    pub uid: u32,
    pub message_id: Option<String>,
    /// Root of the `References` chain, used later for conversation grouping.
    pub thread_key: Option<String>,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_addr: Option<String>,
    /// Comma separated list of recipients, formatted for display.
    pub to_addrs: String,
    pub cc_addrs: String,
    pub sent_at: Option<i64>,
    pub received_at: i64,
    pub snippet: Option<String>,
    pub size: Option<u32>,
    pub flags: Vec<String>,
    pub has_attachments: bool,
    pub body_fetched: bool,
}

impl MessageHeader {
    pub fn is_seen(&self) -> bool {
        self.has_flag("\\Seen")
    }

    pub fn is_flagged(&self) -> bool {
        self.has_flag("\\Flagged")
    }

    pub fn is_draft(&self) -> bool {
        self.has_flag("\\Draft")
    }

    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
    }

    /// `"Ada Lovelace" <ada@example.com>` when both parts are known.
    pub fn from_display(&self) -> String {
        match (&self.from_name, &self.from_addr) {
            (Some(name), Some(addr)) if !name.is_empty() => format!("{name} <{addr}>"),
            (_, Some(addr)) => addr.clone(),
            (Some(name), None) => name.clone(),
            (None, None) => "(unknown sender)".to_owned(),
        }
    }
}

impl fmt::Display for MessageHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} — {}", self.from_display(), self.subject)
    }
}

/// A message discovered on the server that should be inserted or refreshed in
/// the local cache. UID is only unique within a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessage {
    pub folder_id: FolderId,
    pub uid: u32,
    pub message_id: Option<String>,
    pub thread_key: Option<String>,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_addr: Option<String>,
    pub to_addrs: String,
    pub cc_addrs: String,
    pub sent_at: Option<i64>,
    pub received_at: i64,
    pub snippet: Option<String>,
    pub size: Option<u32>,
    pub flags: Vec<String>,
    pub has_attachments: bool,
}

impl NewMessage {
    pub fn new(folder_id: FolderId, uid: u32) -> Self {
        Self {
            folder_id,
            uid,
            message_id: None,
            thread_key: None,
            subject: String::new(),
            from_name: None,
            from_addr: None,
            to_addrs: String::new(),
            cc_addrs: String::new(),
            sent_at: None,
            received_at: crate::now_unix(),
            snippet: None,
            size: None,
            flags: Vec::new(),
            has_attachments: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpsertedMessage {
    pub id: MessageId,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMeta {
    /// Identifier of the MIME part inside its message, unique per message.
    /// Attachment downloads will map this back to a part in a later milestone.
    pub part_id: String,
    pub filename: Option<String>,
    pub mime_type: String,
    pub size: Option<u32>,
    pub content_id: Option<String>,
    pub is_inline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutboxState {
    #[default]
    Queued,
    Sending,
    Sent,
    Failed,
}

impl OutboxState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }

    pub fn from_store(value: &str) -> Self {
        match value {
            "sending" => Self::Sending,
            "sent" => Self::Sent,
            "failed" => Self::Failed,
            _ => Self::Queued,
        }
    }
}

/// A message waiting to be submitted over SMTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    pub id: i64,
    pub account_id: AccountId,
    pub raw: Vec<u8>,
    pub envelope_json: String,
    pub state: OutboxState,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// SMTP envelope stored alongside a queued message.
///
/// The composer stores it explicitly instead of letting the SMTP layer read the
/// headers: Bcc recipients must receive the message without a `Bcc` header
/// leaking to everybody else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutgoingEnvelope {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

impl OutgoingEnvelope {
    pub fn recipients(&self) -> Vec<&str> {
        self.to
            .iter()
            .chain(self.cc.iter())
            .chain(self.bcc.iter())
            .map(String::as_str)
            .collect()
    }

    pub fn validate(&self) -> crate::error::Result<()> {
        if self.from.trim().is_empty() {
            return Err(crate::error::Error::Protocol(
                "outgoing message has no sender".into(),
            ));
        }
        let recipients = self.recipients();
        if recipients.is_empty() {
            return Err(crate::error::Error::Protocol(
                "outgoing message has no recipients".into(),
            ));
        }
        if recipients.iter().any(|address| address.trim().is_empty()) {
            return Err(crate::error::Error::Protocol(
                "outgoing message has an empty recipient address".into(),
            ));
        }
        Ok(())
    }
}
