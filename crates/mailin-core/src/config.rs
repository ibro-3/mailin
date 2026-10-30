use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

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
    /// Enable quick actions (swipe on GTK, hover on WinUI) on message rows.
    pub quick_actions: bool,
    /// Which actions are available in the quick-actions overlay.
    pub quick_action_items: Vec<QuickAction>,
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
            quick_actions: true,
            quick_action_items: vec![
                QuickAction::Archive,
                QuickAction::Delete,
                QuickAction::Flag,
                QuickAction::MarkRead,
            ],
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
    pub poll_interval_secs: u64,
    pub max_parallel_accounts: u8,
    pub body_sync: BodySyncPolicy,
    /// Keep message headers for this many days; `0` disables pruning.
    pub retention_days: u32,
}

impl Default for SyncPreferences {
    fn default() -> Self {
        Self {
            poll_interval_secs: 60,
            max_parallel_accounts: 3,
            body_sync: BodySyncPolicy::OnOpen,
            retention_days: 0,
        }
    }
}

/// Application preferences. Accounts and credentials are *not* stored here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub ui: UiPreferences,
    pub sync: SyncPreferences,
}

impl Config {
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

    /// Write `config.toml` atomically: a sibling temporary file replaces the
    /// old one only once it has been fully written.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure()?;
        let file = paths.config_file();
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("could not serialize config: {e}")))?;
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes())?;
        restrict_to_owner(&tmp)?;
        std::fs::rename(&tmp, &file)?;
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
        if !(1..=16).contains(&self.sync.max_parallel_accounts) {
            return Err(Error::Config(
                "sync.max_parallel_accounts must be between 1 and 16".into(),
            ));
        }
        Ok(())
    }
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

        let mut config = Config::default();
        config.sync.max_parallel_accounts = 0;
        assert!(config.validate().is_err());
    }
}
