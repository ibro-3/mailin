//! Support helpers: redacted diagnostics and local-data backups (D4/D5).
//!
//! Both are deliberately one-shot functions over paths the caller already
//! knows, so the front-ends only have to wire a button to them.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::client::Mailin;
use crate::config::{Config, Paths};
use crate::error::{Error, Result};

/// Maximum number of log lines included in a diagnostics report.
const LOG_LINES: usize = 100;

/// Build a redacted diagnostics report suitable for pasting into an issue.
///
/// No message bodies, addresses, passwords or search queries are included.
pub fn build_diagnostics(mailin: &Mailin, config: &Config, paths: &Paths) -> String {
    let mut out = String::new();
    out.push_str(&format!("Mailin {}\n", env!("CARGO_PKG_VERSION")));
    out.push_str(&format!("OS: {}\n", std::env::consts::OS));
    out.push_str(&format!("Arch: {}\n", std::env::consts::ARCH));

    let schema = mailin
        .store()
        .schema_version()
        .map(|version| version.to_string())
        .unwrap_or_else(|error| format!("unavailable: {error}"));
    out.push_str(&format!("DB schema version: {schema}\n"));

    let account_count = mailin
        .store()
        .list_accounts()
        .map(|accounts| accounts.len())
        .unwrap_or(0);
    out.push_str(&format!("Accounts: {account_count}\n"));
    out.push_str("Features: IMAP, SMTP, CalDAV, OpenPGP, Lua plugins\n");
    out.push_str(&format!(
        "Keyring: {}\n",
        if cfg!(feature = "keyring") {
            "enabled"
        } else {
            "disabled"
        }
    ));
    out.push_str("\n--- Preferences (redacted) ---\n");
    let mut redacted = config.clone();
    redacted.search = Default::default();
    match toml::to_string_pretty(&redacted) {
        Ok(text) => out.push_str(&text),
        Err(error) => out.push_str(&format!("unavailable: {error}\n")),
    }

    out.push_str("\n--- Last log lines ---\n");
    let mut lines = Vec::new();
    for file in [paths.log_file(), paths.log_file().with_extension("log.1")] {
        if let Ok(text) = std::fs::read_to_string(file) {
            lines.extend(text.lines().map(str::to_owned));
        }
    }
    let start = lines.len().saturating_sub(LOG_LINES);
    for line in &lines[start..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Write a zip containing a consistent snapshot of the database and the
/// preferences file (never credentials) to `destination`.
///
/// The database is copied with SQLite's `VACUUM INTO`, which produces a
/// consistent snapshot even while the client is running, without touching
/// the live file.
pub fn backup_local_data(paths: &Paths, destination: &Path) -> Result<()> {
    paths.ensure()?;
    let db = paths.database_file();
    let snapshot = paths
        .data_dir
        .join(format!("mailin-backup-{}.db", crate::now_unix()));

    // Always clean up the temporary snapshot, even when zipping fails.
    let result = (|| -> Result<()> {
        let conn = rusqlite::Connection::open(&db)?;
        conn.execute("VACUUM INTO ?1", [snapshot.to_string_lossy().as_ref()])?;
        drop(conn);

        let file = File::create(destination)?;
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o600);

        let zip_error = |error: zip::result::ZipError| {
            Error::Config(format!("could not write the backup archive: {error}"))
        };
        zip.start_file("mailin.db", options).map_err(zip_error)?;
        let mut bytes = Vec::new();
        File::open(&snapshot)?.read_to_end(&mut bytes)?;
        zip.write_all(&bytes)?;

        let config_file = paths.config_file();
        if config_file.exists() {
            zip.start_file("config.toml", options).map_err(zip_error)?;
            let mut bytes = Vec::new();
            File::open(&config_file)?.read_to_end(&mut bytes)?;
            zip.write_all(&bytes)?;
        }
        zip.finish().map_err(zip_error)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&snapshot);
    result
}

/// Suggested filename for a backup, including the current date/time.
pub fn backup_filename() -> PathBuf {
    PathBuf::from(format!("mailin-backup-{}.zip", crate::now_unix()))
}

/// Reject destinations that are not absolute, to avoid surprising relative
/// writes from a file chooser.
pub fn validate_backup_destination(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::Config(
            "backup destination must be an absolute path".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_filename_has_the_expected_shape() {
        let name = backup_filename();
        let name = name.to_string_lossy();
        assert!(name.starts_with("mailin-backup-"));
        assert!(name.ends_with(".zip"));
    }

    #[test]
    fn relative_backup_destinations_are_rejected() {
        assert!(validate_backup_destination(Path::new("backup.zip")).is_err());
        assert!(validate_backup_destination(Path::new("/tmp/backup.zip")).is_ok());
    }
}
