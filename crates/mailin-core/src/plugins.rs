//! Plugin system: Lua-based extensibility for Mailin.

use crate::error::{Error, Result};
use crate::model::Plugin;
use crate::store::Store;
use mlua::{ChunkMode, Lua, Table, Value, VmState};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;

/// Wall-clock budget for a single plugin hook invocation.
const PLUGIN_HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// CPU budget per Lua invocation: the interrupt callback errors out once more
/// than this many VM instructions have executed, which terminates runaway
/// loops (`while true do end`) instead of pinning the Lua mutex forever.
const LUA_INSTRUCTION_BUDGET: u64 = 10_000_000;

/// Timeout for a single plugin HTTP request.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Shared instruction counter checked by the Lua interrupt callback.
#[derive(Default)]
struct InstructionBudget {
    counter: AtomicU64,
}

impl InstructionBudget {
    /// Reset the counter for a new Lua invocation.
    fn reset(&self) {
        self.counter.store(0, Ordering::Relaxed);
    }

    /// Install the interrupt callback on the Lua state.
    fn install(self: &Arc<Self>, lua: &Lua) -> Result<()> {
        let budget = Arc::clone(self);
        lua.set_interrupt(move |_| {
            let used = budget.counter.fetch_add(1, Ordering::Relaxed);
            if used >= LUA_INSTRUCTION_BUDGET {
                Err(mlua::Error::RuntimeError(format!(
                    "plugin exceeded its CPU budget ({LUA_INSTRUCTION_BUDGET} VM instructions)"
                )))
            } else {
                Ok(VmState::Continue)
            }
        });
        Ok(())
    }
}

/// Run a blocking operation without pinning the tokio executor.
///
/// Inside a multi-threaded runtime this uses `block_in_place`; on a
/// current-thread runtime (where `block_in_place` panics) or on a plain
/// thread the work hops to a short-lived OS thread so `reqwest::blocking`
/// does not panic inside a runtime.
fn run_blocking<T, F>(work: F) -> T
where
    T: Send,
    F: FnOnce() -> T + Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(work),
            _ => std::thread::scope(|scope| {
                scope
                    .spawn(work)
                    .join()
                    .expect("blocking operation thread panicked")
            }),
        },
        Err(_) => work(),
    }
}

/// A timeout-guarded shared `reqwest::blocking` client for plugin HTTP calls.
fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// Parse the http entries of a plugin's `permissions` list into allowed URL
/// prefixes. Entries that do not start with `http:`/`https:` are ignored; a
/// trailing `*` wildcard is stripped so the remainder can be prefix-matched.
fn parse_allowed_hosts(permissions_json: &str) -> HashSet<String> {
    serde_json::from_str::<Vec<serde_json::Value>>(permissions_json)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .filter(|s| s.starts_with("http:") || s.starts_with("https:"))
        .map(|s| s.trim_end_matches('*').to_owned())
        .collect()
}

/// Whether a URL is covered by at least one allowed prefix.
fn host_permitted(url: &str, allowed: &HashSet<String>) -> bool {
    !allowed.is_empty() && allowed.iter().any(|prefix| url.starts_with(prefix.as_str()))
}

/// The error returned to Lua when a URL is not covered by the plugin's
/// manifest permissions.
fn denied_url(url: &str) -> mlua::Error {
    mlua::Error::RuntimeError(format!(
        "permission denied: {url} is not covered by the plugin's manifest permissions"
    ))
}

/// Plugin system managing all loaded plugins and their Lua states.
pub struct PluginSystem {
    lua: Arc<Mutex<Lua>>,
    store: Arc<Store>,
    plugins: Arc<RwLock<HashMap<String, Plugin>>>,
    plugin_dir: Arc<PathBuf>,
    /// Instruction budget shared with the Lua interrupt callback.
    budget: Arc<InstructionBudget>,
}

impl std::fmt::Debug for PluginSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Lua` does not implement `Debug`, so only surface the loadable fields.
        f.debug_struct("PluginSystem")
            .field("plugin_dir", &self.plugin_dir)
            .finish_non_exhaustive()
    }
}

impl PluginSystem {
    /// Create a new plugin system.
    pub fn new(store: Arc<Store>, data_dir: impl AsRef<Path>) -> Result<Self> {
        let budget = Arc::new(InstructionBudget::default());
        let lua = Arc::new(Mutex::new(Self::create_lua_state(&budget)?));
        let data_dir = Arc::new(data_dir.as_ref().to_path_buf());
        let plugin_dir = Arc::new(data_dir.join("plugins"));

        Ok(Self {
            lua,
            store,
            plugins: Arc::new(RwLock::new(HashMap::new())),
            plugin_dir,
            budget,
        })
    }

    /// Create a sandboxed Lua state with the Mailin API.
    fn create_lua_state(budget: &Arc<InstructionBudget>) -> Result<Lua> {
        let lua = Lua::new();
        budget.install(&lua)?;

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
            lua.create_function(|_, _key: String| Ok(Value::Nil))?,
        )?;

        // mailin.accounts.list()
        api.set(
            "accounts_list",
            lua.create_function(|lua, ()| lua.create_table())?,
        )?;

        // mailin.messages.query(filter)
        api.set(
            "messages_query",
            lua.create_function(|lua, _filter: Table| lua.create_table())?,
        )?;

        // mailin.rules.add(action)
        api.set(
            "rules_add",
            lua.create_function(|_, _action: Table| Ok(Value::Nil))?,
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
            lua.create_function(|_, _key: String| Ok(Value::Nil))?,
        )?;
        storage.set(
            "set",
            lua.create_function(|_, (_key, _value): (String, Value)| Ok(()))?,
        )?;
        api.set("storage", storage)?;

        // mailin.events.on(event_name, handler)
        let events = lua.create_table()?;
        events.set(
            "on",
            lua.create_function(|_, (_event_name, _handler): (String, Value)| Ok(()))?,
        )?;
        api.set("events", events)?;

        // mailin.http.get(url) / post(url, body)
        //
        // The calls run through `run_blocking` so `reqwest::blocking` never
        // executes (and panics) inside the tokio runtime, and through a
        // timeout-guarded client so a hung server cannot pin the Lua mutex.
        let http = lua.create_table()?;
        http.set(
            "get",
            lua.create_function(|lua, url: String| {
                let text = run_blocking(move || {
                    http_client()
                        .get(&url)
                        .send()
                        .and_then(|resp| resp.text())
                        .map_err(|e| mlua::Error::RuntimeError(e.to_string()))
                })?;
                lua.create_string(&text)
            })?,
        )?;
        http.set(
            "post",
            lua.create_function(|lua, (url, body): (String, String)| {
                let text = run_blocking(move || {
                    http_client()
                        .post(&url)
                        .body(body)
                        .send()
                        .and_then(|resp| resp.text())
                        .map_err(|e| mlua::Error::RuntimeError(e.to_string()))
                })?;
                lua.create_string(&text)
            })?,
        )?;
        api.set("http", http)?;

        lua.globals().set("mailin", api)?;

        // Set sandbox limits
        lua.set_memory_limit(10 * 1024 * 1024)?; // 10 MB
        // CPU budget: the interrupt callback installed from the shared
        // `InstructionBudget` terminates runaway loops after
        // `LUA_INSTRUCTION_BUDGET` VM instructions.

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
                        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string())
                        else {
                            continue;
                        };
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
            return Err(Error::Config(format!(
                "Plugin source not found: {}",
                plugin.source_path
            )));
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

        let permissions = manifest
            .get("permissions")
            .and_then(|v| v.as_array())
            .map(|arr| serde_json::to_string(arr).unwrap_or_else(|_| "[]".to_string()))
            .unwrap_or_else(|| "[]".to_string());

        // Enforce the manifest's HTTP permissions before the plugin script
        // runs, so a plugin cannot use the network while loading.
        let allowed_hosts = parse_allowed_hosts(&permissions);
        self.restrict_http_for_plugin(&allowed_hosts)?;

        let script = std::fs::read_to_string(&main_path)?;

        // Execute the plugin in the sandboxed Lua state
        {
            let lua = self.lua.lock().map_err(|_| Error::LockPoisoned)?;
            self.budget.reset();
            lua.load(&script)
                .set_mode(ChunkMode::Text)
                .exec()
                .map_err(|e| Error::Plugin(e.to_string()))?;
        }

        let name = manifest
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                plugin_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "plugin".to_string())
            });

        let version = manifest
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0")
            .to_string();

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

    /// Re-register `mailin.http.get`/`post` with a whitelist derived from the
    /// plugin's manifest permissions.
    ///
    /// All plugins share one Lua state, so these restrictions are global:
    /// the most recently loaded plugin's permission set applies. A plugin
    /// that declares no `http:`/`https:` permission denies all HTTP access.
    fn restrict_http_for_plugin(&self, allowed: &HashSet<String>) -> Result<()> {
        let allowed = allowed.clone();
        let lua = self.lua.lock().map_err(|_| Error::LockPoisoned)?;
        let api: Table = lua
            .globals()
            .get("mailin")
            .map_err(|e| Error::Plugin(e.to_string()))?;
        let http: Table = api
            .get("http")
            .map_err(|e| Error::Plugin(e.to_string()))?;

        let permitted = allowed.clone();
        http.set(
            "get",
            lua.create_function(move |lua, url: String| {
                if !host_permitted(&url, &permitted) {
                    return Err(denied_url(&url));
                }
                let text = run_blocking(move || {
                    http_client()
                        .get(&url)
                        .send()
                        .and_then(|resp| resp.text())
                        .map_err(|e| mlua::Error::RuntimeError(e.to_string()))
                })?;
                lua.create_string(&text)
            })?,
        )?;
        http.set(
            "post",
            lua.create_function(move |lua, (url, body): (String, String)| {
                if !host_permitted(&url, &allowed) {
                    return Err(denied_url(&url));
                }
                let text = run_blocking(move || {
                    http_client()
                        .post(&url)
                        .body(body)
                        .send()
                        .and_then(|resp| resp.text())
                        .map_err(|e| mlua::Error::RuntimeError(e.to_string()))
                })?;
                lua.create_string(&text)
            })?,
        )?;
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
    ///
    /// The Lua call runs on a blocking thread with a wall-clock timeout so a
    /// misbehaving plugin cannot stall the async runtime; the instruction
    /// budget terminates runaway loops inside the VM.
    pub async fn run_hook(&self, hook: &str, args: Value) -> Result<Value> {
        let lua = Arc::clone(&self.lua);
        let budget = Arc::clone(&self.budget);
        let hook_name = hook.to_owned();
        let hook = hook.to_owned();
        let task = tokio::task::spawn_blocking(move || {
            let lua = lua.lock().map_err(|_| Error::LockPoisoned)?;
            budget.reset();
            let globals = lua.globals();
            let hook_fn: mlua::Function = globals
                .get(hook_name.as_str())
                .map_err(|e| Error::Plugin(e.to_string()))?;
            hook_fn
                .call(args)
                .map_err(|e| Error::Plugin(format!("Hook {hook_name} failed: {e}")))
        });
        match tokio::time::timeout(PLUGIN_HOOK_TIMEOUT, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => Err(Error::Plugin(format!("hook task panicked: {e}"))),
            Err(_) => {
                tracing::warn!(
                    hook = %hook,
                    timeout = ?PLUGIN_HOOK_TIMEOUT,
                    "plugin hook timed out"
                );
                Err(Error::Plugin(format!("plugin hook '{hook}' timed out")))
            }
        }
    }

    /// Call a function exported by a specific plugin.
    pub async fn call_plugin(&self, plugin_name: &str, func: &str, args: Value) -> Result<Value> {
        let plugins = self.plugins.read().await;
        if !plugins.contains_key(plugin_name) {
            return Err(Error::Config(format!("Plugin {} not loaded", plugin_name)));
        }
        drop(plugins);

        let lua = Arc::clone(&self.lua);
        let budget = Arc::clone(&self.budget);
        let plugin_name = plugin_name.to_owned();
        let plugin_name_outer = plugin_name.clone();
        let func = func.to_owned();
        let func_outer = func.clone();
        let task = tokio::task::spawn_blocking(move || {
            let lua = lua.lock().map_err(|_| Error::LockPoisoned)?;
            budget.reset();
            let globals = lua.globals();
            let plugin_table: Table = globals
                .get(plugin_name.as_str())
                .map_err(|e| Error::Plugin(e.to_string()))?;
            let func_value: mlua::Function = plugin_table
                .get(func.as_str())
                .map_err(|e| Error::Plugin(e.to_string()))?;
            func_value.call(args).map_err(|e| {
                Error::Plugin(format!(
                    "Plugin '{}' function '{}' failed: {}",
                    plugin_name, func, e
                ))
            })
        });
        match tokio::time::timeout(PLUGIN_HOOK_TIMEOUT, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => Err(Error::Plugin(format!("plugin task panicked: {e}"))),
            Err(_) => {
                tracing::warn!(
                    plugin = %plugin_name_outer,
                    function = %func_outer,
                    timeout = ?PLUGIN_HOOK_TIMEOUT,
                    "plugin call timed out"
                );
                Err(Error::Plugin(format!(
                    "plugin '{plugin_name_outer}' function '{func_outer}' timed out"
                )))
            }
        }
    }
}

/// Global plugin system instance.
static PLUGIN_SYSTEM: tokio::sync::OnceCell<Arc<PluginSystem>> = tokio::sync::OnceCell::const_new();

/// Initialize the global plugin system.
pub async fn init_plugin_system(
    store: &Store,
    data_dir: impl AsRef<Path>,
) -> Result<Arc<PluginSystem>> {
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

    #[test]
    fn allowed_hosts_are_parsed_from_permissions() {
        let permissions = r#"["http://localhost:8080/api", "network", "https://example.com/*"]"#;
        let allowed = parse_allowed_hosts(permissions);
        assert_eq!(allowed.len(), 2);
        assert!(allowed.contains("http://localhost:8080/api"));
        // The wildcard is stripped so the entry can be prefix-matched.
        assert!(allowed.contains("https://example.com/"));
    }

    #[test]
    fn host_permitted_requires_a_matching_prefix() {
        let allowed: HashSet<String> = ["https://example.com/".to_owned()].into();
        assert!(host_permitted("https://example.com/data", &allowed));
        assert!(!host_permitted("https://evil.com/data", &allowed));
        // An empty allowlist denies everything.
        assert!(!host_permitted("https://example.com", &HashSet::new()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_hook_is_cancelled_on_timeout() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("test.db")).unwrap());
        let system = PluginSystem::new(store, dir.path()).unwrap();
        {
            let lua = system.lua.lock().unwrap();
            lua.load(r#"function forever() while true do end end"#)
                .exec()
                .unwrap();
        }
        let started = std::time::Instant::now();
        let result = system.run_hook("forever", Value::Nil).await;
        // The instruction budget terminates the loop well under the 10 s
        // wall-clock timeout, but the call must return instead of hanging.
        assert!(result.is_err());
        assert!(started.elapsed() < PLUGIN_HOOK_TIMEOUT);
    }
}
