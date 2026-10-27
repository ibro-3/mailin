//! Local SQLite cache: accounts, folders, message headers, bodies,
//! attachments, the outbox and the full text index.
//!
//! A single connection behind a mutex keeps the writer story simple: SQLite in
//! WAL mode allows concurrent readers, and the core's network workers batch
//! their writes. If profiling shows contention we can add a read pool without
//! changing this module's API.

mod migrations;

use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use crate::account::{Account, AccountId};
use crate::error::{Error, Result};
use crate::model::{
    AttachmentData, AttachmentMeta, Folder, FolderId, FolderRole, FolderSyncState, MessageHeader,
    MessageId, NewMessage, OutboxEntry, OutboxState, UpsertedMessage,
};

pub use migrations::SCHEMA_VERSION;

const MESSAGE_COLUMNS: &str = "m.id, m.folder_id, m.uid, m.message_id, m.thread_key, m.subject, \
     m.from_name, m.from_addr, m.to_addrs, m.cc_addrs, m.sent_at, m.received_at, m.snippet, \
     m.size, m.flags, m.has_attachments, m.body_state";

const FOLDER_COLUMNS: &str = "f.id, f.account_id, f.name, f.delimiter, f.role, f.uid_validity, \
     f.uid_next, f.highest_modseq, f.last_synced_at, \
     (SELECT COUNT(*) FROM messages m WHERE m.folder_id = f.id), \
     (SELECT COUNT(*) FROM messages m WHERE m.folder_id = f.id \
        AND instr(' ' || m.flags || ' ', ' \\Seen ') = 0)";

/// Fields needed to create or refresh a folder row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderUpsert {
    pub account_id: AccountId,
    pub name: String,
    pub delimiter: String,
    pub role: FolderRole,
    pub state: FolderSyncState,
}

impl FolderUpsert {
    pub fn new(account_id: AccountId, name: impl Into<String>) -> Self {
        Self {
            account_id,
            name: name.into(),
            delimiter: "/".to_owned(),
            role: FolderRole::Other,
            state: FolderSyncState::default(),
        }
    }
}

#[derive(Debug)]
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        Self::configure(&mut conn)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        Self::configure(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn configure(conn: &mut Connection) -> Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA synchronous = NORMAL;",
        )?;
        Self::migrate(conn)?;
        Ok(())
    }

    fn migrate(conn: &mut Connection) -> Result<()> {
        let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        for (version, sql) in migrations::MIGRATIONS {
            if *version <= current {
                continue;
            }
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
            tracing::debug!(version, "applied database migration");
        }
        Ok(())
    }

    /// Schema version currently stored in the database file.
    pub fn schema_version(&self) -> Result<i64> {
        let conn = self.lock()?;
        Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| Error::LockPoisoned)
    }

    // ---------------------------------------------------------------- accounts

    pub fn upsert_account(&self, account: &Account) -> Result<()> {
        account.validate()?;
        let settings = serde_json::to_string(account)?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO accounts (id, email, display_name, settings_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                 email = excluded.email,
                 display_name = excluded.display_name,
                 settings_json = excluded.settings_json",
            params![
                account.id.to_string(),
                account.email,
                account.display_name,
                settings,
                crate::now_unix(),
            ],
        )?;
        Ok(())
    }

    pub fn account(&self, id: &AccountId) -> Result<Option<Account>> {
        let conn = self.lock()?;
        let settings: Option<String> = conn
            .query_row(
                "SELECT settings_json FROM accounts WHERE id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        settings
            .map(|json| serde_json::from_str(&json).map_err(Error::from))
            .transpose()
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT settings_json FROM accounts ORDER BY display_name COLLATE NOCASE")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut accounts = Vec::new();
        for row in rows {
            accounts.push(serde_json::from_str(&row?)?);
        }
        Ok(accounts)
    }

    pub fn delete_account(&self, id: &AccountId) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM messages_fts WHERE rowid IN (
                 SELECT m.id FROM messages m
                 JOIN folders f ON f.id = m.folder_id
                 WHERE f.account_id = ?1)",
            params![id.to_string()],
        )?;
        let removed = tx.execute(
            "DELETE FROM accounts WHERE id = ?1",
            params![id.to_string()],
        )?;
        tx.commit()?;
        Ok(removed > 0)
    }

    // ----------------------------------------------------------------- folders

    pub fn upsert_folder(&self, folder: &FolderUpsert) -> Result<FolderId> {
        let conn = self.lock()?;
        let id = conn.query_row(
            "INSERT INTO folders
                 (account_id, name, delimiter, role, uid_validity, uid_next, highest_modseq, last_synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(account_id, name) DO UPDATE SET
                 delimiter = excluded.delimiter,
                 role = excluded.role,
                 uid_validity = COALESCE(excluded.uid_validity, folders.uid_validity),
                 uid_next = COALESCE(excluded.uid_next, folders.uid_next),
                 highest_modseq = COALESCE(excluded.highest_modseq, folders.highest_modseq),
                 last_synced_at = COALESCE(excluded.last_synced_at, folders.last_synced_at)
             RETURNING id",
            params![
                folder.account_id.to_string(),
                folder.name,
                folder.delimiter,
                folder.role.as_str(),
                folder.state.uid_validity,
                folder.state.uid_next,
                modseq_to_sql(folder.state.highest_modseq),
                folder.state.last_synced_at,
            ],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(FolderId(id))
    }

    pub fn list_folders(&self, account: &AccountId) -> Result<Vec<Folder>> {
        let conn = self.lock()?;
        let sql = format!(
            "SELECT {FOLDER_COLUMNS} FROM folders f
             WHERE f.account_id = ?1
             ORDER BY CASE f.role
                 WHEN 'inbox' THEN 0 WHEN 'drafts' THEN 1 WHEN 'sent' THEN 2
                 WHEN 'archive' THEN 3 WHEN 'junk' THEN 4 WHEN 'trash' THEN 5
                 ELSE 6 END, f.name COLLATE NOCASE"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![account.to_string()], map_folder)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::from)
    }

    pub fn folder(&self, id: FolderId) -> Result<Option<Folder>> {
        let conn = self.lock()?;
        let sql = format!("SELECT {FOLDER_COLUMNS} FROM folders f WHERE f.id = ?1");
        conn.query_row(&sql, params![id.0], map_folder)
            .optional()
            .map_err(Error::from)
    }

    pub fn folder_by_name(&self, account: &AccountId, name: &str) -> Result<Option<Folder>> {
        let conn = self.lock()?;
        let sql = format!(
            "SELECT {FOLDER_COLUMNS} FROM folders f WHERE f.account_id = ?1 AND f.name = ?2"
        );
        conn.query_row(&sql, params![account.to_string(), name], map_folder)
            .optional()
            .map_err(Error::from)
    }

    pub fn set_folder_sync_state(&self, id: FolderId, state: &FolderSyncState) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE folders SET
                 uid_validity = ?2, uid_next = ?3, highest_modseq = ?4, last_synced_at = ?5
             WHERE id = ?1",
            params![
                id.0,
                state.uid_validity,
                state.uid_next,
                modseq_to_sql(state.highest_modseq),
                state.last_synced_at,
            ],
        )?;
        Ok(())
    }

    /// Remove folders that the server no longer reports. An empty `keep` list
    /// is treated as "no information" and deletes nothing.
    pub fn prune_folders(&self, account: &AccountId, keep: &[String]) -> Result<Vec<FolderId>> {
        if keep.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let mut stale = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id, name FROM folders WHERE account_id = ?1")?;
            let rows = stmt.query_map(params![account.to_string()], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, name) = row?;
                if !keep.iter().any(|kept| kept == &name) {
                    stale.push(FolderId(id));
                }
            }
        }
        for folder in &stale {
            delete_folder_in(&tx, *folder)?;
        }
        tx.commit()?;
        Ok(stale)
    }

    pub fn delete_folder(&self, id: FolderId) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let removed = delete_folder_in(&tx, id)?;
        tx.commit()?;
        Ok(removed)
    }

    // ---------------------------------------------------------------- messages

    /// Insert or refresh a single header. Existing bodies and read state are
    /// preserved; UID is only unique inside a folder.
    pub fn upsert_message(&self, message: &NewMessage) -> Result<UpsertedMessage> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let result = upsert_message_in(&tx, message)?;
        tx.commit()?;
        Ok(result)
    }

    /// Batched form used by the sync worker; one transaction for the whole page.
    pub fn upsert_messages(&self, messages: &[NewMessage]) -> Result<Vec<UpsertedMessage>> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let mut results = Vec::with_capacity(messages.len());
        for message in messages {
            results.push(upsert_message_in(&tx, message)?);
        }
        tx.commit()?;
        Ok(results)
    }

    pub fn message(&self, id: MessageId) -> Result<Option<MessageHeader>> {
        let conn = self.lock()?;
        let sql = format!("SELECT {MESSAGE_COLUMNS} FROM messages m WHERE m.id = ?1");
        conn.query_row(&sql, params![id.0], map_message)
            .optional()
            .map_err(Error::from)
    }

    pub fn message_by_uid(&self, folder: FolderId, uid: u32) -> Result<Option<MessageHeader>> {
        let conn = self.lock()?;
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages m WHERE m.folder_id = ?1 AND m.uid = ?2"
        );
        conn.query_row(&sql, params![folder.0, uid], map_message)
            .optional()
            .map_err(Error::from)
    }

    pub fn list_messages(
        &self,
        folder: FolderId,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<MessageHeader>> {
        let conn = self.lock()?;
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages m
             WHERE m.folder_id = ?1
             ORDER BY m.received_at DESC, m.uid DESC
             LIMIT ?2 OFFSET ?3"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![folder.0, i64::from(limit), i64::from(offset)],
            map_message,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::from)
    }

    pub fn count_messages(&self, folder: FolderId) -> Result<u32> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE folder_id = ?1",
            params![folder.0],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    /// UIDs currently cached for a folder, used to compute incremental ranges
    /// and to detect messages that disappeared from the server.
    pub fn message_uids(&self, folder: FolderId) -> Result<std::collections::HashSet<u32>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT uid FROM messages WHERE folder_id = ?1")?;
        let rows = stmt.query_map(params![folder.0], |row| row.get::<_, u32>(0))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Error::from)
    }

    /// Remove every cached message of a folder, for example after its
    /// UIDVALIDITY changed and the old UIDs became meaningless.
    pub fn clear_folder_messages(&self, folder: FolderId) -> Result<u32> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM messages_fts WHERE rowid IN (
                 SELECT id FROM messages WHERE folder_id = ?1)",
            params![folder.0],
        )?;
        let removed = tx.execute(
            "DELETE FROM messages WHERE folder_id = ?1",
            params![folder.0],
        )?;
        tx.commit()?;
        Ok(removed as u32)
    }

    /// Apply flag updates for many messages in one transaction. UIDs that are
    /// no longer cached are ignored.
    pub fn set_flags_many(&self, folder: FolderId, updates: &[(u32, Vec<String>)]) -> Result<u32> {
        if updates.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let mut changed = 0u32;
        {
            let mut stmt =
                tx.prepare("UPDATE messages SET flags = ?3 WHERE folder_id = ?1 AND uid = ?2")?;
            for (uid, flags) in updates {
                changed += stmt.execute(params![folder.0, uid, flags.join(" ")])? as u32;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn set_flags(&self, folder: FolderId, uid: u32, flags: &[String]) -> Result<bool> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE messages SET flags = ?3 WHERE folder_id = ?1 AND uid = ?2",
            params![folder.0, uid, flags.join(" ")],
        )?;
        Ok(changed > 0)
    }

    /// Store the parsed body and index its plain text. `snippet` is only used
    /// when the header does not have one yet.
    pub fn set_body(
        &self,
        id: MessageId,
        text_plain: Option<&str>,
        html: Option<&str>,
        snippet: Option<&str>,
    ) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT id FROM messages WHERE id = ?1",
                params![id.0],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO bodies (message_id, text_plain, html, fetched_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(message_id) DO UPDATE SET
                 text_plain = COALESCE(excluded.text_plain, bodies.text_plain),
                 html = COALESCE(excluded.html, bodies.html),
                 fetched_at = excluded.fetched_at",
            params![id.0, text_plain, html, crate::now_unix()],
        )?;
        tx.execute(
            "UPDATE messages SET body_state = 1, snippet = COALESCE(?2, snippet) WHERE id = ?1",
            params![id.0, snippet],
        )?;
        let updated = tx.execute(
            "UPDATE messages_fts SET body = ?2 WHERE rowid = ?1",
            params![id.0, text_plain],
        )?;
        if updated == 0 {
            insert_fts_from_message(&tx, id)?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn body(&self, id: MessageId) -> Result<Option<(Option<String>, Option<String>)>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT text_plain, html FROM bodies WHERE message_id = ?1",
            params![id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(Error::from)
    }

    pub fn delete_message(&self, folder: FolderId, uid: u32) -> Result<Option<MessageId>> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM messages WHERE folder_id = ?1 AND uid = ?2",
                params![folder.0, uid],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            return Ok(None);
        };
        tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", params![id])?;
        tx.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(Some(MessageId(id)))
    }

    /// Remove one cached message by its local id. Used after the server has
    /// moved or permanently deleted it.
    pub fn delete_message_by_id(&self, id: MessageId) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", params![id.0])?;
        let removed = tx.execute("DELETE FROM messages WHERE id = ?1", params![id.0])?;
        tx.commit()?;
        Ok(removed > 0)
    }

    // -------------------------------------------------------------- attachments

    pub fn replace_attachments(
        &self,
        message: MessageId,
        attachments: &[AttachmentData],
    ) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM attachments WHERE message_id = ?1",
            params![message.0],
        )?;
        for a in attachments {
            let meta = &a.meta;
            tx.execute(
                "INSERT INTO attachments
                     (message_id, part_id, filename, mime_type, size, content_id, is_inline, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    message.0,
                    meta.part_id,
                    meta.filename,
                    meta.mime_type,
                    meta.size,
                    meta.content_id,
                    i64::from(meta.is_inline),
                    a.data,
                ],
            )?;
        }
        tx.execute(
            "UPDATE messages SET has_attachments = ?2 WHERE id = ?1",
            params![message.0, i64::from(!attachments.is_empty())],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_attachments(&self, message: MessageId) -> Result<Vec<AttachmentMeta>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT part_id, filename, mime_type, size, content_id, is_inline
             FROM attachments WHERE message_id = ?1 ORDER BY part_id",
        )?;
        let rows = stmt.query_map(params![message.0], |row| {
            Ok(AttachmentMeta {
                part_id: row.get(0)?,
                filename: row.get(1)?,
                mime_type: row.get(2)?,
                size: row.get(3)?,
                content_id: row.get(4)?,
                is_inline: row.get::<_, i64>(5)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::from)
    }

    /// One attachment with its decoded bytes. `data` is `None` for metadata
    /// cached before attachment bytes were stored.
    pub fn attachment(&self, message: MessageId, part_id: &str) -> Result<Option<AttachmentData>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT part_id, filename, mime_type, size, content_id, is_inline, data
             FROM attachments WHERE message_id = ?1 AND part_id = ?2",
            params![message.0, part_id],
            |row| {
                Ok(AttachmentData {
                    meta: AttachmentMeta {
                        part_id: row.get(0)?,
                        filename: row.get(1)?,
                        mime_type: row.get(2)?,
                        size: row.get(3)?,
                        content_id: row.get(4)?,
                        is_inline: row.get::<_, i64>(5)? != 0,
                    },
                    data: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(Error::from)
    }

    pub fn attachment_data(&self, message: MessageId, part_id: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT data FROM attachments WHERE message_id = ?1 AND part_id = ?2",
            params![message.0, part_id],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        )
        .optional()
        .map(|value| value.flatten())
        .map_err(Error::from)
    }

    // ------------------------------------------------------------------ search

    /// Full text search over subject, participants and fetched bodies.
    pub fn search(
        &self,
        account: &AccountId,
        query: &str,
        limit: u32,
    ) -> Result<Vec<MessageHeader>> {
        let Some(match_query) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let conn = self.lock()?;
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS}
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             JOIN folders f ON f.id = m.folder_id
             WHERE messages_fts MATCH ?1 AND f.account_id = ?2
             ORDER BY bm25(messages_fts, 10.0, 5.0, 3.0, 2.0, 1.0)
             LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                match_query,
                account.to_string(),
                i64::from(limit.clamp(1, 500))
            ],
            map_message,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::from)
    }

    pub fn unread_total(&self, account: &AccountId) -> Result<u32> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*)
             FROM messages m JOIN folders f ON f.id = m.folder_id
             WHERE f.account_id = ?1 AND instr(' ' || m.flags || ' ', ' \\Seen ') = 0",
            params![account.to_string()],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    // ------------------------------------------------------------------ outbox

    pub fn enqueue_outbox(
        &self,
        account: &AccountId,
        raw: &[u8],
        envelope_json: &str,
    ) -> Result<i64> {
        let conn = self.lock()?;
        let now = crate::now_unix();
        conn.execute(
            "INSERT INTO outbox (account_id, raw, envelope_json, state, attempts, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'queued', 0, ?4, ?4)",
            params![account.to_string(), raw, envelope_json, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Oldest message that still deserves a send attempt.
    pub fn next_queued_outbox(&self) -> Result<Option<OutboxEntry>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, account_id, raw, envelope_json, state, attempts, last_error, created_at, updated_at
             FROM outbox
             WHERE state IN ('queued', 'failed') AND attempts < 5
             ORDER BY created_at, id
             LIMIT 1",
            [],
            map_outbox,
        )
        .optional()
        .map_err(Error::from)
    }

    /// One outbox entry by id, for send-status displays and diagnostics.
    pub fn outbox_entry(&self, id: i64) -> Result<Option<OutboxEntry>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, account_id, raw, envelope_json, state, attempts, last_error, created_at, updated_at
             FROM outbox
             WHERE id = ?1",
            params![id],
            map_outbox,
        )
        .optional()
        .map_err(Error::from)
    }

    pub fn set_outbox_state(
        &self,
        id: i64,
        state: OutboxState,
        error: Option<&str>,
    ) -> Result<bool> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE outbox SET state = ?2, last_error = ?3, updated_at = ?4,
                 attempts = attempts + CASE WHEN ?2 = 'failed' THEN 1 ELSE 0 END
             WHERE id = ?1",
            params![id, state.as_str(), error, crate::now_unix()],
        )?;
        Ok(changed > 0)
    }

    pub fn delete_outbox(&self, id: i64) -> Result<bool> {
        let conn = self.lock()?;
        Ok(conn.execute("DELETE FROM outbox WHERE id = ?1", params![id])? > 0)
    }

    /// A crash can leave entries in `sending`; treat them as queued again so
    /// they are retried after a restart.
    pub fn reset_outbox_sending(&self) -> Result<u32> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE outbox SET state = 'queued', updated_at = ?1 WHERE state = 'sending'",
            params![crate::now_unix()],
        )?;
        Ok(changed as u32)
    }

    pub fn outbox_len(&self) -> Result<u32> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM outbox WHERE state != 'sent'",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }
}

// -------------------------------------------------------------------- helpers

/// IMAP mod-sequences are 63 bit unsigned, SQLite integers are signed.
fn modseq_to_sql(value: Option<u64>) -> Option<i64> {
    value.map(|v| v.min(i64::MAX as u64) as i64)
}

fn modseq_from_sql(value: i64) -> u64 {
    value.max(0) as u64
}

fn delete_folder_in(tx: &Transaction<'_>, folder: FolderId) -> Result<bool> {
    tx.execute(
        "DELETE FROM messages_fts WHERE rowid IN (
             SELECT id FROM messages WHERE folder_id = ?1)",
        params![folder.0],
    )?;
    let removed = tx.execute("DELETE FROM folders WHERE id = ?1", params![folder.0])?;
    Ok(removed > 0)
}

fn upsert_message_in(tx: &Transaction<'_>, message: &NewMessage) -> Result<UpsertedMessage> {
    let folder_exists: Option<i64> = tx
        .query_row(
            "SELECT id FROM folders WHERE id = ?1",
            params![message.folder_id.0],
            |row| row.get(0),
        )
        .optional()?;
    if folder_exists.is_none() {
        return Err(Error::FolderNotFound(message.folder_id.0));
    }

    let flags = message.flags.join(" ");
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM messages WHERE folder_id = ?1 AND uid = ?2",
            params![message.folder_id.0, message.uid],
            |row| row.get(0),
        )
        .optional()?;

    let (id, created) = match existing {
        Some(id) => {
            tx.execute(
                "UPDATE messages SET
                     message_id = ?2, thread_key = ?3, subject = ?4, from_name = ?5,
                     from_addr = ?6, to_addrs = ?7, cc_addrs = ?8, sent_at = ?9,
                     received_at = ?10, size = ?11, flags = ?12, has_attachments = ?13,
                     snippet = COALESCE(?14, snippet)
                 WHERE id = ?1",
                params![
                    id,
                    message.message_id,
                    message.thread_key,
                    message.subject,
                    message.from_name,
                    message.from_addr,
                    message.to_addrs,
                    message.cc_addrs,
                    message.sent_at,
                    message.received_at,
                    message.size,
                    flags,
                    i64::from(message.has_attachments),
                    message.snippet,
                ],
            )?;
            (id, false)
        }
        None => {
            tx.execute(
                "INSERT INTO messages
                     (folder_id, uid, message_id, thread_key, subject, from_name, from_addr,
                      to_addrs, cc_addrs, sent_at, received_at, snippet, size, flags, has_attachments)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    message.folder_id.0,
                    message.uid,
                    message.message_id,
                    message.thread_key,
                    message.subject,
                    message.from_name,
                    message.from_addr,
                    message.to_addrs,
                    message.cc_addrs,
                    message.sent_at,
                    message.received_at,
                    message.snippet,
                    message.size,
                    flags,
                    i64::from(message.has_attachments),
                ],
            )?;
            (tx.last_insert_rowid(), true)
        }
    };

    upsert_fts_headers(tx, MessageId(id), message)?;
    Ok(UpsertedMessage {
        id: MessageId(id),
        created,
    })
}

fn upsert_fts_headers(tx: &Transaction<'_>, id: MessageId, message: &NewMessage) -> Result<()> {
    let updated = tx.execute(
        "UPDATE messages_fts
         SET subject = ?2, from_name = ?3, from_addr = ?4, to_addrs = ?5
         WHERE rowid = ?1",
        params![
            id.0,
            message.subject,
            message.from_name,
            message.from_addr,
            message.to_addrs,
        ],
    )?;
    if updated == 0 {
        tx.execute(
            "INSERT INTO messages_fts (rowid, subject, from_name, from_addr, to_addrs, body)
             VALUES (?1, ?2, ?3, ?4, ?5, '')",
            params![
                id.0,
                message.subject,
                message.from_name,
                message.from_addr,
                message.to_addrs,
            ],
        )?;
    }
    Ok(())
}

/// Rebuild an FTS row from the message table, used when a body arrives for a
/// message whose index row was lost.
fn insert_fts_from_message(tx: &Transaction<'_>, id: MessageId) -> Result<()> {
    tx.execute(
        "INSERT INTO messages_fts (rowid, subject, from_name, from_addr, to_addrs, body)
         SELECT m.id, m.subject, m.from_name, m.from_addr, m.to_addrs, ''
         FROM messages m WHERE m.id = ?1",
        params![id.0],
    )?;
    Ok(())
}

fn map_message(row: &Row<'_>) -> rusqlite::Result<MessageHeader> {
    let flags: String = row.get(14)?;
    Ok(MessageHeader {
        id: MessageId(row.get(0)?),
        folder_id: FolderId(row.get(1)?),
        uid: row.get(2)?,
        message_id: row.get(3)?,
        thread_key: row.get(4)?,
        subject: row.get(5)?,
        from_name: row.get(6)?,
        from_addr: row.get(7)?,
        to_addrs: row.get(8)?,
        cc_addrs: row.get(9)?,
        sent_at: row.get(10)?,
        received_at: row.get(11)?,
        snippet: row.get(12)?,
        size: row.get(13)?,
        flags: flags
            .split_whitespace()
            .map(std::string::ToString::to_string)
            .collect(),
        has_attachments: row.get::<_, i64>(15)? != 0,
        body_fetched: row.get::<_, i64>(16)? != 0,
    })
}

fn map_folder(row: &Row<'_>) -> rusqlite::Result<Folder> {
    let role: String = row.get(4)?;
    Ok(Folder {
        id: FolderId(row.get(0)?),
        account_id: AccountId::parse(&row.get::<_, String>(1)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
        })?,
        name: row.get(2)?,
        delimiter: row.get(3)?,
        role: role.parse().unwrap_or_default(),
        uid_validity: row.get(5)?,
        uid_next: row.get(6)?,
        highest_modseq: row.get::<_, Option<i64>>(7)?.map(modseq_from_sql),
        last_synced_at: row.get(8)?,
        message_count: row.get::<_, i64>(9)? as u32,
        unread_count: row.get::<_, i64>(10)? as u32,
    })
}

fn map_outbox(row: &Row<'_>) -> rusqlite::Result<OutboxEntry> {
    let state: String = row.get(4)?;
    Ok(OutboxEntry {
        id: row.get(0)?,
        account_id: AccountId::parse(&row.get::<_, String>(1)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
        })?,
        raw: row.get(2)?,
        envelope_json: row.get(3)?,
        state: OutboxState::from_store(&state),
        attempts: row.get(5)?,
        last_error: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

/// Turn user input into a safe FTS5 query: every term becomes a quoted prefix
/// term joined with AND. Returns `None` when nothing searchable is left.
fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|term| {
            term.chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, '@' | '.' | '-' | '_' | '+'))
                .collect::<String>()
        })
        .filter(|term| !term.is_empty())
        .take(8)
        .collect();
    if terms.is_empty() {
        return None;
    }
    Some(
        terms
            .iter()
            .map(|term| format!("\"{term}\"*"))
            .collect::<Vec<_>>()
            .join(" AND "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_quotes_and_prefixes_terms() {
        assert_eq!(
            fts_query("hello world"),
            Some("\"hello\"* AND \"world\"*".into())
        );
        assert_eq!(fts_query("  "), None);
        assert_eq!(fts_query("!!!"), None);
        assert_eq!(fts_query("a\"b"), Some("\"ab\"*".into()));
        assert_eq!(
            fts_query("ada@example.com"),
            Some("\"ada@example.com\"*".into())
        );
    }

    #[test]
    fn fts_query_limits_terms() {
        let query = fts_query("a b c d e f g h i j").expect("query");
        assert_eq!(query.matches(" AND ").count(), 7);
    }
}
