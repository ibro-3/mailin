use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::account::AccountId;
use crate::compose::OutgoingMessage;
use std::collections::HashMap;

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
    Snoozed,
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
            Self::Snoozed => "snoozed",
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
        // RFC 6154 doesn't define \Snoozed, but some servers use it or X-Snoozed
        if has("\\Snoozed") || has("X-Snoozed") {
            return Self::Snoozed;
        }

        let leaf = name.rsplit(['/', '.']).next().unwrap_or(name);
        let leaf = leaf.to_ascii_lowercase();
        match leaf.as_str() {
            "sent" | "sent items" | "sent messages" | "sent mail" => Self::Sent,
            "drafts" | "draft" => Self::Drafts,
            "trash" | "deleted" | "deleted items" | "deleted messages" => Self::Trash,
            "junk" | "spam" | "junk e-mail" | "bulk mail" => Self::Junk,
            "archive" | "all mail" | "all" => Self::Archive,
            "snoozed" => Self::Snoozed,
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
            Self::Snoozed => 6,
            Self::Other => 7,
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
            "snoozed" => Self::Snoozed,
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

    /// Bare domain of the sender address, used for per-sender trust checks
    /// such as the remote-content allowlist.
    pub fn from_domain(&self) -> Option<&str> {
        let addr = self.from_addr.as_deref()?;
        // Tolerate `Name <user@host>` shapes by looking at the angle part.
        let addr = addr.rsplit_once('<').map_or(addr, |(_, inner)| inner);
        let addr = addr.trim().trim_end_matches('>');
        addr.rsplit_once('@')
            .map(|(_, domain)| domain)
            .filter(|domain| !domain.is_empty() && !domain.contains(char::is_whitespace))
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
    pub part_id: String,
    pub filename: Option<String>,
    pub mime_type: String,
    pub size: Option<u32>,
    /// `Content-ID` without the surrounding angle brackets, used to resolve
    /// `cid:` references in HTML bodies.
    pub content_id: Option<String>,
    pub is_inline: bool,
}

impl AttachmentMeta {
    /// Filename safe to show or use as a default for "Save as".
    pub fn display_name(&self) -> String {
        let name = self
            .filename
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("attachment");
        let name = name
            .chars()
            .map(|character| {
                if character.is_control() || matches!(character, '/' | '\\') {
                    '_'
                } else {
                    character
                }
            })
            .collect::<String>();
        if name.trim().is_empty() {
            "attachment".to_owned()
        } else {
            name
        }
    }
}

/// Attachment metadata together with the decoded bytes.
///
/// The bytes are optional because a message cached before attachment storage
/// existed has metadata only; the UI can ask the engine to refresh the body to
/// fill them in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentData {
    pub meta: AttachmentMeta,
    pub data: Option<Vec<u8>>,
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

/// A contact for autocomplete, seeded from observed mail or added manually.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    pub id: i64,
    pub account_id: AccountId,
    pub email: String,
    pub display_name: String,
    pub last_used_at: Option<i64>,
    pub times_seen: i64,
    pub manual: bool,
}

/// A message template with `{{variable}}` placeholders for compose prefill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub id: i64,
    pub account_id: AccountId,
    pub name: String,
    pub subject: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Template {
    /// Create an OutgoingMessage from this template with variable substitution.
    pub fn to_draft(&self, from_name: String, from_addr: String) -> OutgoingMessage {
        OutgoingMessage {
            from_name,
            from_addr,
            subject: self.subject.clone(),
            body: self.body.clone(),
            ..OutgoingMessage::default()
        }
    }
}

/// Condition for a rule: field, operator, value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleCondition {
    pub field: String,
    pub op: String,
    pub value: String,
}

/// Action for a rule: kind and value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleAction {
    pub kind: String,
    pub value: String,
}

/// A mail filtering rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: i64,
    pub account_id: AccountId,
    pub name: String,
    pub enabled: bool,
    pub priority: i64,
    pub conditions_json: String,
    pub actions_json: String,
    pub stop: bool,
    pub created_at: i64,
}

/// A scheduled action to be executed at a specific time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledAction {
    pub id: i64,
    pub account_id: AccountId,
    pub due_at: i64,
    pub kind: String,
    pub payload_json: String,
    pub state: String,
    pub created_at: i64,
    pub last_error: Option<String>,
}

/// Payload for the 'snooze_message' scheduled action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnoozePayload {
    pub message_id: i64,
    pub return_to_folder_id: i64,
}

/// Virtual folder kinds for unified views across accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualFolderKind {
    UnifiedInbox = 0,
    Starred = 1,
    AllMail = 2,
}

impl From<VirtualFolderKind> for i64 {
    fn from(kind: VirtualFolderKind) -> Self {
        kind as i64
    }
}

impl TryFrom<i64> for VirtualFolderKind {
    type Error = ();
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(VirtualFolderKind::UnifiedInbox),
            1 => Ok(VirtualFolderKind::Starred),
            2 => Ok(VirtualFolderKind::AllMail),
            _ => Err(()),
        }
    }
}

/// A virtual folder that aggregates messages across accounts/folders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualFolder {
    pub id: i64,
    pub account_id: AccountId,
    pub name: String,
    pub kind: VirtualFolderKind,
    pub query_json: String,
    pub created_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(filename: Option<&str>) -> AttachmentMeta {
        AttachmentMeta {
            part_id: "1".into(),
            filename: filename.map(str::to_owned),
            mime_type: "application/pdf".into(),
            size: None,
            content_id: None,
            is_inline: false,
        }
    }

    #[test]
    fn attachment_names_are_safe_to_use_as_file_names() {
        assert_eq!(
            attachment(Some(" report.pdf ")).display_name(),
            "report.pdf"
        );
        assert_eq!(
            attachment(Some("../../etc/passwd")).display_name(),
            ".._.._etc_passwd"
        );
        assert_eq!(attachment(None).display_name(), "attachment");
        assert_eq!(attachment(Some("   ")).display_name(), "attachment");
    }
}
