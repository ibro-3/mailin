//! Recovery helpers for when the local database cannot be opened.
//!
//! A damaged database file makes `Store::open` fail (the integrity check
//! reports it). The front-ends offer a "Reset local cache" recovery action:
//! salvage what is still readable from the broken file, move the broken
//! files aside (never delete them), and start a fresh database that
//! re-syncs from the server. Accounts and queued outbox mail are re-imported
//! when the broken file is still readable; anything else stays available in
//! the renamed files.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::account::{Account, AccountId};
use crate::config::Paths;
use crate::error::Result;
use crate::store::Store;

/// What the reset was able to salvage from the broken database.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SalvageReport {
    /// Accounts re-imported into the fresh database.
    pub accounts: usize,
    /// Queued outbox messages re-queued into the fresh database.
    pub outbox: usize,
    /// Where the previous files were moved to.
    pub moved_to: Vec<PathBuf>,
}

struct Salvage {
    accounts: Vec<Account>,
    outbox: Vec<(AccountId, Vec<u8>, String)>,
}

/// Read accounts and queued outbox messages straight out of the broken
/// database, without mutating it. `None` when the file is unreadable or
/// fails the integrity check.
fn salvage_from_disk(db: &Path) -> Option<Salvage> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .ok()?;
    if check != "ok" {
        return None;
    }
    let accounts = conn
        .prepare("SELECT settings_json FROM accounts")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| {
                    rows.collect::<std::result::Result<Vec<_>, _>>()
                        .unwrap_or_default()
                })
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|json| serde_json::from_str::<Account>(&json).ok())
        .collect::<Vec<_>>();
    let account_ids: HashSet<AccountId> = accounts.iter().map(|account| account.id).collect();
    let outbox = conn
        .prepare("SELECT account_id, raw, envelope_json FROM outbox")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .ok()
            .map(|rows| {
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap_or_default()
            })
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(account_id, raw, envelope_json)| {
            let parsed = AccountId::parse(&account_id).ok()?;
            account_ids
                .contains(&parsed)
                .then_some((parsed, raw, envelope_json))
        })
        .collect();
    Some(Salvage { accounts, outbox })
}

/// Reset the local cache.
///
/// Renames `mailin.db` and its `-wal`/`-shm` sidecars to
/// `*.corrupt-<unix_seconds>`, opens a fresh database, and re-imports the
/// salvaged accounts and outbox rows. The caller is expected to re-sync
/// afterwards. Never deletes the broken files.
pub fn reset_local_cache(paths: &Paths) -> Result<SalvageReport> {
    paths.ensure()?;
    let db = paths.database_file();

    let salvage = salvage_from_disk(&db);

    let stamp = crate::now_unix();
    let mut moved_to = Vec::new();
    let mut files = vec![db.clone()];
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db.as_os_str().to_owned();
        sidecar.push(suffix);
        files.push(PathBuf::from(sidecar));
    }
    for file in files {
        if file.exists() {
            let mut target = file.as_os_str().to_owned();
            target.push(format!(".corrupt-{stamp}"));
            std::fs::rename(&file, &target)?;
            moved_to.push(PathBuf::from(target));
        }
    }

    let store = Store::open(paths.database_file())?;
    let mut report = SalvageReport {
        accounts: 0,
        outbox: 0,
        moved_to,
    };
    if let Some(salvage) = salvage {
        for account in salvage.accounts {
            if let Err(error) = store.upsert_account(&account) {
                tracing::warn!(%error, account = %account.email, "could not re-import an account during reset");
                continue;
            }
            report.accounts += 1;
        }
        for (account_id, raw, envelope_json) in salvage.outbox {
            if let Err(error) = store.enqueue_outbox(&account_id, &raw, &envelope_json) {
                tracing::warn!(%error, "could not re-queue an outbox message during reset");
                continue;
            }
            report.outbox += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountId;
    use crate::model::OutboxEntry;
    use crate::store::Store;
    use crate::{Account, ServerSettings};

    fn account() -> Account {
        Account::new(
            "Ada Lovelace",
            "ada@example.com",
            ServerSettings::imap("imap.example.com", "ada@example.com"),
            ServerSettings::smtp_starttls("smtp.example.com", "ada@example.com"),
        )
    }

    #[test]
    fn reset_salvages_accounts_and_outbox() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        paths.ensure().expect("ensure");

        let store = Store::open(paths.database_file()).expect("open store");
        let account = account();
        store.upsert_account(&account).expect("account");
        store
            .enqueue_outbox(
                &account.id,
                b"Subject: hi\r\n\r\nbody",
                "{\"from\":\"a@b.c\",\"to\":[\"d@e.f\"],\"cc\":[],\"bcc\":[]}",
            )
            .expect("outbox");
        drop(store);

        let report = reset_local_cache(&paths).expect("reset");
        assert_eq!(report.accounts, 1);
        assert_eq!(report.outbox, 1);
        assert!(!report.moved_to.is_empty());
        assert!(
            report
                .moved_to
                .iter()
                .all(|path| path.to_string_lossy().contains(".corrupt-"))
        );

        let store = Store::open(paths.database_file()).expect("fresh store");
        let accounts = store.list_accounts().expect("accounts");
        assert_eq!(accounts, vec![account.clone()]);
        let next = store.next_queued_outbox().expect("outbox");
        assert!(
            matches!(next, Some(OutboxEntry { ref account_id, .. }) if *account_id == account.id)
        );
    }

    #[test]
    fn reset_on_garbage_file_starts_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        paths.ensure().expect("ensure");
        std::fs::write(paths.database_file(), b"not a sqlite database at all").expect("garbage");

        let report = reset_local_cache(&paths).expect("reset");
        assert_eq!(report.accounts, 0);
        assert_eq!(report.outbox, 0);

        // A fresh database was created and works.
        let store = Store::open(paths.database_file()).expect("fresh store");
        assert!(store.list_accounts().expect("accounts").is_empty());
        // The garbage was moved aside, not deleted.
        assert!(report.moved_to.iter().all(|path| path.exists()));
        assert!(
            report
                .moved_to
                .iter()
                .all(|path| path.to_string_lossy().contains(".corrupt-"))
        );
    }

    #[test]
    fn reset_without_a_database_is_a_no_op_salvage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        paths.ensure().expect("ensure");
        let _ = std::fs::remove_file(paths.database_file());

        let report = reset_local_cache(&paths).expect("reset");
        assert_eq!(report, SalvageReport::default());
    }

    #[test]
    fn account_ids_round_trip() {
        let id = AccountId::new();
        assert_eq!(AccountId::parse(&id.to_string()).expect("parse"), id);
    }
}
