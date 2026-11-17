use std::collections::HashMap;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::account::AccountId;
use crate::error::{Error, Result};
use crate::model::FolderId;

const APP_QUALIFIER: &str = "dev";
const APP_ORGANISATION: &str = "mailin";
const APP_NAME: &str = "Mailin";

/// Filesystem locations used by Mailin.
///
/// `MAILIN_HOME` overrides all three directories with a single root; it exists
/// for integration tests and for portable installs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        if let Some(root) = std::env::var_os("MAILIN_HOME") {
            return Ok(Self::with_root(root));
        }
        let dirs = ProjectDirs::from(APP_QUALIFIER, APP_ORGANISATION, APP_NAME)
            .ok_or_else(|| Error::Config("could not determine the user's home directory".into()))?;
        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
            cache_dir: dirs.cache_dir().to_path_buf(),
        })
    }

    pub fn with_root(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn database_file(&self) -> PathBuf {
        self.data_dir.join("mailin.db")
    }

    /// Current rolling log file (D3). The previous generation is
    /// `mailin.log.1`; both live in the data directory.
    pub fn log_file(&self) -> PathBuf {
        self.data_dir.join("mailin.log")
    }

    /// Reconstruct Paths from the database file path.
    /// Assumes standard layout: data_dir/mailin.db, sibling config_dir/config.toml
    pub fn from_database_file(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref();
        let data_dir = db_path
            .parent()
            .ok_or_else(|| Error::Config("database file has no parent directory".into()))?;
        let root = data_dir
            .parent()
            .ok_or_else(|| Error::Config("data directory has no parent directory".into()))?;
        Ok(Self {
            config_dir: root.join("config"),
            data_dir: data_dir.to_path_buf(),
            cache_dir: root.join("cache"),
        })
    }

    /// Create the directories with user-only permissions.
    pub fn ensure(&self) -> Result<()> {
        for dir in [&self.config_dir, &self.data_dir, &self.cache_dir] {
            std::fs::create_dir_all(dir)?;
            restrict_to_owner(dir)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// Row spacing and preview lines shown in the message list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessageListDensity {
    /// Three lines per row with generous padding.
    #[default]
    Comfortable,
    /// Tighter padding, snippet still visible.
    Cozy,
    /// Minimal padding, no snippet: fits the most messages.
    Compact,
}

/// Thread/conversation view mode for the message list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadMode {
    /// Flat list, one row per message (legacy behavior).
    #[default]
    Off,
    /// Group by conversation, expand/collapse threads.
    Flat,
    /// Group by conversation with nested indentation (future).
    Nested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiPreferences {
    pub theme: Theme,
    /// Render HTML bodies. Remote content stays blocked either way.
    pub html_enabled: bool,
    /// Load remote images and other network resources in HTML mail.
    pub remote_content_enabled: bool,
    /// Number of headers loaded per page in the message list.
    pub page_size: u32,
    /// Mark a message as read when it is opened.
    pub mark_read_on_open: bool,
    /// Show a desktop notification when mail arrives in the background.
    pub notifications: bool,
    /// Row spacing and preview lines in the message list.
    pub message_list_density: MessageListDensity,
    /// Thread/conversation view mode.
    pub thread_mode: ThreadMode,
    /// Enable quick actions (swipe on GTK, hover on WinUI) on message rows.
    pub quick_actions: bool,
    /// Which actions are available in the quick-actions overlay.
    pub quick_action_items: Vec<QuickAction>,
    /// Last selected folder per account, so returning users land where they
    /// left off. Virtual folders are allowed (the user may have opened the
    /// Outbox deliberately); a stale id falls back to Inbox.
    pub last_folders: HashMap<AccountId, FolderId>,
    /// Main window geometry and list/reader split position, persisted on close.
    pub window_width: i32,
    pub window_height: i32,
    pub paned_position: i32,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            html_enabled: true,
            remote_content_enabled: false,
            page_size: 100,
            mark_read_on_open: true,
            notifications: true,
            message_list_density: MessageListDensity::default(),
            thread_mode: ThreadMode::default(),
            quick_actions: true,
            quick_action_items: vec![
                QuickAction::Archive,
                QuickAction::Delete,
                QuickAction::Flag,
                QuickAction::MarkRead,
            ],
            last_folders: HashMap::new(),
            window_width: 1180,
            window_height: 760,
            paned_position: 380,
        }
    }
}

/// A single action offered in the message-row quick-actions overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuickAction {
    Archive,
    Delete,
    Junk,
    Flag,
    Unflag,
    MarkRead,
    MarkUnread,
}

impl QuickAction {
    /// Short label used on the button.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Archive => "Archive",
            Self::Delete => "Delete",
            Self::Junk => "Junk",
            Self::Flag => "Flag",
            Self::Unflag => "Unflag",
            Self::MarkRead => "Read",
            Self::MarkUnread => "Unread",
        }
    }

    /// Icon name for the button.
    pub const fn icon(self) -> &'static str {
        match self {
            Self::Archive => "folder-archive-symbolic",
            Self::Delete => "user-trash-symbolic",
            Self::Junk => "mail-mark-junk-symbolic",
            Self::Flag => "starred-symbolic",
            Self::Unflag => "non-starred-symbolic",
            Self::MarkRead => "mail-read-symbolic",
            Self::MarkUnread => "mail-unread-symbolic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BodySyncPolicy {
    /// Fetch bodies only when the user opens a message.
    #[default]
    OnOpen,
    /// Fetch bodies in the background for each newly discovered message.
    Eager,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncPreferences {
    /// Default poll interval used when an account does not override it in
    /// its own settings.
    pub poll_interval_secs: u64,
    pub body_sync: BodySyncPolicy,
    /// Keep message headers for this many days; `0` disables pruning.
    pub retention_days: u32,
}

impl Default for SyncPreferences {
    fn default() -> Self {
        Self {
            poll_interval_secs: 60,
            body_sync: BodySyncPolicy::OnOpen,
            retention_days: 0,
        }
    }
}

/// Search history entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHistoryEntry {
    pub query: String,
    pub account_id: AccountId,
    pub folder_id: Option<FolderId>,
    pub timestamp: i64,
}

/// Application preferences. Accounts and credentials are *not* stored here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub ui: UiPreferences,
    pub sync: SyncPreferences,
    /// Search history (last 20 per account)
    pub search: SearchHistory,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHistory {
    /// Per-account search history: account_id -> Vec<SearchHistoryEntry>
    pub history: HashMap<AccountId, Vec<SearchHistoryEntry>>,
}

impl SearchHistory {
    /// Get search history for an account (most recent first, max 20).
    pub fn get(&self, account_id: &AccountId) -> Vec<SearchHistoryEntry> {
        self.history
            .get(account_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(20)
            .collect()
    }

    /// Add a search query to history.
    pub fn add(&mut self, account_id: AccountId, query: String, folder_id: Option<FolderId>) {
        let entry = SearchHistoryEntry {
            query,
            account_id,
            folder_id,
            timestamp: crate::now_unix(),
        };
        let list = self.history.entry(account_id).or_default();
        // Remove duplicates
        list.retain(|e| e.query != entry.query);
        // Insert at front
        list.insert(0, entry);
        // Trim to 20
        if list.len() > 20 {
            list.truncate(20);
        }
    }

    /// Remove one query from an account's history (E11).
    pub fn remove(&mut self, account_id: &AccountId, query: &str) {
        if let Some(list) = self.history.get_mut(account_id) {
            list.retain(|entry| entry.query != query);
            if list.is_empty() {
                self.history.remove(account_id);
            }
        }
    }

    /// Drop the whole history for an account (E11).
    pub fn clear(&mut self, account_id: &AccountId) {
        self.history.remove(account_id);
    }
}

impl Config {
    /// Rename `file` out of the way after an unrecoverable read/validation
    /// failure, best effort. Returns where it went.
    fn rename_aside(file: &std::path::Path) -> Option<std::path::PathBuf> {
        let broken = file.with_extension("toml.broken");
        if std::fs::rename(file, &broken).is_ok() {
            return Some(broken);
        }
        let stamped = file.with_file_name(format!("config.toml.broken-{}", crate::now_unix()));
        if std::fs::rename(file, &stamped).is_ok() {
            return Some(stamped);
        }
        None
    }

    /// Describe a recovery that just happened.
    fn recovery_report(
        reason: &dyn std::fmt::Display,
        moved: Option<std::path::PathBuf>,
    ) -> String {
        let where_to = moved
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "the configuration file".to_owned());
        format!(
            "Mailin couldn't read its preferences ({reason}). Defaults were restored; your previous file was saved as {where_to}."
        )
    }

    /// Load `config.toml`, tolerating a broken or invalid file.
    ///
    /// A file that fails to parse or validate is renamed to
    /// `config.toml.broken` and defaults are used, so a damaged preferences
    /// file never prevents startup. Returns the config and, when recovery
    /// happened, a user-facing description of what happened.
    pub fn load_recovering(paths: &Paths) -> Result<(Self, Option<String>)> {
        let file = paths.config_file();
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), None));
            }
            Err(e) => return Err(Error::Io(e)),
        };
        match toml::from_str::<Self>(&text) {
            Ok(config) => match config.validate() {
                Ok(()) => Ok((config, None)),
                Err(error) => {
                    let moved = Self::rename_aside(&file);
                    tracing::error!(%error, path = %file.display(), "ignoring invalid preferences");
                    Ok((Self::default(), Some(Self::recovery_report(&error, moved))))
                }
            },
            Err(error) => {
                let moved = Self::rename_aside(&file);
                tracing::error!(%error, path = %file.display(), "ignoring broken preferences");
                Ok((Self::default(), Some(Self::recovery_report(&error, moved))))
            }
        }
    }

    /// Load `config.toml`, falling back to defaults when it does not exist.
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        match std::fs::read_to_string(&file) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", file.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Write `config.toml` atomically and crash-safely: a sibling temporary
    /// file is fully flushed and synced to disk before it replaces the old
    /// one, and the containing directory is synced after the rename, so a
    /// power loss cannot leave an empty or torn `config.toml`.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure()?;
        let file = paths.config_file();
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("could not serialize config: {e}")))?;
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes())?;
        // Flush + sync so the bytes are durable before the rename.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp)
            .and_then(|file| file.sync_all())
            .map_err(|error| Error::Config(format!("could not sync config: {error}")))?;
        restrict_to_owner(&tmp)?;
        std::fs::rename(&tmp, &file)?;
        // Sync the directory so the rename itself is durable.
        #[cfg(unix)]
        if let Some(parent) = file.parent()
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.ui.page_size == 0 || self.ui.page_size > 1000 {
            return Err(Error::Config(
                "ui.page_size must be between 1 and 1000".into(),
            ));
        }
        if self.sync.poll_interval_secs < 15 {
            return Err(Error::Config(
                "sync.poll_interval_secs must be at least 15".into(),
            ));
        }
        Ok(())
    }
}

/// Compute the "active" state of each quick-action switch row for `actions`,
/// using the default configuration. The preferences "Restore defaults" button
/// applies these to the rows it actually shows. Pure helper so the restore
/// logic is testable without the front-end toolkit.
pub fn quick_action_restore_states(actions: &[QuickAction]) -> Vec<bool> {
    let defaults = UiPreferences::default().quick_action_items;
    actions
        .iter()
        .map(|action| defaults.contains(action))
        .collect()
}

/// Apply toggle states to the current quick-action list. Preserves the
/// existing order, removes disabled actions, and appends newly enabled actions
/// in the order they appear in `toggles`. Actions not mentioned in `toggles`
/// keep their current state. Pure helper so both front-ends can share the
/// toggle semantics.
pub fn reconcile_quick_action_items(
    current: &[QuickAction],
    toggles: &[(QuickAction, bool)],
) -> Vec<QuickAction> {
    let mut result: Vec<QuickAction> = Vec::new();

    // Keep existing actions in their current order, unless explicitly disabled.
    for &action in current {
        let enabled = toggles
            .iter()
            .find(|(candidate, _)| *candidate == action)
            .is_none_or(|(_, enabled)| *enabled);
        if enabled && !result.contains(&action) {
            result.push(action);
        }
    }

    // Append newly enabled actions in the order the toggles are presented.
    for &(action, enabled) in toggles {
        if enabled && !result.contains(&action) {
            result.push(action);
        }
    }

    result
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Directories need the execute bit to be traversable; files do not.
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_action_restore_states_match_defaults() {
        let shown = [
            QuickAction::Archive,
            QuickAction::Delete,
            QuickAction::Flag,
            QuickAction::MarkRead,
            QuickAction::MarkUnread,
            QuickAction::Junk,
        ];
        let states = quick_action_restore_states(&shown);
        let expected: Vec<bool> = shown
            .iter()
            .map(|action| UiPreferences::default().quick_action_items.contains(action))
            .collect();
        assert_eq!(states, expected);
        assert_eq!(states, [true, true, true, true, false, false]);
    }

    #[test]
    fn quick_action_restore_states_is_empty_for_no_rows() {
        assert!(quick_action_restore_states(&[]).is_empty());
    }

    #[test]
    fn reconcile_preserves_order_when_nothing_changes() {
        let current = [QuickAction::Archive, QuickAction::Delete, QuickAction::Flag];
        assert_eq!(reconcile_quick_action_items(&current, &[]), current);
    }

    #[test]
    fn reconcile_removes_disabled_action() {
        let current = [QuickAction::Archive, QuickAction::Delete, QuickAction::Flag];
        let toggles = [(QuickAction::Delete, false)];
        assert_eq!(
            reconcile_quick_action_items(&current, &toggles),
            [QuickAction::Archive, QuickAction::Flag]
        );
    }

    #[test]
    fn reconcile_appends_newly_enabled_action_at_end() {
        let current = [QuickAction::Archive, QuickAction::Delete];
        let toggles = [(QuickAction::Junk, true), (QuickAction::MarkUnread, true)];
        assert_eq!(
            reconcile_quick_action_items(&current, &toggles),
            [
                QuickAction::Archive,
                QuickAction::Delete,
                QuickAction::Junk,
                QuickAction::MarkUnread,
            ]
        );
    }

    #[test]
    fn reconcile_deduplicates_existing_actions() {
        let current = [
            QuickAction::Archive,
            QuickAction::Archive,
            QuickAction::Delete,
        ];
        let toggles = [(QuickAction::Archive, true)];
        assert_eq!(
            reconcile_quick_action_items(&current, &toggles),
            [QuickAction::Archive, QuickAction::Delete]
        );
    }

    #[test]
    fn reconcile_returns_empty_when_everything_disabled() {
        let current = [QuickAction::Archive, QuickAction::Delete];
        let toggles = [(QuickAction::Archive, false), (QuickAction::Delete, false)];
        assert!(reconcile_quick_action_items(&current, &toggles).is_empty());
    }

    #[test]
    fn reconcile_is_idempotent() {
        let current = [QuickAction::Archive, QuickAction::Delete];
        let toggles = [
            (QuickAction::Archive, true),
            (QuickAction::Delete, false),
            (QuickAction::MarkUnread, true),
        ];
        let once = reconcile_quick_action_items(&current, &toggles);
        let twice = reconcile_quick_action_items(&once, &toggles);
        assert_eq!(once, twice);
    }

    #[test]
    fn search_history_removes_and_clears_per_account() {
        let account = AccountId::new();
        let other = AccountId::new();
        let mut history = SearchHistory::default();
        history.add(account, "invoice".to_owned(), None);
        history.add(account, "receipt".to_owned(), None);
        history.add(other, "invoice".to_owned(), None);

        history.remove(&account, "invoice");
        let left: Vec<String> = history.get(&account).into_iter().map(|e| e.query).collect();
        assert_eq!(left, ["receipt"]);
        // Another account keeps its own copy (E11).
        assert_eq!(history.get(&other).len(), 1);

        history.clear(&account);
        assert!(history.get(&account).is_empty());
        assert_eq!(history.get(&other).len(), 1);
    }

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().expect("defaults are valid");
        assert!(Config::default().ui.html_enabled);
        assert!(!Config::default().ui.remote_content_enabled);
    }

    #[test]
    fn config_round_trips_through_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        let mut config = Config::default();
        config.ui.theme = Theme::Dark;
        config.ui.page_size = 250;
        config.sync.poll_interval_secs = 120;
        config.save(&paths).expect("save");

        let loaded = Config::load(&paths).expect("load");
        assert_eq!(loaded, config);
    }

    #[test]
    fn missing_config_uses_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        assert_eq!(Config::load(&paths).expect("load"), Config::default());
    }

    #[test]
    fn density_round_trips_through_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        let mut config = Config::default();
        config.ui.message_list_density = MessageListDensity::Compact;
        config.save(&paths).expect("save");
        let loaded = Config::load(&paths).expect("load");
        assert_eq!(loaded.ui.message_list_density, MessageListDensity::Compact);
    }

    #[test]
    fn configs_written_before_density_keep_the_default() {
        let config: Config = toml::from_str("[ui]\npage_size = 50\n").expect("config");
        assert_eq!(
            config.ui.message_list_density,
            MessageListDensity::Comfortable
        );
        assert_eq!(config.ui.page_size, 50);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        paths.ensure().expect("ensure");
        std::fs::write(paths.config_file(), "[ui]\nunknown_key = true\n").expect("write");
        assert!(Config::load(&paths).is_err());
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        Config::default().save(&paths).expect("save");
        assert!(paths.config_file().exists());
        assert!(!paths.config_file().with_extension("toml.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        Config::default().save(&paths).expect("save");
        let mode = std::fs::metadata(paths.config_file())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(&paths.config_dir)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn validation_rejects_out_of_range_values() {
        let mut config = Config::default();
        config.ui.page_size = 0;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.sync.poll_interval_secs = 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn load_recovering_falls_back_to_defaults_and_renames_the_broken_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        paths.ensure().expect("ensure");
        std::fs::write(paths.config_file(), "this is [ not valid toml").expect("write");

        let (config, warning) = Config::load_recovering(&paths).expect("recovery");
        assert_eq!(config, Config::default());
        let warning = warning.expect("a warning is reported");
        assert!(warning.contains("Defaults were restored"));
        assert!(paths.config_file().with_extension("toml.broken").exists());
        assert!(!paths.config_file().exists());
    }

    #[test]
    fn load_recovering_rejects_out_of_range_values_the_same_way() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        paths.ensure().expect("ensure");
        std::fs::write(paths.config_file(), "[ui]\npage_size = 0\n").expect("write");

        let (config, warning) = Config::load_recovering(&paths).expect("recovery");
        assert_eq!(config, Config::default());
        assert!(warning.is_some());
        assert!(paths.config_file().with_extension("toml.broken").exists());
    }

    #[test]
    fn load_recovering_passes_a_good_file_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        paths.ensure().expect("ensure");
        let mut config = Config::default();
        config.sync.poll_interval_secs = 120;
        config.save(&paths).expect("save");

        let (loaded, warning) = Config::load_recovering(&paths).expect("load");
        assert_eq!(loaded, config);
        assert!(warning.is_none());
        assert!(paths.config_file().exists());
    }

    #[test]
    fn ui_state_survives_a_config_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        paths.ensure().expect("ensure");
        let mut config = Config::default();
        config
            .ui
            .last_folders
            .insert(AccountId::new(), FolderId(42));
        config.ui.window_width = 1440;
        config.ui.window_height = 900;
        config.ui.paned_position = 520;
        config.save(&paths).expect("save");

        let loaded = Config::load(&paths).expect("load");
        assert_eq!(loaded, config);
        assert_eq!(loaded.ui.window_width, 1440);
        assert_eq!(loaded.ui.paned_position, 520);
        assert!(
            loaded
                .ui
                .last_folders
                .values()
                .any(|id| *id == FolderId(42))
        );
    }

    #[test]
    fn configs_without_ui_state_still_load_with_defaults() {
        // Older config.toml files predate the C4/C5 fields. The struct-level
        // `#[serde(default)]` must fill them in rather than rejecting them.
        let toml = "[ui]\npage_size = 50\n";
        let config: Config = toml::from_str(toml).expect("parse");
        assert_eq!(config.ui.page_size, 50);
        assert!(config.ui.last_folders.is_empty());
        assert_eq!(config.ui.window_width, 1180);
        assert_eq!(config.ui.window_height, 760);
        assert_eq!(config.ui.paned_position, 380);
    }
}
