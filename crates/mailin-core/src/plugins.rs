//! Plugin system: Lua-based extensibility for Mailin.
//!
//! Minimal stub implementation - infrastructure ready, full Lua integration
//! to be implemented once mlua API stabilizes.

use crate::error::{Error, Result};
use crate::model::Plugin;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Plugin system managing all loaded plugins.
pub struct PluginSystem {
    plugins: Arc<RwLock<HashMap<String, Plugin>>>,
    data_dir: Arc<std::path::PathBuf>,
}

impl PluginSystem {
    /// Create a new plugin system.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            plugins: Arc::new(RwLock::new(HashMap::new())),
            data_dir: Arc::new(data_dir.as_ref().to_path_buf()),
        }
    }

    /// Load all enabled plugins from the plugins directory.
    pub async fn load_all(&self) -> Result<()> {
        let plugins_dir = self.data_dir.join("plugins");
        if !plugins_dir.exists() {
            return Ok(());
        }

        let entries = std::fs::read_dir(&plugins_dir)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Err(e) = self.load_plugin(&path).await {
                    tracing::warn!(plugin_dir = %path.display(), error = %e, "Failed to load plugin");
                }
            }
        }
        Ok(())
    }

    /// Load a single plugin from a directory (stub).
    async fn load_plugin(&self, plugin_dir: &Path) -> Result<()> {
        let manifest_path = plugin_dir.join("manifest.json");
        if !manifest_path.exists() {
            return Err(Error::Config("Plugin missing manifest.json".into()));
        }

        let manifest_content = std::fs::read_to_string(&manifest_path)?;
        let _manifest: serde_json::Value = serde_json::from_str(&manifest_content)?;

        let _plugin_path = plugin_dir.join("main.lua");
        if !_plugin_path.exists() {
            return Err(Error::Config("Plugin missing main.lua".into()));
        }

        // For now, just register the plugin as a stub
        let plugin = Plugin {
            name: "stub".into(),
            version: "0.1".into(),
            source_path: plugin_dir.to_string_lossy().to_string(),
            enabled: true,
            manifest_json: "{}".into(),
            permissions_json: "[]".into(),
            created_at: crate::now_unix(),
        };

        let mut plugins = self.plugins.write().await;
        plugins.insert(plugin.name.clone(), plugin);

        tracing::info!(name = "stub", "Plugin loaded (stub)");
        Ok(())
    }

    /// Enable/disable a plugin.
    pub async fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        let mut plugins = self.plugins.write().await;
        if let Some(plugin) = plugins.get_mut(name) {
            plugin.enabled = enabled;
        }
        Ok(())
    }

    /// Unload a plugin.
    pub async fn unload(&self, name: &str) -> Result<()> {
        let mut plugins = self.plugins.write().await;
        plugins.remove(name);
        Ok(())
    }

    /// List all loaded plugins.
    pub async fn list_plugins(&self) -> Vec<Plugin> {
        let plugins = self.plugins.read().await;
        plugins.values().cloned().collect()
    }
}

/// Global plugin system instance.
static PLUGIN_SYSTEM: tokio::sync::OnceCell<Arc<PluginSystem>> = tokio::sync::OnceCell::const_new();

/// Initialize the global plugin system.
pub async fn init_plugin_system(data_dir: impl AsRef<std::path::Path>) -> Result<Arc<PluginSystem>> {
    let system = Arc::new(PluginSystem::new(data_dir));
    system.load_all().await?;
    PLUGIN_SYSTEM.set(system.clone()).ok();
    Ok(system.clone())
}

/// Get the global plugin system.
pub fn plugin_system() -> Option<Arc<PluginSystem>> {
    PLUGIN_SYSTEM.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plugin_creation() {
        let plugin = Plugin {
            name: "test".into(),
            version: "1.0".into(),
            source_path: "/tmp/test".into(),
            enabled: true,
            manifest_json: "{}".into(),
            permissions_json: "[]".into(),
            created_at: 0,
        };
        assert_eq!(plugin.name, "test");
    }
}