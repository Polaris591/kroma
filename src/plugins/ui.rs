//! Per-catalog live UI state and real-time event bus for plugins.
//!
//! Each `PluginCatalog` owns its own `PluginUiBus`, preventing cross-session or
//! parallel-test leaks. Frontends (TUI and Desktop) subscribe to real-time
//! `PluginUiEvent` updates instead of polling global state or waiting for a
//! turn to finish.
use crate::panel::{self, PanelBlock};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{mpsc as std_mpsc, Arc, Mutex},
};
use tokio::sync::{broadcast, Notify};

pub const MAX_STATUS_CHARS: usize = 160;
const MAX_WIDGET_TITLE_CHARS: usize = 120;
const MAX_WIDGET_ACTIONS: usize = 16;
const MAX_NOTIFY_CHARS: usize = 500;
const MAX_COMPOSER_CHARS: usize = 32_000;
const MAX_CUSTOM_LINES: usize = 200;
const MAX_CUSTOM_SPANS: usize = 128;
const UI_EVENT_CAPACITY: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiScope {
    #[default]
    Session,
    Global,
}

impl UiScope {
    pub fn from_value(value: Option<&Value>) -> Self {
        match value.and_then(Value::as_str) {
            Some("global") => Self::Global,
            _ => Self::Session,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WidgetPlacement {
    #[default]
    AboveComposer,
    BelowComposer,
    Sidebar,
    Header,
    Footer,
    /// Inside the message stream, so the block scrolls with the conversation.
    Transcript,
}

impl WidgetPlacement {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.unwrap_or("above_composer") {
            "above_composer" => Ok(Self::AboveComposer),
            "below_composer" => Ok(Self::BelowComposer),
            "sidebar" => Ok(Self::Sidebar),
            "header" => Ok(Self::Header),
            "footer" => Ok(Self::Footer),
            "transcript" => Ok(Self::Transcript),
            other => bail!(
                "unknown widget placement '{other}'; expected above_composer, below_composer, sidebar, header, footer, or transcript"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUiAction {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub danger: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginWidget {
    pub plugin_id: String,
    pub id: String,
    pub placement: WidgetPlacement,
    pub title: String,
    pub blocks: Vec<PanelBlock>,
    pub text: String,
    pub actions: Vec<PluginUiAction>,
    pub scope: UiScope,
}

impl PluginWidget {
    pub fn to_json(&self) -> Value {
        json!({
            "plugin_id": self.plugin_id,
            "id": self.id,
            "placement": self.placement,
            "title": self.title,
            "blocks": panel::to_json(&self.blocks),
            "text": self.text,
            "actions": self.actions,
            "scope": self.scope,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginLivePanel {
    pub plugin_id: String,
    pub title: String,
    pub blocks: Vec<PanelBlock>,
    pub text: String,
    pub actions: Vec<PluginUiAction>,
}

impl PluginLivePanel {
    pub fn to_json(&self) -> Value {
        json!({
            "plugin_id": self.plugin_id,
            "title": self.title,
            "blocks": panel::to_json(&self.blocks),
            "text": self.text,
            "actions": self.actions,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginUiEvent {
    StatusChanged {
        statuses: Vec<String>,
    },
    WidgetsChanged {
        widgets: Vec<Value>,
    },
    Notify {
        plugin_id: String,
        message: String,
        level: String,
    },
    ComposerSet {
        plugin_id: String,
        text: String,
        mode: String,
    },
    PanelOpened {
        plugin_id: String,
        title: String,
        blocks: Vec<Value>,
        text: String,
        actions: Vec<PluginUiAction>,
    },
    PanelClosed {
        plugin_id: String,
    },
    DesktopMessage {
        plugin_id: String,
        payload: Value,
    },
}

#[derive(Debug, Clone)]
pub struct CustomTuiSpec {
    pub plugin_id: String,
    pub title: String,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub fullscreen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomTuiSpan {
    pub text: String,
    #[serde(default)]
    pub fg: Option<String>,
    #[serde(default)]
    pub bg: Option<String>,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub dim: bool,
    #[serde(default)]
    pub reverse: bool,
}

#[derive(Debug, Clone)]
pub enum CustomTuiClientEvent {
    Render {
        width: u16,
        height: u16,
        theme: Value,
    },
    Input {
        key: String,
        ch: Option<String>,
        modifiers: Vec<String>,
        width: u16,
        height: u16,
    },
    Cancel,
}

impl CustomTuiClientEvent {
    pub fn to_request(&self) -> Value {
        match self {
            Self::Render {
                width,
                height,
                theme,
            } => json!({
                "type": "ui_render",
                "width": width,
                "height": height,
                "theme": theme,
            }),
            Self::Input {
                key,
                ch,
                modifiers,
                width,
                height,
            } => json!({
                "type": "ui_input",
                "key": key,
                "char": ch,
                "modifiers": modifiers,
                "width": width,
                "height": height,
            }),
            Self::Cancel => json!({"type": "ui_close"}),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CustomTuiFrameUpdate {
    pub lines: Vec<Vec<CustomTuiSpan>>,
    pub cursor: Option<(u16, u16)>,
    pub done: bool,
    pub result: Option<Value>,
}

impl CustomTuiFrameUpdate {
    pub fn from_value(value: &Value) -> Result<Self> {
        let done = value.get("done").and_then(Value::as_bool).unwrap_or(false);
        let result = value.get("result").cloned();
        let cursor = value
            .get("cursor")
            .and_then(Value::as_array)
            .and_then(|arr| {
                let x = arr.first()?.as_u64()? as u16;
                let y = arr.get(1)?.as_u64()? as u16;
                Some((x, y))
            });
        let mut lines = Vec::new();
        if let Some(raw_lines) = value.get("lines").and_then(Value::as_array) {
            for line in raw_lines.iter().take(MAX_CUSTOM_LINES) {
                match line {
                    Value::String(text) => {
                        lines.push(vec![CustomTuiSpan {
                            text: text.chars().take(1024).collect(),
                            fg: None,
                            bg: None,
                            bold: false,
                            dim: false,
                            reverse: false,
                        }]);
                    }
                    Value::Array(spans) => {
                        let mut row = Vec::new();
                        for span in spans.iter().take(MAX_CUSTOM_SPANS) {
                            if let Ok(mut parsed) =
                                serde_json::from_value::<CustomTuiSpan>(span.clone())
                            {
                                parsed.text = parsed.text.chars().take(1024).collect();
                                row.push(parsed);
                            }
                        }
                        lines.push(row);
                    }
                    _ => {}
                }
            }
        }
        Ok(Self {
            lines,
            cursor,
            done,
            result,
        })
    }
}

pub struct CustomTuiChannel {
    pub spec: CustomTuiSpec,
    pub client_tx: std_mpsc::Sender<CustomTuiClientEvent>,
    pub frame_rx: std_mpsc::Receiver<CustomTuiFrameUpdate>,
    pub wake: Arc<Notify>,
}

pub trait CustomTuiHost: Send + Sync {
    fn open(&self, channel: CustomTuiChannel) -> bool;
}

#[derive(Debug, Clone)]
struct StatusEntry {
    text: String,
    scope: UiScope,
}

pub struct PluginUiBus {
    /// Enabled plugin order and optional static `[ui] status` fallback.
    manifest_order: Mutex<Vec<(String, Option<String>)>>,
    /// Live `ui.status` lines keyed by `(plugin_id, status_key)`.
    statuses: Mutex<BTreeMap<(String, String), StatusEntry>>,
    /// Persistent `ui.widget` surfaces keyed by `(plugin_id, widget_id)`.
    widgets: Mutex<BTreeMap<(String, String), PluginWidget>>,
    /// Most recent `ui.panel` overlay/drawer state.
    live_panel: Mutex<Option<PluginLivePanel>>,
    /// Attached interactive terminal host for `ui.custom`.
    custom_tui: Mutex<Option<Arc<dyn CustomTuiHost>>>,
    tx: broadcast::Sender<PluginUiEvent>,
}

impl Default for PluginUiBus {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginUiBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(UI_EVENT_CAPACITY);
        Self {
            manifest_order: Mutex::new(Vec::new()),
            statuses: Mutex::new(BTreeMap::new()),
            widgets: Mutex::new(BTreeMap::new()),
            live_panel: Mutex::new(None),
            custom_tui: Mutex::new(None),
            tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PluginUiEvent> {
        self.tx.subscribe()
    }

    pub fn set_custom_tui_host(&self, host: Option<Arc<dyn CustomTuiHost>>) {
        if let Ok(mut slot) = self.custom_tui.lock() {
            *slot = host;
        }
    }

    pub fn custom_tui_host(&self) -> Option<Arc<dyn CustomTuiHost>> {
        self.custom_tui.lock().ok().and_then(|slot| slot.clone())
    }

    /// Synchronize enabled plugin order & static `[ui] status` fallbacks, and
    /// prune live entries belonging to plugins no longer enabled.
    pub fn sync_manifest(&self, enabled: &[(String, Option<String>)]) {
        let enabled_ids: BTreeSet<&str> = enabled.iter().map(|(id, _)| id.as_str()).collect();
        if let Ok(mut order) = self.manifest_order.lock() {
            *order = enabled.to_vec();
        }
        let mut status_changed = false;
        if let Ok(mut statuses) = self.statuses.lock() {
            let before = statuses.len();
            statuses.retain(|(plugin_id, _), _| enabled_ids.contains(plugin_id.as_str()));
            status_changed = statuses.len() != before;
        }
        let mut widgets_changed = false;
        if let Ok(mut widgets) = self.widgets.lock() {
            let before = widgets.len();
            widgets.retain(|(plugin_id, _), _| enabled_ids.contains(plugin_id.as_str()));
            widgets_changed = widgets.len() != before;
        }
        if let Ok(mut panel) = self.live_panel.lock() {
            if panel
                .as_ref()
                .is_some_and(|p| !enabled_ids.contains(p.plugin_id.as_str()))
            {
                let old = panel.take().expect("checked");
                let _ = self.tx.send(PluginUiEvent::PanelClosed {
                    plugin_id: old.plugin_id,
                });
            }
        }
        if status_changed {
            let _ = self.tx.send(PluginUiEvent::StatusChanged {
                statuses: self.statuses(),
            });
        }
        if widgets_changed {
            let _ = self.tx.send(PluginUiEvent::WidgetsChanged {
                widgets: self.widget_values(),
            });
        }
    }

    /// Clear session-scoped UI entries when switching or resetting a session.
    pub fn clear_session_ui(&self) {
        let mut status_changed = false;
        if let Ok(mut statuses) = self.statuses.lock() {
            let before = statuses.len();
            statuses.retain(|_, entry| entry.scope == UiScope::Global);
            status_changed = statuses.len() != before;
        }
        let mut widgets_changed = false;
        if let Ok(mut widgets) = self.widgets.lock() {
            let before = widgets.len();
            widgets.retain(|_, widget| widget.scope == UiScope::Global);
            widgets_changed = widgets.len() != before;
        }
        if status_changed {
            let _ = self.tx.send(PluginUiEvent::StatusChanged {
                statuses: self.statuses(),
            });
        }
        if widgets_changed {
            let _ = self.tx.send(PluginUiEvent::WidgetsChanged {
                widgets: self.widget_values(),
            });
        }
    }

    pub fn set_status(&self, plugin_id: &str, key: &str, text: &str, scope: UiScope) {
        let text: String = text.trim().chars().take(MAX_STATUS_CHARS).collect();
        let key: String = key.trim().chars().take(64).collect();
        if let Ok(mut statuses) = self.statuses.lock() {
            if text.is_empty() {
                statuses.remove(&(plugin_id.to_owned(), key));
            } else {
                statuses.insert((plugin_id.to_owned(), key), StatusEntry { text, scope });
            }
        }
        let _ = self.tx.send(PluginUiEvent::StatusChanged {
            statuses: self.statuses(),
        });
    }

    pub fn statuses(&self) -> Vec<String> {
        let live = self
            .statuses
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let order = self
            .manifest_order
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let mut out = Vec::new();
        let mut seen_plugins = BTreeSet::new();
        for (plugin_id, fallback) in order {
            seen_plugins.insert(plugin_id.clone());
            let entries: Vec<String> = live
                .iter()
                .filter(|((id, _), _)| id == &plugin_id)
                .map(|(_, entry)| entry.text.clone())
                .filter(|s| !s.trim().is_empty())
                .collect();
            if !entries.is_empty() {
                out.extend(entries);
            } else if let Some(static_line) = fallback.filter(|s| !s.trim().is_empty()) {
                out.push(static_line);
            }
        }
        // Standalone worker tests without catalog sync still surface their status.
        for ((plugin_id, _), entry) in live {
            if !seen_plugins.contains(&plugin_id) && !entry.text.trim().is_empty() {
                out.push(entry.text);
            }
        }
        out
    }

    pub fn notify(&self, plugin_id: &str, message: &str, level: &str) {
        let message: String = message.trim().chars().take(MAX_NOTIFY_CHARS).collect();
        if message.is_empty() {
            return;
        }
        let level = match level {
            "warning" | "warn" => "warning",
            "error" => "error",
            _ => "info",
        }
        .to_owned();
        let _ = self.tx.send(PluginUiEvent::Notify {
            plugin_id: plugin_id.to_owned(),
            message,
            level,
        });
    }

    pub fn set_composer(&self, plugin_id: &str, text: &str, mode: &str) {
        let text: String = text.chars().take(MAX_COMPOSER_CHARS).collect();
        let mode = match mode {
            "append" => "append",
            _ => "replace",
        }
        .to_owned();
        let _ = self.tx.send(PluginUiEvent::ComposerSet {
            plugin_id: plugin_id.to_owned(),
            text,
            mode,
        });
    }

    pub fn set_widget(&self, widget: PluginWidget) {
        if let Ok(mut widgets) = self.widgets.lock() {
            widgets.insert((widget.plugin_id.clone(), widget.id.clone()), widget);
        }
        let _ = self.tx.send(PluginUiEvent::WidgetsChanged {
            widgets: self.widget_values(),
        });
    }

    pub fn remove_widget(&self, plugin_id: &str, id: &str) {
        let removed = if let Ok(mut widgets) = self.widgets.lock() {
            widgets
                .remove(&(plugin_id.to_owned(), id.to_owned()))
                .is_some()
        } else {
            false
        };
        if removed {
            let _ = self.tx.send(PluginUiEvent::WidgetsChanged {
                widgets: self.widget_values(),
            });
        }
    }

    pub fn widgets(&self) -> Vec<PluginWidget> {
        self.widgets
            .lock()
            .map(|guard| guard.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn widget_values(&self) -> Vec<Value> {
        self.widgets().iter().map(PluginWidget::to_json).collect()
    }

    pub fn open_panel(&self, panel: PluginLivePanel) {
        let event = PluginUiEvent::PanelOpened {
            plugin_id: panel.plugin_id.clone(),
            title: panel.title.clone(),
            blocks: panel::to_json(&panel.blocks),
            text: panel.text.clone(),
            actions: panel.actions.clone(),
        };
        if let Ok(mut guard) = self.live_panel.lock() {
            *guard = Some(panel);
        }
        let _ = self.tx.send(event);
    }

    pub fn close_panel(&self, plugin_id: &str) {
        if let Ok(mut guard) = self.live_panel.lock() {
            if guard
                .as_ref()
                .is_some_and(|panel| panel.plugin_id == plugin_id)
            {
                *guard = None;
            }
        }
        let _ = self.tx.send(PluginUiEvent::PanelClosed {
            plugin_id: plugin_id.to_owned(),
        });
    }

    pub fn live_panel(&self) -> Option<PluginLivePanel> {
        self.live_panel.lock().ok().and_then(|guard| guard.clone())
    }

    pub fn emit_desktop(&self, plugin_id: &str, payload: Value) {
        let _ = self.tx.send(PluginUiEvent::DesktopMessage {
            plugin_id: plugin_id.to_owned(),
            payload,
        });
    }
}

pub fn parse_actions(raw: Option<&Value>) -> Vec<PluginUiAction> {
    let Some(items) = raw.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    for item in items.iter().take(MAX_WIDGET_ACTIONS) {
        let Some(id) = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let label = item
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(id);
        actions.push(PluginUiAction {
            id: id.chars().take(64).collect(),
            label: label.chars().take(64).collect(),
            primary: item
                .get("primary")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            danger: item.get("danger").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    actions
}

pub fn parse_blocks_from_args(args: &Value) -> (Vec<PanelBlock>, String) {
    if let Some(blocks_val) = args.get("blocks") {
        let raw = blocks_val.to_string();
        let blocks = panel::parse(&raw);
        let text = panel::render_text(&blocks, 76).join("\n");
        return (blocks, text);
    }
    if let Some(content) = args.get("content") {
        if content.is_string() {
            let s = content.as_str().unwrap_or("");
            let blocks = panel::parse(s);
            let text = panel::render_text(&blocks, 76).join("\n");
            return (blocks, text);
        }
        let raw = content.to_string();
        let blocks = panel::parse(&raw);
        let text = panel::render_text(&blocks, 76).join("\n");
        return (blocks, text);
    }
    if let Some(text) = args.get("text").and_then(Value::as_str) {
        let blocks = panel::parse(text);
        let rendered = panel::render_text(&blocks, 76).join("\n");
        return (blocks, rendered);
    }
    (Vec::new(), String::new())
}

pub fn parse_widget_args(plugin_id: &str, args: &Value) -> Result<Option<PluginWidget>> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .chars()
        .take(64)
        .collect::<String>();
    if args.get("clear").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(None);
    }
    let (blocks, text) = parse_blocks_from_args(args);
    let actions = parse_actions(args.get("actions"));
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .map(|s| {
            s.trim()
                .chars()
                .take(MAX_WIDGET_TITLE_CHARS)
                .collect::<String>()
        })
        .unwrap_or_default();
    if blocks.is_empty() && actions.is_empty() && title.is_empty() {
        return Ok(None);
    }
    let placement = WidgetPlacement::parse(args.get("placement").and_then(Value::as_str))?;
    // A transcript block is conversation content, not a control surface: its
    // buttons could not be clicked in a scrolling, selectable stream.
    if placement == WidgetPlacement::Transcript && !actions.is_empty() {
        bail!("transcript widgets render content only; use above_composer for buttons");
    }
    let scope = UiScope::from_value(args.get("scope"));
    Ok(Some(PluginWidget {
        plugin_id: plugin_id.to_owned(),
        id,
        placement,
        title,
        blocks,
        text,
        actions,
        scope,
    }))
}

pub fn clip_title(raw: Option<&str>, fallback: &str) -> String {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .chars()
        .take(MAX_WIDGET_TITLE_CHARS)
        .collect()
}

const MAX_DESKTOP_ENTRY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DESKTOP_BUNDLE_BYTES: usize = 4 * 1024 * 1024;

fn read_contained_file(
    plugin_root: &std::path::Path,
    base_dir: &std::path::Path,
    rel: &str,
) -> Result<String> {
    let trimmed = rel.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("//")
        || trimmed.starts_with("data:")
        || trimmed.starts_with('/')
        || trimmed.starts_with('\\')
    {
        bail!("external or absolute desktop UI asset '{trimmed}' is not allowed");
    }
    let rel_no_query = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    let candidate = base_dir.join(rel_no_query).canonicalize()?;
    if !candidate.starts_with(plugin_root) || !candidate.is_file() {
        bail!("desktop UI asset '{trimmed}' escapes plugin directory");
    }
    let meta = std::fs::metadata(&candidate)?;
    if meta.len() > MAX_DESKTOP_ENTRY_BYTES {
        bail!("desktop UI asset '{trimmed}' exceeds 2 MiB");
    }
    Ok(std::fs::read_to_string(&candidate)?)
}

fn inline_local_assets(
    plugin_root: &std::path::Path,
    entry_dir: &std::path::Path,
    html: &str,
) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after_lt = &rest[lt..];
        let lower = after_lt.get(..64).unwrap_or(after_lt).to_ascii_lowercase();
        if lower.starts_with("<script") {
            if let Some(gt) = after_lt.find('>') {
                let tag = &after_lt[..=gt];
                let after_tag = &after_lt[gt + 1..];
                if let Some(close) = after_tag.to_ascii_lowercase().find("</script>") {
                    if let Some(src) = extract_attr(tag, "src") {
                        if let Ok(code) = read_contained_file(plugin_root, entry_dir, &src) {
                            let safe = code.replace("</script", "<\\/script");
                            out.push_str("<script>");
                            out.push_str(&safe);
                            out.push_str("</script>");
                            rest = &after_tag[close + "</script>".len()..];
                            continue;
                        }
                    }
                }
            }
        } else if lower.starts_with("<link") {
            if let Some(gt) = after_lt.find('>') {
                let tag = &after_lt[..=gt];
                let rel = extract_attr(tag, "rel").unwrap_or_default();
                if rel.eq_ignore_ascii_case("stylesheet") {
                    if let Some(href) = extract_attr(tag, "href") {
                        if let Ok(css) = read_contained_file(plugin_root, entry_dir, &href) {
                            let safe = css.replace("</style", "<\\/style");
                            out.push_str("<style>");
                            out.push_str(&safe);
                            out.push_str("</style>");
                            rest = &after_lt[gt + 1..];
                            continue;
                        }
                    }
                }
            }
        }
        out.push('<');
        rest = &after_lt[1..];
    }
    out.push_str(rest);
    out
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let pos = lower.find(&needle)?;
    let after = tag[pos + needle.len()..].trim_start();
    let quote = after.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let rest = &after[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_owned())
}

fn bridge_script(plugin_id: &str) -> String {
    let id_json = serde_json::to_string(plugin_id).unwrap_or_else(|_| "\"\"".to_owned());
    format!(
        "<script>(function(){{\
if(window.kroma)return;\
var n=1,p=new Map(),l=new Set(),id={id_json};\
window.addEventListener('message',function(e){{\
var d=e.data;if(!d||d.__kroma_host!==true||d.plugin_id!==id)return;\
if(d.type==='response'&&p.has(d.id)){{var x=p.get(d.id);p.delete(d.id);d.ok?x.resolve(d.result):x.reject(new Error(d.error||'request failed'));}}\
else if(d.type==='event'){{l.forEach(function(fn){{try{{fn(d.payload);}}catch(_){{}}}});}}\
}});\
function post(m){{window.parent.postMessage(Object.assign({{__kroma_plugin:true,plugin_id:id}},m),'*');}}\
window.kroma={{\
pluginId:id,\
request:function(payload){{return new Promise(function(resolve,reject){{var rid=n++;p.set(rid,{{resolve:resolve,reject:reject}});post({{type:'request',id:rid,payload:payload}});}});}},\
runTask:function(text){{post({{type:'run_task',text:String(text==null?'':text)}});}},\
setComposer:function(text,mode){{post({{type:'set_composer',text:String(text==null?'':text),mode:mode||'replace'}});}},\
notify:function(message,level){{post({{type:'notify',message:String(message==null?'':message),level:level||'info'}});}},\
onEvent:function(fn){{l.add(fn);return function(){{l.delete(fn);}};}}\
}};\
}})();</script>"
    )
}

pub fn bundle_desktop_html(
    plugin_dir: &std::path::Path,
    entry: &str,
    plugin_id: &str,
) -> Result<String> {
    let root = plugin_dir.canonicalize()?;
    let entry_path = plugin_dir.join(entry.trim()).canonicalize()?;
    if !entry_path.starts_with(&root) || !entry_path.is_file() {
        bail!("desktop UI entry '{entry}' is outside the plugin directory");
    }
    let meta = std::fs::metadata(&entry_path)?;
    if meta.len() > MAX_DESKTOP_ENTRY_BYTES {
        bail!("desktop UI entry '{entry}' exceeds 2 MiB");
    }
    let html = std::fs::read_to_string(&entry_path)?;
    let entry_dir = entry_path.parent().unwrap_or(&root);
    let inlined = inline_local_assets(&root, entry_dir, &html);
    let bridge = bridge_script(plugin_id);
    let bundled = if let Some(head_end) = inlined.to_ascii_lowercase().find("<head>") {
        let insert_at = head_end + "<head>".len();
        format!("{}{bridge}{}", &inlined[..insert_at], &inlined[insert_at..])
    } else {
        format!("{bridge}{inlined}")
    };
    if bundled.len() > MAX_DESKTOP_BUNDLE_BYTES {
        bail!("desktop UI bundle exceeds 4 MiB");
    }
    Ok(bundled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_is_isolated_per_catalog_and_broadcasts_live_updates() {
        let first = PluginUiBus::new();
        let second = PluginUiBus::new();
        let mut rx = first.subscribe();
        first.sync_manifest(&[("alpha".into(), Some("idle".into()))]);
        first.set_status("alpha", "", "running 1/3", UiScope::Session);
        assert_eq!(first.statuses(), vec!["running 1/3".to_string()]);
        assert!(second.statuses().is_empty());

        let event = rx.try_recv().expect("live event broadcast");
        assert!(matches!(
            event,
            PluginUiEvent::StatusChanged { statuses } if statuses == vec!["running 1/3"]
        ));

        first.clear_session_ui();
        assert_eq!(first.statuses(), vec!["idle".to_string()]);
    }

    #[test]
    fn global_scope_survives_session_reset_while_session_scope_clears() {
        let bus = PluginUiBus::new();
        bus.set_status("hud", "global", "repo: main", UiScope::Global);
        bus.set_status("goal", "", "fix login", UiScope::Session);
        assert_eq!(bus.statuses().len(), 2);
        bus.clear_session_ui();
        assert_eq!(bus.statuses(), vec!["repo: main".to_string()]);
    }

    #[test]
    fn widget_parsing_and_clearing_work() -> Result<()> {
        let widget = parse_widget_args(
            "demo",
            &json!({
                "id": "tasks",
                "placement": "sidebar",
                "title": "Tasks",
                "blocks": [{"type": "progress", "label": "Done", "value": 2, "max": 4}],
                "actions": [{"id": "refresh", "label": "Refresh", "primary": true}]
            }),
        )?
        .expect("widget");
        assert_eq!(widget.placement, WidgetPlacement::Sidebar);
        assert_eq!(widget.actions.len(), 1);
        let clear = parse_widget_args("demo", &json!({"id": "tasks", "clear": true}))?;
        assert!(clear.is_none());
        Ok(())
    }
}
