//! Shared folder-selection semantics for the front-ends.
//!
//! The virtual Outbox must never be selected just because it is the only
//! row available yet; instead the front-ends keep showing a "syncing"
//! state and select the Inbox as soon as it arrives.

use crate::model::{Folder, FolderId, FolderRole};

/// Resolve which folder row in `folders` should be selected.
///
/// The explicitly selected folder wins when it still exists — including
/// the virtual Outbox when the user opened it deliberately. Otherwise the
/// Inbox is preferred. When neither applies, return `None`: the virtual
/// Outbox is never auto-selected, and the caller shows a "syncing folders"
/// state until a real folder arrives.
pub fn resolve_folder_index(folders: &[Folder], selected_id: Option<FolderId>) -> Option<usize> {
    if let Some(id) = selected_id
        && let Some(index) = folders.iter().position(|folder| folder.id == id)
    {
        return Some(index);
    }
    folders
        .iter()
        .position(|folder| folder.role == FolderRole::Inbox && !folder.id.is_virtual_ui())
}

/// True when `folders` contains at least one real (non-virtual) folder.
pub fn has_real_folder(folders: &[Folder]) -> bool {
    folders.iter().any(|folder| !folder.id.is_virtual_ui())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FolderId, FolderRole};

    fn folder(id: i64, role: FolderRole, name: &str) -> Folder {
        Folder {
            id: FolderId(id),
            account_id: crate::AccountId::new(),
            name: name.to_owned(),
            delimiter: "/".to_owned(),
            role,
            uid_validity: None,
            uid_next: None,
            highest_modseq: None,
            last_synced_at: None,
            subscribed: true,
            message_count: 0,
            unread_count: 0,
        }
    }

    fn outbox() -> Folder {
        folder(FolderId::OUTBOX.0, FolderRole::Other, "Outbox")
    }

    #[test]
    fn empty_list_selects_nothing() {
        assert_eq!(resolve_folder_index(&[], None), None);
        assert!(!has_real_folder(&[]));
    }

    #[test]
    fn virtual_outbox_alone_selects_nothing() {
        let folders = vec![outbox()];
        assert_eq!(resolve_folder_index(&folders, None), None);
        assert!(!has_real_folder(&folders));
    }

    #[test]
    fn inbox_is_preferred_when_nothing_is_selected() {
        let folders = vec![
            outbox(), // would be index 0 without the fix
            folder(1, FolderRole::Inbox, "INBOX"),
            folder(2, FolderRole::Archive, "Archive"),
        ];
        assert_eq!(resolve_folder_index(&folders, None), Some(1));
    }

    #[test]
    fn explicit_outbox_selection_is_honoured() {
        let folders = vec![outbox(), folder(1, FolderRole::Inbox, "INBOX")];
        assert_eq!(
            resolve_folder_index(&folders, Some(FolderId::OUTBOX)),
            Some(0)
        );
    }

    #[test]
    fn stale_selection_falls_back_to_inbox() {
        let folders = vec![folder(1, FolderRole::Inbox, "INBOX"), outbox()];
        assert_eq!(resolve_folder_index(&folders, Some(FolderId(999))), Some(0));
    }

    #[test]
    fn stale_selection_without_inbox_selects_nothing() {
        let folders = vec![folder(1, FolderRole::Archive, "Archive"), outbox()];
        assert_eq!(resolve_folder_index(&folders, Some(FolderId(999))), None);
    }

    #[test]
    fn real_folder_detection_ignores_virtual_rows() {
        assert!(!has_real_folder(&[outbox()]));
        assert!(has_real_folder(&[
            outbox(),
            folder(1, FolderRole::Inbox, "INBOX")
        ]));
    }
}
