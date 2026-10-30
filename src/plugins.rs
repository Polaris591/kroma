//! Lightweight process plugins. No runtime ABI, Agent hooks or extra model calls.
//! An enabled plugin exposes tools/commands through a newline-delimited JSON
//! process and contributes static prompt text at runtime composition.
mod context;
mod model;
mod stream;
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
        atomic::{AtomicBool, AtomicU64, Ordering},
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
    /// Optional terminal-safe Alt+letter bindings for this plugin's commands.
    #[serde(default)]
    pub shortcuts: Vec<PluginUiShortcut>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub panel_title: String,
    #[serde(default)]
    pub panel_text: String,
    /// On-demand panel body returned by the plugin process, not TUI polling.
    #[serde(default)]
    pub dynamic_panel: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PluginUiShortcut {
    pub key: String,
    pub command: String,
}

fn parse_alt_shortcut(key: &str) -> Option<char> {
    let value = key.to_ascii_lowercase();
    let letter = value.strip_prefix("alt+")?;
    (letter.len() == 1 && letter.as_bytes()[0].is_ascii_alphabetic())
        .then(|| letter.as_bytes()[0] as char)
}

pub type ModelOverride = (Arc<dyn ModelBackend>, ContextConfig, String);

#[derive(Clone, Debug, Deserialize)]
pub struct PluginModelSpec {
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub streaming: bool,
    /// Provider-advertised tiers; an empty list permits custom levels.
    #[serde(default)]
    pub thinking_levels: Vec<String>,
    #[serde(default)]
    pub reasoning_level: Option<String>,
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
            || self.context.is_some()
            || self
                .model
                .as_ref()
                .is_some_and(|model| model.protocol == "plugin"))
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
            if model.protocol != "plugin"
                && (!model.thinking_levels.is_empty() || model.reasoning_level.is_some())
            {
                bail!(
                    "plugin '{}' custom thinking levels require protocol='plugin'",
                    self.id
                );
            }
            let mut levels = BTreeSet::new();
            if model.thinking_levels.len() > 32
                || model
                    .thinking_levels
                    .iter()
                    .any(|tier| !valid_id(tier) || tier.len() > 32 || !levels.insert(tier.as_str()))
            {
                bail!(
                    "plugin '{}' has invalid or duplicate thinking levels",
                    self.id
                );
            }
            if let Some(level) = &model.reasoning_level {
                if !valid_id(level)
                    || (!model.thinking_levels.is_empty() && !model.thinking_levels.contains(level))
                {
                    bail!(
                        "plugin '{}' default reasoning level is not supported",
                        self.id
                    );
                }
            }
            if model.model.trim().is_empty()
                || (model.protocol != "plugin" && model.base_url.trim().is_empty())
                || (model.streaming && model.protocol != "plugin")
            {
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
        if let Some(ui) = &self.ui {
            if ui.dynamic_panel
                && self
                    .command
                    .as_deref()
                    .is_none_or(|cmd| cmd.trim().is_empty())
            {
                bail!(
                    "plugin '{}' needs a command for its dynamic UI panel",
                    self.id
                );
            }
            let mut bindings = BTreeSet::new();
            for shortcut in &ui.shortcuts {
                let key = parse_alt_shortcut(&shortcut.key).ok_or_else(|| {
                    anyhow!(
                        "plugin '{}' shortcut '{}' must be Alt+A..Z",
                        self.id,
                        shortcut.key
                    )
                })?;
                if !bindings.insert(key) {
                    bail!("plugin '{}' has duplicate Alt+{key} shortcut", self.id);
                }
                let command = shortcut.command.split_whitespace().next().unwrap_or("");
                if !self
                    .commands
                    .iter()
                    .any(|registered| registered.name == command)
                {
                    bail!(
                        "plugin '{}' shortcut must target its own slash command",
                        self.id
                    );
                }
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
    enabled_projects: BTreeSet<String>,
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
    workdir: PathBuf,
    plugins: Vec<Plugin>,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct Plugin {
    manifest: PluginManifest,
    enabled: bool,
    /// None for user-global plugins; project plugins have a full path key.
    project_key: Option<String>,
    worker: Arc<PluginWorker>,
}

impl Plugin {
    fn is_enabled(&self, state: &PluginState) -> bool {
        match &self.project_key {
            Some(key) => state.enabled_projects.contains(key),
            None => state.enabled.contains(&self.manifest.id),
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
                        let project_key = if source == &directory {
                            None
                        } else {
                            Some(format!(
                                "{}::{}",
                                source.canonicalize()?.display(),
                                manifest.id
                            ))
                        };
                        let enabled = match &project_key {
                            Some(key) => state.enabled_projects.contains(key),
                            None => state.enabled.contains(&manifest.id),
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
                        let worker = Arc::new(PluginWorker::new(
                            &dir,
                            workdir,
                            &manifest,
                            serde_json::to_value(settings)?,
                        ));
                        plugins.push(Plugin {
                            manifest,
                            enabled,
                            project_key,
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
            workdir: workdir.to_path_buf(),
            plugins,
            warnings,
        })
    }

    fn refresh(&self) -> Result<Self> {
        Self::discover_at(
            self.directory.clone(),
            self.state_path.clone(),
            &self.workdir,
        )
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
            let active = plugin.is_enabled(&state);
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
        let fresh = self.refresh()?;
        let plugin = fresh
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == id)
            .ok_or_else(|| anyhow!("unknown plugin '{id}'"))?;
        let mut state = PluginState::load(&self.state_path)?;
        let enabled_set = match &plugin.project_key {
            Some(_) => &mut state.enabled_projects,
            None => &mut state.enabled,
        };
        let key = plugin.project_key.as_deref().unwrap_or(id);
        if enabled {
            enabled_set.insert(key.to_owned());
        } else {
            enabled_set.remove(key);
        }
        state.save(&self.state_path)?;
        Ok(format!(
            "Plugin '{id}' {}. Restart Kroma, or run /plugins reload while idle (model/context plugins require restart).",
            if enabled { "enabled" } else { "disabled" }
        ))
    }

    pub fn set_config(&self, id: &str, key: &str, value: Option<&str>) -> Result<String> {
        let fresh = self.refresh()?;
        let plugin = fresh
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == id)
            .ok_or_else(|| anyhow!("unknown plugin '{id}'"))?;
        if !valid_id(key) {
            bail!("config key must be an ASCII identifier");
        }
        let mut state = PluginState::load(&self.state_path)?;
        let settings = match &plugin.project_key {
            Some(_) => &mut state.project_settings,
            None => &mut state.settings,
        };
        let owner = plugin.project_key.as_deref().unwrap_or(id);
        let entry = settings.entry(owner.to_owned()).or_default();
        match value {
            Some(value) => {
                entry.insert(key.to_owned(), toml::Value::String(value.to_owned()));
            }
            None => {
                entry.remove(key);
            }
        }
        if entry.is_empty() {
            settings.remove(owner);
        }
        state.save(&self.state_path)?;
        Ok(format!(
            "Plugin '{id}' setting '{key}' updated. Run /plugins reload while idle, or restart."
        ))
    }

    pub fn show_config(&self, id: &str) -> Result<String> {
        let fresh = self.refresh()?;
        let plugin = fresh
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == id)
            .ok_or_else(|| anyhow!("unknown plugin '{id}'"))?;
        let mut config = plugin.manifest.settings.clone();
        let state = PluginState::load(&self.state_path)?;
        config.extend(plugin.config_overrides(&state).cloned().unwrap_or_default());
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
            .refresh()?
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

    /// Context policies and model adapters hold session/runtime state. Hot
    /// swapping them (or a model-request Hook wrapping the backend) would
    /// silently change the current session's semantics. Require restart.
    pub fn needs_restart(&self) -> bool {
        self.plugins
            .iter()
            .filter(|plugin| plugin.enabled)
            .any(|plugin| {
                plugin.manifest.context.is_some()
                    || plugin.manifest.model.is_some()
                    || plugin
                        .manifest
                        .hooks
                        .iter()
                        .any(|hook| hook == "before_model_request")
            })
    }

    /// Build a replacement plugin view without touching the running agent.
    /// Callers commit it only after tools, commands and event subscriptions
    /// all succeed. Default tools and prompt are reusable lightweight clones.
    pub fn prepare_idle_reload(
        &self,
        base_tools: &ToolRegistry,
        base_prompt: &PromptComposer,
    ) -> Result<(ToolRegistry, PromptComposer)> {
        if self.needs_restart() {
            bail!("model, context or model-request Hook changes require restarting Kroma");
        }
        if !self.warnings.is_empty() {
            bail!("plugin discovery warnings: {}", self.warnings.join("; "));
        }
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
            if plugin
                .manifest
                .hooks
                .iter()
                .any(|hook| hook == "before_tool_call" || hook == "after_tool_result")
            {
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

    /// Resolve an enabled model plugin before initializing the default
    /// provider. This avoids optional /models requests and permits using a
    /// plugin-provided HTTP profile without redundant default credentials.
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
                            )),
                            window,
                        ));
                    }
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
        Ok(format!("{title}\n\n{content}"))
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
        Ok(format!("{title}\n\n{content}"))
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
                if let Err(error) = registry.register_async(&name, move |args, workdir| {
                    let worker = worker.clone();
                    let name = command_name.clone();
                    async move {
                        let reply = worker
                            .invoke_async(json!({
                                "type": "command", "name": name, "arguments": args,
                                "workdir": workdir
                            }))
                            .await?;
                        if reply.is_error {
                            bail!("{}", reply.content);
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
    active: Mutex<Option<(u64, u32)>>,
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
            active: Mutex::new(None),
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
            active: Mutex::new(None),
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
        // Publish the request and its PID under one lock to avoid racing
        // cancellation against the two individual atomic stores.
        if let Ok(active) = self.active.lock() {
            if let Some((request, pid)) = *active {
                if request == id {
                    terminate_plugin(pid);
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
        request.insert("protocol_version".into(), json!(1));
        request.insert("request_id".into(), json!(id));
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
            let mut builder = Command::new(command);
            builder
                .args(&self.args)
                .current_dir(&self.directory)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
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
        // Kill descendants, too (notably subagents launched by a Node worker).
        let _ = Command::new("taskkill.exe")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    unsafe {
        // Workers have their own process group (see spawn below).
        let _ = libc::kill(-(pid as i32), libc::SIGKILL);
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

    fn validate_frame(parsed: &Value, request_id: Option<&Value>, strict: bool) -> Result<()> {
        if strict && parsed.get("protocol_version").and_then(Value::as_u64) != Some(1) {
            bail!("streaming plugin must include protocol_version=1");
        }
        if strict && parsed.get("request_id") != request_id {
            bail!("streaming plugin request_id missing or mismatched");
        }
        if let Some(version) = parsed.get("protocol_version") {
            if version.as_u64() != Some(1) {
                bail!("plugin response protocol version mismatch");
            }
        }
        if let Some(received) = parsed.get("request_id") {
            if Some(received) != request_id {
                bail!("plugin response request_id mismatch");
            }
        }
        Ok(())
    }

    fn request(&mut self, request: Value) -> Result<PluginResponse> {
        let request_id = request.get("request_id").cloned();
        self.write_request(&request)?;
        let parsed = self.read_frame()?;
        Self::validate_frame(&parsed, request_id.as_ref(), false)?;
        serde_json::from_value(parsed).context("invalid plugin response fields")
    }
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        terminate_plugin(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(valid.validate().is_ok());

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
