//! Lightweight process plugins. No runtime ABI, Agent hooks or extra model calls.
//! An enabled plugin exposes tools/commands through a newline-delimited JSON
//! process and contributes static prompt text at runtime composition.
mod approval;
mod context;
#[cfg(test)]
use approval::PluginScanLimits;
use approval::{PluginCodeScan, PLUGIN_SCAN_LIMITS};
mod manifest;
#[cfg(test)]
use manifest::{empty_parameters, scaffold_in, PLUGIN_API_VERSION_V1};
use manifest::{parse_alt_shortcut, scaffold, valid_id};
pub use manifest::{
    ModelOverride, PluginCommandSpec, PluginContextSpec, PluginDesktopUiSpec, PluginLayoutSpec,
    PluginManifest, PluginModelSpec, PluginToolSpec, PluginUiShortcut, PluginUiSpec,
};
mod model;
mod stream;
pub mod ui;
pub use ui::{
    CustomTuiChannel, CustomTuiClientEvent, CustomTuiFrameUpdate, CustomTuiHost, CustomTuiSpan,
    CustomTuiSpec, PluginLivePanel, PluginUiAction, PluginUiBus, PluginUiEvent, PluginWidget,
    UiScope, WidgetPlacement, MAX_STATUS_CHARS,
};
/// Host memory calls are fail-open and bounded apart from the 120-second
/// tool/hook timeout, so a slow plugin cannot stall the model request.
const PLUGIN_MEMORY_CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound on the verified user turn handed to a plugin that asks for it.
const TRUSTED_USER_CHARS: usize = 16_384;
const MAX_INSTALL_ENTRIES: usize = 20_000;
const MAX_INSTALL_BYTES: u64 = 64 * 1024 * 1024;

use crate::{
    commands::{CommandAction, CommandRegistry, CommandReply},
    context::{ContextConfig, ContextFactory},
    events::{EventSink, RuntimeEvent},
    model::ModelBackend,
    prompt::PromptComposer,
    protocol::ToolCall,
    provider::ModelClient,
    tools::{
        AskTool, HookFuture, Tool, ToolContext, ToolFuture, ToolInterceptor, ToolRegistry,
        ToolResult,
    },
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex, RwLock,
    },
    time::{Duration, SystemTime},
};

const PLUGIN_CALL_TIMEOUT: Duration = Duration::from_secs(120);
const PLUGIN_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// Host calls a plugin may make while one request is in flight.
const MAX_HOST_CALLS_PER_REQUEST: usize = 64;
/// Terminal width a panel is laid out for before the TUI wraps it again.
const PANEL_WIDTH: usize = 76;

/// Panels reach the terminal as one string: a title, a blank line, then the
/// rendered blocks. Structured output stays available through
/// `ui_panel_blocks_async` for frontends that draw widgets.
fn render_panel(title: &str, content: &str) -> String {
    let blocks = crate::panel::parse(content);
    let body = crate::panel::render_text(&blocks, PANEL_WIDTH);
    format!("{title}\n\n{}", body.join("\n"))
}

/// The same panel as structured blocks plus its terminal rendering, so a
/// frontend picks whichever it can draw.
fn panel_json(title: &str, content: &str) -> Value {
    let blocks = crate::panel::parse(content);
    json!({
        "title": title,
        "blocks": crate::panel::to_json(&blocks),
        "text": crate::panel::render_text(&blocks, PANEL_WIDTH).join("\n")
    })
}

/// Optional whole-tool deadline for long-running plugin tools. Zero or unset
/// preserves the documented unlimited behavior; hooks and ordinary calls keep
/// their fixed 120-second timeout.
fn plugin_tool_timeout() -> Option<Duration> {
    optional_seconds_env("KROMA_PLUGIN_TOOL_TIMEOUT_SECS")
}

/// The smaller of the manifest declaration and the administrator limit.
/// `None` keeps the documented unlimited behavior for ordinary plugin tools.
fn effective_tool_timeout(declared: Option<u64>, global: Option<Duration>) -> Option<Duration> {
    let declared = declared.map(Duration::from_secs);
    match (declared, global) {
        (Some(declared), Some(global)) => Some(declared.min(global)),
        (declared, global) => declared.or(global),
    }
}

fn optional_seconds_env(name: &str) -> Option<Duration> {
    let Ok(raw) = env::var(name) else {
        return None;
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(seconds) => Some(Duration::from_secs(seconds)),
        Err(_) => {
            crate::runtime_log!("[plugin] ignoring invalid {name}={raw:?}");
            None
        }
    }
}

/// `KROMA_PLUGINS=off` disables every plugin for this process. A subagent
/// child uses it so it starts with no plugin prompts, processes or state; the
/// subagents plugin can opt a child back in with its own setting.
fn plugins_disabled() -> bool {
    env::var("KROMA_PLUGINS").is_ok_and(|value| plugins_off(&value))
}

fn plugins_off(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "off" | "false" | "none"
    )
}

/// Opt-in minimum-privilege mode for plugin processes. Default behavior is
/// unchanged because documented plugins may read a provider key from the host
/// environment. `KROMA_PLUGIN_ISOLATE_ENV=1` removes credential-shaped values.
fn plugin_env_isolation() -> bool {
    env::var("KROMA_PLUGIN_ISOLATE_ENV")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

pub(crate) fn is_host_secret_env(name: &str) -> bool {
    plugin_env_secret(name)
}

fn plugin_env_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "KROMA_API_KEY" | "PGPASSWORD" | "MYSQL_PWD" | "REDISCLI_AUTH" | "COOKIE"
    ) || upper.starts_with("AWS_")
        || upper.starts_with("AZURE_")
        || upper.starts_with("GCP_")
        || upper.starts_with("GOOGLE_")
        || upper.starts_with("GITHUB_")
        || upper.starts_with("SSH_")
        || upper.ends_with("_KEY")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_PASSWORD")
        || upper.ends_with("_PWD")
        || upper.ends_with("_COOKIE")
        || upper.ends_with("_CREDENTIALS")
}

/// A short OS-held lock for plugins.toml read/modify/write. This lock file is
/// intentionally persistent: deleting it could create two independent locks.
struct PluginStateLock {
    _file: fs::File,
}
impl PluginStateLock {
    fn acquire(path: &Path) -> Result<Self> {
        let parent = path.parent().context("plugin settings has no parent")?;
        fs::create_dir_all(parent)?;
        let file =
            crate::file_lock::acquire(&parent.join("plugins.toml.lock"), Duration::from_secs(5))
                .context("locking plugin settings")?;
        Ok(Self { _file: file })
    }
}

#[derive(Default, Serialize, Deserialize)]
struct PluginState {
    #[serde(default)]
    enabled: BTreeSet<String>,
    #[serde(default)]
    enabled_projects: BTreeSet<String>,
    /// A broken optional tool/prompt plugin is skipped, but an explicitly
    /// selected model or context backend must never silently fall back to a
    /// different runtime when its manifest later becomes invalid.
    #[serde(default)]
    critical_plugins: BTreeSet<String>,
    #[serde(default)]
    settings: BTreeMap<String, BTreeMap<String, toml::Value>>,
    #[serde(default)]
    project_settings: BTreeMap<String, BTreeMap<String, toml::Value>>,
}

impl PluginState {
    fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("plugin state needs a parent directory")?;
        // Plugin settings can contain provider API keys. Create the temporary
        // file with owner-only permissions instead of inheriting the umask.
        let temp = crate::private_file::save(
            parent,
            ".plugins",
            "tmp",
            toml::to_string(self)?.as_bytes(),
        )?;
        if let Err(error) = crate::atomic_file::replace(&temp, path) {
            let _ = fs::remove_file(temp);
            return Err(error);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct PluginCatalog {
    capability_filter: Option<crate::capabilities::Capabilities>,
    directory: PathBuf,
    bundled: Option<PathBuf>,
    state_path: PathBuf,
    workdir: PathBuf,
    plugins: Vec<Plugin>,
    /// One explicit directory loaded for this process only; never persisted.
    transient: Option<PathBuf>,
    pub warnings: Vec<String>,
    /// Enabled plugins with unreadable manifests cannot be silently omitted.
    invalid_enabled: Vec<(String, String)>,
    /// Per-catalog live UI state & broadcast bus (isolated across projects/tests).
    ui_bus: Arc<PluginUiBus>,
}

#[derive(Clone)]
struct Plugin {
    manifest: PluginManifest,
    /// What this process actually runs: the user's choice survived approval.
    enabled: bool,
    /// Persisted user choice, which an approval check may still block.
    requested: bool,
    /// Why a requested plugin is not loaded; shown by the GUI and CLI.
    blocked: Option<String>,
    /// None for user-global plugins; project plugins have a full path key.
    project_key: Option<String>,
    /// Explicit `KROMA_PLUGIN_TRY` plugin: not backed by plugins.toml state.
    transient: bool,
    worker: Arc<PluginWorker>,
    event_worker: Option<Arc<PluginWorker>>,
    ui_worker: Arc<PluginWorker>,
}

impl Plugin {
    /// Whether persisted state still asks for this plugin, independent of
    /// whether its current code passed the approval check.
    fn requests(&self, state: &PluginState) -> bool {
        if self.transient {
            return self.enabled;
        }
        match &self.project_key {
            Some(key) => state.enabled_projects.contains(key),
            None => state.enabled.contains(&self.manifest.id),
        }
    }

    fn has_model_hook(&self) -> bool {
        self.manifest
            .hooks
            .iter()
            .any(|hook| hook == "before_model_request")
    }

    fn has_request_context(&self) -> bool {
        self.manifest.has_capability("provide_context")
            || self.manifest.has_capability("request_context")
    }

    fn has_task_summary(&self) -> bool {
        self.manifest.has_capability("task_finished")
            || self.manifest.has_capability("task_summary")
    }

    /// Hooks and context providers are captured when the runtime builds its
    /// model backend, so a change to one of them is not observable from a tool
    /// and command reload alone.
    fn binds_runtime_backend(&self) -> bool {
        self.manifest.model.is_some()
            || self.manifest.context.is_some()
            || self.has_model_hook()
            || self.has_request_context()
            || self.has_task_summary()
    }

    /// Model hooks and both V2 host calls keep process-local state, so the
    /// catalog must treat them like a model/context backend on reload.
    fn has_stateful_backend(&self) -> bool {
        self.enabled && self.binds_runtime_backend()
    }

    /// The one host sentence that tells a user how to apply a change.
    fn apply_hint(&self) -> &'static str {
        if self.binds_runtime_backend() {
            "Restart Kroma to apply: its model/context hooks are bound when the runtime starts."
        } else {
            "Run /plugins reload while idle, or restart Kroma, to apply."
        }
    }

    fn config_overrides<'a>(
        &self,
        state: &'a PluginState,
    ) -> Option<&'a BTreeMap<String, toml::Value>> {
        match &self.project_key {
            Some(key) => state.project_settings.get(key),
            None => state.settings.get(&self.manifest.id),
        }
    }
}

fn is_secret_setting(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    normalized.split('_').any(|part| {
        matches!(
            part,
            "key"
                | "token"
                | "secret"
                | "password"
                | "credential"
                | "credentials"
                | "authorization"
                | "auth"
        )
    })
}

fn redact_secret_settings(settings: &mut BTreeMap<String, toml::Value>) {
    for (key, value) in settings.iter_mut() {
        if is_secret_setting(key) && secret_is_set(value) {
            *value = toml::Value::String("••••".into());
        }
    }
}

/// Empty strings are an explicit "not configured" value, not a secret. GUI
/// hosts need this distinction to show a useful placeholder and to offer an
/// explicit clear action for a previously stored value.
/// Human-readable TOML type of a setting, used by error and success messages.
fn setting_type_name(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(items) => {
            if items
                .iter()
                .all(|item| matches!(item, toml::Value::String(_)))
            {
                "string array"
            } else {
                "array"
            }
        }
        toml::Value::Table(_) => "table",
    }
}

/// A manifest `[settings]` table is the key namespace of `plugins set`, so a
/// misspelled key fails instead of being stored where the plugin never reads
/// it. A plugin without `[settings]` keeps accepting any identifier.
fn check_setting_key(manifest: &PluginManifest, id: &str, key: &str) -> Result<()> {
    if manifest.settings.is_empty() || manifest.settings.contains_key(key) {
        return Ok(());
    }
    let available = manifest
        .settings
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    bail!("plugin '{id}' has no setting '{key}'; available settings: {available}")
}

/// Terminal `set` text is typed from the manifest default of the same key, so
/// `max_results 6` stores an integer and an unreadable value fails loudly
/// instead of being persisted as a string the plugin never parses.
fn typed_setting_value(key: &str, default: &toml::Value, text: &str) -> Result<toml::Value> {
    let expected = setting_type_name(default);
    match default {
        toml::Value::Boolean(_) if text.trim().eq_ignore_ascii_case("true") => {
            Ok(toml::Value::Boolean(true))
        }
        toml::Value::Boolean(_) if text.trim().eq_ignore_ascii_case("false") => {
            Ok(toml::Value::Boolean(false))
        }
        toml::Value::Boolean(_) => {
            bail!("setting '{key}' expects a boolean (true or false), got '{text}'")
        }
        toml::Value::Integer(_) => text
            .trim()
            .parse::<i64>()
            .map(toml::Value::Integer)
            .with_context(|| format!("setting '{key}' expects an integer, got '{text}'")),
        toml::Value::String(_) => Ok(toml::Value::String(text.to_owned())),
        toml::Value::Array(items)
            if items
                .iter()
                .all(|item| matches!(item, toml::Value::String(_))) =>
        {
            Ok(toml::Value::Array(
                text.split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(|item| toml::Value::String(item.to_owned()))
                    .collect(),
            ))
        }
        _ => bail!(
            "setting '{key}' expects a {expected}, which the terminal cannot type; use a host settings form"
        ),
    }
}

/// Enabling stays one command, but a plugin that replaces the model, context,
/// prompt, model requests or built-in tools must say so before a restart.
fn critical_plugin_warning(plugin: &Plugin, id: &str) -> Option<String> {
    let mut impacts = Vec::new();
    if plugin.manifest.model.is_some() {
        impacts.push("replaces the model provider for every session and project".to_owned());
    }
    if plugin.manifest.context.is_some() {
        impacts
            .push("replaces the context/memory backend for every session and project".to_owned());
    }
    if plugin
        .manifest
        .replace_prompt
        .as_deref()
        .is_some_and(|prompt| !prompt.trim().is_empty())
    {
        impacts.push("replaces Kroma's base system prompt".to_owned());
    }
    if plugin.has_model_hook() {
        impacts.push("rewrites every model request (before_model_request)".to_owned());
    }
    let replaced: Vec<&str> = plugin
        .manifest
        .tools
        .iter()
        .filter(|tool| tool.replace)
        .map(|tool| tool.name.as_str())
        .collect();
    if !replaced.is_empty() {
        impacts.push(format!(
            "overrides built-in tools ({})",
            replaced.join(", ")
        ));
    }
    (!impacts.is_empty()).then(|| {
        format!(
            "Warning: critical plugin: it {}. Undo with 'kroma plugins disable {id}'.",
            impacts.join("; ")
        )
    })
}

fn setting_target<'a>(catalog: &'a PluginCatalog, id: &str) -> Result<&'a Plugin> {
    catalog
        .plugins
        .iter()
        .find(|plugin| plugin.manifest.id == id)
        .ok_or_else(|| anyhow!("unknown plugin '{id}'"))
}

/// Shared write path for the terminal and typed forms: both validate the key
/// against `[settings]` and echo the value that is now in effect.
fn store_config_value(
    state_path: &Path,
    plugin: &Plugin,
    id: &str,
    key: &str,
    value: Option<toml::Value>,
) -> Result<String> {
    if plugin.transient {
        bail!("plugin '{id}' is transient (KROMA_PLUGIN_TRY); its settings are not persisted");
    }
    if !valid_id(key) {
        bail!("config key must be an ASCII identifier");
    }
    check_setting_key(&plugin.manifest, id, key)?;
    let mut state = PluginState::load(state_path)?;
    let settings = match &plugin.project_key {
        Some(_) => &mut state.project_settings,
        None => &mut state.settings,
    };
    let owner = plugin.project_key.as_deref().unwrap_or(id);
    let entry = settings.entry(owner.to_owned()).or_default();
    let effective = match value {
        Some(value) => {
            entry.insert(key.to_owned(), value.clone());
            Some(value)
        }
        None => {
            entry.remove(key);
            plugin.manifest.settings.get(key).cloned()
        }
    };
    if entry.is_empty() {
        settings.remove(owner);
    }
    state.save(state_path)?;
    Ok(match effective {
        Some(value) => format!(
            "Plugin '{id}' setting '{key}' = {value} ({}). {}",
            setting_type_name(&value),
            plugin.apply_hint()
        ),
        None => format!(
            "Plugin '{id}' setting '{key}' removed. {}",
            plugin.apply_hint()
        ),
    })
}

/// Fingerprint of plugin sources: folder names plus each `plugin.toml`'s size
/// and modification time, and the settings file's. Cheap enough to run before
/// every message, where re-reading every manifest would not be.
pub fn source_fingerprint_at(roots: &[PathBuf], settings: Option<&Path>) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::UNIX_EPOCH;

    fn stamp(path: &Path, hasher: &mut DefaultHasher) {
        if let Ok(meta) = fs::metadata(path) {
            meta.len().hash(hasher);
            if let Ok(modified) = meta.modified() {
                modified
                    .duration_since(UNIX_EPOCH)
                    .map(|since| since.as_nanos())
                    .unwrap_or(0)
                    .hash(hasher);
            }
        }
    }

    let mut hasher = DefaultHasher::new();
    if let Some(settings) = settings {
        stamp(settings, &mut hasher);
    }
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        let mut names = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        names.sort();
        root.hash(&mut hasher);
        for name in names {
            name.hash(&mut hasher);
            stamp(&root.join(&name).join("plugin.toml"), &mut hasher);
        }
    }
    hasher.finish()
}

fn secret_is_set(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(text) => !text.is_empty(),
        toml::Value::Array(items) => !items.is_empty(),
        toml::Value::Table(table) => !table.is_empty(),
        _ => true,
    }
}

impl PluginCatalog {
    pub fn directory() -> Result<PathBuf> {
        if let Some(dir) = env::var_os("KROMA_PLUGIN_DIR") {
            if dir.is_empty() {
                bail!("KROMA_PLUGIN_DIR cannot be empty");
            }
            return Ok(PathBuf::from(dir));
        }
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .context("HOME/USERPROFILE unavailable; set KROMA_PLUGIN_DIR")?;
        Ok(PathBuf::from(home).join(".kroma").join("plugins"))
    }

    pub fn empty(workdir: &Path) -> Self {
        Self {
            capability_filter: None,
            directory: PathBuf::new(),
            bundled: None,
            state_path: PathBuf::new(),
            workdir: workdir.to_path_buf(),
            plugins: Vec::new(),
            transient: None,
            warnings: Vec::new(),
            invalid_enabled: Vec::new(),
            ui_bus: Arc::new(PluginUiBus::new()),
        }
    }

    pub fn discover(workdir: &Path) -> Result<Self> {
        let directory = Self::directory()?;
        let state_path = directory
            .parent()
            .context("plugin directory needs a parent")?
            .join("plugins.toml");
        let transient = match env::var_os("KROMA_PLUGIN_TRY") {
            Some(value) if value.is_empty() => {
                bail!("KROMA_PLUGIN_TRY cannot be empty")
            }
            Some(value) => Some(PathBuf::from(value)),
            None => None,
        };
        Self::discover_at_with_transient(directory, state_path, workdir, transient)
    }

    /// Cheap change detector for plugin sources: the plugin folders, the size
    /// and modification time of each `plugin.toml`, and the settings file. It
    /// costs a few `stat` calls instead of re-reading every manifest, so a
    /// conversation can check it before every message.
    pub fn source_fingerprint(workdir: &Path) -> u64 {
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut settings = None;
        if let Ok(directory) = Self::directory() {
            if let Some(parent) = directory.parent() {
                settings = Some(parent.join("plugins.toml"));
            }
            roots.push(directory);
        }
        for name in ["KROMA_OFFICIAL_PLUGIN_DIR", "KROMA_PLUGIN_TRY"] {
            if let Some(dir) = env::var_os(name) {
                roots.push(PathBuf::from(dir));
            }
        }
        roots.push(workdir.join(".kroma").join("plugins"));
        source_fingerprint_at(&roots, settings.as_deref())
    }

    /// Explicit paths avoid changing process-wide environment variables in tests.
    pub fn discover_at(directory: PathBuf, state_path: PathBuf, workdir: &Path) -> Result<Self> {
        Self::discover_at_with_transient(directory, state_path, workdir, None)
    }

    fn discover_at_with_transient(
        directory: PathBuf,
        state_path: PathBuf,
        workdir: &Path,
        transient: Option<PathBuf>,
    ) -> Result<Self> {
        let bundled = env::var_os("KROMA_OFFICIAL_PLUGIN_DIR").map(PathBuf::from);
        Self::discover_at_with_bundled(directory, state_path, workdir, transient, bundled)
    }

    /// Official bundles are read-only inventory, not an extra install step.
    /// User-installed plugins win an ID conflict, and all bundles remain
    /// disabled unless the ordinary user settings explicitly enable them.
    fn discover_at_with_bundled(
        directory: PathBuf,
        state_path: PathBuf,
        workdir: &Path,
        transient: Option<PathBuf>,
        bundled: Option<PathBuf>,
    ) -> Result<Self> {
        let ui_bus = Arc::new(PluginUiBus::new());
        if plugins_disabled() {
            // A subagent child starts with no plugins: it pays no plugin
            // prompt, starts no plugin process and inherits no plugin state.
            return Ok(Self {
                capability_filter: None,
                directory,
                bundled,
                state_path,
                workdir: workdir.to_path_buf(),
                plugins: Vec::new(),
                transient,
                warnings: Vec::new(),
                invalid_enabled: Vec::new(),
                ui_bus,
            });
        }
        let mut warnings = Vec::new();
        let mut invalid_enabled = Vec::new();
        let state = match PluginState::load(&state_path) {
            Ok(state) => state,
            Err(error) => {
                warnings.push(format!(
                    "plugin settings unavailable ({error:#}); all plugins disabled until fixed"
                ));
                PluginState::default()
            }
        };
        let mut plugins = Vec::new();
        let mut seen = BTreeSet::new();
        let project_plugins = workdir.join(".kroma").join("plugins");
        let mut sources = vec![(directory.clone(), false)];
        if let Some(bundle) = bundled.as_ref() {
            sources.push((bundle.clone(), false));
        }
        sources.push((project_plugins, true));
        let mut scanned_sources = BTreeSet::new();
        for (source, project) in &sources {
            // The home workspace shares its plugin directory with the user
            // inventory. Canonicalize aliases (including Windows \\?\ paths)
            // before scanning, keeping the first source and its settings scope.
            if !scanned_sources.insert(source.canonicalize().unwrap_or_else(|_| source.clone())) {
                continue;
            }
            let mut entries = match fs::read_dir(source) {
                Ok(entries) => entries
                    .collect::<std::io::Result<Vec<_>>>()
                    .with_context(|| format!("listing {}", source.display()))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warnings.push(format!("{}: {error}", source.display()));
                    continue;
                }
            };
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let dir = entry.path();
                if !dir.is_dir() || !dir.join("plugin.toml").is_file() {
                    continue;
                }
                match PluginManifest::read(&dir) {
                    Ok(manifest) => {
                        if !seen.insert(manifest.id.clone()) {
                            warnings.push(format!(
                                "duplicate plugin id '{}': {}",
                                manifest.id,
                                dir.display()
                            ));
                            continue;
                        }
                        let project_key = if *project {
                            Some(format!(
                                "{}::{}",
                                source.canonicalize()?.display(),
                                manifest.id
                            ))
                        } else {
                            None
                        };
                        let mut settings = manifest.settings.clone();
                        settings.extend(
                            match &project_key {
                                Some(key) => state.project_settings.get(key),
                                None => state.settings.get(&manifest.id),
                            }
                            .cloned()
                            .unwrap_or_default(),
                        );
                        let data_root = if project_key.is_some() {
                            workdir.join(".kroma").join("plugin-data")
                        } else {
                            state_path
                                .parent()
                                .context("plugin state needs a parent")?
                                .join("plugin-data")
                        };
                        let requested = match &project_key {
                            Some(key) => state.enabled_projects.contains(key),
                            None => state.enabled.contains(&manifest.id),
                        };
                        let settings = serde_json::to_value(settings)?;
                        // Disabled discovery reads the manifest and settings
                        // only. A requested plugin -- including a re-enabled
                        // one -- always gets the complete bounded scan, because
                        // that scan is what its approval is bound to.
                        let worker = Arc::new(if requested {
                            PluginWorker::new(
                                &dir,
                                workdir,
                                &manifest,
                                settings,
                                data_root.join(&manifest.id),
                                ui_bus.clone(),
                            )
                        } else {
                            PluginWorker::unscanned(
                                &dir,
                                workdir,
                                &manifest,
                                settings,
                                data_root.join(&manifest.id),
                                ui_bus.clone(),
                            )
                        });
                        // Activation is one user switch, not a fingerprint
                        // approval workflow. Source edits take effect at an
                        // idle reload; process identity scans still determine
                        // whether an existing worker can be safely reused.
                        let blocked = None;
                        let enabled = requested;
                        // A user-installed plugin is trusted by its
                        // installation, so there an incomplete scan only
                        // weakens change tracking. Report it instead of
                        // silently claiming the code is fully tracked.
                        if let Some(reason) = worker.scan.error.as_deref().filter(|_| enabled) {
                            warnings.push(format!(
                                "plugin '{}' code changes may not be noticed: {reason}",
                                manifest.id
                            ));
                        }
                        let event_worker =
                            (!manifest.events.is_empty()).then(|| Arc::new(worker.isolated()));
                        let ui_worker = Arc::new(worker.isolated());
                        plugins.push(Plugin {
                            manifest,
                            enabled,
                            requested,
                            blocked,
                            project_key,
                            transient: false,
                            worker,
                            event_worker,
                            ui_worker,
                        });
                    }
                    Err(error) => {
                        let label = format!("{}: {error:#}", dir.display());
                        let id = entry.file_name().to_string_lossy().to_string();
                        let project_key = if *project {
                            Some(format!("{}::{id}", source.canonicalize()?.display()))
                        } else {
                            None
                        };
                        if state.enabled.contains(&id)
                            || project_key
                                .as_ref()
                                .is_some_and(|key| state.enabled_projects.contains(key))
                        {
                            let critical_key = project_key.as_deref().unwrap_or(&id);
                            if state.critical_plugins.contains(critical_key) {
                                invalid_enabled.push((id.clone(), label.clone()));
                            }
                        }
                        warnings.push(label);
                    }
                }
            }
        }
        if let Some(source) = transient.clone() {
            let source = source
                .canonicalize()
                .with_context(|| format!("opening transient plugin {}", source.display()))?;
            if !source.is_dir() {
                bail!("KROMA_PLUGIN_TRY must point to a plugin directory");
            }
            let manifest = PluginManifest::read(&source)
                .with_context(|| format!("loading transient plugin {}", source.display()))?;
            if !seen.insert(manifest.id.clone()) {
                bail!(
                    "transient plugin '{}' conflicts with an already discovered plugin",
                    manifest.id
                );
            }
            let data_root = state_path
                .parent()
                .context("plugin state needs a parent")?
                .join("plugin-data");
            let worker = Arc::new(PluginWorker::new(
                &source,
                workdir,
                &manifest,
                serde_json::to_value(&manifest.settings)?,
                data_root.join(&manifest.id),
                ui_bus.clone(),
            ));
            let event_worker = (!manifest.events.is_empty()).then(|| Arc::new(worker.isolated()));
            let ui_worker = Arc::new(worker.isolated());
            plugins.push(Plugin {
                manifest,
                enabled: true,
                requested: true,
                blocked: None,
                project_key: None,
                transient: true,
                worker,
                event_worker,
                ui_worker,
            });
        }
        sync_ui_bus(&plugins, &ui_bus);
        Ok(Self {
            capability_filter: None,
            directory,
            bundled,
            state_path,
            workdir: workdir.to_path_buf(),
            plugins,
            transient,
            warnings,
            invalid_enabled,
            ui_bus,
        })
    }

    /// Fail running a broken enabled plugin, while keeping discovery and
    /// disable/uninstall commands usable to repair the manifest.
    pub fn ensure_loadable(&self) -> Result<()> {
        let failures = self
            .invalid_enabled
            .iter()
            .filter(|(id, _)| {
                self.capability_filter.as_ref().is_none_or(|settings| {
                    settings.enabled(crate::capabilities::CapabilityGroup::Plugins, id, true)
                })
            })
            .map(|(_, error)| error.as_str())
            .collect::<Vec<_>>();
        if !failures.is_empty() {
            bail!("enabled plugin cannot load: {}", failures.join("; "));
        }
        Ok(())
    }

    fn refresh(&self) -> Result<Self> {
        Self::discover_at_with_bundled(
            self.directory.clone(),
            self.state_path.clone(),
            &self.workdir,
            self.transient.clone(),
            self.bundled.clone(),
        )
    }

    /// Structured plugin state for GUI hosts, using the same persisted state
    /// and identity rules as the terminal plugin manager.
    pub fn summaries(&self) -> Result<Vec<Value>> {
        let fresh = self.refresh()?;
        let state = PluginState::load(&self.state_path)?;
        Ok(fresh
            .plugins
            .iter()
            .map(|plugin| {
                let mut settings = plugin.manifest.settings.clone();
                if !plugin.transient {
                    settings.extend(plugin.config_overrides(&state).cloned().unwrap_or_default());
                }
                let secret_settings: Vec<String> = settings
                    .keys()
                    .filter(|key| is_secret_setting(key))
                    .cloned()
                    .collect();
                // `secret_settings` identifies sensitive fields; a field can
                // still be unset. Report configured values separately so the
                // GUI neither claims an empty key is set nor hides the
                // ability to clear a stored one.
                let set_secrets: Vec<String> = settings
                    .iter()
                    .filter(|(key, value)| is_secret_setting(key) && secret_is_set(value))
                    .map(|(key, _)| key.clone())
                    .collect();
                redact_secret_settings(&mut settings);
                json!({
                    "id": plugin.manifest.id,
                    "name": plugin.manifest.name,
                    "version": plugin.manifest.version,
                    "description": plugin.manifest.description,
                    "enabled": plugin.enabled,
                    "requested": plugin.requested,
                    "blocked": plugin.blocked,
                    "project": plugin.project_key.is_some(),
                    "transient": plugin.transient,
                    "api_version": plugin.manifest.api_version,
                    "protocol_version": plugin.worker.protocol_version(),
                    "capabilities": plugin.manifest.capabilities,
                    "settings": settings,
                    "secret_settings": secret_settings,
                    "set_secrets": set_secrets,
                    "has_model": plugin.manifest.model.is_some(),
                    "has_context": plugin.manifest.context.is_some(),
                    "has_ui": plugin.manifest.ui.is_some(),
                    "has_desktop_ui": plugin.manifest.ui.as_ref().is_some_and(|ui| ui.desktop.is_some()),
                })
            })
            .collect())
    }

    pub fn list(&self) -> Result<String> {
        let mut fresh = self.refresh()?;
        // Describe current disk state while comparing it with the frozen
        // runtime registry, so `restart to apply` remains accurate.
        for plugin in &mut fresh.plugins {
            plugin.enabled = self
                .plugins
                .iter()
                .find(|old| {
                    old.manifest.id == plugin.manifest.id && old.project_key == plugin.project_key
                })
                .is_some_and(|old| old.enabled);
        }
        fresh.list_loaded()
    }

    fn list_loaded(&self) -> Result<String> {
        // Listing must still work when an edited settings file is invalid;
        // the discovery warning explains why nothing was activated.
        let state = PluginState::load(&self.state_path).unwrap_or_default();
        let mut lines = Vec::new();
        for plugin in &self.plugins {
            let active = plugin.requests(&state);
            let restart = if active != plugin.enabled {
                " (reload or restart to apply)"
            } else {
                ""
            };
            let version = if plugin.manifest.version.is_empty() {
                ""
            } else {
                &plugin.manifest.version
            };
            let scope = if plugin.transient { " [try]" } else { "" };
            lines.push(format!(
                "{}  {} {}  [{}]{}{}",
                if active { "on " } else { "off" },
                plugin.manifest.id,
                version,
                plugin.manifest.name,
                scope,
                restart
            ));
        }
        if lines.is_empty() {
            lines.push(format!(
                "No plugins found in {} (or .kroma/plugins).",
                self.directory.display()
            ));
        }
        if !self.warnings.is_empty() {
            lines.push(format!("Warnings:\n{}", self.warnings.join("\n")));
        }
        Ok(lines.join("\n"))
    }

    pub fn apply_capabilities(&mut self, settings: &crate::capabilities::Capabilities) {
        use crate::capabilities::CapabilityGroup::Plugins;
        for plugin in &mut self.plugins {
            let enabled = settings.enabled(Plugins, &plugin.manifest.id, plugin.requested);
            if enabled && !plugin.enabled {
                let worker = Arc::new(PluginWorker::new(
                    &plugin.worker.directory,
                    &self.workdir,
                    &plugin.manifest,
                    plugin.worker.settings.clone(),
                    plugin.worker.data_dir.clone(),
                    self.ui_bus.clone(),
                ));
                plugin.event_worker =
                    (!plugin.manifest.events.is_empty()).then(|| Arc::new(worker.isolated()));
                plugin.ui_worker = Arc::new(worker.isolated());
                plugin.worker = worker;
            }
            plugin.enabled = enabled;
        }
        self.capability_filter = Some(settings.clone());
        sync_ui_bus(&self.plugins, &self.ui_bus);
    }

    pub fn preview_default_items(&mut self, items: &BTreeMap<String, bool>) -> Result<()> {
        for (id, enabled) in items {
            let plugin = self
                .plugins
                .iter_mut()
                .find(|plugin| &plugin.manifest.id == id)
                .ok_or_else(|| anyhow!("unknown plugin '{id}'"))?;
            if plugin.transient {
                bail!("install transient plugin '{id}' before changing activation");
            }
            plugin.requested = *enabled;
        }
        Ok(())
    }

    pub fn capability_tool_inventory(&self) -> Vec<Value> {
        self.plugins.iter().flat_map(|plugin| plugin.manifest.tools.iter().map(|tool| {
            json!({"type":"function", "function":{"name":tool.name,"description":tool.description}, "source":plugin.manifest.id})
        })).collect()
    }

    pub fn set_enabled_items(&self, items: &BTreeMap<String, bool>) -> Result<()> {
        let _lock = PluginStateLock::acquire(&self.state_path)?;
        let fresh = self.refresh()?;
        let mut state = PluginState::load(&self.state_path)?;
        for (id, enabled) in items {
            let plugin = fresh
                .plugins
                .iter()
                .find(|plugin| &plugin.manifest.id == id)
                .ok_or_else(|| anyhow!("unknown plugin '{id}'"))?;
            if plugin.transient {
                bail!("install transient plugin '{id}' before changing activation");
            }
            let key = plugin.project_key.as_deref().unwrap_or(id).to_owned();
            if *enabled {
                if plugin.project_key.is_some() {
                    state.enabled_projects.insert(key.clone());
                } else {
                    state.enabled.insert(key.clone());
                }
                if plugin.binds_runtime_backend() {
                    state.critical_plugins.insert(key);
                }
            } else {
                state.enabled.remove(&key);
                state.enabled_projects.remove(&key);
                state.critical_plugins.remove(&key);
            }
        }
        state.save(&self.state_path)
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<String> {
        let _lock = PluginStateLock::acquire(&self.state_path)?;
        let fresh = self.refresh()?;
        let mut state = PluginState::load(&self.state_path)?;
        let plugin = fresh.plugins.iter().find(|plugin| plugin.manifest.id == id);
        if plugin.is_some_and(|plugin| plugin.transient) {
            bail!(
                "plugin '{id}' is a transient KROMA_PLUGIN_TRY plugin; install it before changing activation"
            );
        }
        if plugin.is_none() && !enabled && valid_id(id) {
            // A broken manifest is not discoverable, but must still be
            // possible to disable so a later reload can drop the old worker.
            let mut found = state.enabled.remove(id);
            state.critical_plugins.remove(id);
            let project_dir = self.workdir.join(".kroma").join("plugins");
            if let Ok(directory) = project_dir.canonicalize() {
                let key = format!("{}::{id}", directory.display());
                found |= state.enabled_projects.remove(&key);
                state.critical_plugins.remove(&key);
            }
            if found {
                state.save(&self.state_path)?;
                return Ok(format!(
                    "Plugin '{id}' disabled. Reload or restart Kroma to apply."
                ));
            }
        }
        let plugin = plugin.ok_or_else(|| anyhow!("unknown plugin '{id}'"))?;
        let key = plugin.project_key.as_deref().unwrap_or(id).to_owned();
        // Disabling is always possible, including a malformed manifest.
        if !enabled {
            state.enabled.remove(&key);
            state.enabled_projects.remove(&key);
            state.critical_plugins.remove(&key);
            state.save(&self.state_path)?;
            let loaded = self
                .plugins
                .iter()
                .find(|loaded| loaded.project_key.as_deref().unwrap_or(&loaded.manifest.id) == key)
                .unwrap_or(plugin);
            return Ok(format!("Plugin '{id}' disabled. {}", loaded.apply_hint()));
        }
        // No approval fingerprint or second confirmation is needed. Plugin
        // identity still controls lazy worker reuse and idle reload.
        if plugin.project_key.is_some() {
            state.enabled_projects.insert(key.clone());
        } else {
            state.enabled.insert(key.clone());
        }
        if plugin.manifest.model.is_some()
            || plugin.manifest.context.is_some()
            || plugin.has_model_hook()
        {
            state.critical_plugins.insert(key.clone());
        } else {
            state.critical_plugins.remove(&key);
        }
        state.save(&self.state_path)?;
        let mut message = format!("Plugin '{id}' enabled. {}", plugin.apply_hint());
        if let Some(warning) = critical_plugin_warning(plugin, id) {
            message.push('\n');
            message.push_str(&warning);
        }
        Ok(message)
    }

    /// Terminal `set` text is typed from the manifest default of the same key;
    /// typed hosts keep using `set_config_value`.
    pub fn set_config(&self, id: &str, key: &str, value: Option<&str>) -> Result<String> {
        let _lock = PluginStateLock::acquire(&self.state_path)?;
        let fresh = self.refresh()?;
        let plugin = setting_target(&fresh, id)?;
        let value = match value {
            Some(text) => Some(match plugin.manifest.settings.get(key) {
                Some(default) => typed_setting_value(key, default, text)?,
                None => toml::Value::String(text.to_owned()),
            }),
            None => None,
        };
        store_config_value(&self.state_path, plugin, id, key, value)
    }

    /// Preserve boolean/numeric plugin defaults in auto-generated GUI forms: a
    /// typed host passes the value it already converted.
    pub fn set_config_value(
        &self,
        id: &str,
        key: &str,
        value: Option<toml::Value>,
    ) -> Result<String> {
        let _lock = PluginStateLock::acquire(&self.state_path)?;
        let fresh = self.refresh()?;
        let plugin = setting_target(&fresh, id)?;
        store_config_value(&self.state_path, plugin, id, key, value)
    }

    pub fn show_config(&self, id: &str) -> Result<String> {
        let fresh = self.refresh()?;
        let plugin = fresh
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == id)
            .ok_or_else(|| anyhow!("unknown plugin '{id}'"))?;
        let mut config = plugin.manifest.settings.clone();
        if !plugin.transient {
            let state = PluginState::load(&self.state_path)?;
            config.extend(plugin.config_overrides(&state).cloned().unwrap_or_default());
        }
        redact_secret_settings(&mut config);
        Ok(if config.is_empty() {
            "No settings.".into()
        } else {
            toml::to_string_pretty(&config)?
        })
    }

    /// Remove an installed user plugin and its activation/configuration.
    /// Project-local plugin sources are managed in their project directory.
    pub fn uninstall(&self, id: &str) -> Result<String> {
        let _lock = PluginStateLock::acquire(&self.state_path)?;
        if !valid_id(id) {
            bail!(
                "'{id}' is not a plugin id: use letters, digits, '-' or '_' (run `kroma plugins list` to see installed ids)."
            );
        }
        let fresh = self.refresh()?;
        if fresh.plugins.iter().any(|plugin| {
            plugin.transient && plugin.project_key.is_none() && plugin.manifest.id == id
        }) {
            bail!("cannot uninstall transient plugin '{id}'; it is only loaded for this run");
        }
        let target = fresh
            .plugins
            .iter()
            .find(|plugin| plugin.project_key.is_none() && plugin.manifest.id == id)
            .map(|plugin| plugin.worker.directory.clone())
            .unwrap_or_else(|| self.directory.join(id));
        // The user plugin directory is the only uninstallable source. Official
        // bundles are owned by the package manager and project plugins by
        // their workspace. Never delete either through this command.
        if !target.starts_with(&self.directory) || target.parent() != Some(self.directory.as_path())
        {
            bail!("plugin '{id}' is bundled or project-managed; disable it instead of uninstalling its source");
        }
        if !target.is_dir() {
            let project = self.workdir.join(".kroma").join("plugins").join(id);
            if project.is_dir() {
                bail!(
                    "project plugin '{id}' is source-managed; remove it from .kroma/plugins manually"
                );
            }
            bail!("unknown installed plugin '{id}'");
        }
        // Resolve by its installation directory, not the manifest. A broken
        // plugin must remain uninstallable without editing its TOML first.
        let mut state = PluginState::load(&self.state_path)?;
        fs::remove_dir_all(&target).with_context(|| format!("uninstalling plugin '{id}'"))?;
        state.enabled.remove(id);
        state.critical_plugins.remove(id);
        state.settings.remove(id);
        state.save(&self.state_path)?;
        Ok(format!(
            "Uninstalled '{id}'. Reload or restart Kroma to apply."
        ))
    }

    pub fn install(&self, directory: &Path) -> Result<String> {
        self.install_directory(directory, None)
    }

    fn install_directory(
        &self,
        directory: &Path,
        symlink_boundary: Option<&Path>,
    ) -> Result<String> {
        let _lock = PluginStateLock::acquire(&self.state_path)?;
        let source = directory
            .canonicalize()
            .with_context(|| format!("opening {}", directory.display()))?;
        if !source.is_dir() {
            bail!("plugin source must be a directory");
        }
        if let Some(boundary) = symlink_boundary {
            if crate::path_display::strip_prefix_equivalent(&source, boundary).is_none() {
                bail!(
                    "plugin directory symlink escapes checkout root: {}",
                    directory.display()
                );
            }
        }
        let manifest = PluginManifest::read(&source)?;
        // Installing a user override of an official bundled plugin is valid.
        // Only an existing user-installed copy or a project/transient copy
        // blocks the operation; discovery already gives user copies priority.
        if self.refresh()?.plugins.iter().any(|plugin| {
            plugin.manifest.id == manifest.id
                && (plugin.worker.directory.starts_with(&self.directory)
                    || plugin.project_key.is_some()
                    || plugin.transient)
        }) {
            bail!("plugin '{}' is already installed", manifest.id);
        }
        let target = self.directory.join(&manifest.id);
        if target.exists() {
            bail!("plugin destination already exists: {}", target.display());
        }
        if target.starts_with(&source) {
            bail!("cannot install a plugin into itself");
        }
        fs::create_dir_all(&self.directory)?;
        copy_directory(&source, &target, symlink_boundary)
            .with_context(|| format!("installing '{}'", manifest.id))?;
        // Reinstalling an ID must not inherit stale activation or settings.
        let result = (|| -> Result<()> {
            let mut state = PluginState::load(&self.state_path)?;
            state.enabled.remove(&manifest.id);
            state.critical_plugins.remove(&manifest.id);
            state.settings.remove(&manifest.id);
            state.save(&self.state_path)
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&target);
            return Err(error);
        }
        Ok(format!(
            "Installed '{}' (disabled), and nothing runs until you enable it.\n\n{}\n\nEnable with 'kroma plugins enable {}'.",
            manifest.id,
            manifest.describe(),
            manifest.id
        ))
    }

    /// Install from a local directory or a git URL. Remote sources are cloned
    /// with the user's own git (no bundled credential or network stack) and
    /// then go through exactly the same local install path, including the
    /// manifest check and the entry/size budget.
    pub fn install_source(&self, source: &str) -> Result<String> {
        let Some(url) = git_source(source) else {
            return self.install(Path::new(source));
        };
        let nonce = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let checkout =
            env::temp_dir().join(format!("kroma-plugin-clone-{}-{nonce}", std::process::id()));
        let mut git_clone = Command::new("git");
        git_clone
            .args(["clone", "--depth", "1"])
            .arg(&url)
            .arg(&checkout);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            git_clone.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let output = git_clone.output().with_context(|| {
            format!("running git for {url}; install git or pass a local plugin directory")
        })?;
        if !output.status.success() {
            let _ = fs::remove_dir_all(&checkout);
            let reason = String::from_utf8_lossy(&output.stderr);
            bail!(
                "git clone {url} failed: {}",
                reason.trim().lines().last().unwrap_or("no diagnostic")
            );
        }
        let checkout_root = checkout.canonicalize().inspect_err(|_| {
            let _ = fs::remove_dir_all(&checkout);
        })?;
        let manifest_dir = find_manifest_directory(&checkout).inspect_err(|_| {
            let _ = fs::remove_dir_all(&checkout);
        })?;
        let result = self.install_directory(&manifest_dir, Some(&checkout_root));
        let _ = fs::remove_dir_all(&checkout);
        result
    }

    /// Validate a plugin with the same manifest/process contract used by the
    /// runtime. This starts one explicit process for a bounded `ping`; it
    /// never persists activation, calls a model, or uses plugin settings.
    pub fn check(&self, directory: &Path) -> Result<String> {
        let source = directory
            .canonicalize()
            .with_context(|| format!("opening {}", directory.display()))?;
        if !source.is_dir() {
            bail!("plugin source must be a directory");
        }
        let manifest = PluginManifest::read(&source)?;
        let contributions = format!(
            "tools={} commands={} hooks={} events={} context={} model={} ui={}",
            manifest.tools.len(),
            manifest.commands.len(),
            manifest.hooks.len(),
            manifest.events.len(),
            manifest.context.is_some(),
            manifest.model.is_some(),
            manifest.ui.is_some()
        );
        let mut lines = vec![
            manifest.describe(),
            String::new(),
            format!("plugin={} name={}", manifest.id, manifest.name),
            format!(
                "api_version={} protocol_version={}",
                manifest.api_version, manifest.api_version
            ),
            format!(
                "capabilities: {}",
                if manifest.capabilities.is_empty() {
                    "none".to_owned()
                } else {
                    manifest.capabilities.join(",")
                }
            ),
            format!("contributions: {contributions}"),
        ];
        if manifest
            .command
            .as_deref()
            .is_none_or(|command| command.trim().is_empty())
        {
            lines.push("process=not applicable (static prompt/UI only)".into());
            return Ok(lines.join("\n"));
        }
        let data_dir = env::temp_dir().join(format!(
            "kroma-plugin-check-{}-{}-{}",
            std::process::id(),
            manifest.id,
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let worker = Arc::new(PluginWorker::new(
            &source,
            &self.workdir,
            &manifest,
            serde_json::to_value(&manifest.settings)?,
            data_dir.clone(),
            Arc::new(PluginUiBus::new()),
        ));
        let request = json!({"type":"ping", "name":"ping"});
        let reply = std::thread::spawn(move || -> Result<PluginResponse> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()?;
            runtime.block_on(worker.invoke_async_timeout(request, PLUGIN_CHECK_TIMEOUT))
        })
        .join()
        .map_err(|_| anyhow::anyhow!("plugin check worker panicked"));
        let _ = fs::remove_dir_all(&data_dir);
        let reply = reply??;
        let response: String = reply.content.chars().take(200).collect();
        lines.push(format!(
            "smoke=response is_error={} content={}",
            reply.is_error,
            response.replace('\n', "\\n")
        ));
        lines.push(
            "note=a plugin may reject the optional ping; a valid JSON response still proves framing"
                .into(),
        );
        Ok(lines.join("\n"))
    }

    /// Only actual changes to active model/context identity require restart.
    /// Reuse unchanged workers so an unrelated tool reload does not erase
    /// process-local counters, warm caches or event subscription state.
    pub fn prepare_idle_reload_from(
        &self,
        next: &mut Self,
        base_tools: &ToolRegistry,
        base_prompt: &PromptComposer,
    ) -> Result<(ToolRegistry, PromptComposer)> {
        if let Some(settings) = &self.capability_filter {
            next.apply_capabilities(settings);
        }
        next.ensure_loadable()?;
        let desired = PluginState::load(&next.state_path)?;
        for old in self.plugins.iter().filter(|plugin| plugin.enabled) {
            let found = next.plugins.iter().any(|plugin| {
                plugin.manifest.id == old.manifest.id && plugin.project_key == old.project_key
            });
            if !found && old.requests(&desired) && old.has_stateful_backend() {
                bail!(
                    "active model/context plugin '{}' is missing or invalid; fix it or disable it first",
                    old.manifest.id
                );
            }
            if !found && old.has_stateful_backend() {
                bail!(
                    "removing active model/context plugin '{}' requires restart",
                    old.manifest.id
                );
            }
        }
        for plugin in &mut next.plugins {
            let old = self.plugins.iter().find(|old| {
                old.manifest.id == plugin.manifest.id && old.project_key == plugin.project_key
            });
            let Some(old) = old else {
                if plugin.enabled && plugin.has_stateful_backend() {
                    bail!(
                        "enabling model/context plugin '{}' requires restart",
                        plugin.manifest.id
                    );
                }
                plugin.worker = Arc::new(plugin.worker.with_ui_bus(self.ui_bus.clone()));
                plugin.event_worker = plugin
                    .event_worker
                    .as_ref()
                    .map(|w| Arc::new(w.with_ui_bus(self.ui_bus.clone())));
                plugin.ui_worker = Arc::new(plugin.ui_worker.with_ui_bus(self.ui_bus.clone()));
                continue;
            };
            let same_worker = old.worker.same_definition(&plugin.worker);
            if (old.has_stateful_backend() || plugin.has_stateful_backend())
                && (old.enabled != plugin.enabled
                    || old.manifest.model != plugin.manifest.model
                    || old.manifest.context != plugin.manifest.context
                    || old.has_model_hook() != plugin.has_model_hook()
                    || old.has_request_context() != plugin.has_request_context()
                    || old.has_task_summary() != plugin.has_task_summary()
                    || !same_worker)
            {
                bail!(
                    "active model/context plugin '{}' changed; restart Kroma",
                    plugin.manifest.id
                );
            }
            if same_worker {
                // Keep unchanged workers warm, including their state. The
                // definition fingerprint includes plugin-local imported JS,
                // so a helper change invalidates reuse without node_modules.
                plugin.worker = old.worker.clone();
                plugin.ui_worker = old.ui_worker.clone();
                // Event subscriptions use an isolated process; retain its
                // state when an unrelated plugin changes during idle reload.
                if plugin.manifest.events == old.manifest.events {
                    plugin.event_worker = old.event_worker.clone();
                } else {
                    plugin.event_worker = plugin
                        .event_worker
                        .as_ref()
                        .map(|w| Arc::new(w.with_ui_bus(self.ui_bus.clone())));
                }
            } else {
                plugin.worker = Arc::new(plugin.worker.with_ui_bus(self.ui_bus.clone()));
                plugin.event_worker = plugin
                    .event_worker
                    .as_ref()
                    .map(|w| Arc::new(w.with_ui_bus(self.ui_bus.clone())));
                plugin.ui_worker = Arc::new(plugin.ui_worker.with_ui_bus(self.ui_bus.clone()));
            }
        }
        let prepared = next.prepare_idle_reload(base_tools, base_prompt)?;
        next.ui_bus = self.ui_bus.clone();
        sync_ui_bus(&next.plugins, &next.ui_bus);
        Ok(prepared)
    }

    /// Build a replacement plugin view without touching the running agent.
    /// Callers commit it only after tools, commands and event subscriptions
    /// all succeed. Default tools and prompt are reusable lightweight clones.
    pub fn prepare_idle_reload(
        &self,
        base_tools: &ToolRegistry,
        base_prompt: &PromptComposer,
    ) -> Result<(ToolRegistry, PromptComposer)> {
        self.ensure_loadable()?;
        let mut tools = base_tools.clone();
        let mut prompt = base_prompt.clone();
        let warnings = self.contribute(&mut tools, &mut prompt);
        if !warnings.is_empty() {
            bail!("plugin reload conflicts: {}", warnings.join("; "));
        }
        Ok((tools, prompt))
    }

    pub fn contribute(&self, tools: &mut ToolRegistry, prompt: &mut PromptComposer) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut replaced_tools = BTreeSet::new();
        let mut replaced_prompt = false;
        let mut names: BTreeSet<String> = tools
            .definitions()
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
            .collect();
        for plugin in &self.plugins {
            if !plugin.enabled {
                continue;
            }
            if let Some(conflict) = plugin.manifest.tools.iter().find(|tool| {
                (!tool.replace && names.contains(&tool.name))
                    || (tool.replace
                        && (!names.contains(&tool.name) || replaced_tools.contains(&tool.name)))
            }) {
                warnings.push(format!(
                    "{}: tool '{}' has a registration/replacement conflict; skipping plugin",
                    plugin.manifest.id, conflict.name
                ));
                continue;
            }
            // Stage each plugin's full set of contributions on copies. A
            // failed prompt or tool registration must not leave half a plugin
            // installed in the active Agent or its advertised schema.
            let mut candidate_tools = tools.clone();
            let mut candidate_prompt = prompt.clone();
            let mut plugin_errors = Vec::new();
            if let Some(base) = plugin.manifest.replace_prompt.as_deref() {
                if replaced_prompt {
                    warnings.push(format!(
                        "{}: base Prompt already replaced; plugin skipped",
                        plugin.manifest.id
                    ));
                    continue;
                }
                if let Err(error) = candidate_prompt.replace_system_prompt(base) {
                    warnings.push(format!("{}: {error:#}", plugin.manifest.id));
                    continue;
                }
            }
            if let Some(text) = plugin
                .manifest
                .prompt
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                if let Err(error) = candidate_prompt.push_trailing(text) {
                    plugin_errors.push(format!("{}: {error:#}", plugin.manifest.id));
                }
            }
            for tool in &plugin.manifest.tools {
                // Host-run tools have no process to talk to: the name and the
                // model-facing description come from the manifest, and this
                // process owns the arguments, the wait and the answer.
                if tool.host.is_some() {
                    let ask_tool =
                        Arc::new(AskTool::new(tool.name.clone(), tool.description.clone()))
                            as Arc<dyn Tool>;
                    if let Err(error) = candidate_tools.register(ask_tool) {
                        plugin_errors.push(format!("{}: {error:#}", plugin.manifest.id));
                    }
                    continue;
                }
                let adapter = PluginToolAdapter {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.parameters.clone(),
                    streaming: tool.streaming,
                    invalidates_context: tool.invalidates_context,
                    requires_user_context: tool.requires_user_context,
                    requires_user_attachments: tool.requires_user_attachments,
                    may_mutate_worktree: tool.may_mutate_worktree,
                    timeout: effective_tool_timeout(tool.timeout_secs, plugin_tool_timeout()),
                    data_dir: plugin.worker.data_dir.clone(),
                    worker: plugin.worker.clone(),
                };
                let result = if tool.replace {
                    candidate_tools.replace(&tool.name, Arc::new(adapter))
                } else {
                    candidate_tools.register(Arc::new(adapter))
                };
                if let Err(error) = result {
                    plugin_errors.push(format!("{}: {error:#}", plugin.manifest.id));
                }
            }
            if plugin
                .manifest
                .hooks
                .iter()
                .any(|hook| hook == "before_tool_call" || hook == "after_tool_result")
            {
                candidate_tools.register_interceptor(Arc::new(PluginToolHooks {
                    before: plugin
                        .manifest
                        .hooks
                        .iter()
                        .any(|hook| hook == "before_tool_call"),
                    after: plugin
                        .manifest
                        .hooks
                        .iter()
                        .any(|hook| hook == "after_tool_result"),
                    worker: plugin.worker.clone(),
                }));
            }
            if !plugin_errors.is_empty() {
                warnings.extend(plugin_errors);
                continue;
            }
            *tools = candidate_tools;
            *prompt = candidate_prompt;
            replaced_prompt |= plugin.manifest.replace_prompt.is_some();
            for tool in &plugin.manifest.tools {
                names.insert(tool.name.clone());
                if tool.replace {
                    replaced_tools.insert(tool.name.clone());
                }
            }
        }
        warnings
    }

    pub fn model_hooks(
        &self,
        backend: Arc<dyn ModelBackend>,
        budget: ContextConfig,
    ) -> Arc<dyn ModelBackend> {
        let workers = self
            .plugins
            .iter()
            .filter(|plugin| plugin.enabled && plugin.has_model_hook())
            .map(|plugin| plugin.worker.clone())
            .collect::<Vec<_>>();
        let context_workers = self
            .plugins
            .iter()
            .filter(|plugin| plugin.enabled && plugin.has_request_context())
            .map(|plugin| model::CapabilityWorker {
                worker: plugin.worker.clone(),
                source: plugin.manifest.id.clone(),
                operation: if plugin.manifest.has_capability("provide_context") {
                    "provide_context"
                } else {
                    "request_context"
                },
            })
            .collect::<Vec<_>>();
        let summary_workers = self
            .plugins
            .iter()
            // Generic child processes can opt out of post-task plugin writes.
            .filter(|plugin| {
                plugin.enabled
                    && plugin.has_task_summary()
                    && std::env::var("KROMA_CHILD_SKIP_TASK_SUMMARY")
                        .ok()
                        .as_deref()
                        != Some("1")
            })
            .map(|plugin| model::CapabilityWorker {
                worker: plugin.worker.clone(),
                source: plugin.manifest.id.clone(),
                operation: if plugin.manifest.has_capability("task_finished") {
                    "task_finished"
                } else {
                    "task_summary"
                },
            })
            .collect::<Vec<_>>();
        if workers.is_empty() && context_workers.is_empty() && summary_workers.is_empty() {
            backend
        } else {
            Arc::new(model::PluginModelHooks::new(
                backend,
                budget,
                workers,
                context_workers,
                summary_workers,
            ))
        }
    }

    /// Resolve an enabled model plugin before initializing the default
    /// provider. This avoids optional /models requests and permits using a
    /// plugin-provided HTTP profile without redundant default credentials.
    pub fn has_model_override(&self) -> bool {
        self.plugins
            .iter()
            .any(|plugin| plugin.enabled && plugin.manifest.model.is_some())
    }

    pub fn checked_model_override(&self) -> Result<Option<ModelOverride>> {
        self.ensure_loadable()?;
        let (model, warnings) = self.model_override();
        if !warnings.is_empty() {
            bail!(
                "enabled model plugin cannot initialize: {}",
                warnings.join("; ")
            );
        }
        Ok(model)
    }

    pub fn model_override(&self) -> (Option<ModelOverride>, Vec<String>) {
        let mut warnings = Vec::new();
        let mut selected = None;
        for plugin in &self.plugins {
            if !plugin.enabled {
                continue;
            }
            if let Some(spec) = &plugin.manifest.model {
                if selected.is_some() {
                    warnings.push(format!("{}: model already replaced", plugin.manifest.id));
                    continue;
                }
                let prepared = (|| -> Result<(Arc<dyn ModelBackend>, ContextConfig)> {
                    let window = ContextConfig::with_output_reserve(
                        spec.context_window,
                        spec.output_reserve,
                    )?;
                    if spec.protocol == "plugin" {
                        return Ok((
                            Arc::new(model::ProcessModelBackend::new(
                                plugin.worker.clone(),
                                spec.model.clone(),
                                spec.streaming,
                                spec.thinking_levels.clone(),
                                spec.reasoning_level.clone(),
                                spec.image_input,
                            )),
                            window,
                        ));
                    }
                    let key = spec
                        .api_key_env
                        .as_ref()
                        .and_then(|name| env::var(name).ok());
                    // HTTP protocols send an explicit output cap; native ones
                    // otherwise default to 4096. The manifest reserve is the
                    // context budget, so it must also bound the wire value.
                    let client = ModelClient::new(&spec.base_url, key, spec.model.clone())?
                        .with_protocol(&spec.protocol)?
                        .with_output_tokens(Some(spec.output_reserve));
                    Ok((Arc::new(client), window))
                })();
                match prepared {
                    Ok((next, next_budget)) => {
                        selected = Some((next, next_budget, spec.model.clone()));
                    }
                    Err(error) => warnings.push(format!(
                        "{}: model replacement failed: {error:#}",
                        plugin.manifest.id
                    )),
                }
            }
        }
        (selected, warnings)
    }

    pub fn replace_model(
        &self,
        backend: &mut Arc<dyn ModelBackend>,
        budget: &mut ContextConfig,
    ) -> Vec<String> {
        let (selected, warnings) = self.model_override();
        if let Some((model, context, _name)) = selected {
            *backend = model;
            *budget = context;
        }
        warnings
    }

    pub fn replace_context(&self, factory: &mut Arc<dyn ContextFactory>) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut active = false;
        for plugin in &self.plugins {
            if !plugin.enabled {
                continue;
            }
            if let Some(spec) = &plugin.manifest.context {
                if active {
                    warnings.push(format!(
                        "{}: context strategy already replaced",
                        plugin.manifest.id
                    ));
                    continue;
                }
                *factory = Arc::new(context::PluginContextFactory::new(
                    plugin.manifest.id.clone(),
                    spec.version,
                    plugin.worker.clone(),
                ));
                active = true;
            }
        }
        warnings
    }

    pub fn builtin_context_factory(
        &self,
        capabilities: &crate::capabilities::Capabilities,
    ) -> Arc<dyn ContextFactory> {
        let plugins_disabled =
            !capabilities.any_enabled(crate::capabilities::CapabilityGroup::Plugins);
        let mut disabled_strategies = BTreeSet::new();
        let mut enabled_strategies = BTreeSet::new();
        for plugin in &self.plugins {
            if plugin.enabled {
                enabled_strategies.insert(plugin.manifest.id.clone());
            } else {
                disabled_strategies.insert(plugin.manifest.id.clone());
            }
        }
        for (id, allowed) in &capabilities.items.plugins {
            if *allowed {
                enabled_strategies.insert(id.clone());
            } else {
                disabled_strategies.insert(id.clone());
            }
        }
        Arc::new(crate::context::DisabledExternalContextFactory::new(
            plugins_disabled,
            disabled_strategies,
            enabled_strategies,
        ))
    }

    pub fn wrap_events(
        &self,
        host: Option<Arc<dyn EventSink>>,
    ) -> Result<Option<Arc<dyn EventSink>>> {
        let listeners: Vec<_> = self
            .plugins
            .iter()
            .filter(|plugin| plugin.enabled && !plugin.manifest.events.is_empty())
            .map(|plugin| {
                (
                    plugin
                        .manifest
                        .events
                        .iter()
                        .cloned()
                        .collect::<BTreeSet<_>>(),
                    plugin
                        .event_worker
                        .as_ref()
                        .expect("event worker initialized")
                        .clone(),
                )
            })
            .collect();
        if listeners.is_empty() {
            return Ok(host);
        }
        // Independent bounded queues: one stalled subscriber must not block
        // unrelated subscribers. No event thread or IPC when none are enabled.
        let mut subscriptions = Vec::with_capacity(listeners.len());
        for (events, worker) in listeners {
            let (sender, receiver) = mpsc::sync_channel::<Value>(32);
            let stopped = Arc::new(AtomicBool::new(false));
            let stopped_for_thread = stopped.clone();
            let subscriber = worker.clone();
            std::thread::Builder::new()
                .name("kroma-plugin-events".into())
                .spawn(move || {
                    while let Ok(message) = receiver.recv() {
                        if stopped_for_thread.load(Ordering::SeqCst) {
                            break;
                        }
                        let id = worker.next_request.fetch_add(1, Ordering::Relaxed);
                        if let Err(error) =
                            worker.invoke_inner(message, Some((id, &stopped_for_thread)))
                        {
                            if !stopped_for_thread.load(Ordering::SeqCst) {
                                crate::runtime_log!("[plugin] event handler: {error:#}");
                            }
                        }
                    }
                })?;
            subscriptions.push(PluginEventSubscription {
                events,
                sender,
                dropped: AtomicU64::new(0),
                stopped,
                worker: subscriber,
            });
        }
        Ok(Some(Arc::new(PluginEventSink {
            host,
            listeners: subscriptions,
            session: RwLock::new(None),
        })))
    }

    pub fn enabled_model_identity(&self) -> Option<(&str, &str)> {
        self.plugins
            .iter()
            .find(|plugin| plugin.enabled && plugin.manifest.model.is_some())
            .and_then(|plugin| plugin.manifest.model.as_ref())
            .map(|spec| (spec.model.as_str(), spec.base_url.as_str()))
    }

    pub fn selected_model_name(&self) -> Option<&str> {
        self.plugins
            .iter()
            .find(|plugin| plugin.enabled && plugin.manifest.model.is_some())
            .and_then(|plugin| plugin.manifest.model.as_ref())
            .map(|spec| spec.model.as_str())
    }

    /// Parse shortcut keys once per UI configuration, never on every keypress.
    /// Collisions reject reloading the bindings instead of silently stealing
    /// an existing plugin's shortcut; built-in Ctrl keys are not exposed.
    pub fn ui_shortcuts(&self) -> Result<Vec<(char, String)>> {
        let mut registered = BTreeMap::new();
        for plugin in &self.plugins {
            if !plugin.enabled {
                continue;
            }
            if let Some(ui) = &plugin.manifest.ui {
                for shortcut in &ui.shortcuts {
                    let key = parse_alt_shortcut(&shortcut.key).context("invalid shortcut")?;
                    if registered.insert(key, shortcut.command.clone()).is_some() {
                        bail!("Alt+{key} is assigned by multiple enabled plugins");
                    }
                }
            }
        }
        Ok(registered.into_iter().collect())
    }

    pub fn ui_activity_frames(&self) -> Result<Vec<String>> {
        let mut active = None;
        for plugin in self.plugins.iter().filter(|plugin| plugin.enabled) {
            let Some(ui) = &plugin.manifest.ui else {
                continue;
            };
            if ui.activity_frames.is_empty() {
                continue;
            }
            if active.is_some() {
                bail!("multiple plugins declare activity_frames; disable one before reloading");
            }
            active = Some(ui.activity_frames.clone());
        }
        Ok(active.unwrap_or_default())
    }

    pub fn ui_colors(&self) -> Result<BTreeMap<String, String>> {
        let mut colors = BTreeMap::new();
        for plugin in self.plugins.iter().filter(|plugin| plugin.enabled) {
            if let Some(ui) = &plugin.manifest.ui {
                for (slot, value) in &ui.colors {
                    if colors.insert(slot.clone(), value.clone()).is_some() {
                        bail!("multiple plugins define UI color '{slot}'");
                    }
                }
            }
        }
        Ok(colors)
    }

    pub fn ui_layout(&self) -> Result<Option<PluginLayoutSpec>> {
        let mut selected = None;
        for plugin in self.plugins.iter().filter(|plugin| plugin.enabled) {
            if let Some(spec) = plugin
                .manifest
                .ui
                .as_ref()
                .and_then(|ui| ui.layout.as_ref())
            {
                if selected.is_some() {
                    bail!("multiple plugins declare a TUI layout; disable one before reloading");
                }
                selected = Some(spec.clone());
            }
        }
        Ok(selected)
    }

    pub fn ui_tool_labels(&self) -> Result<BTreeMap<String, String>> {
        let mut labels = BTreeMap::new();
        for plugin in self.plugins.iter().filter(|plugin| plugin.enabled) {
            if let Some(ui) = &plugin.manifest.ui {
                for (name, label) in &ui.tool_labels {
                    if labels.insert(name.clone(), label.clone()).is_some() {
                        bail!("multiple plugins define tool label for '{name}'");
                    }
                }
            }
        }
        Ok(labels)
    }

    pub fn ui_bus(&self) -> Arc<PluginUiBus> {
        Arc::clone(&self.ui_bus)
    }

    pub fn clear_session_ui(&self) {
        self.ui_bus.clear_session_ui();
    }

    pub fn ui_statuses(&self) -> Vec<String> {
        self.ui_bus.statuses()
    }

    pub fn ui_widgets(&self) -> Vec<PluginWidget> {
        self.ui_bus.widgets()
    }

    pub fn ui_desktop_surfaces(&self) -> Vec<Value> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.enabled)
            .filter_map(|plugin| {
                let desktop = plugin.manifest.ui.as_ref()?.desktop.as_ref()?;
                let title = if desktop.title.trim().is_empty() {
                    plugin.manifest.name.clone()
                } else {
                    desktop.title.clone()
                };
                Some(json!({
                    "plugin_id": plugin.manifest.id,
                    "title": title,
                    "slot": desktop.slot,
                    "height": desktop.height,
                }))
            })
            .collect()
    }

    pub fn ui_desktop_bundle(&self, id: &str) -> Result<Value> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == id && plugin.enabled)
            .ok_or_else(|| anyhow!("no enabled plugin '{id}'"))?;
        let desktop = plugin
            .manifest
            .ui
            .as_ref()
            .and_then(|ui| ui.desktop.as_ref())
            .context("plugin has no Desktop UI surface")?;
        let title = if desktop.title.trim().is_empty() {
            plugin.manifest.name.clone()
        } else {
            desktop.title.clone()
        };
        let html = ui::bundle_desktop_html(
            &plugin.worker.directory,
            &desktop.entry,
            &plugin.manifest.id,
        )?;
        Ok(json!({
            "plugin_id": plugin.manifest.id,
            "title": title,
            "slot": desktop.slot,
            "height": desktop.height,
            "html": html,
        }))
    }

    pub async fn ui_action_async(
        &self,
        plugin_id: &str,
        widget_id: &str,
        action_id: &str,
        payload: Option<Value>,
        session_id: &str,
        archive: &Path,
    ) -> Result<(String, Option<String>)> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id && plugin.enabled)
            .ok_or_else(|| anyhow!("no enabled plugin '{plugin_id}'"))?;
        if plugin
            .manifest
            .command
            .as_deref()
            .is_none_or(|cmd| cmd.trim().is_empty())
        {
            bail!("plugin '{plugin_id}' has no command to handle UI actions");
        }
        let worker = plugin.ui_worker.clone();
        let reply = worker
            .invoke_async(json!({
                "type": "ui_action",
                "widget_id": widget_id,
                "action_id": action_id,
                "payload": payload.unwrap_or(Value::Null),
                "session_id": session_id,
                "archive": archive,
            }))
            .await?;
        if reply.is_error {
            bail!("{}", reply.content);
        }
        Ok((reply.content, reply.task))
    }

    pub async fn ui_message_async(
        &self,
        plugin_id: &str,
        payload: Value,
        session_id: &str,
        archive: &Path,
    ) -> Result<Value> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id && plugin.enabled)
            .ok_or_else(|| anyhow!("no enabled plugin '{plugin_id}'"))?;
        if plugin
            .manifest
            .command
            .as_deref()
            .is_none_or(|cmd| cmd.trim().is_empty())
        {
            bail!("plugin '{plugin_id}' has no command to handle UI messages");
        }
        let worker = plugin.ui_worker.clone();
        let reply = worker
            .invoke_async(json!({
                "type": "ui_message",
                "payload": payload,
                "session_id": session_id,
                "archive": archive,
            }))
            .await?;
        if reply.is_error {
            bail!("{}", reply.content);
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(&reply.content) {
            return Ok(parsed);
        }
        Ok(json!({"content": reply.content, "task": reply.task}))
    }

    /// Panels an interface can offer, as `(plugin id, title, dynamic)`. The body
    /// still comes from [`Self::ui_panel`], so a static and a requested panel
    /// behave the same for the caller.
    pub fn ui_panels(&self) -> Vec<(String, String, bool)> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.enabled)
            .filter_map(|plugin| {
                let ui = plugin.manifest.ui.as_ref()?;
                if ui.panel_text.trim().is_empty() && !ui.dynamic_panel {
                    return None;
                }
                let title = if ui.panel_title.trim().is_empty() {
                    plugin.manifest.name.clone()
                } else {
                    ui.panel_title.clone()
                };
                Some((plugin.manifest.id.clone(), title, ui.dynamic_panel))
            })
            .collect()
    }

    fn panel_details(&self, id: &str) -> Result<(String, String, bool, Arc<PluginWorker>)> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == id && plugin.enabled)
            .ok_or_else(|| anyhow!("no enabled plugin '{id}'"))?;
        let ui = plugin
            .manifest
            .ui
            .as_ref()
            .context("plugin has no UI panel")?;
        let title = if ui.panel_title.is_empty() {
            plugin.manifest.name.clone()
        } else {
            ui.panel_title.clone()
        };
        Ok((
            title,
            ui.panel_text.clone(),
            ui.dynamic_panel,
            plugin.worker.clone(),
        ))
    }

    pub fn ui_panel(&self, id: &str) -> Result<String> {
        let (title, fallback, dynamic, worker) = self.panel_details(id)?;
        let content = if dynamic {
            let result = worker.invoke(json!({
                "type":"ui", "name":"panel", "arguments":{"id":id}
            }))?;
            if result.is_error {
                bail!("dynamic panel failed: {}", result.content);
            }
            result.content
        } else {
            fallback
        };
        Ok(render_panel(&title, &content))
    }

    /// The same panel with real structure, for a frontend that draws widgets.
    /// One schema serves both: the terminal uses the rendered text.
    pub async fn ui_panel_blocks_async(&self, id: &str) -> Result<Value> {
        let (title, fallback, dynamic, worker) = self.panel_details(id)?;
        let content = if dynamic {
            let result = worker
                .invoke_async(json!({
                    "type":"ui", "name":"panel", "arguments":{"id":id}
                }))
                .await?;
            if result.is_error {
                bail!("dynamic panel failed: {}", result.content);
            }
            result.content
        } else {
            fallback
        };
        Ok(panel_json(&title, &content))
    }

    /// The synchronous form for a blocking host; identical content.
    pub fn ui_panel_blocks(&self, id: &str) -> Result<Value> {
        let (title, fallback, dynamic, worker) = self.panel_details(id)?;
        let content = if dynamic {
            let result = worker.invoke(json!({
                "type":"ui", "name":"panel", "arguments":{"id":id}
            }))?;
            if result.is_error {
                bail!("dynamic panel failed: {}", result.content);
            }
            result.content
        } else {
            fallback
        };
        Ok(panel_json(&title, &content))
    }

    /// TUI requests are cancellable and must not block the input event loop.
    pub async fn ui_panel_async(&self, id: &str) -> Result<String> {
        let (title, fallback, dynamic, worker) = self.panel_details(id)?;
        let content = if dynamic {
            let result = worker
                .invoke_async(json!({
                    "type":"ui", "name":"panel", "arguments":{"id":id}
                }))
                .await?;
            if result.is_error {
                bail!("dynamic panel failed: {}", result.content);
            }
            result.content
        } else {
            fallback
        };
        Ok(render_panel(&title, &content))
    }

    /// The generic turn-end gate, if at least one enabled plugin declared the
    /// `before_turn_end` hook. `None` keeps the no-plugin path unchanged.
    pub fn turn_end_gate(&self) -> Option<TurnEndGate> {
        let workers = self
            .plugins
            .iter()
            .filter(|plugin| {
                plugin.enabled
                    && plugin
                        .manifest
                        .hooks
                        .iter()
                        .any(|hook| hook == "before_turn_end")
            })
            .map(|plugin| (plugin.manifest.id.clone(), plugin.worker.clone()))
            .collect::<Vec<_>>();
        (!workers.is_empty()).then_some(TurnEndGate { workers })
    }

    pub fn command_descriptions(&self) -> Vec<(String, String)> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.enabled)
            .flat_map(|plugin| {
                plugin.manifest.commands.iter().map(move |spec| {
                    (
                        spec.name.clone(),
                        if spec.description.is_empty() {
                            plugin.manifest.name.clone()
                        } else {
                            spec.description.clone()
                        },
                    )
                })
            })
            .collect()
    }

    pub fn register_commands(&self, registry: &mut CommandRegistry) -> Vec<String> {
        let catalog = self.clone();
        let mut warnings = Vec::new();
        if let Err(error) = registry.register("/plugins", move |_, _, args| {
            let parts: Vec<&str> = args.split_whitespace().collect();
            let result = match parts.as_slice() {
                [] | ["list"] => catalog.list(),
                ["enable", id] => catalog.set_enabled(id, true),
                ["disable", id] => catalog.set_enabled(id, false),
                ["install", path] => catalog.install_source(path),
                ["check", path] => catalog.check(Path::new(path)),
                ["uninstall", id] => catalog.uninstall(id),
                ["config", id] => catalog.show_config(id),
                ["view", id] => catalog.ui_panel(id),
                ["set", id, key, value @ ..] if !value.is_empty() => catalog.set_config(id, key, Some(&value.join(" "))),
                ["reset", id, key] => catalog.set_config(id, key, None),
                _ => bail!("Usage: /plugins [list|enable <id>|disable <id>|install <path|git-url>|check <path>|uninstall <id>|config <id>|set <id> <key> <value>|reset <id> <key>]"),
            }?;
            Ok(CommandReply::text(result))
        }) {
            warnings.push(format!("plugins command: {error:#}"));
        }
        for plugin in &self.plugins {
            if !plugin.enabled {
                continue;
            }
            for spec in &plugin.manifest.commands {
                let worker = plugin.worker.clone();
                let name = spec.name.clone();
                let label = format!("{}: {name}", plugin.manifest.id);
                let command_name = name.clone();
                if let Err(error) = registry.register_async_context(&name, move |args, session| {
                    let worker = worker.clone();
                    let name = command_name.clone();
                    async move {
                        let reply = worker
                            .invoke_async(json!({
                                "type": "command", "name": name, "arguments": args,
                                "workdir": session.workdir,
                                "archive": session.archive,
                                "session_id": session.session_id
                            }))
                            .await?;
                        if reply.is_error {
                            bail!("{}", reply.content);
                        }
                        // A command may also start one real task. The body is
                        // the user turn; the host never re-dispatches it as
                        // another command.
                        if let Some(body) = reply.task.as_deref() {
                            let body = body.trim();
                            if body.is_empty() {
                                bail!("{name}: the task body is empty");
                            }
                            return Ok(CommandReply {
                                output: reply.content,
                                action: CommandAction::Task(body.to_owned()),
                            });
                        }
                        Ok(CommandReply::text(reply.content))
                    }
                }) {
                    warnings.push(format!("{label}: {error:#}"));
                }
            }
        }
        warnings
    }

    pub fn cli(args: &[String]) -> Result<()> {
        let workdir = env::var_os("KROMA_WORKDIR")
            .map(PathBuf::from)
            .unwrap_or(env::current_dir()?);
        let catalog = Self::discover(&workdir)?;
        let output = match args {
            [] => catalog.list()?,
            [one] if one == "list" => catalog.list()?,
            [action, id] if action == "enable" => catalog.set_enabled(id, true)?,
            [action, id] if action == "disable" => catalog.set_enabled(id, false)?,
            [action, path] if action == "install" => catalog.install_source(path)?,
            [action, path] if action == "check" => catalog.check(Path::new(path))?,
            [action, id] if action == "uninstall" => catalog.uninstall(id)?,
            [action, id] if action == "config" => catalog.show_config(id)?,
            [action, id] if action == "view" => catalog.ui_panel(id)?,
            [action, id, key, value @ ..] if action == "set" && !value.is_empty() => catalog.set_config(id, key, Some(&value.join(" ")))?,
            [action, id, key] if action == "reset" => catalog.set_config(id, key, None)?,
            [action, id] if action == "new" => scaffold(id)?,
            _ => bail!("Usage: kroma plugins [list|enable <id>|disable <id>|install <path|git-url>|check <path>|new <id>|uninstall <id>|config <id>|set <id> <key> <value>|reset <id> <key>]"),
        };
        crate::cli_println!("{output}");
        Ok(())
    }
}

/// A source counts as remote when it is an explicit URL. A local path that
/// happens to contain dots is still a directory.
fn git_source(source: &str) -> Option<String> {
    let value = source.trim();
    let remote = value.starts_with("git@")
        || value.starts_with("ssh://")
        || value.starts_with("git://")
        || value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("file://");
    remote.then(|| value.to_owned())
}

/// A cloned repository may hold the plugin at its root or in exactly one
/// subdirectory. More than one candidate is reported instead of guessed.
fn find_manifest_directory(root: &Path) -> Result<PathBuf> {
    if root.join("plugin.toml").is_file() {
        return Ok(root.to_path_buf());
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() && path.join("plugin.toml").is_file() {
            candidates.push(path);
        }
    }
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => bail!(
            "no plugin.toml in the repository root or one level below it; install the plugin directory itself"
        ),
        _ => {
            candidates.sort();
            let names = candidates
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "the repository contains several plugins ({names}); install the one you want by its directory"
            )
        }
    }
}

fn copy_directory(source: &Path, target: &Path, boundary: Option<&Path>) -> Result<()> {
    // Copy symlink *targets* so plugin bundles from pnpm or local workspaces
    // are installable without elevated link-creation privileges on Windows.
    // Remote git installs enforce `boundary` so links cannot read host files
    // outside the cloned repository; local installs still guard against cycles.
    struct Budget {
        entries: usize,
        bytes: u64,
    }
    fn copy_tree(
        source: &Path,
        target: &Path,
        boundary: Option<&Path>,
        parents: &mut Vec<PathBuf>,
        budget: &mut Budget,
    ) -> Result<()> {
        let resolved = source.canonicalize()?;
        if let Some(boundary) = boundary {
            if crate::path_display::strip_prefix_equivalent(&resolved, boundary).is_none() {
                bail!(
                    "plugin directory symlink escapes checkout root: {}",
                    source.display()
                );
            }
        }
        if parents.contains(&resolved) {
            bail!("cyclic plugin directory link: {}", source.display());
        }
        budget.entries = budget.entries.saturating_add(1);
        if budget.entries > MAX_INSTALL_ENTRIES {
            bail!("plugin bundle exceeds {MAX_INSTALL_ENTRIES} entries");
        }
        parents.push(resolved);
        let result = (|| -> Result<()> {
            fs::create_dir(target)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                let from = entry.path();
                let to = target.join(entry.file_name());
                if from.is_dir() {
                    copy_tree(&from, &to, boundary, parents, budget)?;
                } else if from.is_file() {
                    if let Some(boundary) = boundary {
                        let resolved_file = from.canonicalize()?;
                        if crate::path_display::strip_prefix_equivalent(&resolved_file, boundary)
                            .is_none()
                        {
                            bail!(
                                "plugin file symlink escapes checkout root: {}",
                                from.display()
                            );
                        }
                    }
                    let size = from.metadata()?.len();
                    budget.bytes = budget.bytes.saturating_add(size);
                    if budget.bytes > MAX_INSTALL_BYTES {
                        bail!(
                            "plugin bundle exceeds {} MiB",
                            MAX_INSTALL_BYTES / (1024 * 1024)
                        );
                    }
                    fs::copy(&from, &to)?;
                } else {
                    bail!("unreadable plugin entry: {}", from.display());
                }
            }
            Ok(())
        })();
        parents.pop();
        if result.is_err() {
            let _ = fs::remove_dir_all(target);
        }
        result
    }
    copy_tree(
        source,
        target,
        boundary,
        &mut Vec::new(),
        &mut Budget {
            entries: 0,
            bytes: 0,
        },
    )
}

struct PluginToolAdapter {
    name: String,
    description: String,
    parameters: Value,
    streaming: bool,
    invalidates_context: bool,
    requires_user_context: bool,
    requires_user_attachments: bool,
    may_mutate_worktree: bool,
    timeout: Option<Duration>,
    data_dir: PathBuf,
    worker: Arc<PluginWorker>,
}

impl Tool for PluginToolAdapter {
    fn invalidates_model_context(&self) -> bool {
        self.invalidates_context
    }
    fn requires_user_attachments(&self) -> bool {
        self.requires_user_attachments
    }
    fn may_mutate_worktree(&self) -> bool {
        self.may_mutate_worktree
    }
    fn manages_timeout(&self) -> bool {
        // Plugin tools enforce their declared/admin deadline or follow the
        // documented unlimited policy; cancellation drops PluginCallGuard.
        true
    }
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> Value {
        json!({"type":"function", "function": {
            "name": self.name, "description": self.description, "parameters": self.parameters
        }})
    }

    fn execute<'a>(&'a self, call: &'a ToolCall, context: ToolContext<'a>) -> ToolFuture<'a> {
        let arguments: Result<Value> =
            serde_json::from_str(&call.function.arguments).map_err(Into::into);
        let worker = self.worker.clone();
        let name = self.name.clone();
        let streaming = self.streaming;
        let timeout = self.timeout;
        let data_dir = self.data_dir.clone();
        let workdir = context.workdir.to_path_buf();
        let archive = context.archive.to_path_buf();
        let model_connection = context.model_connection.cloned();
        // Explicitly declared opt-in only. This is a generic host proof for
        // plugins that must anchor a write to the actual current user turn.
        // A child runtime is driven by a parent model, so its user-role text is
        // machine-written and must never become a trusted human quote.
        let user_context = self
            .requires_user_context
            .then(|| {
                context.user_message.map(|message| {
                    // A quote is at most 4000 characters, so the bound only
                    // truncates genuinely long messages. Say so instead of
                    // reporting a missing quote when the tail was never sent.
                    let truncated = message.chars().count() > TRUSTED_USER_CHARS;
                    json!({
                        "message":message.chars().take(TRUSTED_USER_CHARS).collect::<String>(),
                        "turn":context.user_turn,
                        "truncated":truncated
                    })
                })
            })
            .flatten()
            .filter(|_| trusted_user_source_available());
        let user_attachments = self.requires_user_attachments.then(|| {
            let capture = |items: Option<&[Value]>, prefix: &str| {
                items
                    .into_iter()
                    .flatten()
                    .filter(|part| part["type"] == "kroma_attachment")
                    .enumerate()
                    .filter(|(_, part)| part["kind"] == "image")
                    .map(|(index, part)| {
                        json!({
                            "ref": format!("{prefix}:{}", index + 1),
                            "name": part["name"], "path": part["path"],
                            "mime": part["mime"], "bytes": part["bytes"]
                        })
                    })
                    .collect::<Vec<_>>()
            };
            let mut images = capture(context.user_images, "attachment");
            images.extend(capture(context.previous_user_images, "previous"));
            images
        });
        let call_id = call.id.clone();
        Box::pin(async move {
            let Ok(arguments) = arguments else {
                return ToolResult::error(format!(
                    "invalid plugin tool arguments: {}",
                    arguments.unwrap_err()
                ));
            };
            let payload = json!({"type":if streaming {"tool_stream"} else {"tool"},
                "name":name, "arguments":arguments, "workdir":workdir,
                "archive":archive, "call_id":call_id.clone(), "model_connection":model_connection,
                "trusted_user_context":user_context,
                "trusted_user_attachments":user_attachments});
            if streaming {
                let mut stream = worker.start_stream(payload);
                let consumed = async {
                    let mut completed = None;
                    let mut failure = None;
                    while let Some(frame) = stream.next().await {
                        match frame["event"].as_str() {
                            Some("output_delta") => {
                                let channel = frame["stream"].as_str().unwrap_or("stdout");
                                let delta = frame["delta"].as_str().unwrap_or("");
                                if let Some(output) = context.on_output {
                                    output(channel, delta);
                                }
                            }
                            Some("complete") => {
                                completed = Some(serde_json::from_value::<PluginResponse>(
                                    frame["result"].clone(),
                                ));
                                break;
                            }
                            Some("error") => {
                                failure = Some(ToolResult::error(
                                    frame["message"].as_str().unwrap_or("plugin stream failed"),
                                ));
                                break;
                            }
                            _ => {
                                failure =
                                    Some(ToolResult::error("unsupported plugin tool stream frame"));
                                break;
                            }
                        }
                    }
                    // Join the worker before discarding it: the process lock is
                    // released here, so teardown stays off the async hot path.
                    (stream.finish().await, completed, failure)
                };
                // The total deadline covers waiting, streaming and progress
                // frames alike: heartbeats never extend it.
                let (finished, completed, failure) = match timeout {
                    None => consumed.await,
                    Some(limit) => match tokio::time::timeout(limit, consumed).await {
                        Ok(outcome) => outcome,
                        Err(_) => {
                            worker.discard_process_async();
                            return tool_timeout_error(&name, limit, &data_dir, &call_id);
                        }
                    },
                };
                // A fatal stream outcome discards the worker; a normal result,
                // including a task-level is_error, keeps it reusable.
                if let Some(result) = failure {
                    worker.discard_process();
                    return result;
                }
                return match finished {
                    Err(error) => {
                        worker.discard_process();
                        ToolResult::error(format!("plugin stream failed: {error:#}"))
                    }
                    Ok(()) => match completed {
                        Some(Ok(reply)) if !reply.is_error => {
                            plugin_tool_projection(&archive, &reply.content, false)
                        }
                        Some(Ok(reply)) => plugin_tool_projection(&archive, &reply.content, true),
                        Some(Err(error)) => {
                            worker.discard_process();
                            ToolResult::error(format!("invalid tool completion: {error:#}"))
                        }
                        None => {
                            worker.discard_process();
                            ToolResult::error("plugin stream ended without completion")
                        }
                    },
                };
            }
            // Tool calls can legitimately exceed two minutes (e.g. a coding
            // subagent). The adapter owns the declared deadline; dropping the
            // future still tears down the worker via its guard.
            let reply = match timeout {
                None => worker.invoke_async_tool(payload).await,
                Some(limit) => {
                    match tokio::time::timeout(limit, worker.invoke_async_tool(payload)).await {
                        Ok(reply) => reply,
                        Err(_) => return tool_timeout_error(&name, limit, &data_dir, &call_id),
                    }
                }
            };
            match reply {
                Ok(response) if !response.is_error => {
                    plugin_tool_projection(&archive, &response.content, false)
                }
                Ok(response) => plugin_tool_projection(&archive, &response.content, true),
                Err(error) => ToolResult::error(format!("plugin execution failed: {error:#}")),
            }
        })
    }
}

/// Adapters return facts, not previews. The Agent archives the final post-hook
/// result, retaining its raw content in the WAL.
fn plugin_tool_projection(_archive: &Path, content: &str, is_error: bool) -> ToolResult {
    if is_error {
        ToolResult::error(content)
    } else {
        ToolResult::ok(content)
    }
}

fn tool_timeout_error(name: &str, limit: Duration, data_dir: &Path, call_id: &str) -> ToolResult {
    ToolResult::error(format!(
        "plugin tool '{name}' (call {call_id}) timed out after {}s; no result was returned, late output is discarded, and partial side effects may remain under {}",
        limit.as_secs(),
        data_dir.display()
    ))
}

/// Bounded, host-derived facts a turn-end gate may read. Nothing here is a
/// user instruction and nothing here grants permission.
pub struct TurnEndFacts {
    pub session_id: String,
    pub archive: String,
    pub workdir: String,
    /// The model answer that would end the turn.
    pub final_text: String,
    /// `[{"name":..,"arguments":..}]` for this turn's calls, host-truncated.
    pub tools: Value,
}

/// One plugin's refusal to end the turn, plus the bounded feedback the model
/// receives as the next step.
#[derive(Debug, Clone)]
pub struct TurnEndAdvice {
    pub plugin: String,
    pub feedback: String,
}

/// Every enabled plugin that declared `before_turn_end`, in enablement order.
/// A plugin answers with `{"decision":"finish"}` or
/// `{"decision":"continue","feedback":"..."}`; the first `continue` wins.
pub struct TurnEndGate {
    workers: Vec<(String, Arc<PluginWorker>)>,
}

impl TurnEndGate {
    /// Fail-open on purpose: a broken, slow or unparsable advisory plugin can
    /// never trap the user in a turn that does not end.
    pub async fn gate(&self, facts: &TurnEndFacts) -> Option<TurnEndAdvice> {
        for (id, worker) in &self.workers {
            let request = json!({
                "type": "hook",
                "name": "before_turn_end",
                "arguments": {
                    "final_text": facts.final_text,
                    "tools": facts.tools,
                },
                "session_id": facts.session_id,
                "archive": facts.archive,
                "workdir": facts.workdir,
            });
            let reply = match worker.clone().invoke_async(request).await {
                Ok(reply) if !reply.is_error => reply,
                Ok(reply) => {
                    crate::runtime_log!(
                        "[plugin] {id}: before_turn_end returned an error: {}",
                        reply.content
                    );
                    continue;
                }
                Err(error) => {
                    crate::runtime_log!("[plugin] {id}: before_turn_end skipped: {error:#}");
                    continue;
                }
            };
            let Ok(decision) = serde_json::from_str::<Value>(&reply.content) else {
                crate::runtime_log!("[plugin] {id}: before_turn_end must return a JSON object");
                continue;
            };
            match decision.get("decision").and_then(Value::as_str) {
                Some("finish") => {}
                Some("continue") => {
                    let feedback = decision
                        .get("feedback")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    if feedback.is_empty() {
                        crate::runtime_log!(
                            "[plugin] {id}: before_turn_end asked to continue without feedback; ignored"
                        );
                        continue;
                    }
                    return Some(TurnEndAdvice {
                        plugin: id.clone(),
                        feedback,
                    });
                }
                other => crate::runtime_log!(
                    "[plugin] {id}: before_turn_end returned unknown decision {other:?}; treated as finish"
                ),
            }
        }
        None
    }
}

struct PluginToolHooks {
    before: bool,
    after: bool,
    worker: Arc<PluginWorker>,
}

impl ToolInterceptor for PluginToolHooks {
    fn before_with_context<'a>(
        &'a self,
        call: &'a mut ToolCall,
        ctx: ToolContext<'a>,
    ) -> HookFuture<'a> {
        Box::pin(async move {
            if !self.before {
                return Ok(());
            }
            let worker = self.worker.clone();
            let snapshot = json!({"type":"hook","name":"before_tool_call",
                "arguments":{"name":call.function.name,"arguments":serde_json::from_str::<Value>(&call.function.arguments)?},
                "call_id":call.id, "workdir":ctx.workdir, "archive":ctx.archive});
            let reply = worker.invoke_async(snapshot).await?;
            if reply.is_error {
                bail!("plugin rejected tool call: {}", reply.content);
            }
            let patch: Value = serde_json::from_str(&reply.content)
                .context("before_tool_call must return a JSON patch object")?;
            if let Some(arguments) = patch.get("arguments") {
                if !arguments.is_object() {
                    bail!("before_tool_call arguments must be a JSON object");
                }
                call.function.arguments = arguments.to_string();
            }
            Ok(())
        })
    }

    fn after_with_context<'a>(
        &'a self,
        call: &'a ToolCall,
        result: &'a mut ToolResult,
        ctx: ToolContext<'a>,
    ) -> HookFuture<'a> {
        Box::pin(async move {
            if !self.after {
                return Ok(());
            }
            let worker = self.worker.clone();
            let snapshot = json!({"type":"hook","name":"after_tool_result",
                "arguments":{"name":call.function.name,"content":result.content,"is_error":result.is_error},
                "call_id":call.id, "workdir":ctx.workdir, "archive":ctx.archive});
            let reply = worker.invoke_async(snapshot).await?;
            if reply.is_error {
                bail!("plugin rejected tool result: {}", reply.content);
            }
            let patch: Value = serde_json::from_str(&reply.content)
                .context("after_tool_result must return a JSON patch object")?;
            if let Some(content) = patch.get("content") {
                result.content = content
                    .as_str()
                    .context("result.content must be a string")?
                    .to_owned();
            }
            if let Some(is_error) = patch.get("is_error") {
                result.is_error = is_error
                    .as_bool()
                    .context("result.is_error must be boolean")?;
            }
            Ok(())
        })
    }
}

struct PluginEventSubscription {
    events: BTreeSet<String>,
    sender: SyncSender<Value>,
    dropped: AtomicU64,
    stopped: Arc<AtomicBool>,
    worker: Arc<PluginWorker>,
}

struct PluginEventSink {
    host: Option<Arc<dyn EventSink>>,
    listeners: Vec<PluginEventSubscription>,
    session: RwLock<Option<(String, PathBuf)>>,
}

impl Drop for PluginEventSink {
    fn drop(&mut self) {
        for subscription in &self.listeners {
            subscription.stopped.store(true, Ordering::SeqCst);
            let worker = subscription.worker.clone();
            // Kill a stalled event handler off the UI thread. The worker's
            // active lock prevents killing a different, subsequent request.
            std::thread::spawn(move || worker.cancel_active_request());
        }
    }
}

impl EventSink for PluginEventSink {
    fn set_session_context(&self, id: &str, archive: &Path) {
        if let Ok(mut session) = self.session.write() {
            *session = Some((id.to_owned(), archive.to_path_buf()));
        }
        if let Some(host) = &self.host {
            host.set_session_context(id, archive);
        }
    }

    fn on_event(&self, event: RuntimeEvent<'_>) {
        if let Some(name) = plugin_event_name(&event) {
            if self
                .listeners
                .iter()
                .any(|subscription| subscription.events.contains(name))
            {
                let (session_id, archive) = self
                    .session
                    .read()
                    .ok()
                    .and_then(|s| s.clone())
                    .map_or((None, None), |(id, path)| (Some(id), Some(path)));
                let payload = json!({"type":"event", "name":name,
                    "session_id":session_id,"archive":archive,
                    "arguments":plugin_event_fields(&event)});
                for subscriber in &self.listeners {
                    if subscriber.events.contains(name) {
                        // A slow listener drops only its own notifications;
                        // tools, transcripts, and other plugins stay live.
                        match subscriber.sender.try_send(payload.clone()) {
                            Ok(()) => {}
                            Err(mpsc::TrySendError::Full(_)) => {
                                let count = subscriber.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                                if count.is_power_of_two() {
                                    crate::runtime_log!("[plugin] event subscriber queue full: {count} event(s) dropped");
                                }
                            }
                            Err(mpsc::TrySendError::Disconnected(_)) => {
                                if !subscriber.stopped.swap(true, Ordering::Relaxed) {
                                    crate::runtime_log!("[plugin] event subscriber disconnected");
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(host) = &self.host {
            host.on_event(event);
        }
    }
}

/// Streaming token deltas/tool-output chunks are intentionally excluded.
/// Other events are stable enough for low-cost async subscribers.
fn plugin_event_name(event: &RuntimeEvent<'_>) -> Option<&'static str> {
    Some(match event {
        RuntimeEvent::TaskStarted => "task_started",
        RuntimeEvent::CompactionStarted => "compaction_started",
        RuntimeEvent::TaskFinished { .. } => "task_finished",
        RuntimeEvent::ContextPrepared { .. } => "context_prepared",
        RuntimeEvent::ModelStarted { .. } => "model_started",
        RuntimeEvent::ModelFinished { .. } => "model_finished",
        RuntimeEvent::ModelFailed => "model_failed",
        RuntimeEvent::FailedRequestUsage { .. } | RuntimeEvent::Warning { .. } => return None,
        RuntimeEvent::CompactionUsage { .. } => "compaction_usage",
        RuntimeEvent::TurnGateContinued { .. } => "turn_gate_continued",
        RuntimeEvent::ToolStarted { .. } => "tool_started",
        RuntimeEvent::ToolFinished { .. } => "tool_finished",
        RuntimeEvent::SessionCreated { .. } => "session_created",
        RuntimeEvent::SessionSwitched { .. } => "session_switched",
        RuntimeEvent::SessionCleared { .. } => "session_cleared",
        RuntimeEvent::SessionForked { .. } => "session_forked",
        RuntimeEvent::SessionRewound { .. } => "session_rewound",
        RuntimeEvent::TextDelta(_)
        | RuntimeEvent::ReasoningDelta(_)
        | RuntimeEvent::ToolOutput { .. }
        | RuntimeEvent::FinalAnswerReady => return None,
    })
}

fn plugin_event_fields(event: &RuntimeEvent<'_>) -> Value {
    match event {
        RuntimeEvent::TaskStarted
        | RuntimeEvent::CompactionStarted
        | RuntimeEvent::ModelFailed
        | RuntimeEvent::FailedRequestUsage { .. } => json!({}),
        RuntimeEvent::Warning { message } => json!({"message":message}),
        RuntimeEvent::TaskFinished { status } => json!({"status":status}),
        RuntimeEvent::ContextPrepared {
            messages,
            estimated_input_tokens,
            available_input_tokens,
        } => json!({
            "messages":messages, "estimated_input_tokens":estimated_input_tokens,
            "available_input_tokens":available_input_tokens
        }),
        RuntimeEvent::ModelStarted { request } => json!({"request":request}),
        RuntimeEvent::ModelFinished {
            request,
            tools,
            reasoning,
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cached_tokens,
            cache_write_tokens,
            cache_write_1h_tokens,
        } => json!({
            "request":request, "tools":tools, "reasoning":reasoning, "input_tokens":input_tokens,
            "output_tokens":output_tokens, "reasoning_tokens":reasoning_tokens, "cached_tokens":cached_tokens,
            "cache_write_tokens":cache_write_tokens, "cache_write_1h_tokens":cache_write_1h_tokens
        }),
        RuntimeEvent::CompactionUsage {
            requests,
            input_tokens,
            output_tokens,
            cached_tokens,
            cache_write_tokens,
        } => json!({
            "requests":requests, "input_tokens":input_tokens,
            "output_tokens":output_tokens, "cached_tokens":cached_tokens,
            "cache_write_tokens":cache_write_tokens
        }),
        RuntimeEvent::TurnGateContinued { plugin, feedback } => {
            json!({"plugin":plugin, "feedback":feedback})
        }
        RuntimeEvent::ToolStarted {
            name,
            id,
            arguments,
        } => json!({
            "name":name, "id":id, "arguments":arguments
        }),
        RuntimeEvent::ToolFinished {
            name,
            id,
            result,
            is_error,
        } => json!({
            "name":name, "id":id, "result":result, "is_error":is_error
        }),
        RuntimeEvent::SessionCreated { id } | RuntimeEvent::SessionCleared { id } => {
            json!({"id":id})
        }
        RuntimeEvent::SessionSwitched { from, to } => json!({"from":from, "to":to}),
        RuntimeEvent::SessionForked {
            from,
            to,
            checkpoint,
        } => json!({
            "from":from, "to":to, "checkpoint":checkpoint
        }),
        RuntimeEvent::SessionRewound {
            id,
            saved_future,
            checkpoint,
        } => json!({
            "id":id, "saved_future":saved_future, "checkpoint":checkpoint
        }),
        RuntimeEvent::TextDelta(_)
        | RuntimeEvent::ReasoningDelta(_)
        | RuntimeEvent::ToolOutput { .. }
        | RuntimeEvent::FinalAnswerReady => json!({}),
    }
}

/// True when this process can prove that the current user message was written
/// by a human. A child task is spawned with a parent-model prompt and sets
/// `KROMA_CHILD_SKIP_TASK_SUMMARY=1`; no host-verified human source exists
/// there, so a "trusted user quote" from it would promote generated text into a
/// durable rule.
fn trusted_user_source_available() -> bool {
    trusted_user_source_allowed(
        std::env::var("KROMA_CHILD_SKIP_TASK_SUMMARY")
            .ok()
            .as_deref(),
    )
}

fn trusted_user_source_allowed(child_skip_task_summary: Option<&str>) -> bool {
    child_skip_task_summary != Some("1")
}

/// What Git currently says about one project plugin data directory.
enum PluginDataVisibility {
    /// No Git work tree, or no usable `git` binary to ask.
    Unverifiable,
    Ignored,
    NotIgnored,
    Tracked,
}

fn git_output(workdir: &Path, args: &[&str]) -> Option<std::process::Output> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(workdir)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.output().ok()
}

fn plugin_data_visibility(workdir: &Path, data_dir: &Path) -> PluginDataVisibility {
    let Ok(relative) = data_dir.strip_prefix(workdir) else {
        return PluginDataVisibility::Unverifiable;
    };
    let relative = relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    match git_output(workdir, &["rev-parse", "--git-dir"]) {
        Some(output) if output.status.success() => {}
        _ => return PluginDataVisibility::Unverifiable,
    }
    // The host's own ignore file may be versioned on purpose; anything else
    // stored here is private plugin data.
    if git_output(workdir, &["ls-files", "--", &relative]).is_some_and(|output| {
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .any(|line| !line.is_empty() && !line.ends_with(".gitignore"))
    }) {
        return PluginDataVisibility::Tracked;
    }
    // Probe a child path: a nested `.gitignore` covers the directory contents,
    // never the directory entry itself.
    let probe = format!("{relative}/host-data-probe");
    match git_output(workdir, &["check-ignore", "-q", "--", &probe]) {
        Some(output) if output.status.success() => PluginDataVisibility::Ignored,
        Some(_) => PluginDataVisibility::NotIgnored,
        None => PluginDataVisibility::Unverifiable,
    }
}

/// Protect project plugin data before a lazy worker writes it. The host adds
/// one narrowly scoped `.gitignore` inside the plugin's own data directory and
/// never rewrites the user's existing ignore files; it then asks Git instead of
/// claiming the data is ignored. Already tracked data is refused rather than
/// silently committed with the next `git add`.
fn protect_project_plugin_data(workdir: &Path, data_dir: &Path) -> Result<()> {
    let root = workdir.join(".kroma").join("plugin-data");
    if !data_dir.starts_with(&root) {
        // User-global plugin data does not live in a repository.
        return Ok(());
    }
    let ignore = data_dir.join(".gitignore");
    if !ignore.exists() {
        if let Err(error) = fs::write(&ignore, "*\n!/.gitignore\n") {
            crate::runtime_log!("[plugin] could not write {}: {error}", ignore.display());
        }
    }
    match plugin_data_visibility(workdir, data_dir) {
        PluginDataVisibility::Tracked => bail!(
            "plugin data in {} is tracked by Git; untrack it (git rm --cached) or choose another workdir before private plugin storage writes there",
            data_dir.display()
        ),
        PluginDataVisibility::NotIgnored => crate::runtime_log!(
            "[plugin] {} is not Git-ignored; a user Git rule overrides the plugin-data default and private data may be committed",
            data_dir.display()
        ),
        PluginDataVisibility::Ignored | PluginDataVisibility::Unverifiable => {}
    }
    Ok(())
}

fn sync_ui_bus(plugins: &[Plugin], ui_bus: &PluginUiBus) {
    let enabled: Vec<(String, Option<String>)> = plugins
        .iter()
        .filter(|plugin| plugin.enabled)
        .map(|plugin| {
            (
                plugin.manifest.id.clone(),
                plugin.manifest.ui.as_ref().map(|ui| ui.status.clone()),
            )
        })
        .collect();
    ui_bus.sync_manifest(&enabled);
}

struct PluginWorker {
    directory: PathBuf,
    workdir: PathBuf,
    data_dir: PathBuf,
    /// Manifest protocol version; every envelope and frame for this worker
    /// uses it so V1 plugins stay byte-compatible.
    protocol_version: u32,
    command: Option<String>,
    args: Vec<String>,
    settings: Value,
    /// Content approval is independent of the worker process lifecycle.
    scan: PluginCodeScan,
    process: Mutex<Option<PluginProcess>>,
    id: String,
    next_request: AtomicU64,
    active: Mutex<Option<(u64, u32)>>,
    /// PID already terminated by a cancellation guard. The process object
    /// must not send the whole tree a second teardown signal.
    terminated_pid: Arc<AtomicU32>,
    ui_bus: Arc<PluginUiBus>,
}

impl PluginWorker {
    fn isolated(&self) -> Self {
        Self {
            directory: self.directory.clone(),
            workdir: self.workdir.clone(),
            data_dir: self.data_dir.clone(),
            protocol_version: self.protocol_version,
            command: self.command.clone(),
            args: self.args.clone(),
            settings: self.settings.clone(),
            scan: self.scan.clone(),
            process: Mutex::new(None),
            id: self.id.clone(),
            next_request: AtomicU64::new(1),
            active: Mutex::new(None),
            terminated_pid: Arc::clone(&self.terminated_pid),
            ui_bus: Arc::clone(&self.ui_bus),
        }
    }

    fn with_ui_bus(&self, ui_bus: Arc<PluginUiBus>) -> Self {
        Self {
            directory: self.directory.clone(),
            workdir: self.workdir.clone(),
            data_dir: self.data_dir.clone(),
            protocol_version: self.protocol_version,
            command: self.command.clone(),
            args: self.args.clone(),
            settings: self.settings.clone(),
            scan: self.scan.clone(),
            process: Mutex::new(None),
            id: self.id.clone(),
            next_request: AtomicU64::new(1),
            active: Mutex::new(None),
            terminated_pid: Arc::new(AtomicU32::new(0)),
            ui_bus,
        }
    }

    /// Worker description from manifest and settings metadata only. Used while
    /// a plugin is not requested, so disabled discovery never reads plugin
    /// code. The scan error keeps such a worker from ever being mistaken for a
    /// complete, approved definition.
    fn unscanned(
        dir: &Path,
        workdir: &Path,
        manifest: &PluginManifest,
        settings: Value,
        data_dir: PathBuf,
        ui_bus: Arc<PluginUiBus>,
    ) -> Self {
        Self::with_scan(
            dir,
            workdir,
            manifest,
            settings,
            data_dir,
            PluginCodeScan {
                entrypoints: Vec::new(),
                error: Some("its code was not scanned because the plugin is not enabled".into()),
            },
            ui_bus,
        )
    }

    /// Worker with a complete bounded scan of the plugin's local code.
    fn new(
        dir: &Path,
        workdir: &Path,
        manifest: &PluginManifest,
        settings: Value,
        data_dir: PathBuf,
        ui_bus: Arc<PluginUiBus>,
    ) -> Self {
        let scan = approval::scan(
            dir,
            manifest.command.as_deref(),
            &manifest.args,
            &PLUGIN_SCAN_LIMITS,
        );
        Self::with_scan(dir, workdir, manifest, settings, data_dir, scan, ui_bus)
    }

    fn with_scan(
        dir: &Path,
        workdir: &Path,
        manifest: &PluginManifest,
        settings: Value,
        data_dir: PathBuf,
        scan: PluginCodeScan,
        ui_bus: Arc<PluginUiBus>,
    ) -> Self {
        Self {
            directory: dir.to_path_buf(),
            workdir: workdir.to_path_buf(),
            data_dir,
            protocol_version: manifest.api_version,
            command: manifest.command.clone(),
            args: manifest.args.clone(),
            settings,
            scan,
            process: Mutex::new(None),
            id: manifest.id.clone(),
            next_request: AtomicU64::new(1),
            active: Mutex::new(None),
            terminated_pid: Arc::new(AtomicU32::new(0)),
            ui_bus,
        }
    }

    fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    fn same_definition(&self, next: &Self) -> bool {
        self.directory == next.directory
            && self.workdir == next.workdir
            && self.data_dir == next.data_dir
            && self.protocol_version == next.protocol_version
            && self.command == next.command
            && self.args == next.args
            && self.settings == next.settings
            && self.scan.same_content(&next.scan)
    }

    async fn invoke_async(self: &Arc<Self>, message: Value) -> Result<PluginResponse> {
        self.invoke_async_with_timeout(message, Some(PLUGIN_CALL_TIMEOUT))
            .await
    }

    /// Ordinary plugin tools are unbounded here: the tool adapter applies the
    /// manifest or administrator deadline so streaming and non-streaming calls
    /// share one rule.
    async fn invoke_async_tool(self: &Arc<Self>, message: Value) -> Result<PluginResponse> {
        self.invoke_async_with_timeout(message, None).await
    }

    async fn invoke_async_timeout(
        self: &Arc<Self>,
        message: Value,
        timeout: Duration,
    ) -> Result<PluginResponse> {
        self.invoke_async_with_timeout(message, Some(timeout)).await
    }

    async fn invoke_async_with_timeout(
        self: &Arc<Self>,
        message: Value,
        timeout: Option<Duration>,
    ) -> Result<PluginResponse> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut guard = PluginCallGuard {
            worker: self.clone(),
            id,
            cancelled: cancelled.clone(),
            armed: true,
        };
        let worker = self.clone();
        let task = tokio::task::spawn_blocking(move || {
            worker.invoke_inner(message, Some((id, &cancelled)))
        });
        let outcome = match timeout {
            Some(timeout) => tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| anyhow!("plugin call timed out after {}s", timeout.as_secs()))?,
            None => task.await,
        };
        // On panic/cancellation keep the guard armed; killing the process
        // prevents a stale JSONL response contaminating a later request.
        let response = outcome??;
        guard.armed = false;
        Ok(response)
    }

    /// Drop the current child so the next call starts a fresh worker. A
    /// fatal stream outcome means the process state can no longer be trusted.
    /// Only call this when no request holds the process lock.
    ///
    /// Dropping a `PluginProcess` runs the blocking whole-tree teardown, so the
    /// detached process is released on its own thread, never on the caller.
    fn discard_process(&self) {
        let Ok(mut process) = self.process.lock() else {
            return;
        };
        let Some(detached) = process.take() else {
            return;
        };
        std::thread::spawn(move || drop(detached));
    }

    /// For paths that drop a request mid-flight: wait for the process lock on a
    /// separate thread so the async caller is never blocked.
    fn discard_process_async(self: &Arc<Self>) {
        let worker = self.clone();
        std::thread::spawn(move || worker.discard_process());
    }

    fn cancel_active_request(&self) {
        if let Ok(active) = self.active.lock() {
            if let Some((_, pid)) = *active {
                self.terminated_pid.store(pid, Ordering::SeqCst);
                terminate_plugin_async(pid);
            }
        }
    }

    fn cancel_request(&self, id: u64) {
        // Publish the request and its PID under one lock to avoid racing
        // cancellation against the two individual atomic stores.
        if let Ok(active) = self.active.lock() {
            if let Some((request, pid)) = *active {
                if request == id {
                    self.terminated_pid.store(pid, Ordering::SeqCst);
                    terminate_plugin_async(pid);
                }
            }
        }
    }

    fn invoke(&self, message: Value) -> Result<PluginResponse> {
        self.invoke_inner(message, None)
    }

    fn invoke_inner(
        &self,
        message: Value,
        cancellation: Option<(u64, &AtomicBool)>,
    ) -> Result<PluginResponse> {
        self.invoke_with(message, cancellation, PluginProcess::request)
    }

    fn invoke_with<R>(
        &self,
        mut message: Value,
        cancellation: Option<(u64, &AtomicBool)>,
        execute: impl FnOnce(&mut PluginProcess, Value) -> Result<R>,
    ) -> Result<R> {
        let id = cancellation
            .map(|(id, _)| id)
            .unwrap_or_else(|| self.next_request.fetch_add(1, Ordering::Relaxed));
        let request = message.as_object_mut().context("invalid plugin request")?;
        // Extra envelope fields are ignored by legacy V1 JSONL plugins.
        // New plugins may echo request_id/version for response correlation.
        request.insert("protocol_version".into(), json!(self.protocol_version));
        request.insert("request_id".into(), json!(id));
        request.insert("config".into(), self.settings.clone());
        request.insert("data_dir".into(), json!(self.data_dir));
        request
            .entry("workdir".to_owned())
            .or_insert_with(|| json!(self.workdir));
        let mut process = self
            .process
            .lock()
            .map_err(|_| anyhow!("plugin worker lock poisoned"))?;
        if cancellation.is_some_and(|(_, cancelled)| cancelled.load(Ordering::SeqCst)) {
            bail!("plugin request cancelled before execution");
        }
        if process.is_none() {
            // No persistent data directory or disk IO for disabled/unused
            // plugins. Keep stored plugin data separate from installed code,
            // and make it private before the plugin can write into it.
            fs::create_dir_all(&self.data_dir).with_context(|| {
                format!("creating plugin data directory {}", self.data_dir.display())
            })?;
            protect_project_plugin_data(&self.workdir, &self.data_dir)?;
            let command = self
                .command
                .as_deref()
                .context("plugin has no executable command")?;
            let mut builder = Command::new(command);
            builder
                .args(&self.args)
                .current_dir(&self.directory)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                // Desktop has no console. A console-subsystem plugin runner
                // (e.g. node.exe) would otherwise open a blank terminal for
                // every lazy worker start. Its piped JSONL/stdout/stderr are
                // unaffected by hiding the Windows console window.
                builder.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            if plugin_env_isolation() {
                for (key, _) in env::vars_os() {
                    if plugin_env_secret(&key.to_string_lossy()) {
                        builder.env_remove(key);
                    }
                }
            }
            #[cfg(unix)]
            unsafe {
                use std::os::unix::process::CommandExt;
                builder.pre_exec(|| {
                    if libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
            let mut child = builder
                .spawn()
                .with_context(|| format!("starting plugin command '{command}'"))?;
            let stdin = child.stdin.take().context("plugin stdin unavailable")?;
            let stdout = child.stdout.take().context("plugin stdout unavailable")?;
            if let Some(stderr) = child.stderr.take() {
                let plugin_name = self
                    .directory
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                // Drain stderr so verbose plugins cannot deadlock on a full
                // pipe. Bound diagnostic volume; stdout remains JSONL only.
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut stderr = stderr;
                    let mut buffer = [0u8; 2048];
                    let mut remaining_log_bytes = 64 * 1024;
                    loop {
                        match stderr.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let logged = n.min(remaining_log_bytes);
                                if logged > 0 {
                                    crate::runtime_log!(
                                        "[plugin:{plugin_name}] {}",
                                        String::from_utf8_lossy(&buffer[..logged]).trim_end()
                                    );
                                    remaining_log_bytes -= logged;
                                }
                                // Continue draining even after logs are capped.
                            }
                        }
                    }
                });
            }
            // Forget any earlier cancellation PID before recording the new
            // process; a recycled PID must not suppress its own cleanup.
            self.terminated_pid.store(0, Ordering::SeqCst);
            *process = Some(PluginProcess {
                child,
                stdin,
                stdout: BufReader::new(stdout),
                name: self
                    .directory
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                protocol_version: self.protocol_version,
                terminated_pid: Arc::clone(&self.terminated_pid),
                plugin_id: self.id.clone(),
                ui_bus: Arc::clone(&self.ui_bus),
            });
        }
        if cancellation.is_some_and(|(_, cancelled)| cancelled.load(Ordering::SeqCst)) {
            *process = None;
            bail!("plugin request cancelled before execution");
        }
        if let Some((id, cancelled)) = cancellation {
            let mut active = self
                .active
                .lock()
                .map_err(|_| anyhow!("plugin cancellation lock poisoned"))?;
            if cancelled.load(Ordering::SeqCst) {
                *process = None;
                bail!("plugin request cancelled before execution");
            }
            *active = Some((id, process.as_ref().unwrap().child.id()));
        }
        let result = execute(process.as_mut().expect("plugin process created"), message);
        if cancellation.is_some() {
            *self
                .active
                .lock()
                .map_err(|_| anyhow!("plugin cancellation lock poisoned"))? = None;
        }
        if result.is_err() {
            *process = None;
        }
        result
    }
}

struct PluginCallGuard {
    worker: Arc<PluginWorker>,
    id: u64,
    cancelled: Arc<AtomicBool>,
    armed: bool,
}
impl Drop for PluginCallGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::SeqCst);
            self.worker.cancel_request(self.id);
        }
    }
}

fn terminate_plugin(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Kill descendants, too (notably subagents launched by a Node worker).
        let _ = Command::new("taskkill.exe")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    unsafe {
        // A child Kroma shares the plugin worker's process group, but its
        // Shell owns a separate group. Deliver SIGINT first so Kroma can
        // drop its tool future and terminate that Shell group itself.
        // Force-kill the plugin group after a bounded cleanup opportunity.
        let _ = libc::kill(-(pid as i32), libc::SIGINT);
        std::thread::sleep(Duration::from_millis(600));
        let _ = libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// Cancellation can run on an async/UI thread. Do the potentially blocking
/// whole-tree teardown (taskkill / the Unix SIGKILL grace period) off-thread.
fn terminate_plugin_async(pid: u32) {
    std::thread::spawn(move || terminate_plugin(pid));
}

struct PluginProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Plugin id, so a protocol failure names the plugin that caused it.
    name: String,
    /// Worker protocol version echoed by this process.s responses.
    protocol_version: u32,
    terminated_pid: Arc<AtomicU32>,
    /// Plugin id used as the key for per-catalog UI state (`ui.status`, `ui.widget`, ...).
    plugin_id: String,
    ui_bus: Arc<PluginUiBus>,
}

#[derive(Deserialize)]
struct PluginResponse {
    content: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    code: Option<String>,
    /// Optional command action: the plugin supplied a user-turn body that the
    /// host must run as the next task. Ignored for tool and hook replies.
    #[serde(default)]
    task: Option<String>,
}

impl PluginProcess {
    fn write_request(&mut self, request: &Value) -> Result<()> {
        self.stdin
            .write_all(serde_json::to_string(request)?.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_frame(&mut self) -> Result<Value> {
        const MAX_RESPONSE: usize = 2 * 1024 * 1024;
        let mut response = Vec::new();
        loop {
            let available = self.stdout.fill_buf()?;
            if available.is_empty() {
                bail!("plugin exited without a complete JSON response");
            }
            let take = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |at| at + 1);
            if response.len().saturating_add(take) > MAX_RESPONSE {
                bail!("plugin response exceeds 2 MiB; write large outputs to files");
            }
            let done = available[take - 1] == b'\n';
            response.extend_from_slice(&available[..take]);
            self.stdout.consume(take);
            if done {
                break;
            }
        }
        serde_json::from_slice(&response).context("invalid plugin JSON-line response")
    }

    fn validate_frame(
        &self,
        parsed: &Value,
        request_id: Option<&Value>,
        strict: bool,
        protocol_version: u32,
    ) -> Result<()> {
        let name = &self.name;
        if strict
            && parsed.get("protocol_version").and_then(Value::as_u64)
                != Some(protocol_version as u64)
        {
            bail!(
                "plugin '{name}' streamed a reply without the protocol version this Kroma expects ({protocol_version}). Update the plugin, or run `kroma plugins check`."
            );
        }
        if strict && parsed.get("request_id") != request_id {
            bail!(
                "plugin '{name}' replied to a different request than Kroma sent. The plugin is likely built for another Kroma version: update it, or run `kroma plugins check`."
            );
        }
        if let Some(version) = parsed.get("protocol_version") {
            if version.as_u64() != Some(protocol_version as u64) {
                bail!(
                    "plugin '{name}' uses protocol version {version}, but this Kroma expects {protocol_version}. Update the plugin."
                );
            }
        }
        if let Some(received) = parsed.get("request_id") {
            if Some(received) != request_id {
                bail!(
                    "plugin '{name}' replied to a different request than Kroma sent. The plugin is likely built for another Kroma version: update it, or run `kroma plugins check`."
                );
            }
        }
        Ok(())
    }

    fn request(&mut self, request: Value) -> Result<PluginResponse> {
        let request_id = request.get("request_id").cloned();
        self.write_request(&request)?;
        // A plugin may push host calls before its final response. Only the
        // calls below are answered; anything else is refused explicitly so a
        // typo cannot hang the call. The bound keeps a looping plugin from
        // holding the turn open.
        for _ in 0..MAX_HOST_CALLS_PER_REQUEST {
            let parsed = self.read_frame()?;
            if parsed.get("type").and_then(Value::as_str) == Some("host_call") {
                self.answer_host_call(&parsed)?;
                continue;
            }
            self.validate_frame(&parsed, request_id.as_ref(), false, self.protocol_version)?;
            return serde_json::from_value(parsed).context("invalid plugin response fields");
        }
        bail!(
            "plugin '{}' sent more than {MAX_HOST_CALLS_PER_REQUEST} host calls in one request",
            self.name
        )
    }

    /// Answer one plugin-to-host call (`ui.status`, `ui.notify`, `ui.composer`,
    /// `ui.widget`, `ui.panel`, `ui.desktop_emit`, `ui.ask`, `ui.custom`).
    fn answer_host_call(&mut self, parsed: &Value) -> Result<()> {
        self.answer_host_call_inner(parsed, true)
    }

    fn answer_host_call_inner(&mut self, parsed: &Value, allow_custom: bool) -> Result<()> {
        let name = parsed.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = parsed.get("arguments").cloned().unwrap_or(Value::Null);
        let result = match name {
            "ui.status" => {
                let text = arguments.get("text").and_then(Value::as_str).unwrap_or("");
                let key = arguments.get("key").and_then(Value::as_str).unwrap_or("");
                let scope = UiScope::from_value(arguments.get("scope"));
                self.ui_bus.set_status(&self.plugin_id, key, text, scope);
                json!({"ok": true})
            }
            "ui.notify" => {
                let message = arguments
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let level = arguments
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("info");
                self.ui_bus.notify(&self.plugin_id, message, level);
                json!({"ok": true})
            }
            "ui.composer" | "ui.set_editor" => {
                let text = arguments.get("text").and_then(Value::as_str).unwrap_or("");
                let mode = arguments
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("replace");
                self.ui_bus.set_composer(&self.plugin_id, text, mode);
                json!({"ok": true})
            }
            "ui.widget" => match ui::parse_widget_args(&self.plugin_id, &arguments) {
                Ok(Some(widget)) => {
                    self.ui_bus.set_widget(widget);
                    json!({"ok": true})
                }
                Ok(None) => {
                    let id = arguments
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("default");
                    self.ui_bus.remove_widget(&self.plugin_id, id);
                    json!({"ok": true})
                }
                Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
            },
            "ui.panel" => {
                let action = arguments
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("open");
                if action == "close" {
                    self.ui_bus.close_panel(&self.plugin_id);
                } else {
                    let (blocks, text) = ui::parse_blocks_from_args(&arguments);
                    let title =
                        ui::clip_title(arguments.get("title").and_then(Value::as_str), &self.name);
                    let actions = ui::parse_actions(arguments.get("actions"));
                    self.ui_bus.open_panel(PluginLivePanel {
                        plugin_id: self.plugin_id.clone(),
                        title,
                        blocks,
                        text,
                        actions,
                    });
                }
                json!({"ok": true})
            }
            "ui.desktop_emit" => {
                let payload = arguments.get("payload").cloned().unwrap_or(arguments);
                self.ui_bus.emit_desktop(&self.plugin_id, payload);
                json!({"ok": true})
            }
            "ui.ask" => self.ask_the_person(parsed),
            "ui.custom" if allow_custom => self.run_custom_tui(&arguments)?,
            "ui.custom" => json!({"ok": false, "error": "nested ui.custom calls are not allowed"}),
            other => json!({"ok": false, "error": format!("unsupported host call '{other}'")}),
        };
        self.write_request(&json!({
            "type": "host_call_result",
            "request_id": parsed.get("request_id").cloned().unwrap_or(Value::Null),
            "result": result,
        }))
    }

    fn run_custom_tui(&mut self, arguments: &Value) -> Result<Value> {
        let Some(host) = self.ui_bus.custom_tui_host() else {
            return Ok(json!({
                "ok": false,
                "error": "custom TUI screen is only available in interactive terminal"
            }));
        };
        let spec = CustomTuiSpec {
            plugin_id: self.plugin_id.clone(),
            title: ui::clip_title(arguments.get("title").and_then(Value::as_str), &self.name),
            width: arguments
                .get("width")
                .and_then(Value::as_u64)
                .map(|v| v.clamp(20, 400) as u16),
            height: arguments
                .get("height")
                .and_then(Value::as_u64)
                .map(|v| v.clamp(5, 200) as u16),
            fullscreen: arguments
                .get("fullscreen")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        let (client_tx, client_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::channel();
        let wake = Arc::new(tokio::sync::Notify::new());
        if !host.open(CustomTuiChannel {
            spec,
            client_tx,
            frame_rx,
            wake: Arc::clone(&wake),
        }) {
            return Ok(json!({
                "ok": false,
                "error": "another custom TUI screen is already open"
            }));
        }
        while let Ok(event) = client_rx.recv() {
            if matches!(event, CustomTuiClientEvent::Cancel) {
                let _ = self.write_request(&event.to_request());
                return Ok(json!({"ok": true, "cancelled": true, "result": Value::Null}));
            }
            self.write_request(&event.to_request())?;
            let mut host_calls = 0usize;
            loop {
                let parsed = self.read_frame()?;
                if parsed.get("type").and_then(Value::as_str) == Some("host_call") {
                    host_calls += 1;
                    if host_calls > MAX_HOST_CALLS_PER_REQUEST {
                        bail!(
                            "plugin exceeded {MAX_HOST_CALLS_PER_REQUEST} consecutive host calls during custom TUI event"
                        );
                    }
                    self.answer_host_call_inner(&parsed, false)?;
                    continue;
                }
                let update = CustomTuiFrameUpdate::from_value(&parsed)?;
                let done = update.done;
                let result = update.result.clone();
                let _ = frame_tx.send(update);
                wake.notify_waiters();
                if done {
                    return Ok(json!({
                        "ok": true,
                        "cancelled": false,
                        "result": result.unwrap_or(Value::Null),
                    }));
                }
                break;
            }
        }
        Ok(json!({"ok": true, "cancelled": true, "result": Value::Null}))
    }

    /// One question, answered by whoever is looking at this interface. The
    /// plugin's own process is blocked while it waits, so this must never hold
    /// a lock the interface needs; `ask` only parks this worker thread.
    fn ask_the_person(&self, parsed: &Value) -> Value {
        // A catalog may be bound to one conversation's interactive surface.
        // Fall back to the process-wide host only for the terminal interfaces.
        let Some(asker) = self.ui_bus.asker().or_else(crate::tools::host_asker) else {
            return json!({
                "ok": false,
                "error": "this interface cannot show a question; decide from what you know and say what you chose"
            });
        };
        let arguments = parsed.get("arguments").cloned().unwrap_or(Value::Null);
        let request = match crate::tools::ask_request_from_value(&arguments) {
            Ok(request) => request,
            Err(error) => return json!({"ok": false, "error": format!("{error:#}")}),
        };
        // The worker runs on a blocking thread, which is exactly where waiting
        // is allowed and where the interface keeps repainting.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return json!({"ok": false, "error": "this run has no runtime to wait for an answer"});
        };
        match handle.block_on(asker.ask(request)) {
            Some(crate::tools::AskAnswer::Answered { picked, typed }) => {
                json!({"ok": true, "picked": picked, "typed": typed})
            }
            Some(crate::tools::AskAnswer::Dismissed) | None => {
                json!({"ok": false, "error": "the person closed the question without answering"})
            }
        }
    }
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        let pid = self.child.id();
        // A cancellation guard already sent SIGINT/taskkill for this tree.
        // Repeating the whole-tree teardown here was both redundant and
        // blocking; still reap the direct child handle.
        if self.terminated_pid.load(Ordering::SeqCst) != pid {
            terminate_plugin(pid);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new author must start from a plugin that already passes the host's
    /// own self-check, including the bounded ping each one performs.
    #[test]
    fn a_scaffolded_plugin_passes_its_own_check() -> Result<()> {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            return Ok(());
        }
        let base = env::temp_dir().join(format!(
            "kroma-scaffold-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&base)?;
        let created = scaffold_in(&base, "demo-plugin")?;
        assert!(created.contains("plugins check"), "{created}");
        let catalog =
            PluginCatalog::discover_at(base.join("plugins"), base.join("plugins.toml"), &base)?;
        let report = catalog.check(&base.join("demo-plugin"))?;
        assert!(report.contains("plugin=demo-plugin"), "{report}");
        assert!(report.contains("commands=1"), "{report}");
        assert!(!report.contains("is_error=true"), "{report}");
        assert!(
            report.contains("多出：") && report.contains("/demo-plugin"),
            "the check must first say in plain words what the plugin adds: {report}"
        );
        let _ = fs::remove_dir_all(&base);
        Ok(())
    }

    #[test]
    fn the_plugin_switch_accepts_only_explicit_off_values() {
        for value in ["off", " OFF ", "0", "false", "none"] {
            assert!(plugins_off(value), "{value}");
        }
        for value in ["", "on", "1", "true", "inherit", "yes"] {
            assert!(!plugins_off(value), "{value}");
        }
    }

    #[test]
    fn a_child_runtime_never_offers_a_trusted_human_quote() {
        assert!(trusted_user_source_allowed(None));
        assert!(trusted_user_source_allowed(Some("0")));
        assert!(
            !trusted_user_source_allowed(Some("1")),
            "a subagent child's prompt is model-written, not a user quote"
        );
    }

    #[test]
    fn a_plugin_that_binds_a_runtime_backend_reports_a_restart() -> Result<()> {
        let memory: PluginManifest = toml::from_str(
            "api_version=2
id='memory'
name='Memory'
command='node'
capabilities=['provide_context','task_summary']
",
        )?;
        let ui_bus = Arc::new(PluginUiBus::new());
        let worker = Arc::new(PluginWorker::unscanned(
            Path::new("."),
            Path::new("."),
            &toml::from_str::<PluginManifest>(
                "api_version=2
id='memory'
name='Memory'
command='node'
capabilities=['provide_context','task_summary']
",
            )?,
            json!({}),
            PathBuf::new(),
            ui_bus,
        ));
        let plugin = Plugin {
            manifest: memory,
            enabled: true,
            requested: true,
            blocked: None,
            project_key: None,
            transient: false,
            ui_worker: Arc::clone(&worker),
            worker,
            event_worker: None,
        };
        assert!(plugin.binds_runtime_backend());
        assert!(plugin.apply_hint().contains("Restart Kroma"));
        let tool_only: PluginManifest = toml::from_str(
            "id='hello'
name='Hello'
command='node'
",
        )?;
        let plugin = Plugin {
            manifest: tool_only,
            ..plugin
        };
        assert!(!plugin.binds_runtime_backend());
        assert!(plugin.apply_hint().contains("/plugins reload"));
        Ok(())
    }

    #[test]
    fn a_plugin_or_setting_change_is_detected_by_a_cheap_fingerprint() -> Result<()> {
        let root = test_root("fingerprint");
        let installed = root.join("plugins");
        let project = root.join("project");
        let settings = root.join("plugins.toml");
        fs::create_dir_all(installed.join("first"))?;
        fs::create_dir_all(&project)?;
        fs::write(
            installed.join("first").join("plugin.toml"),
            "id='first'
",
        )?;
        let roots = vec![installed.clone()];
        let before = source_fingerprint_at(&roots, Some(settings.as_path()));
        assert_eq!(
            before,
            source_fingerprint_at(&roots, Some(settings.as_path()))
        );

        fs::create_dir_all(installed.join("second"))?;
        fs::write(
            installed.join("second").join("plugin.toml"),
            "id='second'
",
        )?;
        let added = source_fingerprint_at(&roots, Some(settings.as_path()));
        assert_ne!(added, before, "installing a plugin must be noticed");

        fs::write(
            installed.join("first").join("plugin.toml"),
            "id='first'
version='2'
",
        )?;
        let edited = source_fingerprint_at(&roots, Some(settings.as_path()));
        assert_ne!(edited, added, "editing a manifest must be noticed");

        fs::write(
            &settings,
            "[enabled]
first=true
",
        )?;
        let enabled = source_fingerprint_at(&roots, Some(settings.as_path()));
        assert_ne!(enabled, edited, "enabling a plugin must be noticed");

        fs::remove_dir_all(installed.join("second"))?;
        assert_ne!(
            source_fingerprint_at(&roots, Some(settings.as_path())),
            enabled
        );
        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn obsolete_ui_slots_and_plugin_fingerprints_are_ignored() -> Result<()> {
        let old: PluginManifest = toml::from_str(
            "id='old'\nname='Old'\n[ui.layout]\nheader_rows=3\nfull_width_prose=true\ncomposer_max_rows=6\n",
        )?;
        assert_eq!(old.ui.unwrap().layout.unwrap().composer_max_rows, Some(6));
        let old_state: PluginState =
            toml::from_str("[project_fingerprints]\n'old-project'='prior-approval'\n")?;
        assert!(!toml::to_string(&old_state)?.contains("project_fingerprints"));
        Ok(())
    }

    #[test]
    fn plugin_sources_home_inventory_is_deduplicated_and_keeps_user_settings() -> Result<()> {
        let home = test_root("home-inventory");
        let directory = home.join(".kroma/plugins");
        let state_path = home.join(".kroma/plugins.toml");
        fs::create_dir_all(directory.join("sample"))?;
        fs::write(
            directory.join("sample/plugin.toml"),
            "id='sample'\nname='Sample'\nprompt='Hello'\n[settings]\nlimit=1\n",
        )?;
        PluginState {
            enabled: BTreeSet::from(["sample".into()]),
            settings: BTreeMap::from([(
                "sample".into(),
                BTreeMap::from([("limit".into(), toml::Value::Integer(3))]),
            )]),
            ..Default::default()
        }
        .save(&state_path)?;
        // On Windows canonicalization also adds the \\?\ path prefix used by
        // the Desktop home workspace. Neither spelling is a second install.
        for workdir in [home.clone(), home.canonicalize()?] {
            let catalog = PluginCatalog::discover_at_with_bundled(
                directory.clone(),
                state_path.clone(),
                &workdir,
                None,
                Some(directory.join(".")),
            )?;
            assert!(catalog.warnings.is_empty(), "{:?}", catalog.warnings);
            assert_eq!(catalog.plugins.len(), 1);
            assert!(catalog.plugins[0].project_key.is_none());
            assert!(catalog.plugins[0].enabled);
            assert_eq!(catalog.summaries()?[0]["settings"]["limit"], json!(3));
        }
        fs::remove_dir_all(home)?;
        Ok(())
    }

    #[test]
    fn plugin_sources_distinct_directories_still_report_id_conflicts() -> Result<()> {
        let root = test_root("distinct-plugin-sources");
        let user = root.join("user");
        let bundle = root.join("bundle");
        let workdir = root.join("project");
        let project_plugins = workdir.join(".kroma/plugins");
        for (source, name) in [
            (&user, "User"),
            (&bundle, "Bundle"),
            (&project_plugins, "Project"),
        ] {
            fs::create_dir_all(source.join("sample"))?;
            fs::write(
                source.join("sample/plugin.toml"),
                format!("id='sample'\nname='{name}'\nprompt='Hello'\n"),
            )?;
        }
        fs::create_dir_all(project_plugins.join("local"))?;
        fs::write(
            project_plugins.join("local/plugin.toml"),
            "id='local'\nname='Local'\nprompt='Project only'\n",
        )?;
        let catalog = PluginCatalog::discover_at_with_bundled(
            user,
            root.join("plugins.toml"),
            &workdir,
            None,
            Some(bundle),
        )?;
        assert_eq!(catalog.plugins.len(), 2);
        assert_eq!(catalog.plugins[0].manifest.name, "User");
        assert!(catalog.plugins[0].project_key.is_none());
        assert!(catalog.plugins[1].project_key.is_some());
        assert_eq!(catalog.warnings.len(), 2);
        assert!(catalog
            .warnings
            .iter()
            .all(|warning| warning.contains("duplicate plugin id 'sample'")));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn plugin_sources_overlapping_invalid_manifest_still_blocks_critical_backend() -> Result<()> {
        let home = test_root("invalid-home-plugin");
        let directory = home.join(".kroma/plugins");
        let state_path = home.join(".kroma/plugins.toml");
        fs::create_dir_all(directory.join("broken"))?;
        fs::write(
            directory.join("broken/plugin.toml"),
            "id='broken'\ncommand=7\n",
        )?;
        PluginState {
            enabled: BTreeSet::from(["broken".into()]),
            critical_plugins: BTreeSet::from(["broken".into()]),
            ..Default::default()
        }
        .save(&state_path)?;
        let catalog = PluginCatalog::discover_at_with_bundled(directory, state_path, &home, None, None)?;
        assert_eq!(catalog.warnings.len(), 1);
        assert_eq!(catalog.invalid_enabled.len(), 1);
        assert!(catalog.ensure_loadable().is_err());
        fs::remove_dir_all(home)?;
        Ok(())
    }

    #[test]
    fn official_plugin_bundle_is_read_only_and_user_override_wins() -> Result<()> {
        let root = test_root("bundled-override");
        let user = root.join("user-plugins");
        let bundle = root.join("official");
        let replacement = root.join("replacement");
        let state = root.join("plugins.toml");
        let project = root.join("project");
        fs::create_dir_all(bundle.join("example"))?;
        fs::create_dir_all(&replacement)?;
        fs::create_dir_all(&project)?;
        fs::write(
            bundle.join("example/plugin.toml"),
            "id='example'\nname='Official'\nprompt='Official variant'\n",
        )?;
        fs::write(
            replacement.join("plugin.toml"),
            "id='example'\nname='Custom'\nprompt='Custom variant'\n",
        )?;
        let discover = || {
            PluginCatalog::discover_at_with_bundled(
                user.clone(),
                state.clone(),
                &project,
                None,
                Some(bundle.clone()),
            )
        };
        let initial = discover()?;
        assert!(initial.list()?.contains("off  example"));
        initial.set_enabled("example", true)?;
        let official = discover()?;
        assert!(official.list()?.contains("Official"));
        assert!(
            official.uninstall("example").is_err(),
            "uninstall must never delete package-manager-owned official code"
        );
        assert!(bundle.join("example/plugin.toml").is_file());
        official.install(&replacement)?;
        let custom = discover()?;
        assert!(custom.list()?.contains("Custom"));
        assert!(!custom.list()?.contains("Official"));
        custom.set_enabled("example", true)?;
        let enabled = discover()?;
        assert!(enabled.list()?.contains("on   example"));
        enabled.uninstall("example")?;
        assert!(bundle.join("example/plugin.toml").exists());
        assert!(discover()?.list()?.contains("Official"));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn failed_plugin_contributions_are_atomic_including_prompt_and_tools() -> Result<()> {
        let root = test_root("atomic-contributions");
        let dir = root.join("plugins/half");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("plugin.toml"),
            "id='half'\nname='Half'\ncommand='node'\nreplace_prompt='   '\n[[tools]]\nname='half_tool'\ndescription='should not survive a failed prompt'\n")?;
        let state = root.join("plugins.toml");
        let catalog = PluginCatalog::discover_at(root.join("plugins"), state.clone(), &root)?;
        catalog.set_enabled("half", true)?;
        let active = PluginCatalog::discover_at(root.join("plugins"), state, &root)?;
        let mut tools = ToolRegistry::builtin();
        let mut prompt = PromptComposer::default();
        let original = prompt.initial_history(&root);
        let warnings = active.contribute(&mut tools, &mut prompt);
        assert_eq!(warnings.len(), 1);
        assert!(
            tools.is_builtin_only(),
            "a failed plugin must contribute no tool or hook"
        );
        assert_eq!(
            serde_json::to_value(prompt.initial_history(&root))?,
            serde_json::to_value(original)?,
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn generic_v2_caps_accept_valid_schemas_and_reject_bad_parameter_shapes() -> Result<()> {
        let generic: PluginManifest = toml::from_str(
            "api_version=2\nid='extension'\nname='Extension'\ncommand='node'\ncapabilities=['provide_context','task_finished']\n"
        )?;
        generic.validate()?;
        let bad_schema: PluginManifest = toml::from_str(
            "id='bad'\nname='Bad'\ncommand='node'\n[[tools]]\nname='bad_tool'\ndescription='invalid schema'\n[tools.parameters]\ntype='object'\nrequired=['unknown']\n[tools.parameters.properties.actual]\ntype='string'\n"
        )?;
        assert!(bad_schema
            .validate()
            .unwrap_err()
            .to_string()
            .contains("bad_tool"));
        let invalid_nesting: PluginManifest = toml::from_str(
            "id='bad'\nname='Bad'\ncommand='node'\n[[tools]]\nname='bad_tool'\ndescription='invalid items'\n[tools.parameters]\ntype='object'\n[tools.parameters.properties.data]\ntype='array'\nitems='not a schema'\n"
        )?;
        assert!(invalid_nesting.validate().is_err());
        Ok(())
    }

    #[test]
    fn edited_imported_js_helper_invalidates_worker_reuse_without_touching_main() -> Result<()> {
        let root = test_root("imported-helper");
        fs::write(root.join("main.js"), "require('./helper')")?;
        fs::write(root.join("helper.js"), "module.exports = 1;")?;
        let manifest: PluginManifest =
            toml::from_str("id='helper'\nname='Helper'\ncommand='node'\nargs=['main.js']\n")?;
        let ui_bus = Arc::new(PluginUiBus::new());
        let worker = || {
            PluginWorker::new(
                &root,
                &root,
                &manifest,
                json!({}),
                root.join("data"),
                ui_bus.clone(),
            )
        };
        let before = worker();
        let unchanged = worker();
        assert!(before.same_definition(&unchanged));
        // Same byte count: this must work on filesystems with coarse mtimes.
        fs::write(root.join("helper.js"), "module.exports = 2;")?;
        let changed = worker();
        assert!(!before.same_definition(&changed));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn compaction_started_is_an_accepted_plugin_event() -> Result<()> {
        let manifest: PluginManifest = toml::from_str(
            "id='events'\nname='Events'\ncommand='node'\nevents=['compaction_started']\n",
        )?;
        manifest.validate()?;
        let unsupported: PluginManifest = toml::from_str(
            "id='events'\nname='Events'\ncommand='node'\nevents=['not_a_real_event']\n",
        )?;
        assert!(unsupported.validate().is_err());
        Ok(())
    }

    #[test]
    fn capabilities_are_v2_only_and_require_a_command() {
        let v1: PluginManifest = toml::from_str(
            "id='memory'
             name='Memory'
             command='node'
             capabilities=['request_context']",
        )
        .unwrap();
        assert!(v1
            .validate()
            .unwrap_err()
            .to_string()
            .contains("api_version = 2"));

        let v2: PluginManifest = toml::from_str(
            "api_version = 2
             id='memory'
             name='Memory'
             command='node'
             capabilities=['request_context','task_summary']",
        )
        .unwrap();
        assert!(v2.validate().is_ok(), "{:?}", v2.validate());
        assert!(v2.has_capability("request_context"));

        let unknown: PluginManifest = toml::from_str(
            "api_version = 2
             id='memory'
             name='Memory'
             command='node'
             capabilities=['memory_forget']",
        )
        .unwrap();
        assert!(unknown
            .validate()
            .unwrap_err()
            .to_string()
            .contains("unsupported capability"));

        let no_command: PluginManifest = toml::from_str(
            "api_version = 2
             id='memory'
             name='Memory'
             capabilities=['request_context']",
        )
        .unwrap();
        assert!(no_command
            .validate()
            .unwrap_err()
            .to_string()
            .contains("capabilities require a command"));

        let v3: PluginManifest = toml::from_str(
            "api_version = 3
id='memory'
name='Memory'
prompt='x'",
        )
        .unwrap();
        assert!(v3
            .validate()
            .unwrap_err()
            .to_string()
            .contains("API version 3"));
    }

    #[test]
    fn validates_process_thinking_tiers_and_dynamic_panels() {
        let manifest = r#"
id = "tiers"
name = "Thinking provider"
command = "node"
[model]
protocol = "plugin"
model = "mock"
context_window = 8192
output_reserve = 1024
thinking_levels = ["low", "high"]
reasoning_level = "high"
"#;
        let valid: PluginManifest = toml::from_str(manifest).unwrap();
        assert_eq!(valid.api_version, 1, "V1 manifests remain compatible");
        assert!(valid.validate().is_ok());
        let unsupported: PluginManifest =
            toml::from_str(&format!("api_version = 3\n{manifest}")).unwrap();
        assert!(unsupported
            .validate()
            .unwrap_err()
            .to_string()
            .contains("API version 3"));

        let duplicate = manifest.replace(r#"["low", "high"]"#, r#"["low", "low"]"#);
        let invalid: PluginManifest = toml::from_str(&duplicate).unwrap();
        assert!(invalid.validate().is_err());

        let unknown = manifest.replace(
            r#"reasoning_level = "high""#,
            r#"reasoning_level = "xhigh""#,
        );
        let invalid: PluginManifest = toml::from_str(&unknown).unwrap();
        assert!(invalid.validate().is_err());

        let http = manifest.replace(
            r#"protocol = "plugin""#,
            r#"protocol = "chat"
base_url = "https://example.com""#,
        );
        let invalid: PluginManifest = toml::from_str(&http).unwrap();
        assert!(invalid.validate().is_err());

        let panel: PluginManifest =
            toml::from_str("id='panel'\nname='Panel'\n[ui]\ndynamic_panel=true\n").unwrap();
        assert!(
            panel.validate().is_err(),
            "dynamic panel requires executable"
        );
        let panel: PluginManifest =
            toml::from_str("id='panel'\nname='Panel'\ncommand='node'\n[ui]\ndynamic_panel=true\n")
                .unwrap();
        assert!(panel.validate().is_ok());
    }

    #[test]
    fn invalid_manifests_rejected() {
        let invalid = PluginManifest {
            api_version: PLUGIN_API_VERSION_V1,
            id: "../escape".into(),
            name: "Escape".into(),
            version: String::new(),
            description: String::new(),
            command: None,
            args: vec![],
            prompt: Some("x".into()),
            replace_prompt: None,
            hooks: vec![],
            capabilities: vec![],
            context: None,
            model: None,
            ui: None,
            settings: BTreeMap::new(),
            tools: vec![],
            commands: vec![],
            events: vec![],
        };
        assert!(invalid.validate().is_err());
        let mut valid = invalid;
        valid.id = "demo".into();
        assert!(valid.validate().is_ok());
        valid.tools.push(PluginToolSpec {
            name: "hi".into(),
            description: "hello".into(),
            streaming: false,
            invalidates_context: false,
            requires_user_context: false,
            requires_user_attachments: false,
            may_mutate_worktree: true,
            timeout_secs: None,
            replace: false,
            host: None,
            parameters: empty_parameters(),
        });
        assert!(valid.validate().is_err());
        valid.command = Some("node".into());
        assert!(valid.validate().is_ok());
        for invalid in [Some(0), Some(86_401)] {
            valid.tools[0].timeout_secs = invalid;
            assert!(valid.validate().is_err(), "timeout_secs={invalid:?}");
        }
        valid.tools[0].timeout_secs = Some(600);
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn tool_deadline_is_the_smaller_of_manifest_and_administrator_limit() {
        use std::time::Duration;
        assert_eq!(effective_tool_timeout(None, None), None);
        assert_eq!(
            effective_tool_timeout(Some(600), None),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            effective_tool_timeout(None, Some(Duration::from_secs(30))),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            effective_tool_timeout(Some(600), Some(Duration::from_secs(30))),
            Some(Duration::from_secs(30)),
            "the administrator limit caps a larger manifest declaration"
        );
        assert_eq!(
            effective_tool_timeout(Some(10), Some(Duration::from_secs(30))),
            Some(Duration::from_secs(10)),
            "a shorter manifest declaration still wins"
        );
    }

    #[test]
    fn remote_copy_directory_blocks_symlink_escape_and_cycles() -> Result<()> {
        let root = test_root("symlink-boundary");
        let checkout = root.join("checkout");
        let plugin_src = checkout.join("plugin");
        let outside = root.join("outside");
        fs::create_dir_all(&plugin_src)?;
        fs::create_dir_all(&outside)?;
        fs::write(plugin_src.join("plugin.toml"), "id='demo'\nname='Demo'\n")?;
        fs::write(outside.join("secret.txt"), "top-secret")?;
        #[cfg(unix)]
        let (file_ok, dir_ok, cycle_ok) = {
            use std::os::unix::fs::symlink;
            (
                symlink(outside.join("secret.txt"), plugin_src.join("leak.txt")).is_ok(),
                symlink(&outside, plugin_src.join("escape_dir")).is_ok(),
                symlink(&plugin_src, plugin_src.join("loop")).is_ok(),
            )
        };
        #[cfg(windows)]
        let (file_ok, dir_ok, cycle_ok) = {
            use std::os::windows::fs::{symlink_dir, symlink_file};
            (
                symlink_file(outside.join("secret.txt"), plugin_src.join("leak.txt")).is_ok(),
                symlink_dir(&outside, plugin_src.join("escape_dir")).is_ok(),
                symlink_dir(&plugin_src, plugin_src.join("loop")).is_ok(),
            )
        };

        let checkout_canon = checkout.canonicalize()?;
        if file_ok {
            let err = copy_directory(&plugin_src, &root.join("dest1"), Some(&checkout_canon))
                .expect_err("remote checkout must reject file symlink outside root");
            assert!(err.to_string().contains("escapes checkout root"), "{err}");
            let _ = fs::remove_file(plugin_src.join("leak.txt"));
        }
        if dir_ok {
            let err = copy_directory(&plugin_src, &root.join("dest2"), Some(&checkout_canon))
                .expect_err("remote checkout must reject dir symlink outside root");
            assert!(err.to_string().contains("escapes checkout root"), "{err}");
            let _ = fs::remove_dir(plugin_src.join("escape_dir"));
        }
        if cycle_ok {
            let err = copy_directory(&plugin_src, &root.join("dest3"), None)
                .expect_err("cyclic directory symlink must be rejected");
            assert!(
                err.to_string().contains("cyclic plugin directory link"),
                "{err}"
            );
            let _ = fs::remove_dir(plugin_src.join("loop"));
        }
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn plugin_tool_adapter_manages_timeout_with_or_without_deadline() -> Result<()> {
        let ui_bus = Arc::new(PluginUiBus::new());
        let manifest: PluginManifest = toml::from_str(
            "id='demo'
name='Demo'
command='node'
",
        )?;
        let worker = Arc::new(PluginWorker::unscanned(
            Path::new("."),
            Path::new("."),
            &manifest,
            json!({}),
            PathBuf::new(),
            ui_bus,
        ));
        for timeout in [None, Some(Duration::from_secs(900))] {
            let adapter = PluginToolAdapter {
                name: "demo_tool".into(),
                description: "demo".into(),
                parameters: empty_parameters(),
                streaming: false,
                invalidates_context: false,
                requires_user_context: false,
                requires_user_attachments: false,
                may_mutate_worktree: true,
                timeout,
                data_dir: PathBuf::new(),
                worker: Arc::clone(&worker),
            };
            assert!(adapter.manages_timeout());
            assert!(adapter.may_mutate_worktree());
        }
        let manifest_ro: PluginManifest = toml::from_str(
            "id='demo'
name='Demo'
command='node'
[[tools]]
name='ro_tool'
description='read only'
may_mutate_worktree=false
",
        )?;
        let ro = PluginToolAdapter {
            name: manifest_ro.tools[0].name.clone(),
            description: manifest_ro.tools[0].description.clone(),
            parameters: manifest_ro.tools[0].parameters.clone(),
            streaming: false,
            invalidates_context: false,
            requires_user_context: false,
            requires_user_attachments: false,
            may_mutate_worktree: manifest_ro.tools[0].may_mutate_worktree,
            timeout: None,
            data_dir: PathBuf::new(),
            worker,
        };
        assert!(!ro.may_mutate_worktree());
        Ok(())
    }

    #[tokio::test]
    async fn capability_items_filter_plugins_without_changing_defaults_and_survive_reload(
    ) -> Result<()> {
        use crate::capabilities::Capabilities;
        let root = test_root("capability-items");
        for id in ["one", "two"] {
            let directory = root.join("plugins").join(id);
            fs::create_dir_all(&directory)?;
            fs::write(directory.join("plugin.toml"), format!("id='{id}'\nname='{id}'\ncommand='node'\nargs=['main.js']\nprompt='PROMPT-{id}'\n[ui]\nstatus='STATUS-{id}'\n[[tools]]\nname='tool_{id}'\ndescription='probe'\n[tools.parameters]\ntype='object'\n"))?;
            fs::write(
                directory.join("main.js"),
                "throw new Error('must remain lazy');\n",
            )?;
        }
        let state = root.join("plugins.toml");
        let discover = || PluginCatalog::discover_at(root.join("plugins"), state.clone(), &root);
        let catalog = discover()?;
        catalog.set_enabled_items(&BTreeMap::from([
            ("one".into(), true),
            ("two".into(), false),
        ]))?;
        let saved = fs::read(&state)?;
        let mut selected = discover()?;
        let mut settings = Capabilities::default();
        settings.items.plugins.insert("one".into(), false);
        settings.items.plugins.insert("two".into(), true);
        selected.apply_capabilities(&settings);
        assert!(
            !selected
                .plugins
                .iter()
                .find(|plugin| plugin.manifest.id == "one")
                .unwrap()
                .enabled
        );
        let two = selected
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == "two")
            .unwrap();
        assert!(two.enabled && !two.requested);
        assert!(!two
            .worker
            .scan
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not enabled")));
        let mut tools = ToolRegistry::new();
        let mut prompt = PromptComposer::default();
        assert!(selected.contribute(&mut tools, &mut prompt).is_empty());
        let names = serde_json::to_string(&tools.definitions())?;
        assert!(names.contains("tool_two"));
        assert!(!names.contains("tool_one"));
        let mut projection = vec![crate::protocol::Message::new("system", "base")];
        prompt.apply(&mut projection);
        assert!(projection[0]
            .content
            .as_deref()
            .unwrap()
            .contains("PROMPT-two"));
        assert!(!projection[0]
            .content
            .as_deref()
            .unwrap()
            .contains("PROMPT-one"));
        let statuses = selected.ui_statuses().join(" ");
        assert!(statuses.contains("STATUS-two") && !statuses.contains("STATUS-one"));
        let mut reloaded = discover()?;
        let (tools, _) = selected.prepare_idle_reload_from(
            &mut reloaded,
            &ToolRegistry::new(),
            &PromptComposer::default(),
        )?;
        let names = serde_json::to_string(&tools.definitions())?;
        assert!(names.contains("tool_two") && !names.contains("tool_one"));
        assert_eq!(
            fs::read(&state)?,
            saved,
            "session selection rewrote global defaults"
        );
        let invalid = BTreeMap::from([("one".into(), false), ("unknown".into(), true)]);
        assert!(catalog.set_enabled_items(&invalid).is_err());
        assert_eq!(
            fs::read(&state)?,
            saved,
            "invalid batch partially persisted"
        );
        catalog.set_enabled_items(&BTreeMap::from([
            ("one".into(), false),
            ("two".into(), true),
        ]))?;
        let global = discover()?;
        assert!(
            !global
                .plugins
                .iter()
                .find(|plugin| plugin.manifest.id == "one")
                .unwrap()
                .requested
        );
        assert!(
            global
                .plugins
                .iter()
                .find(|plugin| plugin.manifest.id == "two")
                .unwrap()
                .requested
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    fn settings_catalog(label: &str) -> Result<(PathBuf, PluginCatalog)> {
        let root = test_root(label);
        let dir = root.join("plugins/web");
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("plugin.toml"),
            "id='web'\nname='Web'\nprompt='Use web_search.'\n[settings]\nprovider='auto'\nmax_results=6\npaid_fallback=false\nsearch_priority=['parallel','brave']\n",
        )?;
        let catalog =
            PluginCatalog::discover_at(root.join("plugins"), root.join("plugins.toml"), &root)?;
        Ok((root, catalog))
    }

    fn stored_settings(catalog: &PluginCatalog) -> Result<toml::Value> {
        Ok(toml::from_str(&catalog.show_config("web")?)?)
    }

    #[test]
    fn set_rejects_unknown_keys_and_lists_available_settings() -> Result<()> {
        let (root, catalog) = settings_catalog("set-unknown-key")?;
        let error = catalog
            .set_config("web", "max_result", Some("6"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("no setting 'max_result'"), "{error}");
        assert!(error.contains("max_results"), "{error}");
        assert!(
            catalog.set_config("web", "max_result", None).is_err(),
            "reset must not invent an undeclared key"
        );
        assert!(
            stored_settings(&catalog)?.get("max_result").is_none(),
            "a rejected key must not be written"
        );
        // A plugin without [settings] keeps accepting any identifier.
        let free = root.join("plugins/free");
        fs::create_dir_all(&free)?;
        fs::write(
            free.join("plugin.toml"),
            "id='free'\nname='Free'\nprompt='hello'\n",
        )?;
        let catalog =
            PluginCatalog::discover_at(root.join("plugins"), root.join("plugins.toml"), &root)?;
        catalog.set_config("free", "extra_label", Some("value"))?;
        let free_config = catalog.show_config("free")?;
        assert!(
            free_config.contains("extra_label = \"value\""),
            "{free_config}"
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn set_converts_values_from_the_manifest_default_type() -> Result<()> {
        let (root, catalog) = settings_catalog("set-types")?;
        let message = catalog.set_config("web", "max_results", Some("6"))?;
        assert!(message.contains("= 6 (integer)"), "{message}");
        assert_eq!(
            stored_settings(&catalog)?.get("max_results"),
            Some(&toml::Value::Integer(6))
        );
        let message = catalog.set_config("web", "paid_fallback", Some("TRUE"))?;
        assert!(message.contains("= true (boolean)"), "{message}");
        assert_eq!(
            stored_settings(&catalog)?.get("paid_fallback"),
            Some(&toml::Value::Boolean(true))
        );
        catalog.set_config("web", "paid_fallback", Some("False"))?;
        assert_eq!(
            stored_settings(&catalog)?.get("paid_fallback"),
            Some(&toml::Value::Boolean(false))
        );
        let message =
            catalog.set_config("web", "search_priority", Some(" parallel , you ,, exa "))?;
        assert!(message.contains("(string array)"), "{message}");
        assert_eq!(
            stored_settings(&catalog)?.get("search_priority"),
            Some(&toml::Value::Array(vec![
                toml::Value::String("parallel".into()),
                toml::Value::String("you".into()),
                toml::Value::String("exa".into()),
            ]))
        );
        let message = catalog.set_config("web", "provider", Some("searxng"))?;
        assert!(message.contains("\"searxng\" (string)"), "{message}");
        assert_eq!(
            stored_settings(&catalog)?.get("provider"),
            Some(&toml::Value::String("searxng".into()))
        );
        // A typed host keeps passing the value it already converted.
        catalog.set_config_value("web", "max_results", Some(toml::Value::Integer(3)))?;
        assert_eq!(
            stored_settings(&catalog)?.get("max_results"),
            Some(&toml::Value::Integer(3))
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn set_rejects_values_that_do_not_match_the_default_type() -> Result<()> {
        let (root, catalog) = settings_catalog("set-bad-values")?;
        let error = catalog
            .set_config("web", "max_results", Some("six"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("integer"), "{error}");
        let error = catalog
            .set_config("web", "paid_fallback", Some("yes"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("boolean"), "{error}");
        let config = stored_settings(&catalog)?;
        assert_eq!(
            config.get("max_results"),
            Some(&toml::Value::Integer(6)),
            "a rejected value must not be stored"
        );
        assert_eq!(
            config.get("paid_fallback"),
            Some(&toml::Value::Boolean(false)),
            "a rejected value must not be stored"
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn enabling_a_critical_plugin_warns_about_scope_and_undo() -> Result<()> {
        let root = test_root("critical-warning");
        let hook = root.join("plugins/model-hook");
        fs::create_dir_all(&hook)?;
        fs::write(
            hook.join("plugin.toml"),
            "id='model-hook'\nname='Hook'\ncommand='node'\nhooks=['before_model_request']\n[[tools]]\nname='web_search'\ndescription='replacement tool'\nreplace=true\n",
        )?;
        let model = root.join("plugins/model");
        fs::create_dir_all(&model)?;
        fs::write(
            model.join("plugin.toml"),
            "id='model'\nname='Model'\ncommand='node'\n[model]\nmodel='process-demo'\nprotocol='plugin'\ncontext_window=32768\noutput_reserve=2048\n",
        )?;
        let plain = root.join("plugins/plain");
        fs::create_dir_all(&plain)?;
        fs::write(
            plain.join("plugin.toml"),
            "id='plain'\nname='Plain'\nprompt='hello'\n",
        )?;
        let catalog =
            PluginCatalog::discover_at(root.join("plugins"), root.join("plugins.toml"), &root)?;
        let message = catalog.set_enabled("model-hook", true)?;
        assert!(message.contains("Warning: critical plugin"), "{message}");
        assert!(message.contains("before_model_request"), "{message}");
        assert!(
            message.contains("overrides built-in tools (web_search)"),
            "{message}"
        );
        assert!(
            message.contains("kroma plugins disable model-hook"),
            "{message}"
        );
        let message = catalog.set_enabled("model", true)?;
        assert!(
            message.contains("replaces the model provider for every session and project"),
            "{message}"
        );
        let plain_message = catalog.set_enabled("plain", true)?;
        assert!(!plain_message.contains("Warning"), "{plain_message}");
        fs::remove_dir_all(root)?;
        Ok(())
    }
    fn test_root(label: &str) -> PathBuf {
        let root = env::temp_dir().join(format!(
            "kroma-plugin-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn old_project_enabled_state_needs_no_additional_approval() {
        let root = test_root("stale-approval");
        let plugins = root.join("plugins");
        let project = root.join("project");
        let dir = project.join(".kroma").join("plugins").join("legacy");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&plugins).unwrap();
        fs::write(
            dir.join("plugin.toml"),
            "id='legacy'\nname='Legacy'\nprompt='legacy audit'\ncommand='python'\nargs=['main.py']\n",
        )
        .unwrap();
        fs::write(dir.join("main.py"), "print(1)\n").unwrap();
        // State written by an older version: enabled, but no content fingerprint.
        let key = format!(
            "{}::legacy",
            dir.parent().unwrap().canonicalize().unwrap().display()
        );
        let state = root.join("plugins.toml");
        // A TOML literal string keeps Windows path separators intact.
        fs::write(
            &state,
            format!(
                "enabled_projects = ['{key}']
"
            ),
        )
        .unwrap();
        let catalog = PluginCatalog::discover_at(plugins, state, &project).unwrap();
        assert!(
            catalog.plugins[0].enabled,
            "an explicit enablement is sufficient"
        );
        assert!(catalog.plugins[0].requested);
        assert!(catalog.plugins[0].blocked.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_plugin_code_edits_do_not_demand_reapproval() {
        let root = test_root("approval-bytes");
        let plugins = root.join("plugins");
        let project = root.join("project");
        let dir = project.join(".kroma").join("plugins").join("scripted");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&plugins).unwrap();
        fs::write(
            dir.join("plugin.toml"),
            "id='scripted'\nname='Scripted'\nprompt='scripted audit'\ncommand='python'\nargs=['main.py']\n",
        )
        .unwrap();
        fs::write(dir.join("main.py"), "print(1)\n").unwrap();
        let state = root.join("plugins.toml");
        let catalog = PluginCatalog::discover_at(plugins.clone(), state.clone(), &project).unwrap();
        catalog.set_enabled("scripted", true).unwrap();
        let approved =
            PluginCatalog::discover_at(plugins.clone(), state.clone(), &project).unwrap();
        assert!(approved.plugins[0].enabled);
        // Same-size edits are seen on next discovery/reload, not blocked by
        // an approval queue. The user retains the original enabled switch.
        fs::write(dir.join("main.py"), "print(2)\n").unwrap();
        let changed = PluginCatalog::discover_at(plugins.clone(), state.clone(), &project).unwrap();
        assert!(changed.plugins[0].enabled);
        assert!(changed.plugins[0].requested);
        let summaries = changed.summaries().unwrap();
        assert_eq!(
            summaries[0]["enabled"], true,
            "a user-enabled project plugin stays enabled after an edit"
        );
        assert_eq!(summaries[0]["requested"], true);
        assert!(summaries[0]["blocked"].is_null());
        fs::write(
            dir.join("plugin.toml"),
            "id='scripted'\nname='Renamed'\nprompt='scripted audit'\ncommand='python'\nargs=['main.py']\n",
        )
        .unwrap();
        let manifest_changed = PluginCatalog::discover_at(plugins, state, &project).unwrap();
        assert!(manifest_changed.plugins[0].enabled);
        assert!(manifest_changed.plugins[0].requested);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plugin_sources_change_without_a_second_activation_prompt() {
        let root = test_root("all-files");
        let project = root.join("project");
        let dir = project.join(".kroma/plugins/demo");
        let plugins = root.join("plugins");
        let state = root.join("plugins.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&plugins).unwrap();
        fs::write(
            dir.join("plugin.toml"),
            "id='demo'
name='Demo'
prompt='audit'
",
        )
        .unwrap();
        for name in ["helper.py", "helper", "config.custom", "large.js"] {
            let bytes = if name == "large.js" {
                vec![b'a'; 1024 * 1024 + 1]
            } else {
                b"old".to_vec()
            };
            fs::write(dir.join(name), &bytes).unwrap();
            let catalog =
                PluginCatalog::discover_at(plugins.clone(), state.clone(), &project).unwrap();
            catalog.set_enabled("demo", true).unwrap();
            let approved =
                PluginCatalog::discover_at(plugins.clone(), state.clone(), &project).unwrap();
            assert!(approved.plugins[0].enabled);
            let mut changed = bytes;
            changed[0] = b'b';
            fs::write(dir.join(name), changed).unwrap();
            let catalog =
                PluginCatalog::discover_at(plugins.clone(), state.clone(), &project).unwrap();
            assert!(
                catalog.plugins[0].enabled,
                "{name} edit must not disable user choice"
            );
        }
        // A broken requested plugin can always be disabled, without approving it.
        let oversized = fs::File::create(dir.join("oversized")).unwrap();
        oversized
            .set_len(PLUGIN_SCAN_LIMITS.max_file_bytes + 1)
            .unwrap();
        drop(oversized);
        let catalog = PluginCatalog::discover_at(plugins.clone(), state.clone(), &project).unwrap();
        assert!(catalog.set_enabled("demo", true).is_ok());
        catalog.set_enabled("demo", false).unwrap();
        let disabled = PluginCatalog::discover_at(plugins, state, &project).unwrap();
        assert!(!disabled.plugins[0].requested);
        assert!(disabled.plugins[0].worker.scan.entrypoints.is_empty());
        assert!(disabled.plugins[0]
            .worker
            .scan
            .error
            .as_ref()
            .unwrap()
            .contains("not enabled"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_scans_fail_closed_at_each_bound() {
        let root = test_root("scan-bounds");
        fs::write(root.join("helper"), "abcd").unwrap();
        for limits in [
            PluginScanLimits {
                max_entries: 0,
                ..PLUGIN_SCAN_LIMITS
            },
            PluginScanLimits {
                max_bytes: 3,
                ..PLUGIN_SCAN_LIMITS
            },
            PluginScanLimits {
                max_file_bytes: 3,
                ..PLUGIN_SCAN_LIMITS
            },
        ] {
            assert!(approval::scan(&root, None, &[], &limits).error.is_some());
        }
        assert!(
            approval::scan(&root, Some("./missing"), &[], &PLUGIN_SCAN_LIMITS)
                .error
                .is_some()
        );
        fs::create_dir_all(root.join("__pycache__")).unwrap();
        fs::write(root.join("__pycache__/helper.pyc"), "cache").unwrap();
        let scan = approval::scan(&root, None, &[], &PLUGIN_SCAN_LIMITS);
        assert!(scan.error.is_none());
        assert_eq!(scan.entrypoints.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transient_try_is_enabled_but_never_persisted_or_configurable() {
        let root = test_root("try");
        let plugins = root.join("plugins");
        let source = root.join("source");
        let project = root.join("project");
        fs::create_dir_all(&plugins).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            source.join("plugin.toml"),
            "id='transient-demo'
name='Transient demo'
prompt='try me'
",
        )
        .unwrap();
        let state = root.join("plugins.toml");
        let catalog = PluginCatalog::discover_at_with_transient(
            plugins,
            state.clone(),
            &project,
            Some(source.clone()),
        )
        .unwrap();
        let listed = catalog.list().unwrap();
        assert!(
            listed.contains("transient-demo") && listed.contains("[try]"),
            "{listed}"
        );
        assert!(listed.starts_with("on "), "{listed}");
        assert!(!state.exists(), "transient discovery must not write state");
        let summaries = catalog.summaries().unwrap();
        assert_eq!(summaries[0]["enabled"], true);
        assert_eq!(summaries[0]["transient"], true);
        assert_eq!(summaries[0]["protocol_version"], 1);
        assert_eq!(summaries[0]["capabilities"], json!([]));
        assert!(catalog.set_enabled("transient-demo", false).is_err());
        assert!(catalog.uninstall("transient-demo").is_err());
        assert!(
            !state.exists(),
            "rejected operations must not persist state"
        );
        let check = catalog.check(&source).unwrap();
        assert!(check.contains("process=not applicable"), "{check}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nested_plugin_manifest_error_explains_the_correct_install_path() -> Result<()> {
        let root = test_root("nested-manifest");
        fs::create_dir_all(root.join("plugin"))?;
        fs::write(
            root.join("plugin/plugin.toml"),
            "api_version=2\nid='nested'\nname='Nested'\nprompt='fixture'\n",
        )?;
        let error = PluginManifest::read(&root).unwrap_err().to_string();
        assert!(error.contains("manifest is under"), "{error}");
        assert!(error.contains("plugins check"), "{error}");
        assert!(error.contains("plugins install"), "{error}");
        assert_eq!(PluginManifest::read(&root.join("plugin"))?.id, "nested");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn plugin_check_smoke_talks_to_the_process_without_persisting_activation() {
        if Command::new("node").arg("--version").output().is_err() {
            return;
        }
        let root = test_root("check");
        let plugins = root.join("plugins");
        let source = root.join("source");
        let project = root.join("project");
        fs::create_dir_all(&plugins).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            source.join("plugin.toml"),
            "id='checked'
name='Checked'
command='node'
args=['main.js']
[[tools]]
name='ping_tool'
description='Ping'
[tools.parameters]
type='object'
",
        )
        .unwrap();
        fs::write(
            source.join("main.js"),
            "require('node:readline').createInterface({input:process.stdin}).on('line',line=>{const r=JSON.parse(line);process.stdout.write(JSON.stringify({request_id:r.request_id,protocol_version:r.protocol_version,content:'pong'})+String.fromCharCode(10));});",
        )
        .unwrap();
        let state = root.join("plugins.toml");
        let catalog = PluginCatalog::discover_at(plugins, state.clone(), &project).unwrap();
        let check = catalog.check(&source).unwrap();
        assert!(
            check.contains("api_version=1 protocol_version=1"),
            "{check}"
        );
        assert!(
            check.contains("smoke=response is_error=false content=pong"),
            "{check}"
        );
        assert!(!state.exists(), "check must not write activation state");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plugin_summaries_redact_secret_settings() {
        let mut settings = BTreeMap::new();
        settings.insert("exa_api_key".into(), toml::Value::String("sk-live".into()));
        settings.insert("mcp_token".into(), toml::Value::String("tok".into()));
        settings.insert("provider".into(), toml::Value::String("auto".into()));
        settings.insert("empty_secret".into(), toml::Value::String(String::new()));
        redact_secret_settings(&mut settings);
        assert_eq!(settings["exa_api_key"].as_str(), Some("••••"));
        assert_eq!(settings["mcp_token"].as_str(), Some("••••"));
        assert_eq!(settings["provider"].as_str(), Some("auto"));
        assert_eq!(settings["empty_secret"].as_str(), Some(""));
    }

    #[test]
    fn a_host_run_tool_needs_no_command_and_declares_no_local_schema() {
        let minimal: PluginManifest = toml::from_str(
            "id='ask'
name='Ask'
[[tools]]
name='ask'
host='ask_user'
description='Ask the user'
",
        )
        .unwrap();
        assert!(minimal.validate().is_ok());
        assert!(minimal
            .tools
            .iter()
            .all(|tool| tool.host.as_deref() == Some("ask_user")));

        let unknown: PluginManifest = toml::from_str(
            "id='ask'
name='Ask'
[[tools]]
name='ask'
host='teleport'
description='Ask'
",
        )
        .unwrap();
        assert!(unknown
            .validate()
            .unwrap_err()
            .to_string()
            .contains("unknown host mechanism"));

        let own_schema: PluginManifest = toml::from_str(
            "id='ask'
name='Ask'
[[tools]]
name='ask'
host='ask_user'
description='Ask'
[tools.parameters]
type='object'
[tools.parameters.properties.choice]
type='string'
",
        )
        .unwrap();
        assert!(own_schema.validate().is_err());

        // A tool with no host still needs a process to run in.
        let process_tool: PluginManifest = toml::from_str(
            "id='x'
name='X'
[[tools]]
name='x'
description='x'
",
        )
        .unwrap();
        assert!(process_tool.validate().is_err());
    }

    #[test]
    fn before_turn_end_is_admitted_and_unknown_hooks_are_rejected() {
        let gate: PluginManifest = toml::from_str(
            "api_version=2
id='gate'
name='Gate'
command='node'
hooks=['before_turn_end']
",
        )
        .unwrap();
        gate.validate().unwrap();

        let unknown: PluginManifest = toml::from_str(
            "id='gate'
name='Gate'
command='node'
hooks=['before_everything']
",
        )
        .unwrap();
        assert!(unknown.validate().is_err());
    }
}
