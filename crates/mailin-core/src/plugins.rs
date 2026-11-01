//! Plugin system: Lua-based extensibility for Mailin.

use crate::error::{Error, Result};
use crate::model::Plugin;
use crate::store::Store;
use mlua::{ChunkMode, Lua, Table, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

/// Plugin system managing all loaded plugins and their Lua states.
pub struct PluginSystem {
    lua: Arc<Mutex<Lua>>,
    store: Arc<Store>,
    plugins: Arc<RwLock<HashMap<String, Plugin>>>,
    data_dir: Arc<PathBuf>,
    plugin_dir: Arc<PathBuf>,
}

impl PluginSystem {
    /// Create a new plugin system.
    pub fn new(store: Arc<Store>, data_dir: impl AsRef<Path>) -> Result<Self> {
        let lua = Arc::new(Mutex::new(Self::create_lua_state()?));
        let data_dir = Arc::new(data_dir.as_ref().to_path_buf());
        let plugin_dir = Arc::new(data_dir.join("plugins"));

        Ok(Self {
            lua,
            store,
            plugins: Arc::new(RwLock::new(HashMap::new())),
            data_dir,
            plugin_dir,
        })
    }

    /// Create a sandboxed Lua state with the Mailin API.
    fn create_lua_state() -> Result<Lua> {
        let lua = Lua::new();

        // Create the mailin global API
        let api = lua.create_table()?;

        // mailin.version
        api.set("version", env!("CARGO_PKG_VERSION"))?;

        // mailin.log(level, message)
        api.set(
            "log",
            lua.create_function(|_, (level, message): (String, String)| {
                let level = level.to_lowercase();
                match level.as_str() {
                    "error" => tracing::error!("[plugin] {}", message),
                    "warn" => tracing::warn!("[plugin] {}", message),
                    "info" => tracing::info!("[plugin] {}", message),
                    "debug" => tracing::debug!("[plugin] {}", message),
                    _ => tracing::info!("[plugin] {}", message),
                }
                Ok(())
            })?,
        )?;

        // mailin.config.get(key)
        api.set(
            "config_get",
            lua.create_function(|_, key: String| {
                Ok(Value::Nil)
            })?,
        )?;

        // mailin.accounts.list()
        api.set(
            "accounts_list",
            lua.create_function(|lua, ()| {
                Ok(lua.create_table()?)
            })?,
        )?;

        // mailin.messages.query(filter)
        api.set(
            "messages_query",
            lua.create_function(|lua, _filter: Table| {
                Ok(lua.create_table()?)
            })?,
        )?;

        // mailin.rules.add(action)
        api.set(
            "rules_add",
            lua.create_function(|_, _action: Table| {
                Ok(Value::Nil)
            })?,
        )?;

        // mailin.ui.notify(title, body)
        api.set(
            "ui_notify",
            lua.create_function(|_, (title, body): (String, String)| {
                tracing::info!("[plugin notify] {}: {}", title, body);
                Ok(())
            })?,
        )?;

        // mailin.ui.show_message(message)
        api.set(
            "ui_show_message",
            lua.create_function(|_, message: String| {
                tracing::info!("[plugin message] {}", message);
                Ok(())
            })?,
        )?;

        // mailin.storage.get(key) / mailin.storage.set(key, value)
        let storage = lua.create_table()?;
        storage.set(
            "get",
            lua.create_function(|_, _key: String| {
                Ok(Value::Nil)
            })?,
        )?;
        storage.set(
            "set",
            lua.create_function(|_, (_key, _value): (String, Value)| {
                Ok(())
            })?,
        )?;
        api.set("storage", storage)?;

        // mailin.events.on(event_name, handler)
        let events = lua.create_table()?;
        events.set(
            "on",
            lua.create_function(|_, (_event_name, _handler): (String, Value)| {
                Ok(())
            })?,
        )?;
        api.set("events", events)?;

        // mailin.http.get(url) / post(url, body)
        let http = lua.create_table()?;
        http.set(
            "get",
            lua.create_function(|lua, url: String| {
                let resp = reqwest::blocking::get(&url)
                    .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                let text = resp.text()
                    .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                Ok(lua.create_string(&text)?)
            })?,
        )?;
        http.set(
            "post",
            lua.create_function(|lua, (url, body): (String, String)| {
                let client = reqwest::blocking::Client::new();
                let resp = client.post(&url).body(body).send()
                    .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                let text = resp.text()
                    .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                Ok(lua.create_string(&text)?)
            })?,
        )?;
        api.set("http", http)?;

        lua.globals().set("mailin", api)?;

        // Set sandbox limits
        lua.set_memory_limit(10 * 1024 * 1024)?; // 10 MB
        // Note: cpu_limit requires feature "send" in mlua, skip for now

        Ok(lua)
    }

    /// Load all enabled plugins from the database and plugins directory.
    pub async fn load_all(&self) -> Result<()> {
        // Load from database
        let plugins = self.store.list_plugins()?;
        let mut loaded = self.plugins.write().await;
        for plugin in plugins {
            if plugin.enabled {
                if let Err(e) = self.load_plugin_lua(&plugin).await {
                    tracing::warn!(plugin = %plugin.name, error = %e, "Failed to load plugin");
                } else {
                    loaded.insert(plugin.name.clone(), plugin);
                }
            }
        }

        // Also scan plugins directory for new plugins
        if self.plugin_dir.exists() {
            let entries = std::fs::read_dir(&*self.plugin_dir)?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let manifest_path = path.join("manifest.json");
                    if manifest_path.exists() {
                        let name = path.file_name().unwrap().to_string_lossy().to_string();
                        if !loaded.contains_key(&name) {
                            if let Err(e) = self.load_plugin_from_dir(&path).await {
                                tracing::warn!(plugin_dir = %path.display(), error = %e, "Failed to load plugin from directory");
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Load a plugin from the database record.
    async fn load_plugin_lua(&self, plugin: &Plugin) -> Result<()> {
        let plugin_path = PathBuf::from(&plugin.source_path);
        if !plugin_path.exists() {
            return Err(Error::Config(format!("Plugin source not found: {}", plugin.source_path)));
        }
        self.load_plugin_from_dir(&plugin_path).await
    }

    /// Load a plugin from a directory.
    async fn load_plugin_from_dir(&self, plugin_dir: &Path) -> Result<()> {
        let manifest_path = plugin_dir.join("manifest.json");
        let manifest_content = std::fs::read_to_string(&manifest_path)?;
        let manifest: serde_json::Value = serde_json::from_str(&manifest_content)?;

        let main_path = plugin_dir.join("main.lua");
        if !main_path.exists() {
            return Err(Error::Config("Plugin missing main.lua".into()));
        }

        let script = std::fs::read_to_string(&main_path)?;

        // Execute the plugin in the sandboxed Lua state
        {
            let lua = self.lua.lock().unwrap();
            lua.load(&script).set_mode(ChunkMode::Text).exec()
                .map_err(|e| Error::Plugin(e.to_string()))?;
        }

        let name = manifest.get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| plugin_dir.file_name().unwrap().to_string_lossy().to_string());

        let version = manifest.get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0")
            .to_string();

        let permissions = manifest.get("permissions")
            .and_then(|v| v.as_array())
            .map(|arr| serde_json::to_string(arr).unwrap_or_else(|_| "[]".to_string()))
            .unwrap_or_else(|| "[]".to_string());

        let plugin = Plugin {
            name: name.clone(),
            version,
            source_path: plugin_dir.to_string_lossy().to_string(),
            enabled: true,
            manifest_json: manifest_content,
            permissions_json: permissions,
            created_at: crate::now_unix(),
        };

        // Save to database
        self.store.upsert_plugin(
            &plugin.name,
            &plugin.version,
            &plugin.source_path,
            plugin.enabled,
            &plugin.manifest_json,
            &plugin.permissions_json,
        )?;

        // Register in memory
        let mut plugins = self.plugins.write().await;
        plugins.insert(name.clone(), plugin);

        tracing::info!(plugin = %name, "Plugin loaded");
        Ok(())
    }

    /// Enable/disable a plugin.
    pub async fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        let mut plugins = self.plugins.write().await;
        if let Some(plugin) = plugins.get_mut(name) {
            plugin.enabled = enabled;
            self.store.upsert_plugin(
                &plugin.name,
                &plugin.version,
                &plugin.source_path,
                enabled,
                &plugin.manifest_json,
                &plugin.permissions_json,
            )?;
        }
        Ok(())
    }

    /// Unload a plugin.
    pub async fn unload(&self, name: &str) -> Result<()> {
        let mut plugins = self.plugins.write().await;
        if plugins.remove(name).is_some() {
            self.store.delete_plugin(name)?;
        }
        Ok(())
    }

    /// List all loaded plugins.
    pub async fn list_plugins(&self) -> Vec<Plugin> {
        let plugins = self.plugins.read().await;
        plugins.values().cloned().collect()
    }

    /// Get a plugin by name.
    pub async fn get_plugin(&self, name: &str) -> Option<Plugin> {
        let plugins = self.plugins.read().await;
        plugins.get(name).cloned()
    }

    /// Execute a plugin hook (e.g., "on_message_received", "on_compose").
    pub async fn run_hook(&self, hook: &str, args: Value) -> Result<Value> {
        let lua = self.lua.lock().unwrap();
        let globals = lua.globals();
        let hook_fn: mlua::Function = globals.get(hook)
            .map_err(|e| Error::Plugin(e.to_string()))?;
        let result = hook_fn.call(args)
            .map_err(|e| Error::Plugin(format!("Hook {} failed: {}", hook, e)))?;
        Ok(result)
    }

    /// Call a function exported by a specific plugin.
    pub async fn call_plugin(&self, plugin_name: &str, func: &str, args: Value) -> Result<Value> {
        let plugins = self.plugins.read().await;
        if !plugins.contains_key(plugin_name) {
            return Err(Error::Config(format!("Plugin {} not loaded", plugin_name)));
        }
        drop(plugins);

        let lua = self.lua.lock().unwrap();
        let globals = lua.globals();
        let plugin_table: Table = globals.get(plugin_name)
            .map_err(|e| Error::Plugin(e.to_string()))?;
        let func_name = func;
        let func: mlua::Function = plugin_table.get(func_name)
            .map_err(|e| Error::Plugin(e.to_string()))?;
        let result = func.call(args)
            .map_err(|e| Error::Plugin(format!("Plugin '{}' function '{}' failed: {}", plugin_name, func_name, e)))?;
        Ok(result)
    }
}

/// Global plugin system instance.
static PLUGIN_SYSTEM: tokio::sync::OnceCell<Arc<PluginSystem>> = tokio::sync::OnceCell::const_new();

/// Initialize the global plugin system.
pub async fn init_plugin_system(store: &Store, data_dir: impl AsRef<Path>) -> Result<Arc<PluginSystem>> {
    let store = Arc::new(store.clone());
    let system = Arc::new(PluginSystem::new(store, data_dir)?);
    system.load_all().await?;
    PLUGIN_SYSTEM.set(system.clone()).ok();
    Ok(system)
}

/// Get the global plugin system.
pub fn plugin_system() -> Option<Arc<PluginSystem>> {
    PLUGIN_SYSTEM.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_plugin_system_creation() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("test.db")).unwrap());
        let system = PluginSystem::new(store, dir.path()).unwrap();
        let plugins = system.list_plugins().await;
        assert!(plugins.is_empty());
    }

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