//! Bringing a folder's cached headers in line with the server.
//!
//! Full syncs run `UID SEARCH ALL`, add missing headers, drop messages that
//! disappeared and refresh every flag. Incremental syncs only ask for UIDs
//! newer than the highest cached one, which is what an IDLE wake-up or a poll
//! does.

use crate::account::AccountId;
use crate::client::{Event, Mailin};
use crate::error::Result;
use crate::imap::{ImapSession, RemoteFolder, RemoteHeader};
use crate::model::{Folder, FolderRole, FolderSyncState, NewMessage};
use crate::store::FolderUpsert;

/// Header fetches are batched to keep single IMAP responses modest.
const HEADER_CHUNK: usize = 50;
/// Above this size a full flag refresh would produce a huge response; the
/// flags of new messages are still stored with their headers.
const FLAG_REFRESH_LIMIT: usize = 50_000;

/// Parse a comma-separated address list into individual email addresses.
/// Handles "Name <email>" format and bare emails.
pub(crate) fn parse_address_list(input: &str) -> Vec<String> {
    let mut addresses = Vec::new();
    for token in input.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        // Extract email from "Name <email>" format
        let email = if let Some((_, rest)) = token.rsplit_once('<') {
            rest.trim_end().trim_end_matches('>').trim().to_string()
        } else {
            token.to_string()
        };
        if !email.is_empty() && !email.contains(char::is_whitespace) {
            addresses.push(email.to_lowercase());
        }
    }
    addresses
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncMode {
    /// Add new messages, detect deletions and refresh flags.
    Full,
    /// Only add messages with UIDs newer than the highest cached UID.
    Incremental,
}

/// Create or update local rows for the folders reported by `LIST`. Returns the
/// refreshed folder list ordered for the UI.
pub(crate) fn refresh_folders(
    mailbox: &Mailin,
    account_id: AccountId,
    remote: &[RemoteFolder],
) -> Result<Vec<Folder>> {
    let store = mailbox.store();
    let mut keep = Vec::with_capacity(remote.len());
    for folder in remote.iter().filter(|folder| folder.is_selectable()) {
        let mut upsert = FolderUpsert::new(account_id, folder.name.clone());
        upsert.delimiter = folder.delimiter.clone();
        upsert.role = folder.role;
        store.upsert_folder(&upsert)?;
        keep.push(folder.name.clone());
    }

    let removed = store.prune_folders(&account_id, &keep)?;
    for folder_id in removed {
        mailbox.publish(Event::MessagesChanged { folder_id });
    }
    mailbox.publish(Event::FoldersChanged { account_id });
    store.list_folders(&account_id)
}

pub(crate) async fn sync_folder(
    session: &mut ImapSession,
    mailbox: &Mailin,
    folder: &Folder,
    mode: SyncMode,
) -> Result<()> {
    let selected = session.select(&folder.name).await?;
    let uid_validity = selected.uid_validity;
    let mut notify = folder.last_synced_at.is_some();
    let mut mode = mode;
    let mut state = folder.sync_state();

    if state.uid_validity.is_some() && state.uid_validity != uid_validity {
        tracing::info!(
            folder = %folder.name,
            old = ?state.uid_validity,
            new = ?uid_validity,
            "UIDVALIDITY changed; dropping cached messages"
        );
        mailbox.store().clear_folder_messages(folder.id)?;
        state = FolderSyncState::default();
        mode = SyncMode::Full;
        // Re-downloading a mailbox is not a delivery: suppress notifications
        // for the whole rebuilt page.
        notify = false;
    }
    if state.uid_validity.is_none() && folder.last_synced_at.is_none() {
        mode = SyncMode::Full;
    }

    let local = mailbox.store().message_uids(folder.id)?;

    // CHANGEDSINCE flag-delta sync (CONDSTORE):
    // If the server supports CONDSTORE and we have a previous modseq, fetch
    // only the messages whose flags have changed since the last sync.
    // This avoids the expensive full flag refresh on large folders.
    if mode == SyncMode::Incremental
        && session.supports_condstore()
        && let Some(last_modseq) = state.highest_modseq
    {
        // Search for messages with MODSEQ changed since our last sync
        // CHANGEDSINCE returns UIDs where any flag changed
        match session.search(&format!("CHANGEDSINCE {last_modseq}")).await {
            Ok(changed_uids) if !changed_uids.is_empty() => {
                tracing::debug!(
                    folder = %folder.name,
                    count = changed_uids.len(),
                    since_modseq = last_modseq,
                    "CONDSTORE CHANGEDSINCE found flag changes"
                );
                // Fetch flags only for changed messages
                let changed_vec: Vec<u32> = changed_uids.into_iter().collect();
                let flag_updates = session.fetch_flags_for_uids(&changed_vec).await?;
                mailbox.store().set_flags_many(folder.id, &flag_updates)?;
            }
            Ok(_) => {
                tracing::debug!(
                    folder = %folder.name,
                    since_modseq = last_modseq,
                    "no flag changes since last sync"
                );
            }
            Err(e) => {
                tracing::warn!(
                    folder = %folder.name,
                    error = %e,
                    "CHANGEDSINCE search failed, falling back to full flag refresh"
                );
            }
        }
    }

    let remote = match mode {
        SyncMode::Full => session.search("ALL").await?,
        SyncMode::Incremental => {
            let from = local.iter().copied().max().unwrap_or(0).saturating_add(1);
            // `UID n:*` may return the highest UID even when it is below `n`,
            // so filter the answer instead of trusting the range.
            session
                .search(&format!("UID {from}:*"))
                .await?
                .into_iter()
                .filter(|uid| *uid >= from)
                .collect()
        }
    };

    let mut new_uids: Vec<u32> = remote.difference(&local).copied().collect();
    new_uids.sort_unstable();
    let new_count = new_uids.len();
    let mut created = 0usize;
    for chunk in new_uids.chunks(HEADER_CHUNK) {
        let headers = session.fetch_headers(chunk).await?;
        let messages: Vec<NewMessage> = headers
            .iter()
            .map(|header| message_from_header(folder.id, header))
            .collect();
        created += mailbox
            .store()
            .upsert_messages(&messages)?
            .iter()
            .filter(|result| result.created)
            .count();

        // Seed contacts from new messages
        for message in &messages {
            let account_id = folder.account_id;
            // From address
            if let Some(addr) = &message.from_addr {
                let name = message.from_name.as_deref().unwrap_or("");
                let _ = mailbox.store().upsert_contact(&account_id, addr, name, false);
            }
            // To addresses
            for addr in parse_address_list(&message.to_addrs) {
                let _ = mailbox.store().upsert_contact(&account_id, &addr, "", false);
            }
            // Cc addresses
            for addr in parse_address_list(&message.cc_addrs) {
                let _ = mailbox.store().upsert_contact(&account_id, &addr, "", false);
            }
        }
    }

    let mut deleted = 0usize;
    if mode == SyncMode::Full {
        for uid in local.difference(&remote) {
            if mailbox.store().delete_message(folder.id, *uid)?.is_some() {
                deleted += 1;
            }
        }
        if remote.len() <= FLAG_REFRESH_LIMIT {
            let flags = session.fetch_all_flags().await?;
            mailbox.store().set_flags_many(folder.id, &flags)?;
        }
    }

    let new_state = FolderSyncState {
        uid_validity: uid_validity.or(state.uid_validity),
        uid_next: selected.uid_next,
        highest_modseq: selected.highest_modseq,
        last_synced_at: Some(crate::now_unix()),
    };
    mailbox
        .store()
        .set_folder_sync_state(folder.id, &new_state)?;

    tracing::debug!(
        folder = %folder.name,
        mode = ?mode,
        fetched = new_count,
        created,
        deleted,
        "folder synchronized"
    );

    // Only announce mail that arrived after a first successful sync; the
    // initial population of a mailbox must not produce a burst of
    // notifications.
    if created > 0 && notify && folder.role == FolderRole::Inbox {
        mailbox.publish(Event::NewMessages {
            account_id: folder.account_id,
            folder_id: folder.id,
            count: u32::try_from(created).unwrap_or(u32::MAX),
        });
    }

    mailbox.publish(Event::MessagesChanged {
        folder_id: folder.id,
    });
    if created + deleted > 0 {
        mailbox.publish(Event::FoldersChanged {
            account_id: folder.account_id,
        });
    }
    Ok(())
}

pub(crate) fn message_from_header(
    folder_id: crate::model::FolderId,
    header: &RemoteHeader,
) -> NewMessage {
    let parsed = &header.parsed;
    NewMessage {
        folder_id,
        uid: header.uid,
        message_id: parsed.message_id.clone(),
        thread_key: parsed.thread_key.clone(),
        subject: parsed.subject.clone(),
        from_name: parsed.from_name.clone(),
        from_addr: parsed.from_addr.clone(),
        to_addrs: parsed.to_addrs.clone(),
        cc_addrs: parsed.cc_addrs.clone(),
        sent_at: parsed.sent_at,
        received_at: header
            .internal_date
            .or(parsed.sent_at)
            .unwrap_or_else(crate::now_unix),
        snippet: None,
        size: header.size,
        flags: header.flags.clone(),
        has_attachments: false,
    }
}

/// UIDs that exist remotely but not locally, in ascending order.
#[cfg(test)]
pub(crate) fn difference(
    local: &std::collections::HashSet<u32>,
    remote: &std::collections::HashSet<u32>,
) -> Vec<u32> {
    let mut uids: Vec<u32> = remote.difference(local).copied().collect();
    uids.sort_unstable();
    uids
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::imap::parse::ParsedHeader;

    fn header(uid: u32) -> RemoteHeader {
        RemoteHeader {
            uid,
            flags: vec!["\\Seen".into()],
            internal_date: Some(1_700_000_000),
            size: Some(2048),
            header: None,
            parsed: ParsedHeader {
                subject: "Subject".into(),
                from_name: Some("Ada".into()),
                from_addr: Some("ada@example.com".into()),
                to_addrs: "grace@example.com".into(),
                sent_at: Some(1_600_000_000),
                message_id: Some("id@example.com".into()),
                thread_key: Some("root@example.com".into()),
                ..ParsedHeader::default()
            },
        }
    }

    #[test]
    fn new_uids_are_sorted() {
        let local = HashSet::from([1, 3]);
        let remote = HashSet::from([4, 2, 3]);
        assert_eq!(difference(&local, &remote), vec![2, 4]);
    }

    #[test]
    fn server_header_becomes_a_new_message() {
        let folder = crate::model::FolderId(7);
        let message = message_from_header(folder, &header(42));
        assert_eq!(message.folder_id, folder);
        assert_eq!(message.uid, 42);
        assert_eq!(message.subject, "Subject");
        assert_eq!(message.from_name.as_deref(), Some("Ada"));
        assert_eq!(message.from_addr.as_deref(), Some("ada@example.com"));
        assert_eq!(message.to_addrs, "grace@example.com");
        assert_eq!(message.message_id.as_deref(), Some("id@example.com"));
        assert_eq!(message.thread_key.as_deref(), Some("root@example.com"));
        assert_eq!(message.sent_at, Some(1_600_000_000));
        // INTERNALDATE wins over the Date header for ordering.
        assert_eq!(message.received_at, 1_700_000_000);
        assert_eq!(message.size, Some(2048));
        assert_eq!(message.flags, vec!["\\Seen".to_owned()]);
    }

    #[test]
    fn missing_internal_date_falls_back_to_the_date_header() {
        let mut header = header(7);
        header.internal_date = None;
        let message = message_from_header(crate::model::FolderId(1), &header);
        assert_eq!(message.received_at, 1_600_000_000);
    }
}
