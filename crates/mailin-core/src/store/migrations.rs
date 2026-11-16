/// Ordered list of schema migrations. Never edit a migration that has shipped;
/// append a new one and bump [`SCHEMA_VERSION`].
pub(crate) const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../../migrations/0001_init.sql")),
    (2, include_str!("../../migrations/0002_attachment_data.sql")),
    (3, include_str!("../../migrations/0003_templates.sql")),
    (4, include_str!("../../migrations/0004_contacts.sql")),
    (
        5,
        include_str!("../../migrations/0005_scheduled_actions.sql"),
    ),
    (6, include_str!("../../migrations/0006_rules.sql")),
    (7, include_str!("../../migrations/0007_virtual_folders.sql")),
    (8, include_str!("../../migrations/0008_pgp_keys.sql")),
    (9, include_str!("../../migrations/0009_calendars.sql")),
    (10, include_str!("../../migrations/0010_plugins.sql")),
    (11, include_str!("../../migrations/0011_threading.sql")),
    (12, include_str!("../../migrations/0012_pgp_mime.sql")),
    (13, include_str!("../../migrations/0013_references.sql")),
    (
        14,
        include_str!("../../migrations/0014_folder_subscribed.sql"),
    ),
    (
        15,
        include_str!("../../migrations/0015_folders_prune_watermark.sql"),
    ),
];

pub const SCHEMA_VERSION: i64 = 15;

/// Snapshot the database before pending migrations are applied, so a failed
/// migration never leaves the user with nothing to fall back to. Best
/// effort: a backup failure is logged and ignored — a migration must never
/// be blocked by inability to snapshot.
///
/// Keeps the two most recent `*.backup-<version>` files next to the
/// database file.
pub(crate) fn backup_before_migrate(conn: &rusqlite::Connection, db_path: &std::path::Path) {
    let current: i64 = match conn.query_row("PRAGMA user_version", [], |row| row.get(0)) {
        Ok(version) => version,
        Err(error) => {
            tracing::warn!(%error, "could not read the schema version for a pre-migration backup");
            return;
        }
    };
    if current >= SCHEMA_VERSION {
        return;
    }
    // A zero-byte file has no data worth snapshotting.
    if std::fs::metadata(db_path)
        .map(|meta| meta.len() == 0)
        .unwrap_or(true)
    {
        return;
    }
    let mut name = db_path.as_os_str().to_owned();
    name.push(format!(".backup-{current}"));
    let target = std::path::PathBuf::from(name);
    // `VACUUM INTO` refuses to overwrite an existing file.
    let _ = std::fs::remove_file(&target);
    let escaped = target.to_string_lossy().replace('\'', "''");
    match conn.execute_batch(&format!("VACUUM INTO '{escaped}'")) {
        Ok(()) => tracing::info!(path = %target.display(), "pre-migration backup written"),
        Err(error) => {
            tracing::warn!(%error, path = %target.display(), "could not write a pre-migration backup");
            return;
        }
    }
    prune_old_backups(db_path, 2);
}

/// Keep the `keep` most recent `<file>.backup-<n>` files next to the
/// database file.
fn prune_old_backups(db_path: &std::path::Path, keep: usize) {
    let Some(parent) = db_path.parent() else {
        return;
    };
    let Some(base) = db_path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!("{base}.backup-");
    let mut backups: Vec<(i64, std::path::PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(suffix) = name.strip_prefix(&prefix) else {
                continue;
            };
            if let Ok(version) = suffix.parse::<i64>() {
                backups.push((version, entry.path()));
            }
        }
    }
    backups.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in backups.into_iter().skip(keep) {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(%error, path = %path.display(), "could not remove an old pre-migration backup");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_ascending_and_unique() {
        let mut previous = 0;
        for (version, sql) in MIGRATIONS {
            assert!(*version > previous, "migration {version} is out of order");
            assert!(!sql.trim().is_empty(), "migration {version} is empty");
            previous = *version;
        }
        assert_eq!(previous, SCHEMA_VERSION);
    }

    #[test]
    fn backup_before_migrate_writes_a_snapshot_and_keeps_two() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("mailin.db");
        let conn = rusqlite::Connection::open(&db).expect("open");
        conn.execute_batch(
            "PRAGMA user_version = 0; \
             CREATE TABLE items (x INTEGER) STRICT; \
             INSERT INTO items VALUES (42);",
        )
        .expect("seed");

        // Backup is written with the current version in the name.
        backup_before_migrate(&conn, &db);
        let backup_zero = dir.path().join("mailin.db.backup-0");
        assert!(backup_zero.exists());
        let snapshot = rusqlite::Connection::open(&backup_zero).expect("backup opens");
        let x: i64 = snapshot
            .query_row("SELECT x FROM items", [], |row| row.get(0))
            .expect("row");
        assert_eq!(x, 42);
        drop(snapshot);

        // Further backups prune down to the two most recent.
        conn.pragma_update(None, "user_version", 1).expect("one");
        backup_before_migrate(&conn, &db);
        conn.pragma_update(None, "user_version", 2).expect("two");
        backup_before_migrate(&conn, &db);
        assert!(!dir.path().join("mailin.db.backup-0").exists());
        assert!(dir.path().join("mailin.db.backup-1").exists());
        assert!(dir.path().join("mailin.db.backup-2").exists());

        // Up to date: no backup is written.
        let latest = SCHEMA_VERSION;
        conn.pragma_update(None, "user_version", latest)
            .expect("latest");
        let before = std::fs::read_dir(dir.path()).expect("dir").count();
        backup_before_migrate(&conn, &db);
        let after = std::fs::read_dir(dir.path()).expect("dir").count();
        assert_eq!(before, after);
    }
}
