//! Lightweight process plugins. No runtime ABI, Agent hooks or extra model calls.
//! An enabled plugin exposes tools/commands through a newline-delimited JSON
//! process and contributes static prompt text at runtime composition.
mod context;
mod model;
use crate::{
    commands::{CommandRegistry, CommandReply},
    context::{ContextConfig, ContextFactory},
    events::{EventSink, RuntimeEvent},
    model::ModelBackend,
    prompt::PromptComposer,
    protocol::ToolCall,
    provider::ModelClient,
    tools::{HookFuture, Tool, ToolContext, ToolFuture, ToolInterceptor, ToolRegistry, ToolResult},
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
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub replace_prompt: Option<String>,
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub context: Option<PluginContextSpec>,
    #[serde(default)]
    pub model: Option<PluginModelSpec>,
    #[serde(default)]
    pub ui: Option<PluginUiSpec>,
    #[serde(default)]
    pub settings: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub tools: Vec<PluginToolSpec>,
    #[serde(default)]
    pub commands: Vec<PluginCommandSpec>,
    #[serde(default)]
    pub events: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PluginUiSpec {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub panel_title: String,
    #[serde(default)]
    pub panel_text: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PluginModelSpec {
    pub model: String,
    pub base_url: String,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    pub api_key_env: Option<String>,
    pub context_window: usize,
    pub output_reserve: usize,
}
fn default_protocol() -> String {
    "chat".into()
}

#[derive(Clone, Debug, Deserialize)]
pub struct PluginContextSpec {
    #[serde(default = "context_version")]
    pub version: u32,
}

fn context_version() -> u32 {
    1
}

#[derive(Clone, Debug, Deserialize)]
pub struct PluginToolSpec {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub replace: bool,
    #[serde(default = "empty_parameters")]
    pub parameters: Value,
}

fn empty_parameters() -> Value {
    json!({"type": "object", "properties": {}})
}

#[derive(Clone, Debug, Deserialize)]
pub struct PluginCommandSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

impl PluginManifest {
    fn read(directory: &Path) -> Result<Self> {
        let path = directory.join("plugin.toml");
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let manifest: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        if !valid_id(&self.id) {
            bail!("plugin id must contain only ASCII letters, digits, '-' or '_'");
        }
        if self.name.trim().is_empty() {
            bail!("plugin '{}' has no name", self.id);
        }
        if self.tools.is_empty()
            && self.commands.is_empty()
            && self.events.is_empty()
            && self.hooks.is_empty()
            && self.context.is_none()
            && self.model.is_none()
            && self.ui.is_none()
            && self
                .replace_prompt
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
            && self.prompt.as_deref().unwrap_or("").trim().is_empty()
        {
            bail!("plugin '{}' has no contributions", self.id);
        }
        if (!self.tools.is_empty()
            || !self.commands.is_empty()
            || !self.events.is_empty()
            || !self.hooks.is_empty()
            || self.context.is_some())
            && self
                .command
                .as_deref()
                .is_none_or(|cmd| cmd.trim().is_empty())
        {
            bail!(
                "plugin '{}' needs a command to run tools or commands",
                self.id
            );
        }
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            if !valid_tool_name(&tool.name)
                || tool.description.trim().is_empty()
                || !tool.parameters.is_object()
            {
                bail!("plugin '{}' has an invalid tool '{}'", self.id, tool.name);
            }
            if !names.insert(tool.name.clone()) {
                bail!("plugin '{}' declares tool '{}' twice", self.id, tool.name);
            }
        }
        names.clear();
        for command in &self.commands {
            if !command.name.starts_with('/') || !valid_tool_name(&command.name[1..]) {
                bail!(
                    "plugin '{}' has an invalid slash command '{}'",
                    self.id,
                    command.name
                );
            }
            if !names.insert(command.name.clone()) {
                bail!(
                    "plugin '{}' declares command '{}' twice",
                    self.id,
                    command.name
                );
            }
        }
        if let Some(model) = &self.model {
            ContextConfig::with_output_reserve(model.context_window, model.output_reserve)?;
            if model.model.trim().is_empty() || model.base_url.trim().is_empty() {
                bail!("plugin '{}' has invalid model configuration", self.id);
            }
        }
        if self
            .context
            .as_ref()
            .is_some_and(|context| context.version == 0)
        {
            bail!("plugin '{}' context version cannot be zero", self.id);
        }
        for hook in &self.hooks {
            if !matches!(
                hook.as_str(),
                "before_tool_call" | "after_tool_result" | "before_model_request"
            ) {
                bail!("plugin '{}' declares unsupported hook '{hook}'", self.id);
            }
        }
        for name in &self.events {
            if !matches!(
                name.as_str(),
                "task_started"
                    | "task_finished"
                    | "context_prepared"
                    | "model_started"
                    | "model_finished"
                    | "model_failed"
                    | "compaction_usage"
                    | "tool_started"
                    | "tool_finished"
                    | "session_created"
                    | "session_switched"
                    | "session_cleared"
                    | "session_forked"
                    | "session_rewound"
            ) {
                bail!("plugin '{}' declares unsupported event '{name}'", self.id);
            }
        }
        Ok(())
    }
}

fn valid_id(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[derive(Default, Serialize, Deserialize)]
struct PluginState {
    #[serde(default)]
    enabled: BTreeSet<String>,
    #[serde(default)]
    settings: BTreeMap<String, BTreeMap<String, toml::Value>>,
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
        fs::create_dir_all(parent)?;
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let temp = parent.join(format!(".plugins-{}-{nanos}.tmp", std::process::id()));
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(toml::to_string(self)?.as_bytes())?;
            file.sync_all()?;
            drop(file); // Windows cannot atomically replace an open temporary file.
            crate::atomic_file::replace(&temp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

#[derive(Clone)]
pub struct PluginCatalog {
    directory: PathBuf,
    state_path: PathBuf,
    plugins: Vec<Plugin>,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct Plugin {
    manifest: PluginManifest,
    enabled: bool,
    worker: Arc<PluginWorker>,
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

    pub fn discover(workdir: &Path) -> Result<Self> {
        let directory = Self::directory()?;
        let state_path = directory
            .parent()
            .context("plugin directory needs a parent")?
            .join("plugins.toml");
        Self::discover_at(directory, state_path, workdir)
    }

    /// Explicit paths avoid changing process-wide environment variables in tests.
    pub fn discover_at(directory: PathBuf, state_path: PathBuf, workdir: &Path) -> Result<Self> {
        let mut warnings = Vec::new();
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
        for source in [&directory, &workdir.join(".kroma").join("plugins")] {
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
                        let enabled = state.enabled.contains(&manifest.id);
                        let mut settings = manifest.settings.clone();
                        settings.extend(
                            state
                                .settings
                                .get(&manifest.id)
                                .cloned()
                                .unwrap_or_default(),
                        );
                        let worker = Arc::new(PluginWorker::new(
                            &dir,
                            workdir,
                            &manifest,
                            serde_json::to_value(settings)?,
                        ));
                        plugins.push(Plugin {
                            manifest,
                            enabled,
                            worker,
                        });
                    }
                    Err(error) => warnings.push(format!("{}: {error:#}", dir.display())),
                }
            }
        }
        Ok(Self {
            directory,
            state_path,
            plugins,
            warnings,
        })
    }

    pub fn list(&self) -> Result<String> {
        // Listing must still work when an edited settings file is invalid;
        // the discovery warning explains why nothing was activated.
        let enabled = PluginState::load(&self.state_path)
            .unwrap_or_default()
            .enabled;
        let mut lines = Vec::new();
        for plugin in &self.plugins {
            let active = enabled.contains(&plugin.manifest.id);
            let restart = if active != plugin.enabled {
                " (restart to apply)"
            } else {
                ""
            };
            let version = if plugin.manifest.version.is_empty() {
                ""
            } else {
                &plugin.manifest.version
            };
            lines.push(format!(
                "{}  {} {}  [{}]{}",
                if active { "on " } else { "off" },
                plugin.manifest.id,
                version,
                plugin.manifest.name,
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

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<String> {
        if !self.plugins.iter().any(|plugin| plugin.manifest.id == id) {
            bail!("unknown plugin '{id}'");
        }
        let mut state = PluginState::load(&self.state_path)?;
        if enabled {
            state.enabled.insert(id.to_owned());
        } else {
            state.enabled.remove(id);
        }
        state.save(&self.state_path)?;
        Ok(format!(
            "Plugin '{id}' {}. Restart Kroma to apply.",
            if enabled { "enabled" } else { "disabled" }
        ))
    }

    pub fn set_config(&self, id: &str, key: &str, value: Option<&str>) -> Result<String> {
        if !self.plugins.iter().any(|plugin| plugin.manifest.id == id) {
            bail!("unknown plugin '{id}'");
        }
        if !valid_id(key) {
            bail!("config key must be an ASCII identifier");
        }
        let mut state = PluginState::load(&self.state_path)?;
        let entry = state.settings.entry(id.to_owned()).or_default();
        match value {
            Some(value) => {
                entry.insert(key.to_owned(), toml::Value::String(value.to_owned()));
            }
            None => {
                entry.remove(key);
            }
        }
        if entry.is_empty() {
            state.settings.remove(id);
        }
        state.save(&self.state_path)?;
        Ok(format!(
            "Plugin '{id}' setting '{key}' updated. Restart Kroma to apply."
        ))
    }

    pub fn show_config(&self, id: &str) -> Result<String> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == id)
            .ok_or_else(|| anyhow!("unknown plugin '{id}'"))?;
        let mut config = plugin.manifest.settings.clone();
        let state = PluginState::load(&self.state_path)?;
        config.extend(state.settings.get(id).cloned().unwrap_or_default());
        Ok(if config.is_empty() {
            "No settings.".into()
        } else {
            toml::to_string_pretty(&config)?
        })
    }

    pub fn install(&self, directory: &Path) -> Result<String> {
        let source = directory
            .canonicalize()
            .with_context(|| format!("opening {}", directory.display()))?;
        if !source.is_dir() {
            bail!("plugin source must be a directory");
        }
        let manifest = PluginManifest::read(&source)?;
        if self
            .plugins
            .iter()
            .any(|plugin| plugin.manifest.id == manifest.id)
        {
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
        copy_directory(&source, &target)
            .with_context(|| format!("installing '{}'", manifest.id))?;
        // Reinstalling an ID must not inherit stale activation or settings.
        let result = (|| -> Result<()> {
            let mut state = PluginState::load(&self.state_path)?;
            state.enabled.remove(&manifest.id);
            state.settings.remove(&manifest.id);
            state.save(&self.state_path)
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&target);
            return Err(error);
        }
        Ok(format!(
            "Installed '{}' (disabled). Enable with 'kroma plugins enable {}'.",
            manifest.id, manifest.id
        ))
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
            if let Some(base) = plugin.manifest.replace_prompt.as_deref() {
                if replaced_prompt {
                    warnings.push(format!(
                        "{}: base Prompt already replaced; plugin skipped",
                        plugin.manifest.id
                    ));
                    continue;
                }
                if let Err(error) = prompt.replace_system_prompt(base) {
                    warnings.push(format!("{}: {error:#}", plugin.manifest.id));
                    continue;
                }
                replaced_prompt = true;
            }
            if let Some(text) = plugin
                .manifest
                .prompt
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                if let Err(error) = prompt.push_trailing(text) {
                    warnings.push(format!("{}: {error:#}", plugin.manifest.id));
                }
            }
            for tool in &plugin.manifest.tools {
                let adapter = PluginToolAdapter {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.parameters.clone(),
                    worker: plugin.worker.clone(),
                };
                let result = if tool.replace {
                    tools.replace(&tool.name, Arc::new(adapter))
                } else {
                    tools.register(Arc::new(adapter))
                };
                if let Err(error) = result {
                    warnings.push(format!("{}: {error:#}", plugin.manifest.id));
                } else {
                    names.insert(tool.name.clone());
                    if tool.replace {
                        replaced_tools.insert(tool.name.clone());
                    }
                }
            }
            if !plugin.manifest.hooks.is_empty() {
                tools.register_interceptor(Arc::new(PluginToolHooks {
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
            .filter(|plugin| {
                plugin.enabled
                    && plugin
                        .manifest
                        .hooks
                        .iter()
                        .any(|hook| hook == "before_model_request")
            })
            .map(|plugin| plugin.worker.clone())
            .collect::<Vec<_>>();
        if workers.is_empty() {
            backend
        } else {
            Arc::new(model::PluginModelHooks::new(backend, budget, workers))
        }
    }

    pub fn replace_model(
        &self,
        backend: &mut Arc<dyn ModelBackend>,
        budget: &mut ContextConfig,
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut selected = false;
        for plugin in &self.plugins {
            if !plugin.enabled {
                continue;
            }
            if let Some(spec) = &plugin.manifest.model {
                if selected {
                    warnings.push(format!("{}: model already replaced", plugin.manifest.id));
                    continue;
                }
                let prepared = (|| -> Result<(Arc<dyn ModelBackend>, ContextConfig)> {
                    let window = ContextConfig::with_output_reserve(
                        spec.context_window,
                        spec.output_reserve,
                    )?;
                    let key = spec
                        .api_key_env
                        .as_ref()
                        .and_then(|name| env::var(name).ok());
                    let client = ModelClient::new(&spec.base_url, key, spec.model.clone())?
                        .with_protocol(&spec.protocol)?;
                    Ok((Arc::new(client), window))
                })();
                match prepared {
                    Ok((next, next_budget)) => {
                        *backend = next;
                        *budget = next_budget;
                        selected = true;
                    }
                    Err(error) => warnings.push(format!(
                        "{}: model replacement failed: {error:#}",
                        plugin.manifest.id
                    )),
                }
            }
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
                    Arc::new(plugin.worker.isolated()),
                )
            })
            .collect();
        if listeners.is_empty() {
            return Ok(host);
        }
        // A single best-effort background queue keeps slow plugins off the model's hot path.
        let (sender, receiver) = mpsc::sync_channel::<(Arc<PluginWorker>, Value)>(64);
        std::thread::Builder::new()
            .name("kroma-plugin-events".into())
            .spawn(move || {
                while let Ok((worker, message)) = receiver.recv() {
                    if let Err(error) = worker.invoke(message) {
                        crate::runtime_log!("[plugin] event handler: {error:#}");
                    }
                }
            })?;
        Ok(Some(Arc::new(PluginEventSink {
            host,
            listeners,
            sender,
        })))
    }

    pub fn selected_model_name(&self) -> Option<&str> {
        self.plugins
            .iter()
            .find(|plugin| plugin.enabled && plugin.manifest.model.is_some())
            .and_then(|plugin| plugin.manifest.model.as_ref())
            .map(|spec| spec.model.as_str())
    }

    pub fn ui_statuses(&self) -> Vec<String> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.enabled)
            .filter_map(|plugin| {
                plugin
                    .manifest
                    .ui
                    .as_ref()
                    .map(|ui| ui.status.clone())
                    .filter(|status| !status.trim().is_empty())
            })
            .collect()
    }

    pub fn ui_panel(&self, id: &str) -> Result<String> {
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
        Ok(format!(
            "{}\n\n{}",
            if ui.panel_title.is_empty() {
                &plugin.manifest.name
            } else {
                &ui.panel_title
            },
            ui.panel_text
        ))
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
                ["install", path] => catalog.install(Path::new(path)),
                ["config", id] => catalog.show_config(id),
                ["view", id] => catalog.ui_panel(id),
                ["set", id, key, value @ ..] if !value.is_empty() => catalog.set_config(id, key, Some(&value.join(" "))),
                ["reset", id, key] => catalog.set_config(id, key, None),
                _ => bail!("Usage: /plugins [list|enable <id>|disable <id>|install <path>|config <id>|set <id> <key> <value>|reset <id> <key>]"),
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
                if let Err(error) = registry.register(&name, move |agent, _, args| {
                    let reply = worker.invoke(json!({
                        "type": "command", "name": command_name, "arguments": args,
                        "workdir": agent.workdir()
                    }))?;
                    if reply.is_error {
                        bail!("{}", reply.content);
                    }
                    Ok(CommandReply::text(reply.content))
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
            [action, path] if action == "install" => catalog.install(Path::new(path))?,
            [action, id] if action == "config" => catalog.show_config(id)?,
            [action, id] if action == "view" => catalog.ui_panel(id)?,
            [action, id, key, value @ ..] if action == "set" && !value.is_empty() => catalog.set_config(id, key, Some(&value.join(" ")))?,
            [action, id, key] if action == "reset" => catalog.set_config(id, key, None)?,
            _ => bail!("Usage: kroma plugins [list|enable <id>|disable <id>|install <path>|config <id>|set <id> <key> <value>|reset <id> <key>]"),
        };
        println!("{output}");
        Ok(())
    }
}

fn copy_directory(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir(target).with_context(|| format!("creating {}", target.display()))?;
    let result = (|| {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let source_child = entry.path();
            let target_child = target.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                bail!("plugin installation does not copy symlinks");
            }
            if kind.is_dir() {
                copy_directory(&source_child, &target_child)?;
            } else if kind.is_file() {
                fs::copy(&source_child, &target_child)?;
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(target);
    }
    result
}

struct PluginToolAdapter {
    name: String,
    description: String,
    parameters: Value,
    worker: Arc<PluginWorker>,
}

impl Tool for PluginToolAdapter {
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
        let workdir = context.workdir.to_path_buf();
        Box::pin(async move {
            let Ok(arguments) = arguments else {
                return ToolResult::error(format!(
                    "invalid plugin tool arguments: {}",
                    arguments.unwrap_err()
                ));
            };
            let result = worker
                .invoke_async(
                    json!({"type":"tool", "name":name, "arguments":arguments, "workdir":workdir}),
                )
                .await;
            match result {
                Ok(response) if !response.is_error => ToolResult::ok(response.content),
                Ok(response) => ToolResult::error(response.content),
                Err(error) => ToolResult::error(format!("plugin execution failed: {error:#}")),
            }
        })
    }
}

struct PluginToolHooks {
    before: bool,
    after: bool,
    worker: Arc<PluginWorker>,
}

impl ToolInterceptor for PluginToolHooks {
    fn before<'a>(&'a self, call: &'a mut ToolCall, workdir: &'a Path) -> HookFuture<'a> {
        Box::pin(async move {
            if !self.before {
                return Ok(());
            }
            let worker = self.worker.clone();
            let snapshot = json!({"type":"hook","name":"before_tool_call",
                "arguments":{"name":call.function.name,"arguments":serde_json::from_str::<Value>(&call.function.arguments)?},
                "workdir":workdir});
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

    fn after<'a>(
        &'a self,
        call: &'a ToolCall,
        result: &'a mut ToolResult,
        workdir: &'a Path,
    ) -> HookFuture<'a> {
        Box::pin(async move {
            if !self.after {
                return Ok(());
            }
            let worker = self.worker.clone();
            let snapshot = json!({"type":"hook","name":"after_tool_result",
                "arguments":{"name":call.function.name,"content":result.content,"is_error":result.is_error},
                "workdir":workdir});
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

struct PluginEventSink {
    host: Option<Arc<dyn EventSink>>,
    listeners: Vec<(BTreeSet<String>, Arc<PluginWorker>)>,
    sender: SyncSender<(Arc<PluginWorker>, Value)>,
}

impl EventSink for PluginEventSink {
    fn on_event(&self, event: RuntimeEvent<'_>) {
        if let Some(name) = plugin_event_name(&event) {
            if self
                .listeners
                .iter()
                .any(|(events, _)| events.contains(name))
            {
                let payload =
                    json!({"type":"event", "name":name, "arguments":plugin_event_fields(&event)});
                for (events, worker) in &self.listeners {
                    if events.contains(name) {
                        // Never hold up the agent for a plugin: overflow drops only
                        // notifications, not tools, prompts or durable session state.
                        let _ = self.sender.try_send((worker.clone(), payload.clone()));
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
        RuntimeEvent::TaskFinished { .. } => "task_finished",
        RuntimeEvent::ContextPrepared { .. } => "context_prepared",
        RuntimeEvent::ModelStarted { .. } => "model_started",
        RuntimeEvent::ModelFinished { .. } => "model_finished",
        RuntimeEvent::ModelFailed => "model_failed",
        RuntimeEvent::CompactionUsage { .. } => "compaction_usage",
        RuntimeEvent::ToolStarted { .. } => "tool_started",
        RuntimeEvent::ToolFinished { .. } => "tool_finished",
        RuntimeEvent::SessionCreated { .. } => "session_created",
        RuntimeEvent::SessionSwitched { .. } => "session_switched",
        RuntimeEvent::SessionCleared { .. } => "session_cleared",
        RuntimeEvent::SessionForked { .. } => "session_forked",
        RuntimeEvent::SessionRewound { .. } => "session_rewound",
        RuntimeEvent::TextDelta(_)
        | RuntimeEvent::ReasoningDelta(_)
        | RuntimeEvent::ToolOutput { .. } => return None,
    })
}

fn plugin_event_fields(event: &RuntimeEvent<'_>) -> Value {
    match event {
        RuntimeEvent::TaskStarted | RuntimeEvent::ModelFailed => json!({}),
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
        } => json!({
            "request":request, "tools":tools, "reasoning":reasoning, "input_tokens":input_tokens,
            "output_tokens":output_tokens, "reasoning_tokens":reasoning_tokens, "cached_tokens":cached_tokens
        }),
        RuntimeEvent::CompactionUsage {
            requests,
            input_tokens,
            output_tokens,
            cached_tokens,
        } => json!({
            "requests":requests, "input_tokens":input_tokens,
            "output_tokens":output_tokens, "cached_tokens":cached_tokens
        }),
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
        | RuntimeEvent::ToolOutput { .. } => json!({}),
    }
}

struct PluginWorker {
    directory: PathBuf,
    workdir: PathBuf,
    command: Option<String>,
    args: Vec<String>,
    settings: Value,
    process: Mutex<Option<PluginProcess>>,
    next_request: AtomicU64,
    active_request: AtomicU64,
    active_pid: AtomicU32,
}

impl PluginWorker {
    fn isolated(&self) -> Self {
        Self {
            directory: self.directory.clone(),
            workdir: self.workdir.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            settings: self.settings.clone(),
            process: Mutex::new(None),
            next_request: AtomicU64::new(1),
            active_request: AtomicU64::new(0),
            active_pid: AtomicU32::new(0),
        }
    }

    fn new(dir: &Path, workdir: &Path, manifest: &PluginManifest, settings: Value) -> Self {
        Self {
            directory: dir.to_path_buf(),
            workdir: workdir.to_path_buf(),
            command: manifest.command.clone(),
            args: manifest.args.clone(),
            settings,
            process: Mutex::new(None),
            next_request: AtomicU64::new(1),
            active_request: AtomicU64::new(0),
            active_pid: AtomicU32::new(0),
        }
    }

    async fn invoke_async(self: &Arc<Self>, message: Value) -> Result<PluginResponse> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut guard = PluginCallGuard {
            worker: self.clone(),
            id,
            cancelled: cancelled.clone(),
            armed: true,
        };
        let worker = self.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            worker.invoke_inner(message, Some((id, &cancelled)))
        })
        .await?;
        guard.armed = false;
        outcome
    }

    fn cancel_request(&self, id: u64) {
        if self.active_request.load(Ordering::SeqCst) == id {
            let pid = self.active_pid.load(Ordering::SeqCst);
            if pid != 0 {
                terminate_plugin(pid);
            }
        }
    }

    fn invoke(&self, message: Value) -> Result<PluginResponse> {
        self.invoke_inner(message, None)
    }

    fn invoke_inner(
        &self,
        mut message: Value,
        cancellation: Option<(u64, &AtomicBool)>,
    ) -> Result<PluginResponse> {
        let request = message.as_object_mut().context("invalid plugin request")?;
        request.insert("config".into(), self.settings.clone());
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
            let command = self
                .command
                .as_deref()
                .context("plugin has no executable command")?;
            let mut child = Command::new(command)
                .args(&self.args)
                .current_dir(&self.directory)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("starting plugin command '{command}'"))?;
            let stdin = child.stdin.take().context("plugin stdin unavailable")?;
            let stdout = child.stdout.take().context("plugin stdout unavailable")?;
            *process = Some(PluginProcess {
                child,
                stdin,
                stdout: BufReader::new(stdout),
            });
        }
        if cancellation.is_some_and(|(_, cancelled)| cancelled.load(Ordering::SeqCst)) {
            *process = None;
            bail!("plugin request cancelled before execution");
        }
        if let Some((id, _)) = cancellation {
            self.active_pid
                .store(process.as_ref().unwrap().child.id(), Ordering::SeqCst);
            self.active_request.store(id, Ordering::SeqCst);
        }
        let result = process
            .as_mut()
            .expect("plugin process created")
            .request(message);
        self.active_request.store(0, Ordering::SeqCst);
        self.active_pid.store(0, Ordering::SeqCst);
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
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            let _ = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
        }
    }
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(pid as i32, libc::SIGKILL);
    }
}

struct PluginProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[derive(Deserialize)]
struct PluginResponse {
    content: String,
    #[serde(default)]
    is_error: bool,
}

impl PluginProcess {
    fn request(&mut self, request: Value) -> Result<PluginResponse> {
        self.stdin
            .write_all(serde_json::to_string(&request)?.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
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
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_manifests_rejected() {
        let invalid = PluginManifest {
            id: "../escape".into(),
            name: "Escape".into(),
            version: String::new(),
            description: String::new(),
            command: None,
            args: vec![],
            prompt: Some("x".into()),
            replace_prompt: None,
            hooks: vec![],
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
            replace: false,
            parameters: empty_parameters(),
        });
        assert!(valid.validate().is_err());
        valid.command = Some("node".into());
        assert!(valid.validate().is_ok());
    }
}
