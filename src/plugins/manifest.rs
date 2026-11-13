//! Plugin manifest schema, validation, plain-language summary, and starter scaffold.
use crate::{context::ContextConfig, model::ModelBackend};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::Path,
    sync::Arc,
};

pub(super) const PLUGIN_API_VERSION_V1: u32 = 1;
pub(super) const PLUGIN_API_VERSION_V2: u32 = 2;
pub(super) const PLUGIN_CAPABILITIES: [&str; 4] = [
    "provide_context",
    "task_finished",
    "request_context",
    "task_summary",
];

fn default_plugin_api_version() -> u32 {
    PLUGIN_API_VERSION_V1
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
pub struct PluginManifest {
    #[serde(default = "default_plugin_api_version")]
    pub api_version: u32,
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
    /// V2 host capabilities; V1 manifests must leave this empty.
    #[serde(default)]
    pub capabilities: Vec<String>,
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
    /// Host-rendered activity indicator, at most one enabled plugin may own it.
    #[serde(default)]
    pub activity_frames: Vec<String>,
    /// Display labels for registered tool names, without changing dispatch.
    #[serde(default)]
    pub tool_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub layout: Option<PluginLayoutSpec>,
    /// Optional palette overrides rendered by the host, never raw ANSI escapes.
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub panel_title: String,
    #[serde(default)]
    pub panel_text: String,
    /// On-demand panel body returned by the plugin process, not TUI polling.
    #[serde(default)]
    pub dynamic_panel: bool,
    /// Optional custom Desktop HTML/JS surface rendered in a sandboxed iframe.
    #[serde(default)]
    pub desktop: Option<PluginDesktopUiSpec>,
}

fn default_desktop_slot() -> String {
    "sidebar".to_owned()
}

/// Custom Desktop webview surface inside a sandboxed host slot.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PluginDesktopUiSpec {
    pub entry: String,
    #[serde(default = "default_desktop_slot")]
    pub slot: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub height: Option<u16>,
}

/// Minimal host-owned layout knobs; plugins never receive raw terminal control.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct PluginLayoutSpec {
    #[serde(default)]
    pub composer_max_rows: Option<u16>,
    /// Desktop reading column, in pixels. None keeps the built-in width.
    #[serde(default)]
    pub desktop_content_width: Option<u16>,
    /// "visible" or "hidden": whether the workspace sidebar starts open.
    #[serde(default)]
    pub desktop_sidebar: Option<String>,
    /// "regular" or "compact": spacing and type scale.
    #[serde(default)]
    pub desktop_density: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PluginUiShortcut {
    pub key: String,
    pub command: String,
}

pub(super) fn parse_alt_shortcut(key: &str) -> Option<char> {
    let value = key.to_ascii_lowercase();
    let letter = value.strip_prefix("alt+")?;
    (letter.len() == 1 && letter.as_bytes()[0].is_ascii_alphabetic())
        .then(|| letter.as_bytes()[0] as char)
}

pub type ModelOverride = (Arc<dyn ModelBackend>, ContextConfig, String);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
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
    /// Explicit vision support for process model adapters. Unknown by default.
    #[serde(default)]
    pub image_input: Option<bool>,
}
fn default_protocol() -> String {
    "chat".into()
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
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
    pub streaming: bool,
    #[serde(default)]
    pub invalidates_context: bool,
    #[serde(default)]
    pub requires_user_context: bool,
    #[serde(default)]
    pub requires_user_attachments: bool,
    /// Conservative recovery default: unknown plugin tools may mutate the project
    /// worktree unless the manifest explicitly marks them read-only.
    #[serde(default = "default_true")]
    pub may_mutate_worktree: bool,
    /// Optional whole-call deadline for this tool, enforced by the host for
    /// streaming and non-streaming calls alike. A global
    /// `KROMA_PLUGIN_TOOL_TIMEOUT_SECS` value caps it when smaller.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub replace: bool,
    /// `Some("ask_user")` means the host itself runs this tool: no process is
    /// started and the host owns the argument shape.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default = "empty_parameters")]
    pub parameters: Value,
}

pub(super) fn empty_parameters() -> Value {
    json!({"type": "object", "properties": {}})
}

/// Admit the small, standard JSON Schema subset accepted by tool providers.
/// Reject structurally invalid declarations at discovery rather than letting
/// one plugin's malformed schema break every model request. This is a shape
/// check, not a second JSON Schema interpreter or a runtime argument filter.
fn validate_tool_schema(schema: &Value) -> Result<()> {
    fn walk(value: &Value, depth: usize, remaining: &mut usize) -> Result<()> {
        if depth > 12 || *remaining == 0 {
            bail!("tool JSON schema exceeds the supported complexity budget");
        }
        *remaining -= 1;
        let map = value.as_object().context("tool schema must be an object")?;
        if let Some(kind) = map.get("type") {
            let check = |name: &str| {
                matches!(
                    name,
                    "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
                )
            };
            if !kind.as_str().is_some_and(check) {
                bail!("tool schema has an unsupported type");
            }
        }
        if let Some(props) = map.get("properties") {
            let props = props
                .as_object()
                .context("schema properties must be an object")?;
            if map.get("type").and_then(Value::as_str) != Some("object") {
                bail!("schema properties require type=object");
            }
            for (_, child) in props {
                walk(child, depth + 1, remaining)?;
            }
        }
        if let Some(required) = map.get("required") {
            let required = required
                .as_array()
                .context("schema required must be an array")?;
            let props = map
                .get("properties")
                .and_then(Value::as_object)
                .context("schema required needs properties")?;
            let mut names = BTreeSet::new();
            for item in required {
                let name = item
                    .as_str()
                    .context("schema required keys must be strings")?;
                if !props.contains_key(name) || !names.insert(name) {
                    bail!("schema required key is absent or duplicated");
                }
            }
        }
        if let Some(items) = map.get("items") {
            if map.get("type").and_then(Value::as_str) != Some("array") {
                bail!("schema items require type=array");
            }
            walk(items, depth + 1, remaining)?;
        }
        if let Some(extra) = map.get("additionalProperties") {
            if !extra.is_boolean() {
                walk(extra, depth + 1, remaining)?;
            }
        }
        if let Some(variants) = map.get("anyOf").or_else(|| map.get("oneOf")) {
            let variants = variants
                .as_array()
                .filter(|items| !items.is_empty())
                .context("schema union must be a nonempty array")?;
            for child in variants {
                walk(child, depth + 1, remaining)?;
            }
        }
        if let Some(values) = map.get("enum") {
            if values.as_array().is_none_or(Vec::is_empty) {
                bail!("schema enum must be a nonempty array");
            }
        }
        Ok(())
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        bail!("tool parameters must declare type=object");
    }
    walk(schema, 0, &mut 256)
}

#[derive(Clone, Debug, Deserialize)]
pub struct PluginCommandSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

impl PluginManifest {
    pub(super) fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|item| item == capability)
    }

    /// Plain-language summary of what installing this plugin adds. It is shown
    /// before the plugin is enabled, so a person can judge it without reading
    /// the manifest or the code.
    pub fn describe(&self) -> String {
        let mut lines = Vec::new();
        let version = if self.version.is_empty() {
            String::new()
        } else {
            format!(" {}", self.version)
        };
        lines.push(format!("{}{version} ({})", self.name, self.id));
        if !self.description.trim().is_empty() {
            lines.push(format!("  {}", self.description.trim()));
        }
        let mut adds = Vec::new();
        if !self.commands.is_empty() {
            let list = self
                .commands
                .iter()
                .map(|command| match command.description.trim() {
                    "" => command.name.clone(),
                    text => format!("{}（{text}）", command.name),
                })
                .collect::<Vec<_>>()
                .join("、");
            adds.push(format!("命令：{list}"));
        }
        let tools = self
            .tools
            .iter()
            .map(|tool| {
                if tool.host.as_deref() == Some("ask_user") {
                    format!("{}（宿主自己弹窗问人）", tool.name)
                } else {
                    tool.name.clone()
                }
            })
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            adds.push(format!("给 AI 的工具：{}", tools.join("、")));
        }
        let hooks = self
            .hooks
            .iter()
            .map(|hook| match hook.as_str() {
                "before_turn_end" => "AI 想收工时先问它一句（before_turn_end）".into(),
                "before_tool_result" => "工具出结果前可以改写（before_tool_result）".into(),
                "after_tool_result" => "工具出结果后可以改写（after_tool_result）".into(),
                other => format!("钩子 {other}"),
            })
            .collect::<Vec<_>>();
        if !hooks.is_empty() {
            adds.push(format!("接入点：{}", hooks.join("、")));
        }
        if let Some(ui) = &self.ui {
            let mut bits = Vec::new();
            if !ui.status.trim().is_empty() {
                bits.push("一行状态".to_string());
            }
            if !ui.panel_title.trim().is_empty() || !ui.panel_text.trim().is_empty() {
                bits.push("一个面板".to_string());
            }
            if !ui.shortcuts.is_empty() {
                bits.push(format!("{} 个快捷键", ui.shortcuts.len()));
            }
            if !ui.activity_frames.is_empty() {
                bits.push("活动动画".to_string());
            }
            if !ui.colors.is_empty() {
                bits.push("颜色".to_string());
            }
            if ui.desktop.is_some() {
                bits.push("自定义桌面页面".to_string());
            }
            if !bits.is_empty() {
                adds.push(format!("界面：{}", bits.join("、")));
            }
        }
        if !self.settings.is_empty() {
            adds.push(format!(
                "设置项：{}",
                self.settings.keys().cloned().collect::<Vec<_>>().join("、")
            ));
        }
        if !adds.is_empty() {
            lines.push("  装上之后多出：".into());
            for add in adds {
                lines.push(format!("   · {add}"));
            }
        }
        let runner = match (self.command.as_deref(), self.prompt.as_deref()) {
            (Some(command), _) if !command.trim().is_empty() => {
                let args = self.args.join(" ");
                format!("  怎么跑：启动本机程序 {command} {args}")
            }
            (_, Some(_)) => "  怎么跑：只提供提示词，不启动程序".to_string(),
            _ => "  怎么跑：只提供提示词，不启动程序".to_string(),
        };
        lines.push(runner.trim_end().to_string());
        lines.push("  插件就是你信任的本机程序：装之前先看它要来干什么。".into());
        lines.join("\n")
    }

    /// Capabilities are a V2-only host contract and always need a process.
    fn validate_capabilities(&self) -> Result<()> {
        if self.capabilities.is_empty() {
            return Ok(());
        }
        if self.api_version != PLUGIN_API_VERSION_V2 {
            bail!(
                "plugin '{}' capabilities require api_version = {}",
                self.id,
                PLUGIN_API_VERSION_V2
            );
        }
        if self
            .command
            .as_deref()
            .is_none_or(|cmd| cmd.trim().is_empty())
        {
            bail!("plugin '{}' capabilities require a command", self.id);
        }
        let mut seen = BTreeSet::new();
        for capability in &self.capabilities {
            if !PLUGIN_CAPABILITIES.contains(&capability.as_str()) {
                bail!(
                    "plugin '{}' declares unsupported capability '{}'",
                    self.id,
                    capability
                );
            }
            if !seen.insert(capability.as_str()) {
                bail!(
                    "plugin '{}' declares capability '{}' twice",
                    self.id,
                    capability
                );
            }
        }
        Ok(())
    }

    pub(super) fn read(directory: &Path) -> Result<Self> {
        let path = directory.join("plugin.toml");
        if !path.is_file() {
            let nested = directory.join("plugin").join("plugin.toml");
            if nested.is_file() {
                bail!(
                    "plugin manifest is under {}. Use that directory for `kroma plugins check` or `kroma plugins install`",
                    nested.display()
                );
            }
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let manifest: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        manifest.validate()?;
        if let Some(desktop) = manifest.ui.as_ref().and_then(|ui| ui.desktop.as_ref()) {
            let root = directory
                .canonicalize()
                .with_context(|| format!("resolving plugin directory {}", directory.display()))?;
            let resolved = directory
                .join(&desktop.entry)
                .canonicalize()
                .with_context(|| {
                    format!(
                        "plugin '{}' desktop UI entry '{}' not found",
                        manifest.id, desktop.entry
                    )
                })?;
            if !resolved.starts_with(&root) || !resolved.is_file() {
                bail!(
                    "plugin '{}' desktop UI entry '{}' must be a file inside the plugin directory",
                    manifest.id,
                    desktop.entry
                );
            }
        }
        Ok(manifest)
    }

    pub(super) fn validate(&self) -> Result<()> {
        if !matches!(
            self.api_version,
            PLUGIN_API_VERSION_V1 | PLUGIN_API_VERSION_V2
        ) {
            bail!(
                "plugin '{}' requires API version {}; Kroma supports versions {} and {}",
                self.id,
                self.api_version,
                PLUGIN_API_VERSION_V1,
                PLUGIN_API_VERSION_V2
            );
        }
        self.validate_capabilities()?;
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
            && self.capabilities.is_empty()
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
        // Host-run tools execute in this process, so a plugin made only of
        // them is declarative and needs no command.
        if (!self.commands.is_empty()
            || !self.events.is_empty()
            || !self.hooks.is_empty()
            || self.context.is_some()
            || self
                .model
                .as_ref()
                .is_some_and(|model| model.protocol == "plugin")
            || self.tools.iter().any(|tool| tool.host.is_none()))
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
            validate_tool_schema(&tool.parameters)
                .with_context(|| format!("plugin '{}' tool '{}'", self.id, tool.name))?;
            if !names.insert(tool.name.clone()) {
                bail!("plugin '{}' declares tool '{}' twice", self.id, tool.name);
            }
            if let Some(host) = tool.host.as_deref() {
                if host != "ask_user" {
                    bail!(
                        "plugin '{}' tool '{}' names an unknown host mechanism '{}'",
                        self.id,
                        tool.name,
                        host
                    );
                }
                if tool.streaming
                    || tool.invalidates_context
                    || tool.requires_user_context
                    || tool.requires_user_attachments
                    || !tool.may_mutate_worktree
                    || tool.replace
                    || tool.timeout_secs.is_some()
                    || tool.parameters != empty_parameters()
                {
                    bail!(
                        "plugin '{}' tool '{}' is run by the host and takes only a name and description",
                        self.id,
                        tool.name
                    );
                }
            }
            if tool
                .timeout_secs
                .is_some_and(|secs| !(1..=86_400).contains(&secs))
            {
                bail!(
                    "plugin '{}' tool '{}' has an invalid timeout_secs; use 1..=86400",
                    self.id,
                    tool.name
                );
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
                && (!model.thinking_levels.is_empty()
                    || model.reasoning_level.is_some()
                    || model.image_input.is_some())
            {
                bail!(
                    "plugin '{}' custom model capabilities require protocol='plugin'",
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
                "before_tool_call"
                    | "after_tool_result"
                    | "before_model_request"
                    | "before_turn_end"
            ) {
                bail!("plugin '{}' declares unsupported hook '{hook}'", self.id);
            }
        }
        if let Some(ui) = &self.ui {
            for (slot, color) in &ui.colors {
                let hex = color.strip_prefix('#').unwrap_or(color);
                if !matches!(
                    slot.as_str(),
                    "accent"
                        | "background"
                        | "border"
                        | "text"
                        | "muted"
                        | "user"
                        | "tool"
                        | "code"
                ) || hex.len() != 6
                    || !hex.bytes().all(|ch| ch.is_ascii_hexdigit())
                {
                    bail!("plugin '{}' has an invalid UI color '{slot}'", self.id);
                }
            }
            if let Some(layout) = &ui.layout {
                if layout
                    .composer_max_rows
                    .is_some_and(|rows| !(3..=14).contains(&rows))
                {
                    bail!("plugin '{}' has invalid UI layout dimensions", self.id);
                }
                if layout
                    .desktop_content_width
                    .is_some_and(|width| !(360..=1400).contains(&width))
                {
                    bail!(
                        "plugin '{}' desktop content width must be between 360 and 1400 pixels",
                        self.id
                    );
                }
                if layout
                    .desktop_sidebar
                    .as_deref()
                    .is_some_and(|value| !matches!(value, "visible" | "hidden"))
                {
                    bail!(
                        "plugin '{}' desktop sidebar must be 'visible' or 'hidden'",
                        self.id
                    );
                }
                if layout
                    .desktop_density
                    .as_deref()
                    .is_some_and(|value| !matches!(value, "regular" | "compact"))
                {
                    bail!(
                        "plugin '{}' desktop density must be 'regular' or 'compact'",
                        self.id
                    );
                }
            }
            if !ui.activity_frames.is_empty()
                && (ui.activity_frames.len() < 2
                    || ui.activity_frames.len() > 12
                    || ui.activity_frames.iter().any(|frame| {
                        frame.trim().is_empty()
                            || frame.chars().any(char::is_control)
                            || unicode_width::UnicodeWidthStr::width(frame.as_str()) > 8
                    }))
            {
                bail!(
                    "plugin '{}' activity_frames must contain 2–12 short terminal-safe frames",
                    self.id
                );
            }
            for (tool, label) in &ui.tool_labels {
                if !valid_tool_name(tool)
                    || label.trim().is_empty()
                    || label.chars().count() > 48
                    || label.chars().any(char::is_control)
                {
                    bail!("plugin '{}' has an invalid tool display label", self.id);
                }
            }
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
            if let Some(desktop) = &ui.desktop {
                let entry = Path::new(desktop.entry.trim());
                if desktop.entry.trim().is_empty()
                    || entry.is_absolute()
                    || entry
                        .components()
                        .any(|component| !matches!(component, std::path::Component::Normal(_)))
                {
                    bail!(
                        "plugin '{}' desktop UI entry '{}' must be a safe relative path inside the plugin directory",
                        self.id,
                        desktop.entry
                    );
                }
                if !matches!(
                    desktop.slot.as_str(),
                    "sidebar" | "tab" | "above_composer" | "panel" | "workspace"
                ) {
                    bail!(
                        "plugin '{}' desktop UI slot '{}' must be sidebar, tab, above_composer, panel, or workspace",
                        self.id,
                        desktop.slot
                    );
                }
            }
            let mut bindings = BTreeSet::new();
            for shortcut in &ui.shortcuts {
                let key = parse_alt_shortcut(&shortcut.key).ok_or_else(|| {
                    anyhow::anyhow!(
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
                    | "compaction_started"
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

pub(super) fn valid_id(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(super) fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

const PLUGIN_TEMPLATE_MANIFEST: &str = r#"api_version = 2
id = "PLUGIN_ID"
name = "PLUGIN_NAME"
version = "0.1.0"
description = "一句话说明这个插件干什么"
command = "node"
args = ["main.js"]

# 每一样都要你显式写出来，装的人才知道它要干什么。
[[commands]]
name = "/PLUGIN_ID"
description = "命令说明：/PLUGIN_ID 后面跟的文字会原样交给插件"

[[tools]]
name = "PLUGIN_ID_do"
description = "给 AI 的工具：说明它做什么、什么时候该用"
"#;

const PLUGIN_TEMPLATE_SCRIPT: &str = r#"// 一个最小插件：读一行 JSON，回一行 JSON。
// 照着改就行；协议是 api_version 2。
const readline = require("node:readline");

const reply = (request, content, extra = {}) => {
  process.stdout.write(
    JSON.stringify({
      protocol_version: 2,
      request_id: request.request_id,
      content,
      ...extra,
    }) + "\n",
  );
};

const interface_ = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
interface_.on("line", (line) => {
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    return;
  }
  // 宿主自检会先发一个 ping，回什么都行，能回就证明协议通了。
  if (request.type === "ping") return reply(request, "ok");

  // 命令：想让 AI 立刻开干，就回一个 task，内容会当成用户说的话。
  if (request.type === "command") {
    const text = String(request.arguments || "").trim();
    if (!text) return reply(request, "用法：/PLUGIN_ID <要做的事>");
    return reply(request, `开跑：${text}`, { task: text });
  }

  // 工具：request.arguments 是模型给的参数。
  if (request.type === "tool") {
    const name = String(request.arguments?.name || "世界");
    return reply(request, `你好，${name}`);
  }

  // 钩子 / 事件：不需要就回空。before_turn_end 回
  // {"content":"{\"decision\":\"continue\",\"feedback\":\"...\"}"} 可以让 AI 继续干。
  return reply(request, "");
});
"#;

const PLUGIN_TEMPLATE_README: &str = r#"# PLUGIN_NAME

PLUGIN_TEMPLATE_DESCRIPTION

## 装

```
kroma plugins check .        # 先自检，不启动模型、不连网
kroma plugins install .      # 复制到 ~/.kroma/plugins/PLUGIN_ID
kroma plugins enable PLUGIN_ID
```

改完代码后重启 Kroma，或在界面里 `/plugins reload`。

## 写

- `plugin.toml` 声明它要加什么：命令、工具、钩子、界面、设置项。
- `main.js` 读一行 JSON、回一行 JSON。上面注释里三个分支就是全部协议。
- 状态想跨轮记住：写到宿主给的 `data_dir`，按 `session_id` 各存一个文件。

协议全文见 `docs/plugin-contract-v1.md`。
"#;

/// Create one ready-to-run plugin directory. The scaffold must pass
/// `kroma plugins check` before a single line is edited, so a new author starts
/// from a working protocol rather than a broken one.
pub(super) fn scaffold(id: &str) -> Result<String> {
    scaffold_in(&env::current_dir()?, id)
}

pub(super) fn scaffold_in(base: &Path, id: &str) -> Result<String> {
    if !valid_id(id) {
        bail!("plugin id may only contain letters, digits, '-' and '_'");
    }
    let target = base.join(id);
    if target.exists() {
        bail!("{} already exists", target.display());
    }
    let fill = |text: &str| {
        text.replace("PLUGIN_NAME", id)
            .replace("PLUGIN_TEMPLATE_DESCRIPTION", &format!("{id} 插件"))
            .replace("PLUGIN_ID", id)
    };
    fs::create_dir_all(&target)?;
    for (name, body) in [
        ("plugin.toml", PLUGIN_TEMPLATE_MANIFEST),
        ("main.js", PLUGIN_TEMPLATE_SCRIPT),
        ("README.md", PLUGIN_TEMPLATE_README),
    ] {
        fs::write(target.join(name), fill(body))?;
    }
    Ok(format!(
        "Created {} (plugin.toml, main.js, README.md).\n\n\
Try it without installing:\n  kroma plugins check {}\n\n\
Then install and enable:\n  kroma plugins install {}\n  kroma plugins enable {}\n\n\
After editing code, restart Kroma or run '/plugins reload'.",
        target.display(),
        target.display(),
        target.display(),
        id
    ))
}
