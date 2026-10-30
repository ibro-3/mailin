//! Store, search and outbox behaviour against a real SQLite database.

use mailin_core::{
    Account, AttachmentData, AttachmentMeta, Contact, FolderRole, FolderSyncState, FolderUpsert, NewMessage,
    OutboxState, Paths, Security, ServerSettings, Store, Template,
};

fn account() -> Account {
    Account::new(
        "Ada Lovelace",
        "ada@example.com",
        ServerSettings::imap("imap.example.com", "ada@example.com"),
        ServerSettings::smtp_starttls("smtp.example.com", "ada@example.com"),
    )
}

fn store_with_account() -> (Store, Account) {
    let store = Store::open_in_memory().expect("store");
    let account = account();
    store.upsert_account(&account).expect("account");
    (store, account)
}

fn folder(store: &Store, account: &Account, name: &str, role: FolderRole) -> mailin_core::FolderId {
    let mut upsert = FolderUpsert::new(account.id, name);
    upsert.role = role;
    upsert.delimiter = "/".to_owned();
    store.upsert_folder(&upsert).expect("folder")
}

fn message(folder: mailin_core::FolderId, uid: u32, subject: &str) -> NewMessage {
    let mut message = NewMessage::new(folder, uid);
    message.subject = subject.to_owned();
    message.from_name = Some("Grace Hopper".to_owned());
    message.from_addr = Some("grace@example.com".to_owned());
    message.to_addrs = "ada@example.com".to_owned();
    message.received_at = 1_700_000_000 + i64::from(uid);
    message
}

#[test]
fn schema_is_created_and_migrations_are_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("mailin.db");

    let store = Store::open(&path).expect("open");
    assert_eq!(
        store.schema_version().expect("version"),
        mailin_core::store::SCHEMA_VERSION
    );
    let account = account();
    store.upsert_account(&account).expect("account");
    drop(store);

    let reopened = Store::open(&path).expect("reopen");
    assert_eq!(
        reopened.schema_version().expect("version"),
        mailin_core::store::SCHEMA_VERSION
    );
    assert_eq!(reopened.list_accounts().expect("accounts"), vec![account]);
}

#[cfg(unix)]
#[test]
fn database_file_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("mailin.db");
    let store = Store::open(&path).expect("open");
    drop(store);

    let mode = std::fs::metadata(&path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn accounts_round_trip_including_security_and_sync_settings() {
    let (store, mut account) = store_with_account();
    account.imap.security = Security::StartTls;
    account.sync.folder_allowlist = vec!["INBOX".into(), "Archive".into()];
    store.upsert_account(&account).expect("update");

    let loaded = store.account(&account.id).expect("get").expect("present");
    assert_eq!(loaded, account);
}

#[test]
fn accounts_are_validated_before_insert() {
    let store = Store::open_in_memory().expect("store");
    let mut account = account();
    account.smtp.host.clear();
    assert!(store.upsert_account(&account).is_err());
    assert!(store.list_accounts().expect("list").is_empty());
}

#[test]
fn folders_are_listed_by_role_and_carry_counts() {
    let (store, account) = store_with_account();
    folder(&store, &account, "Archive/2024", FolderRole::Archive);
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);
    let sent = folder(&store, &account, "Sent", FolderRole::Sent);

    let mut read = message(inbox, 1, "Read");
    read.flags = vec!["\\Seen".into()];
    store.upsert_message(&read).expect("read");
    store
        .upsert_message(&message(inbox, 2, "Unread"))
        .expect("unread");
    store
        .upsert_message(&message(sent, 1, "Re: Unread"))
        .expect("sent");

    let folders = store.list_folders(&account.id).expect("folders");
    let names: Vec<_> = folders.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["INBOX", "Sent", "Archive/2024"]);
    assert_eq!(folders[0].message_count, 2);
    assert_eq!(folders[0].unread_count, 1);
    assert_eq!(folders[0].role, FolderRole::Inbox);
    assert_eq!(folders[1].message_count, 1);
}

#[test]
fn folder_upsert_keeps_state_and_can_update_role() {
    let (store, account) = store_with_account();
    let mut upsert = FolderUpsert::new(account.id, "Inbox");
    upsert.role = FolderRole::Other;
    let id = store.upsert_folder(&upsert).expect("insert");

    let state = FolderSyncState {
        uid_validity: Some(42),
        uid_next: Some(100),
        highest_modseq: Some(7),
        last_synced_at: Some(1_700_000_000),
    };
    let mut update = FolderUpsert::new(account.id, "Inbox");
    update.role = FolderRole::Inbox;
    update.state = state;
    assert_eq!(store.upsert_folder(&update).expect("update"), id);

    let folder = store.folder(id).expect("get").expect("present");
    assert_eq!(folder.role, FolderRole::Inbox);
    assert_eq!(folder.sync_state(), state);
}

#[test]
fn messages_are_deduplicated_by_folder_and_uid() {
    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);

    let first = store
        .upsert_message(&message(inbox, 7, "Hello"))
        .expect("insert");
    assert!(first.created);
    let second = store
        .upsert_message(&message(inbox, 7, "Hello again"))
        .expect("update");
    assert!(!second.created);
    assert_eq!(second.id, first.id);

    let messages = store.list_messages(inbox, 0, 10).expect("list");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].subject, "Hello again");
}

#[test]
fn same_uid_in_different_folders_is_allowed() {
    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);
    let sent = folder(&store, &account, "Sent", FolderRole::Sent);

    store
        .upsert_message(&message(inbox, 1, "in inbox"))
        .expect("a");
    store
        .upsert_message(&message(sent, 1, "in sent"))
        .expect("b");
    assert_eq!(store.count_messages(inbox).expect("count"), 1);
    assert_eq!(store.count_messages(sent).expect("count"), 1);
}

#[test]
fn messages_are_paginated_newest_first() {
    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);
    let batch: Vec<_> = (1..=10).map(|uid| message(inbox, uid, "msg")).collect();
    store.upsert_messages(&batch).expect("bulk");

    let first = store.list_messages(inbox, 0, 3).expect("page 1");
    assert_eq!(first.iter().map(|m| m.uid).collect::<Vec<_>>(), [10, 9, 8]);
    let second = store.list_messages(inbox, 3, 3).expect("page 2");
    assert_eq!(second.iter().map(|m| m.uid).collect::<Vec<_>>(), [7, 6, 5]);
    assert!(
        store
            .list_messages(inbox, 20, 3)
            .expect("past end")
            .is_empty()
    );
}

#[test]
fn body_survives_header_refresh_and_is_searchable() {
    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);
    let inserted = store
        .upsert_message(&message(inbox, 1, "Quarterly report"))
        .expect("insert");

    assert!(
        store
            .set_body(
                inserted.id,
                Some("The numbers look good"),
                None,
                Some("The numbers…")
            )
            .expect("body")
    );
    let refreshed = store
        .upsert_message(&message(inbox, 1, "Quarterly report"))
        .expect("refresh");
    assert!(!refreshed.created);

    let header = store.message(inserted.id).expect("get").expect("present");
    assert!(header.body_fetched);
    let (text, html) = store.body(inserted.id).expect("body").expect("present");
    assert_eq!(text.as_deref(), Some("The numbers look good"));
    assert_eq!(html, None);

    let hits = store.search(&account.id, "numbers", 10).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, inserted.id);
}

#[test]
fn set_body_reports_missing_messages() {
    let store = Store::open_in_memory().expect("store");
    assert!(
        !store
            .set_body(mailin_core::MessageId(999), Some("x"), None, None)
            .expect("missing")
    );
}

#[test]
fn search_covers_headers_bodies_and_is_scoped_to_the_account() {
    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);

    let mut from_grace = message(inbox, 1, "Compiler notes");
    from_grace.from_name = Some("Grace Hopper".into());
    from_grace.from_addr = Some("grace@example.com".into());
    store.upsert_message(&from_grace).expect("a");

    let mut from_ada = message(inbox, 2, "Analytical engine");
    from_ada.from_name = Some("Ada Lovelace".into());
    from_ada.from_addr = Some("ada@example.com".into());
    from_ada.to_addrs = "grace@example.com".into();
    let ada = store.upsert_message(&from_ada).expect("b");
    store
        .set_body(
            ada.id,
            Some("Difference engine and analytical engine"),
            None,
            None,
        )
        .expect("body");

    let by_sender = store.search(&account.id, "hopper", 10).expect("sender");
    assert_eq!(by_sender.len(), 1);
    assert_eq!(by_sender[0].uid, 1);

    // Recipients are indexed too, so "grace" matches both messages.
    let by_participant = store.search(&account.id, "grace", 10).expect("participant");
    assert_eq!(by_participant.len(), 2);

    let by_subject = store.search(&account.id, "engine", 10).expect("subject");
    assert_eq!(by_subject.len(), 1);
    assert_eq!(by_subject[0].uid, 2);

    let by_body = store.search(&account.id, "difference", 10).expect("body");
    assert_eq!(by_body.len(), 1);

    let empty = store.search(&account.id, "   ", 10).expect("empty");
    assert!(empty.is_empty());

    let punctuation = store
        .search(&account.id, "\"()*&^%", 10)
        .expect("punctuation");
    assert!(punctuation.is_empty());

    // A second account must never see the first account's mail.
    let other = Account::new(
        "Other",
        "other@example.com",
        ServerSettings::imap("imap.other.test", "other@example.com"),
        ServerSettings::smtp("smtp.other.test", "other@example.com"),
    );
    store.upsert_account(&other).expect("other account");
    let other_folder = folder(&store, &other, "INBOX", FolderRole::Inbox);
    store
        .upsert_message(&message(other_folder, 1, "Unrelated"))
        .expect("other message");
    let scoped = store.search(&account.id, "unrelated", 10).expect("scoped");
    assert!(scoped.is_empty());
}

#[test]
fn flags_can_be_updated_and_are_parsed() {
    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);
    store
        .upsert_message(&message(inbox, 1, "Hi"))
        .expect("insert");

    assert!(
        store
            .set_flags(inbox, 1, &["\\Seen".into(), "\\Flagged".into()])
            .expect("flags")
    );
    let header = store
        .message_by_uid(inbox, 1)
        .expect("get")
        .expect("present");
    assert!(header.is_seen());
    assert!(header.is_flagged());
    assert_eq!(store.unread_total(&account.id).expect("unread"), 0);
    assert!(!store.set_flags(inbox, 99, &[]).expect("missing"));
}

#[test]
fn attachments_are_replaced_atomically() {
    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);
    let inserted = store
        .upsert_message(&message(inbox, 1, "With files"))
        .expect("insert");

    let attachments = vec![
        AttachmentData {
            meta: AttachmentMeta {
                part_id: "1.1".into(),
                filename: Some("report.pdf".into()),
                mime_type: "application/pdf".into(),
                size: Some(2048),
                content_id: None,
                is_inline: false,
            },
            data: Some(b"%PDF".to_vec()),
        },
        AttachmentData {
            meta: AttachmentMeta {
                part_id: "1.2".into(),
                filename: None,
                mime_type: "image/png".into(),
                size: None,
                content_id: Some("logo".into()),
                is_inline: true,
            },
            data: Some(vec![0x89, b'P', b'N', b'G']),
        },
    ];
    store
        .replace_attachments(inserted.id, &attachments)
        .expect("replace");

    let loaded = store.list_attachments(inserted.id).expect("list");
    assert_eq!(
        loaded,
        attachments
            .iter()
            .map(|attachment| attachment.meta.clone())
            .collect::<Vec<_>>()
    );
    let report = store
        .attachment(inserted.id, "1.1")
        .expect("attachment")
        .expect("present");
    assert_eq!(report.data.as_deref(), Some(b"%PDF".as_slice()));
    assert_eq!(
        store
            .attachment_data(inserted.id, "1.2")
            .expect("data")
            .as_deref(),
        Some(&[0x89, b'P', b'N', b'G'][..])
    );
    assert!(
        store
            .attachment(inserted.id, "missing")
            .expect("missing")
            .is_none()
    );
    let header = store.message(inserted.id).expect("get").expect("present");
    assert!(header.has_attachments);

    store.replace_attachments(inserted.id, &[]).expect("clear");
    assert!(
        store
            .list_attachments(inserted.id)
            .expect("list")
            .is_empty()
    );
    let header = store.message(inserted.id).expect("get").expect("present");
    assert!(!header.has_attachments);
}

#[test]
fn deleting_by_uid_removes_the_index_entry() {
    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);
    let inserted = store
        .upsert_message(&message(inbox, 3, "Temporary"))
        .expect("insert");
    assert_eq!(
        store
            .search(&account.id, "temporary", 10)
            .expect("search")
            .len(),
        1
    );

    let removed = store.delete_message(inbox, 3).expect("delete");
    assert_eq!(removed, Some(inserted.id));
    assert!(
        store
            .search(&account.id, "temporary", 10)
            .expect("search")
            .is_empty()
    );
    assert_eq!(store.delete_message(inbox, 3).expect("delete again"), None);
}

#[test]
fn pruning_folders_removes_their_messages_from_the_index() {
    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);
    let old = folder(&store, &account, "Old/Bin", FolderRole::Other);
    store
        .upsert_message(&message(inbox, 1, "Keep me"))
        .expect("keep");
    store
        .upsert_message(&message(old, 1, "Remove me"))
        .expect("remove");

    let removed = store
        .prune_folders(&account.id, &["INBOX".to_owned()])
        .expect("prune");
    assert_eq!(removed, vec![old]);
    assert!(store.folder(old).expect("folder").is_none());
    assert!(
        store
            .search(&account.id, "remove", 10)
            .expect("search")
            .is_empty()
    );
    assert_eq!(
        store.search(&account.id, "keep", 10).expect("search").len(),
        1
    );

    // No folder list means "no information", never "delete everything".
    assert!(
        store
            .prune_folders(&account.id, &[])
            .expect("noop")
            .is_empty()
    );
    assert!(store.folder(inbox).expect("folder").is_some());
}

#[test]
fn deleting_an_account_cascades_and_clears_the_index() {
    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);
    store
        .upsert_message(&message(inbox, 1, "Ephemeral"))
        .expect("insert");

    assert!(store.delete_account(&account.id).expect("delete"));
    assert!(store.list_accounts().expect("accounts").is_empty());
    assert!(store.list_folders(&account.id).expect("folders").is_empty());
    assert!(
        store
            .search(&account.id, "ephemeral", 10)
            .expect("search")
            .is_empty()
    );
    assert!(!store.delete_account(&account.id).expect("again"));
}

#[test]
fn outbox_lifecycle() {
    let (store, account) = store_with_account();
    let id = store
        .enqueue_outbox(
            &account.id,
            b"Subject: hi\r\n\r\nbody",
            r#"{"to":["a@b.c"]}"#,
        )
        .expect("enqueue");
    assert_eq!(store.outbox_len().expect("len"), 1);

    let entry = store.next_queued_outbox().expect("next").expect("present");
    assert_eq!(entry.id, id);
    assert_eq!(entry.state, OutboxState::Queued);
    assert_eq!(entry.raw, b"Subject: hi\r\n\r\nbody");
    assert_eq!(
        store.outbox_entry(id).expect("by id").expect("present").id,
        id
    );

    store
        .set_outbox_state(id, OutboxState::Failed, Some("550 rejected"))
        .expect("failed");
    let entry = store.next_queued_outbox().expect("next").expect("retry");
    assert_eq!(entry.attempts, 1);
    assert_eq!(entry.last_error.as_deref(), Some("550 rejected"));

    store
        .set_outbox_state(id, OutboxState::Sent, None)
        .expect("sent");
    assert_eq!(store.outbox_len().expect("len"), 0);
    assert!(store.next_queued_outbox().expect("next").is_none());
    assert!(store.delete_outbox(id).expect("delete"));
}

#[test]
fn outbox_gives_up_after_five_attempts() {
    let (store, account) = store_with_account();
    let id = store
        .enqueue_outbox(&account.id, b"raw", "{}")
        .expect("enqueue");
    for expected in 1..=5 {
        assert!(store.next_queued_outbox().expect("next").is_some());
        store
            .set_outbox_state(id, OutboxState::Failed, Some("try again"))
            .expect("fail");
        let entry = store.next_queued_outbox().expect("next");
        if expected == 5 {
            assert!(entry.is_none(), "must stop after five attempts");
        }
    }
}

#[test]
fn bulk_upsert_is_transactional_and_reports_created_rows() {
    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);

    let batch: Vec<_> = (1..=5_000).map(|uid| message(inbox, uid, "bulk")).collect();
    let results = store.upsert_messages(&batch).expect("bulk");
    assert_eq!(results.len(), 5_000);
    assert!(results.iter().all(|r| r.created));
    assert_eq!(store.count_messages(inbox).expect("count"), 5_000);

    let again = store.upsert_messages(&batch).expect("bulk again");
    assert!(again.iter().all(|r| !r.created));
    assert_eq!(store.count_messages(inbox).expect("count"), 5_000);
}

#[test]
fn upserting_a_message_into_a_missing_folder_fails() {
    let store = Store::open_in_memory().expect("store");
    let result = store.upsert_message(&message(mailin_core::FolderId(42), 1, "orphan"));
    assert!(result.is_err());
}

#[test]
fn paths_derive_database_location() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = Paths::with_root(dir.path());
    assert_eq!(paths.database_file(), dir.path().join("data/mailin.db"));
    paths.ensure().expect("ensure");
    assert!(paths.database_file().parent().expect("parent").is_dir());
}

#[test]
fn sync_helpers_track_uids_flags_and_clear_a_folder() {
    use std::collections::HashSet;

    let (store, account) = store_with_account();
    let inbox = folder(&store, &account, "INBOX", FolderRole::Inbox);
    for uid in 1..=3 {
        store
            .upsert_message(&message(inbox, uid, "sync helpers"))
            .expect("insert");
    }
    assert_eq!(
        store.message_uids(inbox).expect("uids"),
        HashSet::from([1, 2, 3])
    );

    let changed = store
        .set_flags_many(
            inbox,
            &[
                (1, vec!["\\Seen".to_owned()]),
                (99, vec!["\\Flagged".to_owned()]),
            ],
        )
        .expect("flags");
    assert_eq!(changed, 1, "unknown UIDs must be ignored");
    assert!(
        store
            .message_by_uid(inbox, 1)
            .expect("message")
            .expect("present")
            .is_seen()
    );

    assert_eq!(store.clear_folder_messages(inbox).expect("clear"), 3);
    assert_eq!(store.count_messages(inbox).expect("count"), 0);
    assert!(
        store
            .search(&account.id, "sync helpers", 10)
            .expect("search")
            .is_empty(),
        "clearing a folder must clear its index entries"
    );
}

#[test]
fn interrupted_outbox_sends_are_queued_again() {
    let (store, account) = store_with_account();
    let id = store
        .enqueue_outbox(&account.id, b"raw", "{}")
        .expect("enqueue");
    store
        .set_outbox_state(id, OutboxState::Sending, None)
        .expect("sending");

    assert_eq!(store.reset_outbox_sending().expect("reset"), 1);
    let entry = store.next_queued_outbox().expect("next").expect("present");
    assert_eq!(entry.state, OutboxState::Queued);
    assert_eq!(entry.attempts, 0);
}

#[test]
fn template_crud() {
    let (store, account) = store_with_account();

    // Create template
    let id = store
        .upsert_template(&account.id, "welcome", "Welcome {{name}}", "Hello {{name}},\n\nWelcome!")
        .expect("upsert");
    assert!(id > 0);

    // List templates
    let templates = store.list_templates(&account.id).expect("list");
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].name, "welcome");
    assert_eq!(templates[0].subject, "Welcome {{name}}");
    assert_eq!(templates[0].body, "Hello {{name}},\n\nWelcome!");

    // Update template
    let id2 = store
        .upsert_template(&account.id, "welcome", "Welcome {{name}}!", "Hello {{name}}!\n\nWelcome aboard!")
        .expect("update");
    assert_eq!(id, id2, "upsert returns same id");
    let templates = store.list_templates(&account.id).expect("list again");
    assert_eq!(templates[0].subject, "Welcome {{name}}!");
    assert_eq!(templates[0].body, "Hello {{name}}!\n\nWelcome aboard!");

    // Delete template
    assert!(store.delete_template(&account.id, "welcome").expect("delete"));
    assert!(store.list_templates(&account.id).expect("list empty").is_empty());
    assert!(!store.delete_template(&account.id, "welcome").expect("delete missing"));
}

#[test]
fn templates_are_per_account() {
    let store = Store::open_in_memory().expect("store");
    let account1 = Account::new(
        "Ada",
        "ada@example.com",
        ServerSettings::imap("imap.example.com", "ada@example.com"),
        ServerSettings::smtp_starttls("smtp.example.com", "ada@example.com"),
    );
    let account2 = Account::new(
        "Bob",
        "bob@example.com",
        ServerSettings::imap("imap.example.com", "bob@example.com"),
        ServerSettings::smtp_starttls("smtp.example.com", "bob@example.com"),
    );
    store.upsert_account(&account1).expect("account1");
    store.upsert_account(&account2).expect("account2");

    store
        .upsert_template(&account1.id, "template", "Subj", "Body")
        .expect("upsert1");
    store
        .upsert_template(&account2.id, "template", "Subj2", "Body2")
        .expect("upsert2");

    let t1 = store.list_templates(&account1.id).expect("list1");
    let t2 = store.list_templates(&account2.id).expect("list2");
    assert_eq!(t1.len(), 1);
    assert_eq!(t2.len(), 1);
    assert_eq!(t1[0].subject, "Subj");
    assert_eq!(t2[0].subject, "Subj2");
}

#[test]
fn contact_crud() {
    let (store, account) = store_with_account();

    // Create contact
    let id = store
        .upsert_contact(&account.id, "grace@example.com", "Grace Hopper", false)
        .expect("upsert");
    assert!(id > 0);

    // List contacts
    let contacts = store.list_contacts(&account.id).expect("list");
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0].email, "grace@example.com");
    assert_eq!(contacts[0].display_name, "Grace Hopper");
    assert_eq!(contacts[0].times_seen, 1);
    assert!(!contacts[0].manual);

    // Update contact (increment times_seen)
    let id2 = store
        .upsert_contact(&account.id, "grace@example.com", "Grace", false)
        .expect("update");
    assert_eq!(id, id2, "upsert returns same id");
    let contacts = store.list_contacts(&account.id).expect("list again");
    assert_eq!(contacts[0].times_seen, 2);
    assert_eq!(contacts[0].display_name, "Grace Hopper"); // original name preserved

    // Manual contact should set manual flag
    let id3 = store
        .upsert_contact(&account.id, "manual@example.com", "Manual Entry", true)
        .expect("manual upsert");
    let contacts = store.list_contacts(&account.id).expect("list manual");
    let manual = contacts.iter().find(|c| c.email == "manual@example.com").expect("manual contact");
    assert!(manual.manual);

    // Delete contact
    assert!(store.delete_contact(&account.id, "grace@example.com").expect("delete"));
    assert!(store.list_contacts(&account.id).expect("list empty").len() == 1); // only manual remains
    assert!(!store.delete_contact(&account.id, "grace@example.com").expect("delete missing"));
}

#[test]
fn contacts_are_per_account() {
    let store = Store::open_in_memory().expect("store");
    let account1 = Account::new(
        "Ada",
        "ada@example.com",
        ServerSettings::imap("imap.example.com", "ada@example.com"),
        ServerSettings::smtp_starttls("smtp.example.com", "ada@example.com"),
    );
    let account2 = Account::new(
        "Bob",
        "bob@example.com",
        ServerSettings::imap("imap.example.com", "bob@example.com"),
        ServerSettings::smtp_starttls("smtp.example.com", "bob@example.com"),
    );
    store.upsert_account(&account1).expect("account1");
    store.upsert_account(&account2).expect("account2");

    store
        .upsert_contact(&account1.id, "contact@example.com", "Contact 1", false)
        .expect("upsert1");
    store
        .upsert_contact(&account2.id, "contact@example.com", "Contact 2", false)
        .expect("upsert2");

    let c1 = store.list_contacts(&account1.id).expect("list1");
    let c2 = store.list_contacts(&account2.id).expect("list2");
    assert_eq!(c1.len(), 1);
    assert_eq!(c2.len(), 1);
    assert_eq!(c1[0].display_name, "Contact 1");
    assert_eq!(c2[0].display_name, "Contact 2");
}

#[test]
fn contact_search_ranks_by_frequency_and_recency() {
    let (store, account) = store_with_account();

    // Add contacts with different times_seen
    store.upsert_contact(&account.id, "a@example.com", "A", false).expect("a");
    store.upsert_contact(&account.id, "b@example.com", "B", false).expect("b");
    store.upsert_contact(&account.id, "c@example.com", "C", false).expect("c");
    // Manually increment times_seen for 'b'
    store.upsert_contact(&account.id, "b@example.com", "B", false).expect("b again");
    store.upsert_contact(&account.id, "b@example.com", "B", false).expect("b again");

    // Search for 'example' - should rank by times_seen DESC
    let results = store.search_contacts(&account.id, "example", 10).expect("search");
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].email, "b@example.com"); // 3 times_seen
    assert_eq!(results[1].email, "a@example.com"); // 1 times_seen
    assert_eq!(results[2].email, "c@example.com"); // 1 times_seen

    // Prefix search
    let results = store.search_contacts(&account.id, "a@ex", 10).expect("prefix search");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].email, "a@example.com");

    // Case insensitive
    let results = store.search_contacts(&account.id, "A@EX", 10).expect("case insensitive");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].email, "a@example.com");

    // Limit
    let results = store.search_contacts(&account.id, "example", 2).expect("limit");
    assert_eq!(results.len(), 2);
}

#[test]
fn rule_crud() {
    let (store, account) = store_with_account();

    // Create rule
    let id = store
        .upsert_rule(
            &account.id,
            "Test Rule",
            true,
            10,
            r#"[{"field": "from", "op": "contains", "value": "test"}]"#,
            r#"[{"kind": "move", "value": "Archive"}]"#,
            false,
        )
        .expect("upsert");
    assert!(id > 0);

    // List rules
    let rules = store.list_rules(&account.id).expect("list");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, "Test Rule");
    assert!(rules[0].enabled);
    assert_eq!(rules[0].priority, 10);

    // Update rule
    let id2 = store
        .upsert_rule(
            &account.id,
            "Test Rule",
            false,
            20,
            r#"[{"field": "to", "op": "contains", "value": "update"}]"#,
            r#"[{"kind": "flag", "value": "\\Flagged"}]"#,
            true,
        )
        .expect("update");
    assert_eq!(id, id2);
    let rules = store.list_rules(&account.id).expect("list again");
    assert_eq!(rules[0].enabled, false);
    assert_eq!(rules[0].priority, 20);
    assert!(rules[0].stop);

    // Delete rule
    assert!(store.delete_rule(&account.id, "Test Rule").expect("delete"));
    assert!(store.list_rules(&account.id).expect("list empty").is_empty());
    assert!(!store.delete_rule(&account.id, "Test Rule").expect("delete missing"));
}

#[test]
fn rules_are_per_account() {
    let store = Store::open_in_memory().expect("store");
    let account1 = Account::new(
        "Ada",
        "ada@example.com",
        ServerSettings::imap("imap.example.com", "ada@example.com"),
        ServerSettings::smtp_starttls("smtp.example.com", "ada@example.com"),
    );
    let account2 = Account::new(
        "Bob",
        "bob@example.com",
        ServerSettings::imap("imap.example.com", "bob@example.com"),
        ServerSettings::smtp_starttls("smtp.example.com", "bob@example.com"),
    );
    store.upsert_account(&account1).expect("account1");
    store.upsert_account(&account2).expect("account2");

    store
        .upsert_rule(&account1.id, "rule", true, 0, "[]", "[]", false)
        .expect("upsert1");
    store
        .upsert_rule(&account2.id, "rule", true, 0, "[]", "[]", false)
        .expect("upsert2");

    let r1 = store.list_rules(&account1.id).expect("list1");
    let r2 = store.list_rules(&account2.id).expect("list2");
    assert_eq!(r1.len(), 1);
    assert_eq!(r2.len(), 1);
}
