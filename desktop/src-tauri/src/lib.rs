//! Tauri commands only translate GUI actions into the existing Kroma Rust host.
//! No second Agent Loop, HTTP server, or terminal subprocess.
mod plugins_ui;
mod workspace_review;

use anyhow::{bail, Context, Result};
use kroma::{
    app::{self, DefaultRuntime},
    config::Config,
    desktop_host::{self, DesktopEvents},
    events::EventSink,
    host_attachments, mcp,
    plugins::PluginCatalog,
    profiles::{Profiles, ProviderProfile},
    sessions::SessionCatalog,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tauri::{ipc::Channel, AppHandle, Manager, State};
use tokio::sync::watch;

pub(crate) struct Host {
    project: PathBuf,
    generation: u64,
    /// Monotonic token for concurrent IPC connect attempts. Only the newest
    /// attempt may publish a runtime; older completions are discarded.
    connect_epoch: u64,
    runtime: Option<DefaultRuntime>,
    /// Set while one IPC command temporarily took the runtime out of the host.
    /// Every other command that needs an idle, connected host must see this,
    /// not only an in-flight task.
    runtime_owned: Option<RuntimeOwner>,
    /// Monotonic source of runtime_owned tokens.
    runtime_lease: u64,
    /// Last transcript projection published for this project. A command that
    /// owns the runtime must not make the open conversation disappear.
    last_snapshot: Option<Value>,
    connected_profile: Option<String>,
    events: Arc<DesktopEvents>,
    cancel: Option<watch::Sender<bool>>,
    task_input: Option<kroma::steering::TaskInput>,
    /// Plugin sources as of the last load, so a plugin installed from a
    /// terminal is picked up by the next message instead of a restart.
    plugins_fingerprint: u64,
    /// One question at a time from a plugin, delivered to the open window.
    asker: Arc<DesktopAsker>,
    app_handle: Option<AppHandle>,
    ui_forwarder: Option<tauri::async_runtime::JoinHandle<()>>,
    /// Cloned catalog + active session coordinates so UI actions & custom panels
    /// still work while a turn holds `runtime`.
    active_plugins: Option<(PluginCatalog, String, PathBuf)>,
}

/// A plugin's question reaching the window. The plugin's own worker thread
/// waits on the matching sender; the window answers with `answer_question`.
#[derive(Default)]
struct DesktopAsker {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<kroma::tools::AskAnswer>>>,
    /// The channel of the message that is running right now, if any.
    channel: Mutex<Option<Channel<Value>>>,
}

impl DesktopAsker {
    fn set_channel(&self, channel: Option<Channel<Value>>) {
        if let Ok(mut current) = self.channel.lock() {
            *current = channel;
        }
    }

    /// Answer one open question. Unknown or stale ids are ignored: a question
    /// that already timed out must never be "answered" later.
    fn answer(&self, id: u64, answer: kroma::tools::AskAnswer) -> bool {
        let sender = self.pending.lock().ok().and_then(|mut map| map.remove(&id));
        sender.is_some_and(|sender| sender.send(answer).is_ok())
    }

    /// A finished or cancelled turn leaves no answerable question behind.
    fn clear_pending(&self) {
        if let Ok(mut map) = self.pending.lock() {
            map.clear();
        }
    }
}

impl kroma::tools::Asker for DesktopAsker {
    fn ask<'a>(
        &'a self,
        request: kroma::tools::AskRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<kroma::tools::AskAnswer>> + Send + 'a>,
    > {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let channel = self
                .channel
                .lock()
                .ok()
                .and_then(|channel| channel.as_ref().cloned())?;
            let (sender, wait) = tokio::sync::oneshot::channel();
            self.pending.lock().ok()?.insert(id, sender);
            let sent = channel
                .send(json!({
                    "kind": "ask",
                    "ask": {
                        "id": id,
                        "question": request.question,
                        "options": request.options,
                        "multiple": request.multiple,
                    }
                }))
                .is_ok();
            if !sent {
                self.pending.lock().ok()?.remove(&id);
                return None;
            }
            wait.await.ok()
        })
    }
}

/// Cloned cheaply so blocking work can run on Tauri's blocking pool without
/// holding the UI command thread.
#[derive(Clone)]
pub(crate) struct DesktopState(Arc<Mutex<Host>>);

pub(crate) fn ui_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn home() -> Result<PathBuf> {
    let raw = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("HOME/USERPROFILE is unavailable")?;
    // Every project the Desktop opens is canonicalized before it becomes the
    // Agent's workdir, and the session catalog is keyed by that exact string.
    // Keeping the raw home path would give one directory two catalogs: the
    // sidebar would show it twice and deleting its conversations would look
    // in the wrong catalog ("session ... does not exist").
    Ok(raw.canonicalize().unwrap_or(raw))
}

/// Resolve an attachment path. Any file the user picks is allowed: they chose it
/// in a native dialog, and blocking other drives only broke ordinary work.
fn ensure_attachment_path(raw: &Path) -> Result<PathBuf> {
    let full = raw
        .canonicalize()
        .with_context(|| format!("附件不存在：{}", raw.display()))?;
    if !full.is_file() {
        bail!("只能添加文件：{}", full.display());
    }
    Ok(full)
}

/// Validate a Markdown export destination. The file itself may not exist yet,
/// so its already existing parent directory is what must be writable.
fn ensure_export_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("md"))
    {
        bail!("请选择有效的绝对 Markdown 文件保存路径（.md）");
    }
    let name = path.file_name().context("请选择有效的 Markdown 文件名")?;
    let parent = path.parent().context("请选择有效的 Markdown 文件名")?;
    let parent = parent
        .canonicalize()
        .with_context(|| format!("保存目录不存在：{}", parent.display()))?;
    Ok(parent.join(name))
}

/// Only a real PNG/JPEG/WebP image can be stored as a pasted screenshot; the
/// renderer must not use this command as a general file writer.
const MAX_PASTED_IMAGE_BYTES: usize = 8 * 1024 * 1024;

fn pasted_image_kind(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.starts_with(PNG) {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

fn validate_pasted_image(bytes: &[u8], extension: &str) -> Result<&'static str> {
    if bytes.is_empty() {
        bail!("粘贴的图片为空，请重新复制后再试");
    }
    if bytes.len() > MAX_PASTED_IMAGE_BYTES {
        bail!("粘贴的图片超过 8 MiB，请压缩后再试");
    }
    pasted_image_kind(bytes)
        .with_context(|| format!("仅支持 PNG / JPEG / WebP 图片，无法保存 .{extension} 内容"))
}

fn last_project_file() -> Result<PathBuf> {
    Ok(home()?.join(".kroma").join("desktop.json"))
}
/// A short recent-project list is UI metadata only; never scans a whole disk.
fn recent_projects(project: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![project.to_path_buf()];
    if let Ok(path) = last_project_file() {
        if let Ok(bytes) = fs::read(path) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(previous) = value["project"].as_str() {
                    candidates.push(PathBuf::from(previous));
                }
                if let Some(entries) = value["recent_projects"].as_array() {
                    for entry in entries.iter().filter_map(Value::as_str) {
                        candidates.push(PathBuf::from(entry));
                    }
                }
            }
        }
    }
    let mut unique = Vec::new();
    for candidate in candidates {
        if !candidate.is_dir() {
            continue;
        }
        if !unique.contains(&candidate) {
            unique.push(candidate);
        }
        if unique.len() == 6 {
            break;
        }
    }
    unique
}

/// A free, deterministic title rather than a model request for every chat.
/// Never copy obvious credentials or multiline logs into a visible sidebar.
fn initial_session_title(input: &str) -> Option<String> {
    let first = input.lines().map(str::trim).find(|line| !line.is_empty())?;
    let lower = first.to_ascii_lowercase();
    if first.starts_with('`')
        || first.starts_with('{')
        || first.starts_with('[')
        || [
            "sk-",
            "ghp_",
            "gho_",
            "bearer ",
            "password=",
            "api_key=",
            "api-key:",
            "token=",
            "-----begin",
            "://",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return None;
    }
    let title = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = title.chars().take(48).collect::<String>();
    (!title.is_empty()).then_some(title)
}

/// Lazily label legacy untitled conversations when they are actually opened.
fn backfill_session_title(agent: &kroma::agent::Agent) -> Option<String> {
    let first = agent
        .messages()
        .iter()
        .find(|message| message.role == "user")
        .and_then(|message| message.content.as_deref())
        .and_then(initial_session_title)?;
    let catalog = SessionCatalog::new(agent.workdir()).ok()?;
    if catalog.has_name(agent.session_id()).ok()? {
        return None;
    }
    match catalog.name_session_if_unnamed(agent.session_id(), &first) {
        Ok(true) => Some(first),
        Ok(false) => None,
        Err(error) => {
            eprintln!("[desktop] cannot update legacy session title: {error:#}");
            None
        }
    }
}

fn initial_project() -> PathBuf {
    last_project_file()
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|v| v["project"].as_str().map(PathBuf::from))
        .filter(|p| p.is_dir())
        // File-manager launches commonly inherit the executable's directory.
        // Never treat the installation/build directory as an agent workspace.
        .unwrap_or_else(|| home().unwrap_or_else(|_| std::env::current_dir().unwrap_or_default()))
}

impl DesktopState {
    fn new() -> Self {
        let asker = Arc::new(DesktopAsker::default());
        // Plugins reach the same window the model's ask tool uses.
        kroma::tools::set_host_asker(asker.clone());
        Self(Arc::new(Mutex::new(Host {
            project: initial_project(),
            generation: 0,
            connect_epoch: 0,
            runtime: None,
            runtime_owned: None,
            runtime_lease: 0,
            last_snapshot: None,
            connected_profile: None,
            events: Arc::new(DesktopEvents::default()),
            cancel: None,
            task_input: None,
            plugins_fingerprint: 0,
            asker,
            app_handle: None,
            ui_forwarder: None,
            active_plugins: None,
        })))
    }
}

/// Why the runtime is temporarily outside the host. The token proves that the
/// command which took it is also the one putting it back.
#[derive(Clone, Copy)]
struct RuntimeOwner {
    token: u64,
    purpose: RuntimePurpose,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RuntimePurpose {
    Compact,
    Rewind,
    Fork,
}

impl RuntimePurpose {
    fn busy_message(self) -> &'static str {
        match self {
            Self::Compact => "正在压缩上下文，请稍候",
            Self::Rewind => "正在回退本轮对话，请稍候",
            Self::Fork => "正在从该轮分叉新会话，请稍候",
        }
    }
}

pub(crate) fn require_idle(host: &Host) -> Result<()> {
    if host.cancel.is_some() {
        bail!("A task is running. Cancel or finish it before changing sessions or plugins.");
    }
    if let Some(owner) = host.runtime_owned {
        bail!("{}", owner.purpose.busy_message());
    }
    Ok(())
}

/// Take the runtime for one IPC command. While the lease is held, every other
/// command that needs an idle host is refused instead of observing a host with
/// no model and clobbering the transcript.
fn claim_runtime(
    host: &mut Host,
    purpose: RuntimePurpose,
) -> Result<(DefaultRuntime, RuntimeOwner)> {
    require_idle(host)?;
    let runtime = host.runtime.take().context("Connect a model first")?;
    host.runtime_lease = host.runtime_lease.wrapping_add(1);
    let owner = RuntimeOwner {
        token: host.runtime_lease.max(1),
        purpose,
    };
    host.runtime_owned = Some(owner);
    Ok((runtime, owner))
}

/// Put a claimed runtime back. The token is validated and cleared first; a
/// mismatched lease is logged instead of silently publishing a stale runtime.
fn return_runtime(host: &mut Host, runtime: DefaultRuntime, owner: RuntimeOwner) {
    if host
        .runtime_owned
        .is_some_and(|current| current.token != owner.token)
    {
        eprintln!("[desktop] runtime lease mismatch; restoring the newest runtime");
    }
    host.runtime_owned = None;
    host.runtime = Some(runtime);
}

pub(crate) async fn with_host<T, F>(
    state: State<'_, DesktopState>,
    work: F,
) -> std::result::Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&mut Host) -> Result<T> + Send + 'static,
{
    let state = state.inner().0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut host = state.lock().map_err(ui_error)?;
        work(&mut host).map_err(ui_error)
    })
    .await
    .map_err(ui_error)?
}

fn profile_list() -> Result<Value> {
    let profiles = Profiles::load()?;
    Ok(json!({
        "active": profiles.active,
        "profiles": profiles.profiles.iter().map(|(id,p)|json!({
            "name":id,"base_url":p.base_url,"model":p.model,
            "has_key":p.api_key.as_ref().is_some_and(|k|!k.is_empty()),
            "protocol":p.api_protocol.as_deref().unwrap_or("chat"),
            "reasoning":p.reasoning_effort.as_deref().unwrap_or("auto"),
            "deepseek_thinking":p.deepseek_thinking.as_deref().unwrap_or("off"),
            "context_window":p.context_window,"max_output_tokens":p.max_output_tokens,
            "native_web_search":p.native_web_search(),
            "thinking_levels":p.thinking_levels
        })).collect::<Vec<_>>()
    }))
}

/// A compact navigation tree. Ordinary chats live in the home workspace,
/// while coding chats belong to their own workdir's existing session catalog.
fn session_groups_for(project: &Path) -> Vec<Value> {
    let global = home().ok().filter(|path| path.is_dir());
    let mut dirs = recent_projects(project);
    if let Some(root) = global.as_ref() {
        dirs.retain(|path| path != root);
        dirs.insert(0, root.to_path_buf());
    }
    dirs.into_iter()
        .filter_map(|path| {
            let sessions = sessions_for(&path).ok()?;
            Some(json!({
                "project":path,
                "is_default":global.as_deref() == Some(path.as_path()),
                "sessions":sessions,
            }))
        })
        .collect()
}

fn sessions_for(project: &Path) -> Result<Value> {
    let catalog = SessionCatalog::new(project)?;
    let recent = catalog.recent()?;
    let mut entries = Vec::new();
    for entry in catalog.list()? {
        entries.push(json!({
            "id":entry.id,
            "name":entry.name,
            "recent":recent.as_deref()==Some(&entry.id),
            "updated_at":entry.modified.and_then(|time| {
                time.duration_since(std::time::UNIX_EPOCH).ok().map(|duration| duration.as_secs())
            }),
        }));
    }
    Ok(json!(entries))
}

fn view(host: &mut Host) -> Result<Value> {
    let snapshot = if let Some(runtime) = &host.runtime {
        let current = desktop_host::snapshot_with_plugins(&runtime.agent, Some(&runtime.plugins))?;
        host.last_snapshot = Some(current.clone());
        Some(current)
    } else {
        // Ready still means "a runtime is installed", but the open
        // conversation keeps its last durable projection while a command owns
        // the runtime; it must never flash as an empty/new session.
        host.last_snapshot.clone()
    };
    let plugins = PluginCatalog::discover(&host.project)?;
    Ok(json!({
        "project":host.project,
        "recent_projects":recent_projects(&host.project),
        "default_project":home().ok(),
        // Only use the actual OS user profile for a friendly UI alias.
        // This is display metadata; never rewrite or merge session catalogs.
        "user_home":std::env::var_os("USERPROFILE").or_else(||std::env::var_os("HOME")).map(PathBuf::from),
        "session_groups":session_groups_for(&host.project),
        "ready":host.runtime.is_some(),
        "running":host.cancel.is_some(),
        // Capture the actual connected backend, not the editable default
        // profile (which may change without rebuilding this runtime).
        "connected_model": host.runtime.as_ref().and_then(|runtime| {
            runtime.selected_model_name.clone().or_else(||
                runtime.agent.model_report().map(|report| report.model.clone()))
        }),
        "connected_profile":host.runtime.as_ref().and(host.connected_profile.as_ref()),
        "reasoning_level":host.runtime.as_ref().and_then(|runtime|runtime.agent.reasoning_level()),
        "thinking_levels":host.runtime.as_ref().map(|runtime|runtime.agent.thinking_levels()),
        "snapshot":snapshot,
        "sessions":sessions_for(&host.project)?,
        "profiles":profile_list()?,
        "plugins":plugins.summaries()?,
        "plugin_warnings":plugins.warnings,
        "mcp_project":{
            "configured":host.project.join(".kroma/mcp.json").is_file(),
            "trusted":mcp::project_mcp_trusted(&host.project).unwrap_or(false),
        },
    }))
}

#[tauri::command]
async fn bootstrap(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    with_host(state, view).await
}

/// Read-only review: the user's full working tree, including pre-existing
/// edits. This is deliberately not called "the changes made by this task".
/// Search only user-authored text in a bounded set of recent session
/// archives. The scan runs on a blocking pool and never holds the Agent mutex.
#[tauri::command]
async fn search_sessions(
    state: State<'_, DesktopState>,
    query: String,
) -> std::result::Result<Value, String> {
    let trimmed = query.trim();
    if trimmed.chars().count() < 2 || trimmed.chars().count() > 120 {
        return Ok(json!({"hits":[]}));
    }
    let projects = {
        let host = state.0.lock().map_err(ui_error)?;
        let mut projects = recent_projects(&host.project);
        if let Ok(home) = home() {
            if home.is_dir() && !projects.contains(&home) {
                projects.insert(0, home);
            }
        }
        projects.truncate(6);
        projects
    };
    tauri::async_runtime::spawn_blocking(move || -> Result<Value> {
        let mut hits = Vec::new();
        for project in projects {
            let catalog = match SessionCatalog::new(&project) {
                Ok(catalog) => catalog,
                Err(_) => continue,
            };
            let remaining = 24_usize.saturating_sub(hits.len());
            if remaining == 0 {
                break;
            }
            match catalog.search_user_messages(&query, 25, remaining) {
                Ok(entries) => {
                    for entry in entries {
                        hits.push(json!({
                            "project":project,
                            "id":entry.id,
                            "snippet":entry.snippet,
                        }));
                    }
                }
                Err(error) => {
                    eprintln!("[desktop] search skipped {}: {error:#}", project.display());
                }
            }
        }
        Ok(json!({"hits":hits}))
    })
    .await
    .map_err(ui_error)?
    .map_err(ui_error)
}

/// Per-turn snapshot summaries. The anchor is the transcript length the
/// snapshot precedes, so the UI can pair stored snapshots with rendered turns
/// without assuming one snapshot per user message.
#[tauri::command]
async fn checkpoint_turns(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    with_host(state, |host| {
        let Some(runtime) = host.runtime.as_ref() else {
            return Ok(json!({"turns": [], "diffs": []}));
        };
        let workdir = runtime.agent.workdir();
        let session = runtime.agent.session_id();
        let turns = kroma::checkpoints::snapshot_turns(workdir, session)?;
        let diffs = kroma::checkpoints::turn_diffs(workdir, session, 25)?;
        Ok(json!({
            "turns": turns.iter()
                .map(|entry| json!({"turn": entry.turn, "anchor": entry.anchor}))
                .collect::<Vec<_>>(),
            "diffs": diffs.iter()
                .map(|diff| json!({
                    "turn": diff.turn,
                    "added": diff.added,
                    "removed": diff.removed,
                    "files": diff.files.len(),
                    "truncated": diff.truncated,
                }))
                .collect::<Vec<_>>(),
        }))
    })
    .await
}

#[tauri::command]
async fn turn_files(
    state: State<'_, DesktopState>,
    turn: u64,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        let Some(runtime) = host.runtime.as_ref() else {
            return Ok(json!([]));
        };
        let diff = kroma::checkpoints::turn_diff(
            runtime.agent.workdir(),
            runtime.agent.session_id(),
            turn,
        )?;
        Ok(json!(diff
            .map(|diff| diff
                .files
                .iter()
                .map(|file| json!({
                    "path": file.path,
                    "status": file.status,
                    "added": file.added,
                    "removed": file.removed,
                }))
                .collect::<Vec<_>>())
            .unwrap_or_default()))
    })
    .await
}

#[tauri::command]
async fn turn_file_diff(
    state: State<'_, DesktopState>,
    turn: u64,
    path: String,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        let Some(runtime) = host.runtime.as_ref() else {
            return Ok(json!({"diff": "", "truncated": false}));
        };
        let diff = kroma::checkpoints::turn_file_diff(
            runtime.agent.workdir(),
            runtime.agent.session_id(),
            turn,
            &path,
        )?;
        let truncated = diff.len() > 200_000;
        let text = if truncated {
            diff.chars().take(200_000).collect::<String>()
        } else {
            diff
        };
        Ok(json!({"diff": text, "truncated": truncated}))
    })
    .await
}

/// Rewinding is two independent Core operations: history truncation and file
/// restore from the pre-task snapshot. The caller decides which ones run, so
/// the UI can never imply "undo" while leaving files behind.
/// Manual context compaction. The original transcript is never rewritten:
/// compressing changes the next request projection only.
/// Project files for the composer's @ reference. Read-only, capped, and
/// hidden from a console: this runs while the user is typing.
fn list_project_files(project: &Path, query: &str) -> Result<Value> {
    use std::process::{Command, Stdio};
    let mut command = Command::new("git");
    command
        .current_dir(project)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Ok(json!([]));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let needle = query.trim().to_lowercase();
    let mut scored: Vec<(u8, usize, String)> = Vec::new();
    for path in text
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .take(20_000)
    {
        let lower = path.to_lowercase();
        if !needle.is_empty() && !lower.contains(&needle) {
            continue;
        }
        let name = lower.rsplit('/').next().unwrap_or("");
        // A hit on the file name outranks a hit somewhere in its path.
        let rank = if needle.is_empty() {
            3
        } else if name.starts_with(&needle) {
            0
        } else if name.contains(&needle) {
            1
        } else {
            2
        };
        scored.push((rank, lower.len(), path.to_owned()));
    }
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    scored.truncate(12);
    Ok(json!(scored
        .into_iter()
        .map(|(_, _, path)| path)
        .collect::<Vec<_>>()))
}

/// Per-project session footprint. "Empty" means the archive holds no user
/// message at all; only small files are parsed, so this stays cheap.
fn storage_of(path: &Path) -> Result<Value> {
    let catalog = SessionCatalog::new(path)?;
    let mut sessions = 0usize;
    let mut empty = 0usize;
    let mut bytes = 0u64;
    let mut empty_ids: Vec<String> = Vec::new();
    for entry in catalog.list()? {
        sessions += 1;
        let Ok(file) = catalog.path_for(&entry.id) else {
            continue;
        };
        let size = fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0);
        bytes += size;
        if size > 4096 {
            continue;
        }
        let has_user = fs::read(&file)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .map(|value| {
                value["messages"]
                    .as_array()
                    .map(|messages| messages.iter().any(|message| message["role"] == "user"))
                    .unwrap_or(false)
            })
            .unwrap_or(true);
        if !has_user {
            empty += 1;
            empty_ids.push(entry.id.clone());
        }
    }
    Ok(
        json!({"project": path, "sessions": sessions, "empty": empty, "bytes": bytes, "empty_ids": empty_ids}),
    )
}

#[tauri::command]
async fn storage_report(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    with_host(state, |host| {
        let mut roots = vec![host.project.clone()];
        if let Ok(root) = home() {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        for path in recent_projects(&host.project) {
            if !roots.contains(&path) {
                roots.push(path);
            }
        }
        roots.truncate(6);
        let mut rows = Vec::new();
        for root in roots {
            if let Ok(row) = storage_of(&root) {
                rows.push(row);
            }
        }
        Ok(json!(rows))
    })
    .await
}

/// Pasted screenshots land in a temp dir so they can flow through the same
/// attachment importer as files picked from disk.
#[tauri::command]
async fn save_pasted_image(
    state: State<'_, DesktopState>,
    bytes: Vec<u8>,
    extension: String,
) -> std::result::Result<Value, String> {
    with_host(state, move |_host| {
        let extension = extension.to_ascii_lowercase();
        let kind = validate_pasted_image(&bytes, &extension)?;
        let dir = std::env::temp_dir().join("kroma-paste");
        fs::create_dir_all(&dir)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("paste-{stamp}.{kind}"));
        fs::write(&path, &bytes)?;
        Ok(json!(path.to_string_lossy().into_owned()))
    })
    .await
}

#[tauri::command]
async fn project_files(
    state: State<'_, DesktopState>,
    query: String,
) -> std::result::Result<Value, String> {
    let project = state.0.lock().map_err(ui_error)?.project.clone();
    tauri::async_runtime::spawn_blocking(move || {
        list_project_files(&project, &query).map_err(ui_error)
    })
    .await
    .map_err(ui_error)?
}

#[tauri::command]
async fn compact_context(
    state: State<'_, DesktopState>,
    instructions: String,
) -> std::result::Result<Value, String> {
    let (mut runtime, owner) = {
        let mut host = state.0.lock().map_err(ui_error)?;
        claim_runtime(&mut host, RuntimePurpose::Compact).map_err(ui_error)?
    };
    let outcome = runtime.agent.compact_with_instructions(&instructions).await;
    let mut host = state.0.lock().map_err(ui_error)?;
    return_runtime(&mut host, runtime, owner);
    match outcome {
        Ok(report) => Ok(json!({
            "compacted": report.compacted,
            "before": report.before_tokens,
            "after": report.after_tokens,
            "view": view(&mut host).map_err(ui_error)?,
        })),
        Err(error) => Err(ui_error(error)),
    }
}

#[tauri::command]
async fn rewind_turn(
    state: State<'_, DesktopState>,
    message_index: usize,
    snapshot_turn: Option<u64>,
    restore_files: bool,
) -> std::result::Result<Value, String> {
    let (mut runtime, owner) = {
        let mut host = state.0.lock().map_err(ui_error)?;
        claim_runtime(&mut host, RuntimePurpose::Rewind).map_err(ui_error)?
    };
    let outcome = async {
        let workdir = runtime.agent.workdir().to_path_buf();
        let session = runtime.agent.session_id().to_owned();
        let points = runtime.agent.history();
        // A stale index from a re-rendered transcript must never silently
        // rewind or fork at the start of the conversation.
        if message_index >= runtime.agent.messages().len() {
            anyhow::bail!("this turn is no longer part of the session");
        }
        // Largest completed point that ends at or before this turn's prompt.
        let target = points
            .iter()
            .rposition(|point| point.until <= message_index)
            .unwrap_or(0);
        let saved = runtime.agent.rewind_async(target).await?;
        if restore_files {
            if let Some(turn) = snapshot_turn {
                if let Some(hash) = kroma::checkpoints::turn_hash(&workdir, &session, turn)? {
                    kroma::checkpoints::restore(&workdir, &session, &hash)?;
                }
            }
        }
        Ok::<_, anyhow::Error>(saved)
    }
    .await;
    let mut host = state.0.lock().map_err(ui_error)?;
    return_runtime(&mut host, runtime, owner);
    match outcome {
        Ok(saved) => Ok(json!({"saved": saved, "view": view(&mut host).map_err(ui_error)?})),
        Err(error) => Err(ui_error(error)),
    }
}

#[tauri::command]
async fn fork_turn(
    state: State<'_, DesktopState>,
    message_index: usize,
) -> std::result::Result<Value, String> {
    let (mut runtime, owner) = {
        let mut host = state.0.lock().map_err(ui_error)?;
        claim_runtime(&mut host, RuntimePurpose::Fork).map_err(ui_error)?
    };
    let outcome = async {
        // A stale index from a re-rendered transcript must never silently
        // rewind or fork at the start of the conversation.
        if message_index >= runtime.agent.messages().len() {
            anyhow::bail!("this turn is no longer part of the session");
        }
        // Keep this turn and stop before the next prompt: the branch is the
        // conversation as it stood when this answer finished.
        let messages = runtime.agent.messages();
        let until = messages
            .iter()
            .enumerate()
            .skip(message_index + 1)
            .find(|(_, message)| message.role == "user")
            .map(|(index, _)| index)
            .unwrap_or(messages.len());
        runtime.agent.fork_at_async(until).await
    }
    .await;
    let mut host = state.0.lock().map_err(ui_error)?;
    return_runtime(&mut host, runtime, owner);
    match outcome {
        Ok(id) => Ok(json!({"session_id": id, "view": view(&mut host).map_err(ui_error)?})),
        Err(error) => Err(ui_error(error)),
    }
}

#[tauri::command]
async fn workspace_changes(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    let project = state.inner().0.lock().map_err(ui_error)?.project.clone();
    tauri::async_runtime::spawn_blocking(move || {
        workspace_review::overview(&project).map_err(ui_error)
    })
    .await
    .map_err(ui_error)?
}

#[tauri::command]
async fn workspace_file_diff(
    state: State<'_, DesktopState>,
    path: String,
) -> std::result::Result<Value, String> {
    let project = state.inner().0.lock().map_err(ui_error)?.project.clone();
    tauri::async_runtime::spawn_blocking(move || {
        workspace_review::file_diff(&project, &path).map_err(ui_error)
    })
    .await
    .map_err(ui_error)?
}

#[tauri::command]
async fn open_project(
    state: State<'_, DesktopState>,
    path: String,
) -> std::result::Result<Value, String> {
    let canonical = PathBuf::from(path).canonicalize().map_err(ui_error)?;
    if !canonical.is_dir() {
        return Err("Select a project directory".into());
    }
    with_host(state, move |host| {
        require_idle(host)?;
        if canonical != host.project {
            plugins_ui::stop_ui_forwarder(host);
            host.project = canonical.clone();
            host.generation += 1;
            host.runtime = None;
            host.active_plugins = None;
            host.last_snapshot = None;
            host.connected_profile = None;
            host.task_input = None;
            host.events = Arc::new(DesktopEvents::default());
        }
        if let Ok(path) = last_project_file() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let projects = recent_projects(&canonical);
            let _ = fs::write(
                path,
                json!({"project":canonical,"recent_projects":projects}).to_string(),
            );
        }
        view(host)
    })
    .await
}

#[tauri::command]
async fn connect(
    state: State<'_, DesktopState>,
    profile: Option<String>,
) -> std::result::Result<Value, String> {
    let (project, version, epoch) = {
        let mut host = state.0.lock().map_err(ui_error)?;
        require_idle(&host).map_err(ui_error)?;
        host.connect_epoch = host.connect_epoch.wrapping_add(1);
        (host.project.clone(), host.generation, host.connect_epoch)
    };
    let config = if let Some(name) = profile.as_ref().filter(|s| !s.is_empty()) {
        let profiles = Profiles::load().map_err(ui_error)?;
        let chosen = profiles
            .profiles
            .get(name)
            .context("Unknown model profile")
            .map_err(ui_error)?;
        Config::from_profile(chosen, project.clone(), Some(name.clone())).map_err(ui_error)?
    } else {
        Config::from_env_for_workdir(project.clone()).map_err(ui_error)?
    };
    let requested_profile = config.selected_profile.clone();
    let events = Arc::new(DesktopEvents::default());
    let observer: Arc<dyn EventSink> = events.clone();
    let mut runtime = app::default_runtime_with(config, move |components| {
        components.events = Some(observer);
        components.defer_initial_session = true;
        Ok(())
    })
    .await
    .map_err(ui_error)?;
    let mut host = state.0.lock().map_err(ui_error)?;
    require_idle(&host).map_err(ui_error)?;
    if host.generation != version {
        return Err("Project changed during connection".into());
    }
    if host.connect_epoch != epoch {
        return Err("A newer connection attempt superseded this one".into());
    }
    // Picking a model in Desktop switches the current connection only.
    // Persisting the CLI/TUI startup default requires an explicit action.
    let selected = host
        .runtime
        .as_ref()
        .map(|r| r.agent.session_id().to_string())
        .filter(|s| s != "unknown");
    host.runtime.take(); // release lock before reopening the previous session
    let resume = selected.or_else(|| {
        SessionCatalog::new(&project)
            .ok()
            .and_then(|catalog| catalog.recent().ok().flatten())
    });
    if let Some(id) = resume {
        if let Err(error) = runtime.agent.resume_session(&id) {
            // A second Kroma process may own that history. Show the welcome
            // screen and keep the other process's session intact.
            eprintln!("[desktop] could not resume {id}: {error:#}");
        } else {
            let _ = backfill_session_title(&runtime.agent);
        }
    }
    let ui_bus = runtime.plugins.ui_bus();
    host.events = events;
    host.runtime = Some(runtime);
    host.connected_profile = requested_profile;
    host.plugins_fingerprint = PluginCatalog::source_fingerprint(&host.project);
    host.task_input = host.runtime.as_ref().map(|r| r.agent.task_input());
    plugins_ui::sync_active_plugins(&mut host);
    plugins_ui::start_ui_forwarder(&mut host, ui_bus);
    view(&mut host).map_err(ui_error)
}

/// Adjust only the current running model's next requests. Permanent model
/// defaults are edited explicitly in the Settings page, never by a toolbar click.
#[tauri::command]
async fn set_reasoning(
    state: State<'_, DesktopState>,
    level: String,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        require_idle(host)?;
        let runtime = host.runtime.as_ref().context("Connect a model first")?;
        runtime.agent.set_reasoning_level(&level)?;
        view(host)
    })
    .await
}

#[derive(Deserialize)]
struct ProfileInput {
    name: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    #[serde(default)]
    clear_key: bool,
    protocol: String,
    reasoning: String,
    #[serde(default)]
    deepseek_thinking: Option<String>,
    context_window: Option<usize>,
    max_output_tokens: Option<usize>,
    /// `None` is an older Desktop caller and must retain the saved value.
    #[serde(default)]
    native_web_search: Option<bool>,
}

fn merge_profile_input(old: ProviderProfile, profile: ProfileInput) -> ProviderProfile {
    let inline_key = profile.api_key.filter(|v| !v.is_empty());
    // The Desktop form has no env-reference field, so an ordinary save keeps
    // the stored reference. A new inline key, or an explicit clear, must drop
    // it so the profile is left with exactly one credential.
    let (api_key, api_key_env) = if profile.clear_key {
        (None, None)
    } else if let Some(key) = inline_key {
        (Some(key), None)
    } else {
        (old.api_key, old.api_key_env)
    };
    ProviderProfile {
        base_url: profile.base_url,
        model: profile.model,
        api_key,
        api_key_env,
        api_protocol: Some(profile.protocol),
        reasoning_effort: Some(profile.reasoning),
        deepseek_thinking: profile.deepseek_thinking.filter(|v| v != "off"),
        context_window: profile.context_window,
        max_output_tokens: profile.max_output_tokens,
        native_web_search: profile.native_web_search.or(old.native_web_search),
        // These fields are not exposed by the compact Desktop form. Preserve
        // them across saves, including the explicit catalog alias.
        thinking_levels: old.thinking_levels,
        catalog_model: old.catalog_model,
    }
}

#[tauri::command]
async fn save_profile(
    state: State<'_, DesktopState>,
    profile: ProfileInput,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        require_idle(host)?;
        let name = profile.name.clone();
        let merged = {
            let profiles = Profiles::load()?;
            let old = profiles.profiles.get(&name).cloned().unwrap_or_default();
            merge_profile_input(old, profile)
        };
        Profiles::update(|profiles| {
            profiles.profiles.insert(name.clone(), merged);
            // Saving a different profile must not quietly change the shared
            // startup default used by CLI/TUI. Choose the first on setup.
            if profiles.active.is_none() {
                profiles.active = Some(name.clone());
            }
            Ok(())
        })?;
        profile_list()
    })
    .await
}

#[tauri::command]
async fn set_default_profile(
    state: State<'_, DesktopState>,
    name: String,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        require_idle(host)?;
        Profiles::update(|profiles| {
            if !profiles.profiles.contains_key(&name) {
                bail!("Unknown model profile");
            }
            profiles.active = Some(name.clone());
            Ok(())
        })?;
        profile_list()
    })
    .await
}

#[tauri::command]
async fn list_sessions(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    with_host(state, |host| sessions_for(&host.project)).await
}

#[tauri::command]
async fn history_page(
    state: State<'_, DesktopState>,
    before: Option<usize>,
    limit: Option<usize>,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        let runtime = host.runtime.as_ref().context("Connect a model first")?;
        desktop_host::snapshot_page(&runtime.agent, before, limit.unwrap_or(200))
    })
    .await
}

#[tauri::command]
async fn switch_session(
    state: State<'_, DesktopState>,
    id: String,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        require_idle(host)?;
        let (snapshot, title, connected_model, reasoning_level, thinking_levels) = {
            let runtime = host.runtime.as_mut().context("Connect a model first")?;
            runtime.agent.resume_session(&id)?;
            runtime.plugins.clear_session_ui();
            let title = backfill_session_title(&runtime.agent);
            let snapshot = desktop_host::snapshot_page_with_plugins(
                &runtime.agent,
                None,
                120,
                Some(&runtime.plugins),
            )?;
            let connected_model = runtime.selected_model_name.clone().or_else(|| {
                runtime
                    .agent
                    .model_report()
                    .map(|report| report.model.clone())
            });
            let reasoning_level = runtime.agent.reasoning_level();
            let thinking_levels = runtime.agent.thinking_levels().to_vec();
            (
                snapshot,
                title,
                connected_model,
                reasoning_level,
                thinking_levels,
            )
        };
        plugins_ui::sync_active_plugins(host);
        Ok(json!({
            "snapshot":snapshot,
            "title":title,
            "connected_model":connected_model,
            "reasoning_level":reasoning_level,
            "thinking_levels":thinking_levels,
        }))
    })
    .await
}

#[tauri::command]
async fn new_session(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    with_host(state, |host| {
        require_idle(host)?;
        let runtime = host.runtime.as_mut().context("Connect a model first")?;
        runtime.agent.new_session()?;
        runtime.plugins.clear_session_ui();
        plugins_ui::sync_active_plugins(host);
        view(host)
    })
    .await
}

#[tauri::command]
async fn delete_session(
    state: State<'_, DesktopState>,
    id: String,
    project: String,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        require_idle(host)?;
        let path = PathBuf::from(&project).canonicalize()?;
        let global = home().ok();
        if path != host.project
            && global.as_ref() != Some(&path)
            && !recent_projects(&host.project).contains(&path)
        {
            bail!("Select the project before deleting one of its conversations");
        }
        let catalog = SessionCatalog::new(&path)?;
        if !catalog.path_for(&id)?.is_file() {
            // The row was already removed (or never reached this catalog).
            // Answer with the current state instead of failing on a missing
            // file the user cannot act on.
            return view(host);
        }
        let is_active = path == host.project
            && host
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.agent.session_id() == id);
        if is_active {
            // Release the lock before moving files into local trash.
            host.runtime
                .as_mut()
                .context("No active runtime")?
                .agent
                .enter_welcome()?;
        }
        if let Err(error) = catalog.trash_session(&id) {
            if is_active {
                if let Some(runtime) = host.runtime.as_mut() {
                    let _ = runtime.agent.resume_session(&id);
                }
            }
            return Err(error);
        }
        view(host)
    })
    .await
}

#[tauri::command]
async fn restore_deleted_session(
    state: State<'_, DesktopState>,
    project: String,
    id: String,
    title: Option<String>,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        require_idle(host)?;
        let path = PathBuf::from(project).canonicalize()?;
        let global = home().ok();
        if path != host.project
            && global.as_ref() != Some(&path)
            && !recent_projects(&host.project).contains(&path)
        {
            bail!("Select the project before restoring its conversations");
        }
        let catalog = SessionCatalog::new(&path)?;
        catalog.restore_session(&id)?;
        if let Some(title) = title.filter(|name| !name.trim().is_empty() && name != "新对话") {
            if let Err(error) = catalog.name_session(&id, &title) {
                eprintln!("[desktop] restoring session name: {error:#}");
            }
        }
        view(host)
    })
    .await
}

/// Export only user/assistant-visible conversation text. Private system
/// prompts, tool logs and model reasoning are deliberately excluded.
fn transcript_markdown(messages: &[kroma::protocol::Message], title: &str) -> String {
    let clean_title = title.lines().next().unwrap_or("Kroma 会话").trim();
    let mut output = format!(
        "# {}\n\n> Kroma Desktop 导出 · 不包含系统提示词、工具日志和模型思考\n",
        if clean_title.is_empty() {
            "Kroma 会话"
        } else {
            clean_title
        }
    );
    for message in messages {
        let role = match message.role.as_str() {
            "user" => "用户",
            "assistant" => "Kroma",
            _ => continue,
        };
        let Some(content) = message.content.as_deref() else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        output.push_str("\n---\n\n## ");
        output.push_str(role);
        output.push_str("\n\n");
        output.push_str(content.trim());
        output.push('\n');
    }
    output
}

#[tauri::command]
async fn export_session(
    state: State<'_, DesktopState>,
    id: String,
    path: String,
) -> std::result::Result<(), String> {
    let destination = PathBuf::from(path);
    with_host(state, move |host| {
        require_idle(host)?;
        // A save dialog can point anywhere the user chose, including another drive.
        let destination = ensure_export_path(&destination)?;
        let runtime = host.runtime.as_ref().context("Connect a model first")?;
        ensure_session_target(&id, runtime.agent.session_id())?;
        let catalog = SessionCatalog::new(&host.project)?;
        let title = catalog
            .list()?
            .into_iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.name)
            .unwrap_or_else(|| "Kroma 会话".to_owned());
        let markdown = transcript_markdown(runtime.agent.messages(), &title);
        if markdown.len() > 25 * 1024 * 1024 {
            bail!("会话导出超过 25 MiB，请先拆分历史记录");
        }
        fs::write(&destination, markdown)
            .with_context(|| format!("无法写入 {}", destination.display()))?;
        Ok(())
    })
    .await
}

#[tauri::command]
async fn rename_session(
    state: State<'_, DesktopState>,
    id: String,
    title: String,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        require_idle(host)?;
        let agent = &host
            .runtime
            .as_ref()
            .context("Connect a model first")?
            .agent;
        // A late IPC request must never rename whichever conversation happens
        // to be active now. The requested ID is part of the mutation contract.
        ensure_session_target(&id, agent.session_id())?;
        agent.rename_session(&title)?;
        view(host)
    })
    .await
}

fn ensure_session_target(requested: &str, active: &str) -> Result<()> {
    if requested != active || requested.is_empty() || active == "unknown" {
        bail!("会话已切换，请返回目标会话后重试重命名");
    }
    Ok(())
}

#[tauri::command]
fn cancel_task(state: State<'_, DesktopState>) -> std::result::Result<(), String> {
    let host = state.0.lock().map_err(ui_error)?;
    let cancel = host
        .cancel
        .as_ref()
        .context("No task is running")
        .map_err(ui_error)?;
    cancel.send(true).map_err(ui_error)
}

#[tauri::command]
fn steer_task(state: State<'_, DesktopState>, text: String) -> std::result::Result<Value, String> {
    let host = state.0.lock().map_err(ui_error)?;
    let handle = host
        .task_input
        .as_ref()
        .context("No task is running")
        .map_err(ui_error)?;
    handle.steer(text).map_err(ui_error)?;
    Ok(json!({"kind": "steered"}))
}

#[tauri::command]
async fn send_message(
    state: State<'_, DesktopState>,
    text: String,
    paths: Vec<String>,
    channel: Channel<Value>,
) -> std::result::Result<Value, String> {
    let (mut runtime, events, mut cancel_rx) = {
        let mut host = state.0.lock().map_err(ui_error)?;
        require_idle(&host).map_err(ui_error)?;
        // Hot plugging: a plugin installed, enabled or edited outside this window
        // applies to the next message, exactly as in the terminal interface.
        if host.runtime.is_some()
            && PluginCatalog::source_fingerprint(&host.project) != host.plugins_fingerprint
        {
            if let Err(error) = reload_plugins(&mut host) {
                eprintln!("plugin hot reload failed: {error:#}");
            }
            host.plugins_fingerprint = PluginCatalog::source_fingerprint(&host.project);
        }
        let runtime = host
            .runtime
            .take()
            .context("Configure and connect a model first")
            .map_err(ui_error)?;
        let (tx, rx) = watch::channel(false);
        host.task_input = Some(runtime.agent.task_input());
        host.cancel = Some(tx);
        // Questions raised by this turn reach the window that started it.
        host.asker.set_channel(Some(channel.clone()));
        (runtime, host.events.clone(), rx)
    };
    let mut all = Vec::new();
    // Import files after acquiring the agent, but always restore the agent
    // below even if one import fails. Never leave the frontend permanently busy.
    let outcome = async {
        for path in paths.iter().take(8) {
            let source = ensure_attachment_path(Path::new(path))?;
            all.push(host_attachments::import_file(
                &source,
                runtime.agent.workdir(),
            )?);
        }
        if runtime.agent.session_id() == "unknown" {
            runtime.agent.new_session()?;
        }
        let mut prompt = text.clone();
        // A reply a plugin command produced is shown alongside the completion.
        let mut notice: Option<String> = None;
        let first_user_turn = !runtime
            .agent
            .messages()
            .iter()
            .any(|message| message.role == "user");
        // A plugin slash command runs here instead of reaching the model as a
        // prompt. Unknown slash lines keep their old behaviour and are sent.
        if kroma::commands::looks_like_command(&text) {
            let mut registry = kroma::commands::CommandRegistry::new();
            let _ = runtime.plugins.register_commands(&mut registry);
            let name = text.split_whitespace().next().unwrap_or("");
            if registry.knows(name) {
                let input = runtime.agent.task_input();
                match registry
                    .execute_async(&text, &mut runtime.agent, &input)
                    .await
                {
                    Ok(Some(reply)) => {
                        let output = if reply.output.trim().is_empty() {
                            format!("{name} 已执行")
                        } else {
                            reply.output
                        };
                        match reply.action {
                            // The plugin supplied this turn: run it as the
                            // user's message instead of answering it here.
                            kroma::commands::CommandAction::Task(body) => {
                                prompt = body;
                                notice = Some(output);
                            }
                            _ => return Ok(("completed".to_owned(), Some(output))),
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return Ok(("completed".to_owned(), Some(format!("{error:#}"))));
                    }
                }
            }
        }
        let mut interval = tokio::time::interval(Duration::from_millis(32));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut ignore_text = |_: &str| {};
        enum Turn {
            Finished(Result<String>),
            Cancelled,
        }
        let status = {
            let mut request = Box::pin(runtime.agent.run_with_attachments(
                &prompt,
                &all,
                &mut ignore_text,
            ));
            loop {
                tokio::select! {
                    result=&mut request=>break Turn::Finished(result),
                    changed=cancel_rx.changed()=>{
                        if changed.is_ok() && *cancel_rx.borrow() {break Turn::Cancelled;}
                    },
                    _=interval.tick()=>{
                        let pending=events.drain();
                        if !pending.is_empty() {
                            let _=channel.send(json!({"kind":"batch","events":pending}));
                        }
                    }
                }
            }
        }; // Dropping the cancelled Future first terminates active tool workers.
        let result = match status {
            Turn::Finished(result) => result.map(|_| ("completed".to_owned(), notice.take())),
            Turn::Cancelled => {
                runtime.agent.cancel()?;
                Ok(("cancelled".to_owned(), None))
            }
        };
        if first_user_turn
            && runtime
                .agent
                .messages()
                .iter()
                .any(|message| message.role == "user")
        {
            if let Some(title) = initial_session_title(&text) {
                if let Err(error) =
                    SessionCatalog::new(runtime.agent.workdir()).and_then(|catalog| {
                        catalog.name_session_if_unnamed(runtime.agent.session_id(), &title)
                    })
                {
                    eprintln!("[desktop] saving session title: {error:#}");
                }
            }
        }
        result
    }
    .await;
    let result = match outcome {
        Ok((status, text)) => json!({"status":status,"text":text}),
        Err(error) => json!({"status":"failed","error":format!("{error:#}")}),
    };
    let usage = SessionCatalog::new(runtime.agent.workdir())
        .and_then(|catalog| events.persist_usage(&catalog, runtime.agent.session_id()));
    if let Err(error) = usage {
        eprintln!("[desktop] saving usage: {error:#}");
    }
    // Names are sidecar metadata. Refresh the navigation in the same event as
    // the snapshot, avoiding a second full bootstrap/reconnect on completion.
    let groups = session_groups_for(runtime.agent.workdir());
    let sessions = sessions_for(runtime.agent.workdir()).unwrap_or_else(|error| {
        eprintln!("[desktop] listing sessions: {error:#}");
        json!([])
    });
    // Always restore the Agent to the host, even when a UI-only snapshot
    // fails (for example because the usage ledger is temporarily unreadable).
    let snapshot = desktop_host::snapshot_with_plugins(&runtime.agent, Some(&runtime.plugins));
    let last = events.drain();
    if !last.is_empty() {
        let _ = channel.send(json!({"kind":"batch","events":last}));
    }
    if let Ok(ref snapshot) = snapshot {
        let _ = channel.send(json!({"kind":"snapshot","snapshot":snapshot,"sessions":sessions,"session_groups":groups}));
    }
    let _ = channel.send(json!({"kind":"done","result":result}));
    {
        let mut host = state.0.lock().map_err(ui_error)?;
        host.runtime = Some(runtime);
        host.cancel = None;
        host.task_input = host.runtime.as_ref().map(|r| r.agent.task_input());
        plugins_ui::sync_active_plugins(&mut host);
        // The turn is over: a question nobody can answer any more is dropped.
        host.asker.set_channel(None);
        host.asker.clear_pending();
    }
    let snapshot = snapshot.map_err(ui_error)?;
    Ok(json!({"result":result,"snapshot":snapshot,"sessions":sessions,"session_groups":groups}))
}

pub(crate) fn reload_plugins(host: &mut Host) -> Result<()> {
    let Some(runtime) = host.runtime.as_mut() else {
        return Ok(());
    };
    let mut next = PluginCatalog::discover(&host.project)?;
    let (tools, prompt) = runtime.plugins.prepare_idle_reload_from(
        &mut next,
        &runtime.base_tools,
        &runtime.base_prompt,
    )?;
    let sink: Arc<dyn EventSink> = host.events.clone();
    let events = next.wrap_events(Some(sink))?;
    runtime.agent.reload_extensions(tools, prompt, events);
    runtime.plugins = next;
    host.plugins_fingerprint = PluginCatalog::source_fingerprint(&host.project);
    plugins_ui::sync_active_plugins(host);
    Ok(())
}

#[cfg(test)]
fn validate_external_url(raw: &str) -> Result<String> {
    kroma::external::validate_http_url(raw)?;
    Ok(raw.to_owned())
}

#[tauri::command]
fn open_external(url: String) -> std::result::Result<(), String> {
    kroma::external::open_http_url(&url).map_err(ui_error)
}

#[tauri::command]
async fn trust_project_mcp(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    let project = {
        let host = state.0.lock().map_err(ui_error)?;
        host.project.clone()
    };
    let path = mcp::trust_project_mcp(&project).map_err(ui_error)?;
    Ok(json!({"path":path,"trusted":true}))
}

/// The WebView only ever shows the app itself. A remote http(s) navigation is
/// left to `open_external`, so model-authored content cannot replace the UI
/// with a live web page (and its own IPC reach) inside the window.
fn allows_webview_navigation(raw: &str) -> bool {
    let Ok(url) = tauri::Url::parse(raw) else {
        return false;
    };
    match url.scheme() {
        // Packaged app (tauri://localhost) and internal pages.
        "tauri" | "about" | "data" => true,
        // Dev server and the Windows custom-protocol host.
        "http" | "https" => matches!(
            url.host_str(),
            Some("localhost") | Some("127.0.0.1") | Some("tauri.localhost")
        ),
        _ => false,
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("kroma-local-navigation")
                .on_navigation(|_webview, url| allows_webview_navigation(url.as_str()))
                .build(),
        )
        .manage(DesktopState::new())
        .setup(|app| {
            let state = app.state::<DesktopState>();
            plugins_ui::attach_app(&state, app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            search_sessions,
            checkpoint_turns,
            rewind_turn,
            compact_context,
            project_files,
            save_pasted_image,
            storage_report,
            fork_turn,
            turn_files,
            turn_file_diff,
            workspace_changes,
            workspace_file_diff,
            open_project,
            connect,
            set_reasoning,
            save_profile,
            set_default_profile,
            list_sessions,
            history_page,
            switch_session,
            new_session,
            delete_session,
            restore_deleted_session,
            rename_session,
            export_session,
            cancel_task,
            steer_task,
            send_message,
            plugins_ui::plugin_action,
            plugins_ui::plugin_presentation,
            plugins_ui::plugin_panel,
            plugins_ui::plugin_panel_view,
            plugins_ui::plugin_desktop_bundle,
            plugins_ui::plugin_ui_action,
            plugins_ui::plugin_ui_message,
            plugins_ui::answer_question,
            trust_project_mcp,
            open_external
        ])
        .run(tauri::generate_context!())
        .expect("unable to start Kroma Desktop");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn home_has_a_single_canonical_form() {
        // The session catalog is keyed by the workdir string, so the home
        // project must not keep a second (raw) spelling next to the canonical
        // one the Desktop stores; that split hid conversations and made them
        // impossible to delete.
        let resolved = home().expect("home directory");
        let canonical = resolved.canonicalize().expect("home directory exists");
        assert_eq!(resolved, canonical);
    }

    #[test]
    fn session_mutations_never_target_another_open_conversation() {
        assert!(ensure_session_target("session-a", "session-a").is_ok());
        assert!(ensure_session_target("session-a", "session-b").is_err());
        assert!(ensure_session_target("", "").is_err());
        assert!(ensure_session_target("unknown", "unknown").is_err());
    }

    #[test]
    fn an_unknown_question_id_is_never_answered() {
        let asker = DesktopAsker::default();
        assert!(!asker.answer(7, kroma::tools::AskAnswer::Dismissed));
    }

    #[test]
    fn a_finished_turn_drops_its_open_question() {
        let asker = DesktopAsker::default();
        let (sender, wait) = tokio::sync::oneshot::channel();
        asker.pending.lock().expect("pending map").insert(1, sender);
        assert!(asker.answer(
            1,
            kroma::tools::AskAnswer::Answered {
                picked: vec!["Postgres".into()],
                typed: None,
            }
        ));
        assert!(matches!(
            wait.blocking_recv(),
            Ok(kroma::tools::AskAnswer::Answered { picked, .. }) if picked == ["Postgres"]
        ));

        let (sender, wait) = tokio::sync::oneshot::channel();
        asker.pending.lock().expect("pending map").insert(2, sender);
        asker.clear_pending();
        assert!(
            wait.blocking_recv().is_err(),
            "a dropped question has no answer"
        );
        assert!(!asker.answer(2, kroma::tools::AskAnswer::Dismissed));
    }

    #[test]
    fn markdown_export_omits_system_tool_and_private_reasoning_content() {
        let mut answer = kroma::protocol::Message::new("assistant", "最终结果 **通过**");
        answer.reasoning_content = Some("PRIVATE-THOUGHTS".to_owned());
        let markdown = transcript_markdown(
            &[
                kroma::protocol::Message::new("system", "INTERNAL-SYSTEM-PROMPT"),
                kroma::protocol::Message::new("user", "请检查测试结果"),
                kroma::protocol::Message::new("tool", "PRIVATE-TOOL-OUTPUT"),
                answer,
            ],
            "测试会话",
        );
        assert!(markdown.contains("测试会话"));
        assert!(markdown.contains("请检查测试结果"));
        assert!(markdown.contains("最终结果 **通过**"));
        assert!(!markdown.contains("INTERNAL-SYSTEM-PROMPT"));
        assert!(!markdown.contains("PRIVATE-TOOL-OUTPUT"));
        assert!(!markdown.contains("PRIVATE-THOUGHTS"));
    }

    #[test]
    fn first_message_titles_are_short_and_avoid_obvious_secrets() {
        assert_eq!(
            initial_session_title("  修复 Desktop 的工具弹窗  \n更多要求").as_deref(),
            Some("修复 Desktop 的工具弹窗")
        );
        assert_eq!(initial_session_title("sk-secret-value"), None);
        assert_eq!(initial_session_title("API_KEY=abc123"), None);
        assert_eq!(initial_session_title("https://example.com/token"), None);
        assert_eq!(initial_session_title("   \n "), None);
        assert!(
            initial_session_title(&"你好".repeat(60))
                .unwrap()
                .chars()
                .count()
                <= 48
        );
    }

    #[test]
    fn simple_plugin_settings_preserve_typed_values() {
        use plugins_ui::parse_setting;
        assert_eq!(
            parse_setting(json!(true)).unwrap(),
            toml::Value::Boolean(true)
        );
        assert_eq!(parse_setting(json!(3)).unwrap(), toml::Value::Integer(3));
        assert_eq!(
            parse_setting(json!("hello")).unwrap(),
            toml::Value::String("hello".into())
        );
    }

    #[test]
    fn only_http_links_can_leave_the_webview() {
        assert!(validate_external_url("https://example.org/a?b=c").is_ok());
        assert!(validate_external_url("http://example.org").is_ok());
        assert!(validate_external_url("file:///etc/passwd").is_err());
        assert!(validate_external_url("javascript:alert(1)").is_err());
        assert!(validate_external_url("https://example.org/\nmalicious").is_err());
    }

    fn profile_input() -> ProfileInput {
        ProfileInput {
            name: "work".into(),
            base_url: "https://proxy.example/v1".into(),
            model: "renamed".into(),
            api_key: None,
            clear_key: false,
            protocol: "anthropic".into(),
            reasoning: "auto".into(),
            deepseek_thinking: None,
            context_window: Some(200_000),
            max_output_tokens: Some(8_000),
            native_web_search: Some(true),
        }
    }

    fn env_key_profile() -> ProviderProfile {
        ProviderProfile {
            base_url: "https://proxy.example/v1".into(),
            model: "renamed".into(),
            api_key_env: Some("KROMA_TEST_KEY".into()),
            ..Default::default()
        }
    }

    #[test]
    fn saving_a_desktop_profile_preserves_the_catalog_alias() {
        let old = ProviderProfile {
            base_url: "https://proxy.example/v1".into(),
            model: "renamed".into(),
            api_key: Some("old-key".into()),
            thinking_levels: Some(vec!["low".into()]),
            catalog_model: Some("anthropic/claude-sonnet-5-5".into()),
            ..Default::default()
        };
        let saved = merge_profile_input(old, profile_input());
        assert_eq!(
            saved.catalog_model.as_deref(),
            Some("anthropic/claude-sonnet-5-5")
        );
        assert_eq!(saved.thinking_levels, Some(vec!["low".into()]));
        assert_eq!(saved.api_key.as_deref(), Some("old-key"));
        assert_eq!(saved.context_window, Some(200_000));
        assert_eq!(saved.native_web_search, Some(true));
    }

    #[test]
    fn an_old_desktop_caller_keeps_native_search_but_explicit_off_disables_it() {
        let old = ProviderProfile {
            native_web_search: Some(true),
            ..Default::default()
        };
        let mut input = profile_input();
        input.native_web_search = None;
        let saved = merge_profile_input(old, input);
        assert_eq!(saved.native_web_search, Some(true));
        let mut input = profile_input();
        input.native_web_search = Some(false);
        let saved = merge_profile_input(saved, input);
        assert_eq!(saved.native_web_search, Some(false));
    }

    #[test]
    fn an_ordinary_save_keeps_the_profile_env_reference() {
        let saved = merge_profile_input(env_key_profile(), profile_input());
        assert_eq!(saved.api_key_env.as_deref(), Some("KROMA_TEST_KEY"));
        assert_eq!(saved.api_key, None);

        // The form always sends a key field; an empty one means "unchanged".
        let mut untouched = profile_input();
        untouched.api_key = Some(String::new());
        let saved = merge_profile_input(env_key_profile(), untouched);
        assert_eq!(saved.api_key_env.as_deref(), Some("KROMA_TEST_KEY"));
        assert_eq!(saved.api_key, None);
    }

    #[test]
    fn a_new_inline_key_or_an_explicit_clear_drops_the_env_reference() {
        let mut supplied = profile_input();
        supplied.api_key = Some("new-key".into());
        let saved = merge_profile_input(env_key_profile(), supplied);
        assert_eq!(saved.api_key.as_deref(), Some("new-key"));
        assert_eq!(saved.api_key_env, None);

        let mut cleared = profile_input();
        cleared.clear_key = true;
        cleared.api_key = Some("ignored-key".into());
        let saved = merge_profile_input(env_key_profile(), cleared);
        assert_eq!(saved.api_key, None);
        assert_eq!(saved.api_key_env, None);
    }

    fn idle_host(project: PathBuf) -> Host {
        Host {
            project,
            generation: 0,
            connect_epoch: 0,
            runtime: None,
            runtime_owned: None,
            runtime_lease: 0,
            last_snapshot: None,
            connected_profile: None,
            events: Arc::new(DesktopEvents::default()),
            cancel: None,
            task_input: None,
            plugins_fingerprint: 0,
            asker: Arc::new(DesktopAsker::default()),
            app_handle: None,
            ui_forwarder: None,
            active_plugins: None,
        }
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kroma-desktop-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_command_that_owns_the_runtime_blocks_other_mutations() {
        let mut host = idle_host(std::env::temp_dir());
        assert!(
            require_idle(&host).is_ok(),
            "an idle host accepts the change"
        );
        host.runtime_owned = Some(RuntimeOwner {
            token: 1,
            purpose: RuntimePurpose::Compact,
        });
        let error = require_idle(&host).unwrap_err().to_string();
        assert!(error.contains("正在压缩上下文"), "{error}");
        host.runtime_owned = Some(RuntimeOwner {
            token: 2,
            purpose: RuntimePurpose::Rewind,
        });
        assert!(require_idle(&host)
            .unwrap_err()
            .to_string()
            .contains("回退"));
        host.runtime_owned = None;
        assert!(require_idle(&host).is_ok());
    }

    #[test]
    fn a_command_that_owns_the_runtime_keeps_the_last_known_snapshot() {
        let project = scratch_dir("snapshot");
        let mut host = idle_host(project);
        let empty = view(&mut host).unwrap();
        assert!(empty["snapshot"].is_null());
        assert_eq!(empty["ready"], false);
        host.last_snapshot = Some(json!({"session_id": "session-a", "messages": []}));
        let during = view(&mut host).unwrap();
        assert_eq!(during["snapshot"]["session_id"], "session-a");
        // Ready still describes whether a runtime is installed, so the UI
        // keeps its connected-state semantics instead of offering a reconnect.
        assert_eq!(during["ready"], false);
    }

    #[test]
    fn any_chosen_attachment_and_export_path_is_accepted() {
        let root = scratch_dir("paths");
        let inside = root.join("inside");
        let outside = root.join("outside");
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(inside.join("note.txt"), b"hello").unwrap();
        fs::write(outside.join("secret.txt"), b"secret").unwrap();
        assert!(ensure_attachment_path(&inside.join("note.txt")).is_ok());
        assert!(
            ensure_attachment_path(&outside.join("secret.txt")).is_ok(),
            "a file the user picked outside the project is still theirs"
        );
        let folder = root.join("missing");
        let refused = ensure_attachment_path(&folder.join("nope.txt"))
            .unwrap_err()
            .to_string();
        assert!(refused.contains("附件不存在"), "{refused}");
        let saved = ensure_export_path(&inside.join("chat.md")).unwrap();
        assert_eq!(saved, inside.canonicalize().unwrap().join("chat.md"));
        assert!(ensure_export_path(&outside.join("chat.md")).is_ok());
        assert!(ensure_export_path(&inside.join("chat.txt")).is_err());
        assert!(ensure_export_path(Path::new("relative.md")).is_err());
    }

    #[test]
    fn pasted_images_must_be_small_png_jpeg_or_webp() {
        let png = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        assert_eq!(validate_pasted_image(&png, "png").unwrap(), "png");
        assert_eq!(
            validate_pasted_image(&[0xFFu8, 0xD8, 0xFF, 0], "jpeg").unwrap(),
            "jpg"
        );
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(validate_pasted_image(&webp, "webp").unwrap(), "webp");
        assert!(validate_pasted_image(b"plain text", "png").is_err());
        assert!(validate_pasted_image(&[], "png").is_err());
        let oversized = vec![0x89u8; MAX_PASTED_IMAGE_BYTES + 1];
        let error = validate_pasted_image(&oversized, "png")
            .unwrap_err()
            .to_string();
        assert!(error.contains("8 MiB"), "{error}");
    }

    #[test]
    fn plugin_install_requires_a_real_manifest_and_refuses_symlinks() {
        use plugins_ui::validate_plugin_source;
        let root = scratch_dir("plugin");
        let good = root.join("good");
        let empty = root.join("empty");
        fs::create_dir_all(&good).unwrap();
        fs::create_dir_all(&empty).unwrap();
        fs::write(good.join("plugin.toml"), "[plugin]").unwrap();
        assert_eq!(
            validate_plugin_source(&good).unwrap(),
            good.canonicalize().unwrap()
        );
        assert!(validate_plugin_source(&empty).is_err(), "no manifest");
        fs::write(empty.join("plugin.toml"), "plugin = [1, 2").unwrap();
        assert!(validate_plugin_source(&empty).is_err(), "broken toml");
        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&good, &link).unwrap();
            assert!(
                validate_plugin_source(&link).is_err(),
                "symlinked directory"
            );
        }
    }

    #[test]
    fn only_local_origins_may_navigate_the_webview() {
        assert!(allows_webview_navigation("tauri://localhost/index.html"));
        assert!(allows_webview_navigation("http://localhost:1420/"));
        assert!(allows_webview_navigation("http://tauri.localhost/"));
        assert!(allows_webview_navigation("about:blank"));
        assert!(!allows_webview_navigation("https://example.org/phish"));
        assert!(!allows_webview_navigation("http://127.0.0.1.evil.test/"));
        assert!(!allows_webview_navigation("file:///etc/passwd"));
        assert!(!allows_webview_navigation("not a url"));
    }
    #[test]
    fn a_running_task_blocks_session_and_plugin_changes() {
        let project = std::env::temp_dir();
        let mut host = idle_host(project);
        assert!(
            require_idle(&host).is_ok(),
            "an idle host accepts the change"
        );
        let (cancel, _receiver) = watch::channel(false);
        host.cancel = Some(cancel);
        assert!(
            require_idle(&host).is_err(),
            "switching sessions or plugins while a task runs must be refused"
        );
    }
}
