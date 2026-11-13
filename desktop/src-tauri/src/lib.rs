//! Tauri commands only translate GUI actions into the existing Kroma Rust host.
//! No second Agent Loop, HTTP server, or terminal subprocess.
mod plugins_ui;
mod session_files;
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
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

/// One connected conversation. The project is part of the key: two workspaces
/// may hold a session with the same id, and they are different conversations.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionKey {
    pub(crate) project: String,
    pub(crate) session: String,
}

impl SessionKey {
    fn new(project: &Path, session: &str) -> Self {
        Self {
            project: project.to_string_lossy().into_owned(),
            session: session.to_owned(),
        }
    }

    /// A host with no opened session yet is only a shell for the project view.
    fn shell(project: &Path) -> Self {
        Self::new(project, "")
    }

    fn is_shell(&self) -> bool {
        self.session.is_empty()
    }
}
use tauri::{ipc::Channel, AppHandle, Manager, State};
use tokio::sync::watch;

pub(crate) struct Host {
    project: PathBuf,
    /// Stable conversation identity. `""` is an unopened project shell and
    /// `"unknown"` is an unsaved conversation; a task never runs under either
    /// placeholder, it allocates the real id first.
    session: String,
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
    /// Monotonic last-use stamp so idle conversation contexts can be released
    /// instead of keeping every visited WAL in memory forever.
    last_used: u64,
}

/// A plugin's question reaching the window. The plugin's own worker thread
/// waits on the matching sender; the window answers with `answer_question`.
#[derive(Default)]
struct DesktopAsker {
    next_id: AtomicU64,
    pending: Mutex<
        HashMap<
            u64,
            (
                String,
                tokio::sync::oneshot::Sender<kroma::tools::AskAnswer>,
            ),
        >,
    >,
    /// Channels of the messages that are running right now, keyed by the
    /// conversation they belong to.
    channels: Mutex<HashMap<String, Channel<Value>>>,
}

impl DesktopAsker {
    fn register_channel(&self, session: &str, channel: Channel<Value>) {
        if let Ok(mut channels) = self.channels.lock() {
            channels.insert(session.to_owned(), channel);
        }
    }

    fn unregister_channel(&self, session: &str) {
        if let Ok(mut channels) = self.channels.lock() {
            channels.remove(session);
        }
    }

    fn has_pending_for(&self, session: &str) -> bool {
        self.pending
            .lock()
            .map(|map| map.values().any(|(owner, _)| owner == session))
            .unwrap_or(false)
    }

    /// A finished or cancelled turn leaves no answerable question behind, but
    /// never drops another conversation's open question.
    fn clear_pending_for(&self, session: &str) {
        if let Ok(mut map) = self.pending.lock() {
            map.retain(|_, (owner, _)| owner != session);
        }
    }

    /// A plugin's question is delivered to the one task this host is running.
    /// Each conversation owns its own asker, so no other session can receive
    /// or answer it; the project boundary is the host itself.
    fn current_channel(&self) -> Option<(String, Channel<Value>)> {
        let channels = self.channels.lock().ok()?;
        if channels.len() != 1 {
            return None;
        }
        channels
            .iter()
            .next()
            .map(|(session, channel)| (session.clone(), channel.clone()))
    }

    /// Answer one open question. The caller must name the conversation the
    /// question was shown in, so another session's bar can never answer it.
    fn answer(&self, session: &str, id: u64, answer: kroma::tools::AskAnswer) -> bool {
        let sender = self
            .pending
            .lock()
            .ok()
            .and_then(|mut map| match map.get(&id) {
                Some((owner, _)) if owner == session => map.remove(&id),
                _ => None,
            });
        sender.is_some_and(|(_, sender)| sender.send(answer).is_ok())
    }

    /// A finished or cancelled turn leaves no answerable question behind.
    #[cfg(test)]
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
            let (session, channel) = self.current_channel()?;
            let (sender, wait) = tokio::sync::oneshot::channel();
            self.pending
                .lock()
                .ok()?
                .insert(id, (session.clone(), sender));
            let sent = channel
                .send(json!({
                    "kind": "ask",
                    "session": session,
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

/// One live context per opened conversation plus the pointer the window is
/// currently showing. The pointer only decides the default command target; a
/// running task keeps the `Host` it started with, so completion can never
/// write into whichever conversation happens to be visible later.
pub(crate) struct Registry {
    project: PathBuf,
    active: Option<SessionKey>,
    hosts: HashMap<SessionKey, Arc<Mutex<Host>>>,
    app_handle: Option<AppHandle>,
    last_used: u64,
}

/// The Tauri-managed handle. Cloned cheaply so blocking work can run on the
/// blocking pool without holding the UI command thread.
#[derive(Clone)]
pub(crate) struct DesktopState(pub(crate) Arc<Mutex<Registry>>);

pub(crate) fn ui_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

pub(crate) fn home() -> Result<PathBuf> {
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
pub(crate) fn ensure_attachment_path(raw: &Path) -> Result<PathBuf> {
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

/// The one bound for an image attachment: a pasted screenshot, a file picked
/// from disk and a preview read all share it. Only a real PNG/JPEG/WebP image
/// qualifies; the renderer must not use any of this as a general file reader.
pub(crate) const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

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
    if bytes.len() > MAX_IMAGE_BYTES {
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

impl Host {
    fn new(
        project: PathBuf,
        session: String,
        app_handle: Option<AppHandle>,
        asker: Arc<DesktopAsker>,
    ) -> Self {
        Self {
            project,
            session,
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
            app_handle,
            ui_forwarder: None,
            active_plugins: None,
            last_used: 0,
        }
    }

    fn key(&self) -> SessionKey {
        SessionKey::new(&self.project, &self.session)
    }

    /// A connected conversation stays ready while its runtime is temporarily
    /// owned by its own task or command. `unknown`/empty states are not ready.
    fn is_ready(&self) -> bool {
        // A conversation that failed to reopen keeps its snapshot for reading
        // but is not sendable: only an installed runtime (or its own running
        // task / lease) makes it ready.
        self.runtime.is_some() || self.cancel.is_some() || self.runtime_owned.is_some()
    }
}

impl Registry {
    fn touch(&mut self, key: &SessionKey) {
        self.last_used = self.last_used.wrapping_add(1);
        if let Some(host) = self.hosts.get(key) {
            if let Ok(mut host) = host.lock() {
                host.last_used = self.last_used;
            }
        }
    }

    fn insert(&mut self, key: SessionKey, host: Arc<Mutex<Host>>) {
        self.last_used = self.last_used.wrapping_add(1);
        if let Ok(mut host) = host.lock() {
            host.last_used = self.last_used;
        }
        self.hosts.insert(key, host);
    }

    /// Find the host for exactly this conversation. A host whose session id
    /// advanced (an unsaved conversation received its real id) is matched by
    /// identity too, so no lookup loses it while the registry key catches up.
    fn host_for(&self, key: &SessionKey) -> Option<Arc<Mutex<Host>>> {
        if let Some(host) = self.hosts.get(key) {
            return Some(host.clone());
        }
        for (candidate, host) in &self.hosts {
            if candidate.project != key.project || candidate.session == key.session {
                continue;
            }
            if let Ok(current) = host.try_lock() {
                if current.session == key.session {
                    return Some(host.clone());
                }
            }
        }
        None
    }

    /// Release idle conversation contexts beyond the cap. The active session
    /// and every running task are always kept; nothing durable is discarded.
    fn evict_idle(&mut self, keep: usize) {
        if self.hosts.len() <= keep {
            return;
        }
        let active = self.active.clone();
        let mut idle: Vec<(String, String, u64, Arc<Mutex<Host>>)> = Vec::new();
        for (key, host) in &self.hosts {
            if Some(key) == active.as_ref() {
                continue;
            }
            let Ok(current) = host.try_lock() else {
                continue;
            };
            if current.cancel.is_some() || current.runtime_owned.is_some() {
                continue;
            }
            idle.push((
                key.project.clone(),
                key.session.clone(),
                current.last_used,
                host.clone(),
            ));
        }
        idle.sort_by_key(|(_, _, used, _)| *used);
        let excess = self.hosts.len().saturating_sub(keep);
        for (project, session, _, host) in idle.into_iter().take(excess) {
            if let Ok(mut current) = host.lock() {
                plugins_ui::stop_ui_forwarder(&mut current);
            }
            self.hosts.remove(&SessionKey { project, session });
        }
    }
}

impl DesktopState {
    fn new() -> Self {
        let project = initial_project();
        // Terminal-compatible fallback only: Desktop binds every real runtime
        // to its own conversation asker below.
        kroma::tools::set_host_asker(Arc::new(DesktopAsker::default()));
        let shell = SessionKey::shell(&project);
        let host = Arc::new(Mutex::new(Host::new(
            project.clone(),
            String::new(),
            None,
            Arc::new(DesktopAsker::default()),
        )));
        let mut hosts = HashMap::new();
        hosts.insert(shell.clone(), host);
        Self(Arc::new(Mutex::new(Registry {
            project,
            active: Some(shell),
            hosts,
            app_handle: None,
            last_used: 0,
        })))
    }

    fn registry(&self) -> std::result::Result<std::sync::MutexGuard<'_, Registry>, String> {
        self.0.lock().map_err(ui_error)
    }

    /// The context the window shows. Every command that does not name a
    /// conversation explicitly acts on this one only.
    fn active_host(&self) -> std::result::Result<(SessionKey, Arc<Mutex<Host>>), String> {
        let registry = self.registry()?;
        let key = registry
            .active
            .clone()
            .context("No open conversation")
            .map_err(ui_error)?;
        let host = registry
            .hosts
            .get(&key)
            .cloned()
            .context("The open conversation is unavailable")
            .map_err(ui_error)?;
        Ok((key, host))
    }

    fn host_for(&self, key: &SessionKey) -> Option<Arc<Mutex<Host>>> {
        self.0
            .lock()
            .ok()
            .and_then(|registry| registry.host_for(key))
    }

    fn project(&self) -> std::result::Result<PathBuf, String> {
        self.registry().map(|registry| registry.project.clone())
    }

    /// Point the window at one conversation and remember it was used. Returns
    /// the host to read, so the registry lock is never held during I/O.
    fn focus(&self, key: &SessionKey) -> std::result::Result<Arc<Mutex<Host>>, String> {
        let mut registry = self.registry()?;
        let host = registry
            .hosts
            .get(key)
            .cloned()
            .with_context(|| format!("会话尚未打开：{}", key.session))
            .map_err(ui_error)?;
        registry.active = Some(key.clone());
        registry.touch(key);
        Ok(host)
    }

    /// Move a context to its real session id once that id is known. Keeping the
    /// old key would let a later lookup open a second runtime for the same
    /// conversation or address the wrong one.
    fn rekey(
        &self,
        old: &SessionKey,
        new: &SessionKey,
        host: &Arc<Mutex<Host>>,
    ) -> std::result::Result<(SessionKey, Arc<Mutex<Host>>), String> {
        if old == new {
            return Ok((new.clone(), host.clone()));
        }
        let mut registry = self.registry()?;
        registry.hosts.remove(old);
        if let Some(existing) = registry.hosts.get(new).cloned() {
            // Another navigation opened this conversation first. Drop this
            // context's forwarder and keep the existing one; the caller reads
            // the existing host instead of publishing a duplicate.
            if let Ok(mut current) = host.lock() {
                plugins_ui::stop_ui_forwarder(&mut current);
            }
            if registry.active.as_ref() == Some(old) {
                registry.active = Some(new.clone());
            }
            return Ok((new.clone(), existing));
        }
        registry.hosts.insert(new.clone(), host.clone());
        if registry.active.as_ref() == Some(old) {
            registry.active = Some(new.clone());
        }
        Ok((new.clone(), host.clone()))
    }

    fn ensure_shell(&self, project: PathBuf) -> std::result::Result<Arc<Mutex<Host>>, String> {
        let key = SessionKey::shell(&project);
        let mut registry = self.registry()?;
        if let Some(host) = registry.hosts.get(&key).cloned() {
            registry.active = Some(key);
            return Ok(host);
        }
        let host = Arc::new(Mutex::new(Host::new(
            project,
            String::new(),
            registry.app_handle.clone(),
            Arc::new(DesktopAsker::default()),
        )));
        registry.insert(key.clone(), host.clone());
        registry.active = Some(key);
        Ok(host)
    }

    /// Register a freshly connected conversation, unless another navigation
    /// already did. Returns the host that actually owns the key.
    fn install_host(&self, key: SessionKey, host: Arc<Mutex<Host>>) -> Arc<Mutex<Host>> {
        let Ok(mut registry) = self.0.lock() else {
            return host;
        };
        if let Some(existing) = registry.hosts.get(&key).cloned() {
            registry.active = Some(key);
            if let Ok(mut duplicate) = host.lock() {
                plugins_ui::stop_ui_forwarder(&mut duplicate);
            }
            return existing;
        }
        registry.insert(key.clone(), host.clone());
        registry.active = Some(key);
        host
    }

    /// The real per-conversation status the sidebar renders. Read-only and
    /// non-blocking: a locked host is reported busy instead of stalling.
    fn session_states(&self) -> Vec<Value> {
        let Ok(registry) = self.0.lock() else {
            return Vec::new();
        };
        let mut states = Vec::new();
        for (key, host) in &registry.hosts {
            if key.is_shell() {
                continue;
            }
            let (running, waiting, busy) = match host.try_lock() {
                Ok(current) => (
                    current.cancel.is_some(),
                    current.asker.has_pending_for(&key.session),
                    current.runtime_owned.is_some(),
                ),
                Err(_) => (true, false, true),
            };
            if !running && !waiting && !busy {
                continue;
            }
            states.push(json!({
                "project": key.project,
                "session": key.session,
                "running": running,
                "waiting": waiting,
                "busy": busy,
            }));
        }
        states
    }

    fn remove_host(&self, key: &SessionKey) {
        if let Ok(mut registry) = self.0.lock() {
            if let Some(host) = registry.hosts.remove(key) {
                if let Ok(mut host) = host.lock() {
                    plugins_ui::stop_ui_forwarder(&mut host);
                }
            }
            if registry.active.as_ref() == Some(key) {
                registry.active = None;
            }
        }
    }

    /// Keep one context per conversation after the Agent's session identity
    /// changed (fork, imported backup, an unsaved conversation receiving its
    /// first real id). The host lock is released before the registry lock is
    /// taken, so the order is always host-then-registry or registry-only.
    fn reconcile(&self, host: &Arc<Mutex<Host>>) {
        let session = match host.lock() {
            Ok(host) => host.session.clone(),
            Err(_) => return,
        };
        if session.is_empty() {
            return;
        }
        let Ok(mut registry) = self.0.lock() else {
            return;
        };
        let Some(old) = registry
            .hosts
            .iter()
            .find(|(_, candidate)| Arc::ptr_eq(candidate, host))
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        if old.session == session {
            return;
        }
        let new = SessionKey {
            project: old.project.clone(),
            session,
        };
        registry.hosts.remove(&old);
        if !registry.hosts.contains_key(&new) {
            registry.hosts.insert(new.clone(), host.clone());
        }
        if registry.active.as_ref() == Some(&old) {
            registry.active = Some(new);
        }
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
        bail!("This conversation is running a task. Cancel or finish it before changing this session or its plugins.");
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
    let (_, host) = state.active_host()?;
    let probe = host.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut host = host.lock().map_err(ui_error)?;
        work(&mut host).map_err(ui_error)
    })
    .await
    .map_err(ui_error)?;
    // A command may change the Agent's session identity (fork, imported
    // backup, an unsaved conversation receiving its id). Keep the registry key
    // and the active pointer matching the host, without holding a host lock.
    state.inner().reconcile(&probe);
    result
}

/// Resolve an IPC conversation target. Older callers omit both fields and keep
/// operating on the active conversation; a new caller names it exactly.
fn target_key(
    state: &DesktopState,
    project: Option<&str>,
    session: Option<&str>,
) -> std::result::Result<SessionKey, String> {
    match (project.filter(|p| !p.is_empty()), session) {
        (Some(project), Some(session)) => {
            let path = PathBuf::from(project)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(project));
            Ok(SessionKey::new(&path, session))
        }
        _ => state.active_host().map(|(key, _)| key),
    }
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

/// A busy host keeps a complete durable projection. If the pre-task cache is
/// unavailable, read the archive without trying to reopen its writer lock;
/// never publish an empty or session-id-only replacement for existing history.
fn retained_snapshot(host: &Host) -> Result<Option<Value>> {
    if let Some(snapshot) = &host.last_snapshot {
        return Ok(Some(snapshot.clone()));
    }
    if host.session.is_empty() || host.session == "unknown" {
        return Ok(None);
    }
    let catalog = SessionCatalog::new(&host.project)?;
    let archive = catalog.resolve(&host.session)?;
    let mut snapshot = desktop_host::archive_history_page(&archive, &host.session, None, 120)?;
    snapshot["usage"] = serde_json::to_value(catalog.usage(&host.session)?)?;
    snapshot["context_window"] = json!(0);
    snapshot["available_input"] = json!(0);
    snapshot["context"] = Value::Null;
    snapshot["tool_names"] = json!([]);
    Ok(Some(snapshot))
}

pub(crate) fn view(host: &mut Host) -> Result<Value> {
    let snapshot = if let Some(runtime) = &host.runtime {
        let current = desktop_host::snapshot_with_plugins(&runtime.agent, Some(&runtime.plugins))?;
        host.last_snapshot = Some(current.clone());
        Some(current)
    } else {
        // Ready still means "a runtime is installed", but the open
        // conversation keeps its last durable projection while a command owns
        // the runtime; it must never flash as an empty/new session.
        retained_snapshot(host)?
    };
    let plugins = PluginCatalog::discover(&host.project)?;
    Ok(json!({
        "project":host.project,
        // Lets a new renderer tell a per-session host from the older single
        // host that would silently switch the wrong conversation.
        "session_runtime":true,
        "session":host.session,
        "recent_projects":recent_projects(&host.project),
        "default_project":home().ok(),
        // Only use the actual OS user profile for a friendly UI alias.
        // This is display metadata; never rewrite or merge session catalogs.
        "user_home":std::env::var_os("USERPROFILE").or_else(||std::env::var_os("HOME")).map(PathBuf::from),
        "session_groups":session_groups_for(&host.project),
        "ready":host.is_ready(),
        "running":host.cancel.is_some(),
        "waiting":host.asker.has_pending_for(&host.session),
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

fn capability_settings_value(host: &Host) -> Result<Value> {
    let id = host
        .runtime
        .as_ref()
        .map(|runtime| runtime.agent.session_id())
        .or_else(|| {
            host.last_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot["session_id"].as_str())
        })
        .filter(|id| !id.is_empty())
        .unwrap_or(host.session.as_str());
    let id = if id.is_empty() { "unknown" } else { id };
    let definitions = host
        .runtime
        .as_ref()
        .map(|runtime| runtime.capability_tool_definitions())
        .unwrap_or_default();
    kroma::capabilities::settings_value(&host.project, id, definitions)
}

fn apply_runtime_capabilities(
    runtime: &mut DefaultRuntime,
    settings: kroma::capabilities::Capabilities,
) -> Result<()> {
    if runtime.capabilities() == settings {
        return Ok(());
    }
    tokio::task::block_in_place(|| {
        tauri::async_runtime::block_on(runtime.set_capabilities(settings))
    })
}

pub(crate) fn refresh_capabilities(host: &mut Host) -> Result<()> {
    if let Some(runtime) = host.runtime.as_mut() {
        let effective =
            kroma::capabilities::effective_for(&host.project, runtime.agent.session_id())?;
        apply_runtime_capabilities(runtime, effective)?;
    }
    plugins_ui::sync_active_plugins(host);
    if let Some(bus) = host
        .runtime
        .as_ref()
        .filter(|runtime| {
            runtime
                .capabilities()
                .any_enabled(kroma::capabilities::CapabilityGroup::Plugins)
        })
        .map(|runtime| runtime.plugins.ui_bus())
    {
        plugins_ui::start_ui_forwarder(host, bus);
    } else {
        plugins_ui::stop_ui_forwarder(host);
    }
    Ok(())
}

#[tauri::command]
async fn capability_settings(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    with_host(state, |host| capability_settings_value(host)).await
}

#[tauri::command]
async fn set_capability_items(
    state: State<'_, DesktopState>,
    scope: String,
    group: kroma::capabilities::CapabilityGroup,
    items: std::collections::BTreeMap<String, Option<bool>>,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        set_capability_items_value(host, &scope, group, &items)
    })
    .await
}

fn set_capability_items_value(
    host: &mut Host,
    scope: &str,
    group: kroma::capabilities::CapabilityGroup,
    items: &std::collections::BTreeMap<String, Option<bool>>,
) -> Result<Value> {
    use kroma::capabilities::{CapabilityGroup, CapabilityItems};
    require_idle(host)?;
    anyhow::ensure!(
        matches!(scope, "defaults" | "session"),
        "scope must be 'defaults' or 'session'"
    );
    CapabilityItems::default().patch(group, items, scope == "defaults")?;
    let catalog = SessionCatalog::new(&host.project)?;
    let defaults = kroma::capabilities::load_defaults()?;
    let mut next_defaults = defaults.clone();
    let mut id = host
        .runtime
        .as_ref()
        .map(|runtime| runtime.agent.session_id().to_owned())
        .or_else(|| {
            host.last_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot["session_id"].as_str().map(str::to_owned))
        })
        .unwrap_or_else(|| "unknown".into());
    let mut session = catalog.capability_overrides(&id)?;
    session.expand_legacy(&host.project)?;
    if scope == "session" {
        if id == "unknown" {
            let runtime = host
                .runtime
                .as_mut()
                .context("Connect a model before changing session capabilities")?;
            anyhow::ensure!(
                runtime.agent.messages().len() == 1,
                "Cannot replace an unsaved conversation"
            );
            id = runtime.agent.new_session()?;
        }
        session.items.patch(group, items, false)?;
    }
    let mut plugin_defaults = std::collections::BTreeMap::new();
    let mut old_plugin_defaults = std::collections::BTreeMap::new();
    let mut plugin_catalog = None;
    if scope == "defaults" && group == CapabilityGroup::Plugins {
        let plugins = PluginCatalog::discover(&host.project)?;
        for plugin in plugins.summaries()? {
            if let Some(id) = plugin["id"].as_str() {
                old_plugin_defaults.insert(
                    id.to_owned(),
                    plugin["requested"].as_bool().unwrap_or(false),
                );
                if !defaults.plugins || defaults.items.plugins.contains_key(id) {
                    plugin_defaults.insert(
                        id.to_owned(),
                        defaults.enabled(
                            CapabilityGroup::Plugins,
                            id,
                            plugin["requested"].as_bool().unwrap_or(false),
                        ),
                    );
                }
            }
        }
        for (id, enabled) in items {
            plugin_defaults.insert(
                id.clone(),
                enabled.context("default item values cannot be null")?,
            );
        }
        next_defaults.plugins = true;
        next_defaults
            .items
            .plugins
            .retain(|id, _| !old_plugin_defaults.contains_key(id));
        let mut preview = PluginCatalog::discover(&host.project)?;
        preview.preview_default_items(&plugin_defaults)?;
        plugin_catalog = Some(plugins);
    } else if scope == "defaults" {
        next_defaults.items.patch(group, items, true)?;
    }
    let next = session.effective(next_defaults.clone());
    let project = &host.project;
    let persist = || -> Result<()> {
        if let Some(plugins) = plugin_catalog.as_ref() {
            plugins.set_enabled_items(&plugin_defaults)?;
            if next_defaults != defaults {
                if let Err(error) = kroma::capabilities::save_defaults(next_defaults) {
                    if let Err(rollback) = plugins.set_enabled_items(&old_plugin_defaults) {
                        bail!("{error}; restoring plugin defaults failed: {rollback}");
                    }
                    return Err(error);
                }
            }
            Ok(())
        } else if scope == "defaults" {
            kroma::capabilities::patch_defaults(group, items)
        } else {
            catalog.patch_capability_items(&id, project, group, items)
        }
    };
    if let Some(runtime) = host.runtime.as_mut() {
        tokio::task::block_in_place(|| {
            tauri::async_runtime::block_on(runtime.update_capabilities(
                next,
                &plugin_defaults,
                persist,
            ))
        })?;
    } else {
        persist()?;
    }
    host.connect_epoch = host.connect_epoch.wrapping_add(1);
    plugins_ui::sync_active_plugins(host);
    if let Some(runtime) = host.runtime.as_ref() {
        plugins_ui::start_ui_forwarder(host, runtime.plugins.ui_bus());
    }
    if let Some(runtime) = host.runtime.as_ref() {
        host.last_snapshot = Some(desktop_host::snapshot_with_plugins(
            &runtime.agent,
            Some(&runtime.plugins),
        )?);
    }
    capability_settings_value(host)
}

#[tauri::command]
async fn set_capability_settings(
    state: State<'_, DesktopState>,
    scope: String,
    settings: Value,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        require_idle(host)?;
        let defaults = kroma::capabilities::load_defaults()?;
        let catalog = SessionCatalog::new(&host.project)?;
        let (next_defaults, session) = match scope.as_str() {
            "defaults" => {
                let next: kroma::capabilities::Capabilities = serde_json::from_value(settings)?;
                let id = host
                    .runtime
                    .as_ref()
                    .map(|runtime| runtime.agent.session_id())
                    .unwrap_or("unknown");
                (next, catalog.capability_overrides(id)?)
            }
            "session" => {
                let next: kroma::capabilities::CapabilityOverrides =
                    serde_json::from_value(settings)?;
                let runtime = host
                    .runtime
                    .as_mut()
                    .context("Connect a model before changing session capabilities")?;
                if runtime.agent.session_id() == "unknown" {
                    runtime.agent.new_session()?;
                }
                (defaults, next)
            }
            _ => bail!("scope must be 'defaults' or 'session'"),
        };
        let previous = host.runtime.as_ref().map(|runtime| runtime.capabilities());
        if let Some(runtime) = host.runtime.as_mut() {
            apply_runtime_capabilities(runtime, session.effective(next_defaults.clone()))?;
        }
        let saved = if scope == "defaults" {
            kroma::capabilities::save_defaults(next_defaults)
        } else {
            catalog.set_capability_overrides(
                host.runtime
                    .as_ref()
                    .context("Connect a model first")?
                    .agent
                    .session_id(),
                session,
            )
        };
        if let Err(error) = saved {
            if let (Some(runtime), Some(previous)) = (host.runtime.as_mut(), previous) {
                if let Err(rollback) = apply_runtime_capabilities(runtime, previous) {
                    bail!("{error}; restoring previous capabilities failed: {rollback}");
                }
            }
            return Err(error);
        }
        host.connect_epoch = host.connect_epoch.wrapping_add(1);
        refresh_capabilities(host)?;
        // Never serve the previous runtime's tool/UI projection after a toggle.
        if let Some(runtime) = host.runtime.as_ref() {
            host.last_snapshot = Some(desktop_host::snapshot_with_plugins(
                &runtime.agent,
                Some(&runtime.plugins),
            )?);
        }
        capability_settings_value(host)
    })
    .await
}

#[tauri::command]
async fn bootstrap(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    let states = state.inner().session_states();
    with_host(state, move |host| {
        let mut value = view(host)?;
        value["session_states"] = json!(states);
        Ok(value)
    })
    .await
}

/// Read-only review: the user's full working tree, including pre-existing
/// edits. This is deliberately not called "the changes made by this task".
/// Search visible prompts and answers in a bounded set of recent session
/// archives. Tool output and reasoning are never indexed. The scan runs on a
/// blocking pool and never holds the Agent mutex.
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
        let mut projects = recent_projects(&state.project()?);
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
            match catalog.search_conversation_messages(&query, 25, remaining) {
                Ok(entries) => {
                    for entry in entries {
                        hits.push(json!({
                            "project":project,
                            "id":entry.id,
                            "role":entry.role,
                            "message_index":entry.message_index,
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
        let wal_file = PathBuf::from(format!("{}.wal", file.display()));
        let wal_size = fs::metadata(&wal_file).map(|meta| meta.len()).unwrap_or(0);
        bytes += size + wal_size;
        if size > 4096 || wal_size > 0 {
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

/// Which file the renderer may preview. `Draft` is a file the person picked in
/// the native dialog or dropped into the window; `History` may only resolve
/// inside the private attachment store, so a stale WAL entry cannot turn this
/// command into a general file reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewOrigin {
    Draft,
    History,
}

impl PreviewOrigin {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "draft" => Ok(Self::Draft),
            "history" => Ok(Self::History),
            _ => bail!("未知的图片来源"),
        }
    }
}

/// The private store every conversation attachment is copied into.
pub(crate) fn attachment_root() -> Result<PathBuf> {
    Ok(kroma::session_backup::default_attachment_root(&home()?))
}

/// Canonicalize before the prefix check: `..` segments and symlinks resolve to
/// their real target, so neither can leave the attachment store.
pub(crate) fn ensure_history_attachment(root: &Path, raw: &Path) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .with_context(|| format!("附件目录不可用：{}", root.display()))?;
    let full = raw
        .canonicalize()
        .with_context(|| format!("附件不存在：{}", raw.display()))?;
    if !full.starts_with(&root) {
        bail!("只能预览保存到会话附件目录的图片");
    }
    Ok(full)
}

/// A real PNG/JPEG/WebP file inside the shared image bound. The bytes are the
/// only thing that leaves the backend: no path, no base64, no other file kind.
fn read_preview_image(full: &Path) -> Result<Vec<u8>> {
    if !full.metadata()?.is_file() {
        bail!("只能预览普通文件：{}", full.display());
    }
    let mut bytes = Vec::new();
    fs::File::open(full)?
        .take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!("图片超过 8 MiB，无法预览");
    }
    if pasted_image_kind(&bytes).is_none() {
        bail!("只支持 PNG/JPEG/WebP 图片预览");
    }
    Ok(bytes)
}

fn preview_image_bytes(origin: PreviewOrigin, raw: &str) -> Result<Vec<u8>> {
    let path = Path::new(raw);
    let full = match origin {
        PreviewOrigin::Draft => ensure_attachment_path(path)?,
        PreviewOrigin::History => ensure_history_attachment(&attachment_root()?, path)?,
    };
    read_preview_image(&full)
}

/// Read-only image preview as raw bytes. It takes no Agent lock and runs off
/// the UI thread, so a large screenshot cannot stall the window.
#[tauri::command]
async fn preview_image(
    origin: String,
    path: String,
) -> std::result::Result<tauri::ipc::Response, String> {
    let source = PreviewOrigin::parse(&origin).map_err(ui_error)?;
    let bytes = tauri::async_runtime::spawn_blocking(move || preview_image_bytes(source, &path))
        .await
        .map_err(ui_error)?
        .map_err(ui_error)?;
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
async fn project_files(
    state: State<'_, DesktopState>,
    query: String,
) -> std::result::Result<Value, String> {
    let project = state.project()?;
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
    let (_, host) = state.inner().active_host()?;
    let (mut runtime, owner) = {
        let mut host = host.lock().map_err(ui_error)?;
        claim_runtime(&mut host, RuntimePurpose::Compact).map_err(ui_error)?
    };
    let outcome = runtime.agent.compact_with_instructions(&instructions).await;
    let mut host = host.lock().map_err(ui_error)?;
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
    let (_, host) = state.inner().active_host()?;
    let (mut runtime, owner) = {
        let mut host = host.lock().map_err(ui_error)?;
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
    let mut host = host.lock().map_err(ui_error)?;
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
    let (_, host) = state.inner().active_host()?;
    let (mut runtime, owner) = {
        let mut host = host.lock().map_err(ui_error)?;
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
    let saved = match outcome {
        Ok(id) => id,
        Err(error) => {
            let mut host = host.lock().map_err(ui_error)?;
            return_runtime(&mut host, runtime, owner);
            return Err(ui_error(error));
        }
    };
    {
        let mut host = host.lock().map_err(ui_error)?;
        return_runtime(&mut host, runtime, owner);
        // The Agent now belongs to the fork. Move the conversation identity
        // with it; the registry key follows after this lock is released.
        host.session = saved.clone();
        host.connect_epoch = host.connect_epoch.wrapping_add(1);
        host.last_snapshot = None;
        refresh_capabilities(&mut host).map_err(ui_error)?;
    }
    state.inner().reconcile(&host);
    let mut host = host.lock().map_err(ui_error)?;
    Ok(json!({"session_id": saved, "view": view(&mut host).map_err(ui_error)?}))
}

#[tauri::command]
async fn workspace_changes(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    let project = state.project()?;
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
    let project = state.project()?;
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
    // Opening another workspace is navigation, not a mutation of any running
    // conversation: tasks in other projects keep running untouched.
    state.inner().registry()?.project = canonical.clone();
    let host = state.inner().ensure_shell(canonical.clone())?;
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
    tauri::async_runtime::spawn_blocking(move || {
        let mut host = host.lock().map_err(ui_error)?;
        view(&mut host).map_err(ui_error)
    })
    .await
    .map_err(ui_error)?
}

fn connect_config(project: &Path, profile: Option<&str>) -> Result<Config> {
    if let Some(name) = profile.filter(|s| !s.is_empty()) {
        let profiles = Profiles::load()?;
        let chosen = profiles
            .profiles
            .get(name)
            .context("Unknown model profile")?;
        Config::from_profile(chosen, project.to_path_buf(), Some(name.to_owned()))
    } else {
        Config::from_env_for_workdir(project.to_path_buf())
    }
}

async fn build_runtime(
    project: &Path,
    capabilities: kroma::capabilities::Capabilities,
    profile: Option<&str>,
) -> Result<(DefaultRuntime, Arc<DesktopEvents>)> {
    let config = connect_config(project, profile)?;
    let events = Arc::new(DesktopEvents::default());
    let observer: Arc<dyn EventSink> = events.clone();
    let runtime = app::default_runtime_with_capabilities(config, capabilities, move |components| {
        components.events = Some(observer);
        components.defer_initial_session = true;
        Ok(())
    })
    .await?;
    Ok((runtime, events))
}

/// Resume a conversation strictly. A failure means another process owns that
/// history or it is unreadable; the caller must report it instead of publishing
/// an `unknown` runtime under the requested id.
fn resume_runtime(runtime: &mut DefaultRuntime, session: &str) -> Result<()> {
    runtime.agent.resume_session(session)?;
    let _ = backfill_session_title(&runtime.agent);
    Ok(())
}

impl Host {
    /// Bind this conversation's interactive asker to its model tool surface and
    /// to the plugin catalog's UI bus. A plugin question then reaches exactly
    /// this conversation's composer.
    fn bind_asker(&mut self) {
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.agent.set_asker(self.asker.clone());
            runtime.plugins.ui_bus().bind_asker(self.asker.clone());
        }
    }
}

#[tauri::command]
async fn connect(
    state: State<'_, DesktopState>,
    profile: Option<String>,
) -> std::result::Result<Value, String> {
    let (key, host_arc) = state.inner().active_host()?;
    let (key, project, version, epoch, resume, capabilities) = {
        let mut host = host_arc.lock().map_err(ui_error)?;
        require_idle(&host).map_err(ui_error)?;
        host.connect_epoch = host.connect_epoch.wrapping_add(1);
        let catalog = SessionCatalog::new(&host.project).map_err(ui_error)?;
        let resume = host
            .runtime
            .as_ref()
            .map(|runtime| runtime.agent.session_id().to_owned())
            .filter(|id| id != "unknown" && !id.is_empty())
            .or_else(|| {
                (!host.session.is_empty() && host.session != "unknown")
                    .then(|| host.session.clone())
            })
            .or(catalog.recent().map_err(ui_error)?);
        let capabilities = kroma::capabilities::effective_for(
            &host.project,
            resume.as_deref().unwrap_or("unknown"),
        )
        .map_err(ui_error)?;
        (
            key.clone(),
            host.project.clone(),
            host.generation,
            host.connect_epoch,
            resume,
            capabilities,
        )
    };
    // A conversation already owned by another context is reused instead of
    // opening a second runtime for the same archive.
    if let Some(id) = resume.as_ref() {
        let target = SessionKey::new(&project, id);
        if target != key {
            if let Some(existing) = state.inner().host_for(&target) {
                state.inner().focus(&target)?;
                return tauri::async_runtime::spawn_blocking(move || {
                    let mut host = existing.lock().map_err(ui_error)?;
                    view(&mut host).map_err(ui_error)
                })
                .await
                .map_err(ui_error)?;
            }
        }
    }
    let requested_profile = connect_config(&project, profile.as_deref())
        .map_err(ui_error)?
        .selected_profile;
    let (runtime, events) = build_runtime(&project, capabilities, profile.as_deref())
        .await
        .map_err(ui_error)?;
    let (host_arc, resolved) = {
        let mut host = host_arc.lock().map_err(ui_error)?;
        require_idle(&host).map_err(ui_error)?;
        if host.generation != version {
            return Err("Project changed during connection".into());
        }
        if host.connect_epoch != epoch {
            return Err("A newer connection attempt superseded this one".into());
        }
        // The new runtime was built without opening any archive. Release the
        // previous one only now, then resume the requested id strictly: a
        // failure must not leave an `unknown` runtime published under it.
        host.runtime = None;
        host.events = events;
        host.runtime = Some(runtime);
        if let Some(id) = resume.as_ref() {
            let mut runtime = host.runtime.take().expect("runtime installed above");
            if let Err(error) = resume_runtime(&mut runtime, id) {
                // Keep the last durable projection and refuse to send; never
                // install a runtime that belongs to a different conversation.
                return Err(ui_error(anyhow::anyhow!("无法打开会话 {id}：{error:#}")));
            }
            host.runtime = Some(runtime);
            host.session = id.clone();
        } else {
            host.session = "unknown".into();
        }
        host.connected_profile = requested_profile;
        host.plugins_fingerprint = PluginCatalog::source_fingerprint(&host.project);
        host.task_input = host.runtime.as_ref().map(|r| r.agent.task_input());
        host.bind_asker();
        refresh_capabilities(&mut host).map_err(ui_error)?;
        (host_arc.clone(), host.key())
    };
    let (_, host_arc) = state
        .inner()
        .rekey(&key, &resolved, &host_arc)
        .map_err(ui_error)?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut host = host_arc.lock().map_err(ui_error)?;
        view(&mut host).map_err(ui_error)
    })
    .await
    .map_err(ui_error)?
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
        anthropic_cache_ttl: old.anthropic_cache_ttl,
        anthropic_stable_breakpoint: old.anthropic_stable_breakpoint,
        responses_prompt_cache_key: old.responses_prompt_cache_key,
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
        if let Some(runtime) = host.runtime.as_ref() {
            desktop_host::snapshot_page(&runtime.agent, before, limit.unwrap_or(200))
        } else {
            let catalog = SessionCatalog::new(&host.project)?;
            let archive = catalog.resolve(&host.session)?;
            desktop_host::archive_history_page(&archive, &host.session, before, limit.unwrap_or(200))
        }
    })
    .await
}

#[tauri::command]
async fn switch_session(
    state: State<'_, DesktopState>,
    project: Option<String>,
    id: String,
) -> std::result::Result<Value, String> {
    let project = match project.filter(|value| !value.is_empty()) {
        Some(project) => {
            let raw = PathBuf::from(project);
            raw.canonicalize().unwrap_or(raw)
        }
        None => state.project()?,
    };
    let key = SessionKey::new(&project, &id);
    // An already open conversation is reused. Its own lock is independent, so
    // opening it never disturbs a task running in another conversation.
    let host = match state.inner().host_for(&key) {
        Some(host) => {
            state.inner().focus(&key)?;
            host
        }
        None => {
            let capabilities =
                kroma::capabilities::effective_for(&project, &id).map_err(ui_error)?;
            let requested_profile = connect_config(&project, None)
                .map_err(ui_error)?
                .selected_profile;
            let (mut runtime, events) = build_runtime(&project, capabilities, None)
                .await
                .map_err(ui_error)?;
            // Strict: another process may own this archive. Never install a
            // host whose runtime actually belongs to `unknown`.
            resume_runtime(&mut runtime, &id)
                .map_err(|error| ui_error(anyhow::anyhow!("无法打开会话 {id}：{error:#}")))?;
            let app_handle = state
                .inner()
                .registry()
                .map_err(ui_error)?
                .app_handle
                .clone();
            let mut host = Host::new(
                project.clone(),
                id.clone(),
                app_handle,
                Arc::new(DesktopAsker::default()),
            );
            host.events = events;
            host.connected_profile = requested_profile;
            host.runtime = Some(runtime);
            host.plugins_fingerprint = PluginCatalog::source_fingerprint(&project);
            host.task_input = host.runtime.as_ref().map(|r| r.agent.task_input());
            host.bind_asker();
            refresh_capabilities(&mut host).map_err(ui_error)?;
            let host = Arc::new(Mutex::new(host));
            state.inner().install_host(key.clone(), host)
        }
    };
    {
        let mut host = host.lock().map_err(ui_error)?;
        host.bind_asker();
    }
    let mut host = host.lock().map_err(ui_error)?;
    let (snapshot, title, connected_model, reasoning_level, thinking_levels) =
        if let Some(runtime) = host.runtime.as_mut() {
            runtime.plugins.clear_session_ui();
            let title = backfill_session_title(&runtime.agent);
            let snapshot = desktop_host::snapshot_page_with_plugins(
                &runtime.agent,
                None,
                120,
                Some(&runtime.plugins),
            )
            .map_err(ui_error)?;
            let connected_model = runtime.selected_model_name.clone().or_else(|| {
                runtime
                    .agent
                    .model_report()
                    .map(|report| report.model.clone())
            });
            let reasoning_level = runtime.agent.reasoning_level();
            let thinking_levels = runtime.agent.thinking_levels().to_vec();
            host.last_snapshot = Some(snapshot.clone());
            (
                snapshot,
                title,
                connected_model,
                reasoning_level,
                thinking_levels,
            )
        } else {
            // The conversation is running its own task right now. Its last
            // durable projection is what the window can show immediately.
            (
                retained_snapshot(&host)
                    .map_err(ui_error)?
                    .context("该会话尚无可读的历史记录")
                    .map_err(ui_error)?,
                None,
                None,
                None,
                Vec::new(),
            )
        };
    host.connect_epoch = host.connect_epoch.wrapping_add(1);
    Ok(json!({
        "project":host.project,
        "session":host.session,
        "snapshot":snapshot,
        "title":title,
        "connected_model":connected_model,
        "reasoning_level":reasoning_level,
        "thinking_levels":thinking_levels,
    }))
}

#[tauri::command]
async fn new_session(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    let (old_key, host_arc) = state.inner().active_host()?;
    let (project, can_reuse) = {
        let host = host_arc.lock().map_err(ui_error)?;
        let can_reuse =
            host.runtime.is_some() && host.cancel.is_none() && host.runtime_owned.is_none();
        (host.project.clone(), can_reuse)
    };
    // A conversation that is already running keeps its own runtime. A new
    // conversation gets an independent one, so starting it never waits for or
    // mutates a background task.
    if !can_reuse {
        let capabilities = kroma::capabilities::load_defaults().map_err(ui_error)?;
        let (mut runtime, events) = build_runtime(&project, capabilities, None)
            .await
            .map_err(ui_error)?;
        let id = runtime.agent.new_session().map_err(ui_error)?;
        let app_handle = state
            .inner()
            .registry()
            .map_err(ui_error)?
            .app_handle
            .clone();
        let requested_profile = runtime.selected_model_name.clone();
        let mut host = Host::new(
            project.clone(),
            id.clone(),
            app_handle,
            Arc::new(DesktopAsker::default()),
        );
        host.events = events;
        host.connected_profile = requested_profile;
        host.runtime = Some(runtime);
        host.plugins_fingerprint = PluginCatalog::source_fingerprint(&project);
        host.task_input = host.runtime.as_ref().map(|r| r.agent.task_input());
        host.bind_asker();
        refresh_capabilities(&mut host).map_err(ui_error)?;
        let host = Arc::new(Mutex::new(host));
        let host = state
            .inner()
            .install_host(SessionKey::new(&project, &id), host);
        return tauri::async_runtime::spawn_blocking(move || {
            let mut host = host.lock().map_err(ui_error)?;
            view(&mut host).map_err(ui_error)
        })
        .await
        .map_err(ui_error)?;
    }
    let new_key = {
        let mut host = host_arc.lock().map_err(ui_error)?;
        require_idle(&host).map_err(ui_error)?;
        let runtime = host
            .runtime
            .as_mut()
            .context("Connect a model first")
            .map_err(ui_error)?;
        let previous = runtime.capabilities();
        let defaults = kroma::capabilities::load_defaults().map_err(ui_error)?;
        apply_runtime_capabilities(runtime, defaults).map_err(ui_error)?;
        if let Err(error) = runtime.agent.new_session() {
            apply_runtime_capabilities(runtime, previous).map_err(ui_error)?;
            return Err(ui_error(error));
        }
        runtime.plugins.clear_session_ui();
        host.session = runtime.agent.session_id().to_owned();
        host.connect_epoch = host.connect_epoch.wrapping_add(1);
        host.last_snapshot = None;
        refresh_capabilities(&mut host).map_err(ui_error)?;
        host.key()
    };
    let (_, host_arc) = state
        .inner()
        .rekey(&old_key, &new_key, &host_arc)
        .map_err(ui_error)?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut host = host_arc.lock().map_err(ui_error)?;
        view(&mut host).map_err(ui_error)
    })
    .await
    .map_err(ui_error)?
}

#[tauri::command]
async fn delete_session(
    state: State<'_, DesktopState>,
    id: String,
    project: String,
) -> std::result::Result<Value, String> {
    let path = PathBuf::from(&project).canonicalize().map_err(ui_error)?;
    {
        let registry = state.inner().registry()?;
        let global = home().ok();
        if path != registry.project
            && global.as_ref() != Some(&path)
            && !recent_projects(&registry.project).contains(&path)
        {
            return Err(ui_error(anyhow::anyhow!(
                "Select the project before deleting one of its conversations"
            )));
        }
    }
    let catalog = SessionCatalog::new(&path).map_err(ui_error)?;
    if !catalog.path_for(&id).map_err(ui_error)?.is_file() {
        // The row was already removed (or never reached this catalog).
        // Answer with the current state instead of failing on a missing file
        // the user cannot act on.
        return with_host(state, view).await;
    }
    let key = SessionKey::new(&path, &id);
    let host = state.inner().host_for(&key);
    let mut released = false;
    if let Some(host) = host.as_ref() {
        let mut host = host.lock().map_err(ui_error)?;
        if host.cancel.is_some() {
            return Err(ui_error(anyhow::anyhow!(
                "This conversation is running a task. Cancel it before deleting the conversation."
            )));
        }
        if host.session == id {
            // Release the archive lock before moving files into local trash.
            host.runtime
                .as_mut()
                .context("No active runtime")
                .map_err(ui_error)?
                .agent
                .enter_welcome()
                .map_err(ui_error)?;
            released = true;
        }
    }
    if let Err(error) = catalog.trash_session(&id) {
        if released {
            if let Some(host) = host.as_ref() {
                if let Ok(mut host) = host.lock() {
                    if let Some(runtime) = host.runtime.as_mut() {
                        let _ = runtime.agent.resume_session(&id);
                    }
                }
            }
        }
        return Err(ui_error(error));
    }
    state.inner().remove_host(&key);
    let was_active = state
        .inner()
        .registry()
        .map(|registry| registry.active.is_none())
        .unwrap_or(false);
    if was_active {
        let shell = state.inner().ensure_shell(path).map_err(ui_error)?;
        tauri::async_runtime::spawn_blocking(move || {
            let mut host = shell.lock().map_err(ui_error)?;
            view(&mut host).map_err(ui_error)
        })
        .await
        .map_err(ui_error)?
    } else {
        with_host(state, view).await
    }
}

#[tauri::command]
async fn restore_deleted_session(
    state: State<'_, DesktopState>,
    project: String,
    id: String,
    title: Option<String>,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
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

/// Remove the Private Use markers Kroma writes into the transcript for
/// internal attachment and control markers. Nothing else is rewritten.
fn strip_private_use(text: &str) -> String {
    text.chars()
        .filter(|character| !('\u{E000}'..='\u{F8FF}').contains(character))
        .collect()
}

/// A single-line, Markdown-safe label. Control characters (including newlines)
/// are dropped and syntax characters are escaped, so a file name can never
/// become a link, an HTML tag or an extra list entry in the export.
fn markdown_label(raw: &str) -> String {
    let mut label = String::new();
    for character in raw
        .chars()
        .filter(|character| !character.is_control())
        .take(120)
    {
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '<' | '>' | '!' | '|' | '~' | '&'
        ) {
            label.push('\\');
        }
        label.push(character);
    }
    label.trim().to_owned()
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KB", value / KIB)
    } else {
        format!("{:.1} MB", value / (KIB * KIB))
    }
}

/// One readable line per attachment. Only the stored metadata is used: the
/// private path is never written, and the file itself is not copied.
fn attachment_line(item: &Value) -> Option<String> {
    if item["type"] != "kroma_attachment" {
        return None;
    }
    let mut name = markdown_label(item["name"].as_str().unwrap_or(""));
    if name.is_empty() {
        name = "附件".to_owned();
    }
    let kind = match item["kind"].as_str().unwrap_or("file") {
        "image" => "图片".to_owned(),
        "file" => "文件".to_owned(),
        other => markdown_label(other),
    };
    Some(match item["bytes"].as_u64() {
        Some(bytes) => format!("{name} · {kind} · {}", human_bytes(bytes)),
        None => format!("{name} · {kind}"),
    })
}

/// Export only user/assistant-visible conversation text. Private system
/// prompts, tool logs and model reasoning are deliberately excluded. A turn
/// that carried attachments is kept even without text, but the attachments are
/// listed as metadata only: the export never carries the files themselves.
fn transcript_markdown(messages: &[kroma::protocol::Message], title: &str) -> String {
    let clean_title = title.lines().next().unwrap_or("Kroma 会话").trim();
    let mut output = format!(
        "# {}\n\n> Kroma Desktop 导出 · 不包含系统提示词、工具日志和模型思考；附件仅列信息，不含原文件\n",
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
        let content = strip_private_use(message.content.as_deref().unwrap_or(""));
        let attachments = message
            .provider_items
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(attachment_line)
            .collect::<Vec<_>>();
        let body = content.trim();
        if body.is_empty() && attachments.is_empty() {
            continue;
        }
        output.push_str("\n---\n\n## ");
        output.push_str(role);
        output.push_str("\n\n");
        if !body.is_empty() {
            output.push_str(body);
            output.push('\n');
        }
        if !attachments.is_empty() {
            output.push_str("\n附件（仅列信息，不含原文件）：\n\n");
            for line in attachments {
                output.push_str("- ");
                output.push_str(&line);
                output.push('\n');
            }
        }
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
fn cancel_task(
    state: State<'_, DesktopState>,
    project: Option<String>,
    session: Option<String>,
) -> std::result::Result<(), String> {
    let key = target_key(&state, project.as_deref(), session.as_deref())?;
    let host = state
        .host_for(&key)
        .context("That conversation has no running task")
        .map_err(ui_error)?;
    let host = host.lock().map_err(ui_error)?;
    let cancel = host
        .cancel
        .as_ref()
        .context("That conversation has no running task")
        .map_err(ui_error)?;
    cancel.send(true).map_err(ui_error)
}

#[tauri::command]
fn steer_task(
    state: State<'_, DesktopState>,
    text: String,
    project: Option<String>,
    session: Option<String>,
) -> std::result::Result<Value, String> {
    let key = target_key(&state, project.as_deref(), session.as_deref())?;
    let host = state
        .host_for(&key)
        .context("That conversation has no running task")
        .map_err(ui_error)?;
    let host = host.lock().map_err(ui_error)?;
    let handle = host
        .task_input
        .as_ref()
        .context("That conversation has no running task")
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
    project: Option<String>,
    session: Option<String>,
) -> std::result::Result<Value, String> {
    let key = target_key(&state, project.as_deref(), session.as_deref())?;
    let host_arc = state
        .host_for(&key)
        .with_context(|| format!("会话尚未打开：{}", key.session))
        .map_err(ui_error)?;
    let (mut runtime, events, mut cancel_rx, migrated) = {
        let mut host = host_arc.lock().map_err(ui_error)?;
        require_idle(&host).map_err(ui_error)?;
        // Persisted defaults may have changed in another idle host.
        refresh_capabilities(&mut host).map_err(ui_error)?;
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
        // The id is allocated BEFORE the turn starts, so two new chats in one
        // workspace never run under the shared `unknown` placeholder.
        let previous_session = host.session.clone();
        if let Some(runtime) = host.runtime.as_mut() {
            let current = runtime.agent.session_id().to_owned();
            if current.is_empty() || current == "unknown" {
                host.session = runtime.agent.new_session().map_err(ui_error)?;
            }
        }
        // Keep the pre-task transcript visible while this session is busy.
        if let Some(runtime) = host.runtime.as_ref() {
            if let Ok(snapshot) =
                desktop_host::snapshot_with_plugins(&runtime.agent, Some(&runtime.plugins))
            {
                host.last_snapshot = Some(snapshot);
            }
        }
        host.bind_asker();
        let runtime = host
            .runtime
            .take()
            .context("Configure and connect a model first")
            .map_err(ui_error)?;
        let (tx, rx) = watch::channel(false);
        host.task_input = Some(runtime.agent.task_input());
        host.cancel = Some(tx);
        // Questions raised by this turn reach only the window that started it.
        host.asker.register_channel(&host.session, channel.clone());
        let migrated = (previous_session != host.session).then(|| host.session.clone());
        (runtime, host.events.clone(), rx, migrated)
    };
    let mut session_id = migrated.clone().unwrap_or_else(|| key.session.clone());
    if let Some(id) = migrated.as_ref() {
        let new_key = SessionKey {
            project: key.project.clone(),
            session: id.clone(),
        };
        let _ = state.inner().rekey(&key, &new_key, &host_arc);
        let _ = channel.send(json!({
            "kind": "session",
            "project": key.project,
            "session": id,
        }));
        session_id = id.clone();
    }
    let resolved_key = SessionKey {
        project: key.project.clone(),
        session: session_id.clone(),
    };
    let mut all = Vec::new();
    // Import files after acquiring the agent, but always restore the agent
    // below even if one import fails. Never leave the frontend permanently busy.
    // The import is the last step before the Agent writes the user turn, so its
    // failure means nothing was submitted. Every later failure may already be
    // persisted, network or IPC and is never reported as unsubmitted.
    let mut input_rejected = false;
    let outcome = async {
        let imported = (|| -> Result<()> {
            for path in paths.iter().take(8) {
                let source = ensure_attachment_path(Path::new(path))?;
                all.push(host_attachments::import_file(
                    &source,
                    runtime.agent.workdir(),
                )?);
            }
            Ok(())
        })();
        if let Err(error) = imported {
            input_rejected = true;
            return Err(error);
        }
        if runtime.agent.session_id() == "unknown" {
            // Defensive: the id was allocated before the turn started.
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
        Err(error) => {
            let message = format!("{error:#}");
            if input_rejected {
                // Explicit signal: the input was refused before any user turn
                // existed, so the renderer may put the exact prompt and
                // attachment paths back into its draft.
                json!({"status":"failed","error":format!("附件未能加入本次任务：{message}"),"input_rejected":true})
            } else {
                json!({"status":"failed","error":message})
            }
        }
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
        let _ = channel.send(json!({"kind":"snapshot","project":resolved_key.project,"session":resolved_key.session,"snapshot":snapshot,"sessions":sessions,"session_groups":groups}));
    }
    let _ = channel.send(json!({"kind":"done","project":resolved_key.project,"session":resolved_key.session,"result":result}));
    {
        // Restore into the exact context this task started in. The active
        // pointer is irrelevant here: another conversation may be visible now.
        let mut host = host_arc.lock().map_err(ui_error)?;
        host.runtime = Some(runtime);
        host.cancel = None;
        host.task_input = host.runtime.as_ref().map(|r| r.agent.task_input());
        if let Ok(ref snapshot) = snapshot {
            host.last_snapshot = Some(snapshot.clone());
        }
        plugins_ui::sync_active_plugins(&mut host);
        // The turn is over: only this conversation's question is dropped.
        host.asker.unregister_channel(&resolved_key.session);
        host.asker.clear_pending_for(&resolved_key.session);
    }
    if let Ok(mut registry) = state.0.lock() {
        registry.evict_idle(4);
    }
    let snapshot = snapshot.map_err(ui_error)?;
    Ok(json!({
        "project":resolved_key.project,
        "session":resolved_key.session,
        "result":result,
        "snapshot":snapshot,
        "sessions":sessions,
        "session_groups":groups,
    }))
}

pub(crate) fn reload_plugins(host: &mut Host) -> Result<()> {
    let Some(runtime) = host.runtime.as_mut() else {
        return Ok(());
    };
    if !runtime
        .capabilities()
        .any_enabled(kroma::capabilities::CapabilityGroup::Plugins)
    {
        plugins_ui::stop_ui_forwarder(host);
        host.active_plugins = None;
        host.plugins_fingerprint = PluginCatalog::source_fingerprint(&host.project);
        return Ok(());
    }
    let mut next = PluginCatalog::discover(&host.project)?;
    let (tools, prompt) = runtime.prepare_plugin_reload(&mut next)?;
    let sink: Arc<dyn EventSink> = host.events.clone();
    let events = next.wrap_events(Some(sink))?;
    runtime.agent.reload_extensions(tools, prompt, events);
    runtime
        .agent
        .attach_turn_gate(next.turn_end_gate().map(Arc::new));
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

struct SingleInstanceGuard {
    _lock: fs::File,
    port_path: PathBuf,
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.port_path);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InstanceRole {
    /// No lock held (home unavailable or non-contention I/O error): proceed.
    Bypass,
    /// Secondary launch notified (or attempted to notify) the primary window.
    Secondary,
}

fn try_acquire_single_instance(
    kroma_dir: &Path,
) -> std::result::Result<SingleInstanceGuard, InstanceRole> {
    if fs::create_dir_all(kroma_dir).is_err() {
        return Err(InstanceRole::Bypass);
    }
    let lock_path = kroma_dir.join("desktop-instance.lock");
    let port_path = kroma_dir.join("desktop-instance.port");
    match kroma::try_acquire_lock(&lock_path) {
        Ok(lock) => Ok(SingleInstanceGuard {
            _lock: lock,
            port_path,
        }),
        Err(error) if kroma::is_lock_contended(&error) => {
            let _ = signal_existing_instance(&port_path, Duration::from_millis(800));
            Err(InstanceRole::Secondary)
        }
        Err(_) => Err(InstanceRole::Bypass),
    }
}

fn start_focus_listener(
    port_path: &Path,
    on_focus: impl Fn() + Send + Sync + 'static,
) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let token = format!("{}-{}", std::process::id(), nanos);
    let expected = format!("FOCUS {token}");
    let payload = format!("{port} {token}\n");
    let tmp_path = port_path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp_path, payload.as_bytes())?;
    kroma::replace_file_atomically(&tmp_path, port_path)?;

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
            let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
            let Ok(mut writer) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() && line.trim() == expected {
                on_focus();
                let _ = writer.write_all(b"OK\n");
            }
        }
    });
    Ok(())
}

fn signal_existing_instance(port_path: &Path, timeout: Duration) -> bool {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpStream};

    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(raw) = fs::read_to_string(port_path) {
            let mut parts = raw.split_whitespace();
            if let (Some(port_str), Some(token)) = (parts.next(), parts.next()) {
                if let Ok(port) = port_str.parse::<u16>() {
                    let addr = SocketAddr::from(([127, 0, 0, 1], port));
                    if let Ok(mut stream) =
                        TcpStream::connect_timeout(&addr, Duration::from_millis(250))
                    {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
                        if writeln!(stream, "FOCUS {token}").is_ok() {
                            let mut ack = String::new();
                            if BufReader::new(stream).read_line(&mut ack).is_ok()
                                && ack.trim() == "OK"
                            {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

fn focus_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn run() {
    let instance_guard = match home().map(|h| h.join(".kroma")) {
        Ok(kroma_dir) => match try_acquire_single_instance(&kroma_dir) {
            Ok(guard) => Some(guard),
            Err(InstanceRole::Secondary) => return,
            Err(InstanceRole::Bypass) => None,
        },
        Err(_) => None,
    };
    let listener_port_path = instance_guard.as_ref().map(|guard| guard.port_path.clone());

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("kroma-local-navigation")
                .on_navigation(|_webview, url| allows_webview_navigation(url.as_str()))
                .build(),
        )
        .manage(DesktopState::new())
        .setup(move |app| {
            let state = app.state::<DesktopState>();
            plugins_ui::attach_app(&state, app.handle().clone());
            if let Some(port_path) = listener_port_path.as_deref() {
                let handle = app.handle().clone();
                let _ = start_focus_listener(port_path, move || focus_main_window(&handle));
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            capability_settings,
            set_capability_settings,
            set_capability_items,
            search_sessions,
            checkpoint_turns,
            rewind_turn,
            compact_context,
            project_files,
            save_pasted_image,
            preview_image,
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
            session_files::export_session_backup,
            session_files::import_session_backup,
            session_files::inspect_attachments,
            session_files::prepare_followup_attachments,
            session_files::save_attachment_as,
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
            open_external
        ])
        .run(tauri::generate_context!())
        .expect("unable to start Kroma Desktop");
    drop(instance_guard);
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
        assert!(!asker.answer("session-a", 7, kroma::tools::AskAnswer::Dismissed));
    }

    #[test]
    fn a_finished_turn_drops_its_open_question() {
        let asker = DesktopAsker::default();
        let (sender, wait) = tokio::sync::oneshot::channel();
        asker
            .pending
            .lock()
            .expect("pending map")
            .insert(1, ("session-a".into(), sender));
        assert!(asker.answer(
            "session-a",
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
        asker
            .pending
            .lock()
            .expect("pending map")
            .insert(2, ("session-a".into(), sender));
        asker.clear_pending();
        assert!(
            wait.blocking_recv().is_err(),
            "a dropped question has no answer"
        );
        assert!(!asker.answer("session-a", 2, kroma::tools::AskAnswer::Dismissed));
    }

    #[test]
    fn a_question_can_only_be_answered_by_the_conversation_that_showed_it() {
        let asker = DesktopAsker::default();
        let (sender, wait) = tokio::sync::oneshot::channel();
        asker
            .pending
            .lock()
            .expect("pending map")
            .insert(11, ("session-a".into(), sender));
        // The id belongs to A: answering from B is refused, and A can still
        // answer its own question afterwards.
        assert!(!asker.answer("session-b", 11, kroma::tools::AskAnswer::Dismissed));
        assert!(asker.has_pending_for("session-a"));
        assert!(!asker.has_pending_for("session-b"));
        assert!(asker.answer("session-a", 11, kroma::tools::AskAnswer::Dismissed));
        assert!(wait.blocking_recv().is_ok());
    }

    #[test]
    fn the_same_session_id_in_two_projects_is_two_conversations() {
        let first = SessionKey::new(Path::new("D:/one"), "shared-id");
        let second = SessionKey::new(Path::new("D:/two"), "shared-id");
        assert_ne!(first, second);
        assert!(!first.is_shell());
        assert!(SessionKey::shell(Path::new("D:/one")).is_shell());
    }

    #[test]
    fn task_routing_keeps_one_context_per_project_and_session() {
        let first = SessionKey::new(Path::new("D:/one"), "same");
        let second = SessionKey::new(Path::new("D:/two"), "same");
        let first_host = Arc::new(Mutex::new(Host::new(
            PathBuf::from("D:/one"),
            "same".into(),
            None,
            Arc::new(DesktopAsker::default()),
        )));
        let second_host = Arc::new(Mutex::new(Host::new(
            PathBuf::from("D:/two"),
            "same".into(),
            None,
            Arc::new(DesktopAsker::default()),
        )));
        let mut hosts = HashMap::new();
        hosts.insert(first.clone(), first_host.clone());
        hosts.insert(second.clone(), second_host.clone());
        let state = DesktopState(Arc::new(Mutex::new(Registry {
            project: PathBuf::from("D:/one"),
            active: Some(first.clone()),
            hosts,
            app_handle: None,
            last_used: 0,
        })));
        // Two workspaces share the id but never share the task context.
        assert!(Arc::ptr_eq(&state.host_for(&first).unwrap(), &first_host));
        assert!(Arc::ptr_eq(&state.host_for(&second).unwrap(), &second_host));
        assert!(state
            .host_for(&SessionKey::new(Path::new("D:/one"), "missing"))
            .is_none());

        // A new conversation receives its real id once: the old key stops
        // resolving and the registry never holds the same host twice.
        let real = SessionKey::new(Path::new("D:/one"), "new-id");
        first_host.lock().unwrap().session = "new-id".into();
        state.rekey(&first, &real, &first_host).unwrap();
        assert_eq!(state.registry().unwrap().hosts.len(), 2);
        assert!(state.host_for(&first).is_none());
        assert!(Arc::ptr_eq(&state.host_for(&real).unwrap(), &first_host));
    }

    #[test]
    fn the_same_conversation_id_in_two_projects_keeps_two_askers() {
        let project_a = PathBuf::from("D:/one");
        let project_b = PathBuf::from("D:/two");
        let first = Arc::new(Mutex::new(Host::new(
            project_a,
            "same".into(),
            None,
            Arc::new(DesktopAsker::default()),
        )));
        let second = Arc::new(Mutex::new(Host::new(
            project_b,
            "same".into(),
            None,
            Arc::new(DesktopAsker::default()),
        )));
        let (first_sender, mut first_wait) = tokio::sync::oneshot::channel();
        let (second_sender, second_wait) = tokio::sync::oneshot::channel();
        first
            .lock()
            .unwrap()
            .asker
            .pending
            .lock()
            .unwrap()
            .insert(5, ("same".into(), first_sender));
        second
            .lock()
            .unwrap()
            .asker
            .pending
            .lock()
            .unwrap()
            .insert(5, ("same".into(), second_sender));
        // The same numeric id in another project owns a different asker: an
        // answer routed through project B never wakes project A's worker.
        assert!(second
            .lock()
            .unwrap()
            .asker
            .answer("same", 5, kroma::tools::AskAnswer::Dismissed));
        assert!(second_wait.blocking_recv().is_ok());
        assert!(first_wait.try_recv().is_err());
        assert!(first.lock().unwrap().asker.has_pending_for("same"));
    }

    #[test]
    fn a_running_conversation_stays_ready_without_holding_the_active_pointer() {
        let project = scratch_dir("ready-while-running");
        let mut host = Host::new(
            project.clone(),
            "session-a".into(),
            None,
            Arc::new(DesktopAsker::default()),
        );
        assert!(!host.is_ready());
        let (cancel, _receiver) = watch::channel(false);
        host.cancel = Some(cancel);
        assert!(host.is_ready(), "a running session is still connected");
        assert_eq!(host.key().session, "session-a");
        fs::remove_dir_all(project).unwrap();
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
    fn markdown_export_keeps_attachment_only_turns_as_metadata() {
        let mut with_image = kroma::protocol::Message::new("user", "看看这个截图\u{E000}");
        with_image.provider_items = Some(vec![json!({
            "type":"kroma_attachment","kind":"image","name":"截图 1.png",
            "path":"/home/someone/.kroma/attachments/paste-1.png","bytes":4_200_000u64
        })]);
        // A turn that is only an attachment has no text at all.
        let mut only_files = kroma::protocol::Message::new("user", "");
        only_files.content = None;
        only_files.provider_items = Some(vec![
            json!({"type":"kroma_attachment","kind":"file","name":"report.pdf","bytes":2048u64}),
            json!({"type":"kroma_attachment","name":"legacy.bin"}),
        ]);
        let markdown = transcript_markdown(&[with_image, only_files], "附件会话");
        assert!(markdown.contains("不含原文件"), "{markdown}");
        assert!(
            markdown.contains("截图 1.png · 图片 · 4.0 MB"),
            "{markdown}"
        );
        assert!(
            markdown.contains("report.pdf · 文件 · 2.0 KB"),
            "{markdown}"
        );
        assert!(markdown.contains("legacy.bin · 文件"), "{markdown}");
        assert!(
            !markdown.contains(".kroma/attachments"),
            "the private stored path never leaves: {markdown}"
        );
        assert!(!markdown.contains('\u{E000}'), "{markdown}");
    }

    #[test]
    fn attachment_names_cannot_become_markdown_syntax() {
        let mut message = kroma::protocol::Message::new("user", "");
        message.content = None;
        message.provider_items = Some(vec![json!({
            "type":"kroma_attachment","kind":"file",
            "name":"x](https://evil.example)\n# <b>bold</b>","bytes":12u64
        })]);
        let markdown = transcript_markdown(&[message], "测试");
        assert!(!markdown.contains("](https://evil.example)"), "{markdown}");
        assert!(!markdown.contains("<b>"), "{markdown}");
        assert!(!markdown.contains("\n# "), "{markdown}");
        // Every syntax character of the name is escaped, brackets included.
        assert!(
            markdown.contains("x\\]\\(https://evil.example\\)"),
            "{markdown}"
        );
        assert!(markdown.contains("\\<b\\>bold\\</b\\>"), "{markdown}");
    }

    #[test]
    fn history_preview_stays_inside_the_attachment_store() {
        let root = scratch_dir("preview-root");
        let store = root.join("store");
        let outside = root.join("outside");
        fs::create_dir_all(store.join("nested")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(store.join("nested").join("shot.png"), b"bytes").unwrap();
        fs::write(outside.join("secret.png"), b"bytes").unwrap();
        let stored = ensure_history_attachment(&store, &store.join("nested").join("shot.png"))
            .expect("a stored attachment is inside the root");
        assert!(stored.ends_with("shot.png"));
        // `..` segments are resolved before the prefix check.
        let traversal = store
            .join("nested")
            .join("..")
            .join("nested")
            .join("shot.png");
        assert!(ensure_history_attachment(&store, &traversal).is_ok());
        let escaped =
            ensure_history_attachment(&store, &store.join("..").join("outside").join("secret.png"))
                .unwrap_err()
                .to_string();
        assert!(escaped.contains("附件目录"), "{escaped}");
        assert!(ensure_history_attachment(&store, &outside.join("secret.png")).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn image_preview_is_bounded_and_magic_checked() {
        let root = scratch_dir("preview-bytes");
        let png = root.join("shot.png");
        fs::write(&png, b"\x89PNG\r\n\x1a\nbody").unwrap();
        assert_eq!(read_preview_image(&png).unwrap(), b"\x89PNG\r\n\x1a\nbody");

        // SVG and HTML are text, not a supported image, whatever the name says.
        for (name, body) in [
            (
                "drawing.png",
                b"<svg xmlns='http://www.w3.org/2000/svg'/>".as_slice(),
            ),
            (
                "page.webp",
                b"<html><script>alert(1)</script></html>".as_slice(),
            ),
            ("notes.jpg", b"hello".as_slice()),
        ] {
            let path = root.join(name);
            fs::write(&path, body).unwrap();
            let refused = read_preview_image(&path).unwrap_err().to_string();
            assert!(refused.contains("PNG/JPEG/WebP"), "{name}: {refused}");
        }

        let directory = read_preview_image(&root).unwrap_err().to_string();
        assert!(directory.contains("普通文件"), "{directory}");

        let huge = root.join("huge.png");
        fs::write(&huge, b"\x89PNG\r\n\x1a\n").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&huge)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        let refused = read_preview_image(&huge).unwrap_err().to_string();
        assert!(refused.contains("8 MiB"), "{refused}");

        assert!(PreviewOrigin::parse("draft").is_ok());
        assert!(PreviewOrigin::parse("history").is_ok());
        assert!(PreviewOrigin::parse("any").is_err());
        fs::remove_dir_all(&root).unwrap();
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
        Host::new(
            project,
            String::new(),
            None,
            Arc::new(DesktopAsker::default()),
        )
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kroma-desktop-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn invalid_item_commands_do_not_mutate_a_disconnected_host() {
        use kroma::capabilities::CapabilityGroup::Tools;
        use std::collections::BTreeMap;
        let project = scratch_dir("invalid-item-command");
        let mut host = idle_host(project.clone());
        let items = BTreeMap::from([("shell".into(), None)]);
        assert!(set_capability_items_value(&mut host, "defaults", Tools, &items).is_err());
        assert!(set_capability_items_value(&mut host, "invalid", Tools, &items).is_err());
        assert!(set_capability_items_value(&mut host, "session", Tools, &items).is_err());
        assert_eq!(host.connect_epoch, 0);
        assert!(host.last_snapshot.is_none());
        fs::remove_dir_all(project).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn item_commands_create_unknown_session_and_reset_legacy_overrides() -> Result<()> {
        use kroma::capabilities::{Capabilities, CapabilityGroup::Tools, CapabilityOverrides};
        use std::collections::BTreeMap;
        let project = scratch_dir("item-session-command");
        let mut host = idle_host(project.clone());
        let profile = ProviderProfile {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "fixture".into(),
            api_protocol: Some("chat".into()),
            context_window: Some(65536),
            max_output_tokens: Some(4096),
            ..Default::default()
        };
        host.runtime = Some(
            app::default_runtime_with_capabilities(
                Config::from_profile(&profile, project.clone(), None)?,
                Capabilities {
                    mcp: false,
                    plugins: false,
                    ..Default::default()
                },
                |components| {
                    components.defer_initial_session = true;
                    Ok(())
                },
            )
            .await?,
        );
        assert_eq!(host.runtime.as_ref().unwrap().agent.session_id(), "unknown");
        let before = serde_json::to_value(host.runtime.as_ref().unwrap().agent.messages())?;
        let settings = set_capability_items_value(
            &mut host,
            "session",
            Tools,
            &BTreeMap::from([("shell".into(), Some(false))]),
        )?;
        for group in ["tools", "skills", "mcp", "plugins"] {
            assert!(settings["items"][group].is_array());
        }
        let shell = |settings: &Value| {
            settings["items"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == "shell")
                .unwrap()
                .clone()
        };
        assert_eq!(shell(&settings)["session_enabled"], false);
        assert_eq!(shell(&settings)["effective_enabled"], false);
        let id = host.runtime.as_ref().unwrap().agent.session_id().to_owned();
        assert_ne!(id, "unknown");
        assert_eq!(
            serde_json::to_value(host.runtime.as_ref().unwrap().agent.messages())?,
            before
        );
        let catalog = SessionCatalog::new(&project)?;
        catalog.set_capability_overrides(
            &id,
            CapabilityOverrides {
                tools: Some(false),
                ..Default::default()
            },
        )?;
        let legacy = capability_settings_value(&host)?;
        assert_eq!(
            shell(&legacy)["session_enabled"],
            false,
            "legacy gate was hidden"
        );
        let inherited = set_capability_items_value(
            &mut host,
            "session",
            Tools,
            &BTreeMap::from([("shell".into(), None)]),
        )?;
        assert!(shell(&inherited)["session_enabled"].is_null());
        assert_eq!(
            shell(&inherited)["effective_enabled"],
            shell(&inherited)["default_enabled"]
        );
        assert!(catalog.capability_overrides(&id)?.tools.is_none());
        host.runtime = None;
        catalog.trash_session(&id)?;
        fs::remove_dir_all(project)?;
        Ok(())
    }

    #[test]
    fn unreadable_inventory_does_not_hide_capability_switches() {
        let project = scratch_dir("capability-inventory-error");
        fs::create_dir_all(project.join(".kroma")).unwrap();
        fs::write(project.join(".kroma/skills"), "not a directory").unwrap();
        let host = idle_host(project.clone());
        let settings = capability_settings_value(&host).unwrap();
        assert!(settings["defaults"]["skills"].is_boolean());
        assert!(settings["session"]["skills"].is_null());
        assert!(settings["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("Skill")));
        fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn retained_snapshot_keeps_complete_history_while_the_task_owns_the_agent() {
        let mut host = idle_host(std::env::temp_dir());
        host.session = "A".into();
        host.last_snapshot = Some(json!({
            "session_id":"A","messages":[
                {"role":"user","content":"previous question"},
                {"role":"assistant","content":"previous answer"},
            ],"total_messages":2,
        }));
        host.cancel = Some(tokio::sync::watch::channel(false).0);
        let snapshot = retained_snapshot(&host).unwrap().unwrap();
        assert_eq!(snapshot["messages"].as_array().unwrap().len(), 2);
        assert_eq!(snapshot["messages"][0]["content"], "previous question");
        assert!(host.runtime.is_none());
        assert!(host.is_ready());
    }

    #[test]
    fn retained_snapshot_does_not_invent_history_for_an_unsaved_workspace() {
        let mut host = idle_host(std::env::temp_dir());
        assert!(retained_snapshot(&host).unwrap().is_none());
        host.session = "unknown".into();
        assert!(retained_snapshot(&host).unwrap().is_none());
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
        let mut host = idle_host(project.clone());
        let empty = view(&mut host).unwrap();
        assert!(empty["snapshot"].is_null());
        assert_eq!(empty["ready"], false);
        assert_eq!(empty["session_runtime"], true);
        host.last_snapshot = Some(json!({"session_id": "session-a", "messages": []}));
        let during = view(&mut host).unwrap();
        // The shell key has no session id yet, so it is still not a connection.
        assert_eq!(during["snapshot"]["session_id"], "session-a");
        assert_eq!(during["ready"], false);

        // A real conversation keeps its last durable projection (and stays
        // ready) while a command owns its runtime.
        let mut connected = Host::new(
            project.clone(),
            "session-a".into(),
            None,
            Arc::new(DesktopAsker::default()),
        );
        connected.last_snapshot = Some(json!({"session_id": "session-a", "messages": []}));
        connected.runtime_owned = Some(RuntimeOwner {
            token: 1,
            purpose: RuntimePurpose::Rewind,
        });
        assert!(connected.is_ready());
        fs::remove_dir_all(project).unwrap();
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
        let oversized = vec![0x89u8; MAX_IMAGE_BYTES + 1];
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

    #[test]
    fn single_instance_lock_focuses_primary_instead_of_opening_second_webview() {
        let dir = std::env::temp_dir().join(format!(
            "kroma-desktop-single-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let guard = match try_acquire_single_instance(&dir) {
            Ok(guard) => guard,
            Err(_) => panic!("first instance must acquire lock"),
        };
        let focused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&focused);
        start_focus_listener(&guard.port_path, move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .expect("focus listener should bind");

        assert!(matches!(
            try_acquire_single_instance(&dir),
            Err(InstanceRole::Secondary)
        ));
        assert!(
            focused.load(std::sync::atomic::Ordering::SeqCst),
            "secondary launch must wake the primary instance"
        );

        drop(guard);
        assert!(try_acquire_single_instance(&dir).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
