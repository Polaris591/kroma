//! Tauri commands only translate GUI actions into the existing Kroma Rust host.
//! No second Agent Loop, HTTP server, or terminal subprocess.
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
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{ipc::Channel, State};
use tokio::sync::watch;

struct Host {
    project: PathBuf,
    generation: u64,
    /// Monotonic token for concurrent IPC connect attempts. Only the newest
    /// attempt may publish a runtime; older completions are discarded.
    connect_epoch: u64,
    runtime: Option<DefaultRuntime>,
    events: Arc<DesktopEvents>,
    cancel: Option<watch::Sender<bool>>,
    task_input: Option<kroma::steering::TaskInput>,
}

/// Cloned cheaply so blocking work can run on Tauri's blocking pool without
/// holding the UI command thread.
#[derive(Clone)]
struct DesktopState(Arc<Mutex<Host>>);

fn ui_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("HOME/USERPROFILE is unavailable")
}

fn last_project_file() -> Result<PathBuf> {
    Ok(home()?.join(".kroma").join("desktop.json"))
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
        Self(Arc::new(Mutex::new(Host {
            project: initial_project(),
            generation: 0,
            connect_epoch: 0,
            runtime: None,
            events: Arc::new(DesktopEvents::default()),
            cancel: None,
            task_input: None,
        })))
    }
}

fn require_idle(host: &Host) -> Result<()> {
    if host.cancel.is_some() {
        bail!("A task is running. Cancel or finish it before changing sessions or plugins.");
    }
    Ok(())
}

async fn with_host<T, F>(state: State<'_, DesktopState>, work: F) -> std::result::Result<T, String>
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
            "native_web_search":p.native_web_search()
        })).collect::<Vec<_>>()
    }))
}

fn sessions_for(project: &Path) -> Result<Value> {
    let catalog = SessionCatalog::new(project)?;
    let recent = catalog.recent()?;
    let mut entries = Vec::new();
    for entry in catalog.list()? {
        entries.push(json!({"id":entry.id,"name":entry.name,
            "recent":recent.as_deref()==Some(&entry.id)}));
    }
    Ok(json!(entries))
}

fn view(host: &Host) -> Result<Value> {
    let snapshot = match &host.runtime {
        Some(runtime) => Some(desktop_host::snapshot(&runtime.agent)?),
        None => None,
    };
    let plugins = PluginCatalog::discover(&host.project)?;
    Ok(json!({
        "project":host.project,
        "ready":host.runtime.is_some(),
        "running":host.cancel.is_some(),
        // Capture the actual connected backend, not the editable default
        // profile (which may change without rebuilding this runtime).
        "connected_model": host.runtime.as_ref().and_then(|runtime| {
            runtime.selected_model_name.clone().or_else(||
                runtime.agent.model_report().map(|report| report.model.clone()))
        }),
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
    with_host(state, |host| view(host)).await
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
            host.project = canonical.clone();
            host.generation += 1;
            host.runtime = None;
            host.task_input = None;
            host.events = Arc::new(DesktopEvents::default());
        }
        if let Ok(path) = last_project_file() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(path, json!({"project":canonical}).to_string());
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
        }
    }
    host.events = events;
    host.runtime = Some(runtime);
    host.task_input = host.runtime.as_ref().map(|r| r.agent.task_input());
    view(&host).map_err(ui_error)
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
        let runtime = host.runtime.as_mut().context("Connect a model first")?;
        runtime.agent.resume_session(&id)?;
        view(host)
    })
    .await
}

#[tauri::command]
async fn new_session(state: State<'_, DesktopState>) -> std::result::Result<Value, String> {
    with_host(state, |host| {
        require_idle(host)?;
        host.runtime
            .as_mut()
            .context("Connect a model first")?
            .agent
            .enter_welcome()?;
        view(host)
    })
    .await
}

#[tauri::command]
async fn rename_session(
    state: State<'_, DesktopState>,
    title: String,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        require_idle(host)?;
        host.runtime
            .as_ref()
            .context("Connect a model first")?
            .agent
            .rename_session(&title)?;
        view(host)
    })
    .await
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
fn steer_task(state: State<'_, DesktopState>, text: String) -> std::result::Result<(), String> {
    let host = state.0.lock().map_err(ui_error)?;
    // The active Agent is temporarily owned by send_message. Its TaskInput
    // handle must be cloned out before starting so steering works concurrently.
    let handle = host
        .task_input
        .as_ref()
        .context("No task is running")
        .map_err(ui_error)?;
    handle.steer(text).map_err(ui_error)
}

#[tauri::command]
fn queue_followup(
    state: State<'_, DesktopState>,
    text: String,
) -> std::result::Result<usize, String> {
    // The desktop host has no follow-up consumer: nothing would ever run a
    // queued task. Say so instead of accepting work that silently never runs;
    // steering the active task or sending the text when it finishes both work.
    let _ = (state, text);
    Err(
        "Queued follow-up tasks are not supported by the desktop host; steer the running task or send this text when it finishes."
            .to_owned(),
    )
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
        let runtime = host
            .runtime
            .take()
            .context("Configure and connect a model first")
            .map_err(ui_error)?;
        let (tx, rx) = watch::channel(false);
        host.task_input = Some(runtime.agent.task_input());
        host.cancel = Some(tx);
        (runtime, host.events.clone(), rx)
    };
    let mut all = Vec::new();
    // Import files after acquiring the agent, but always restore the agent
    // below even if one import fails. Never leave the frontend permanently busy.
    let outcome = async {
        for path in paths.iter().take(8) {
            all.push(host_attachments::import_file(
                Path::new(path),
                runtime.agent.workdir(),
            )?);
        }
        if runtime.agent.session_id() == "unknown" {
            runtime.agent.new_session()?;
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
                &text,
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
        match status {
            Turn::Finished(result) => result.map(|_| "completed".to_owned()),
            Turn::Cancelled => {
                runtime.agent.cancel()?;
                Ok("cancelled".to_owned())
            }
        }
    }
    .await;
    let result = match outcome {
        Ok(status) => json!({"status":status}),
        Err(error) => json!({"status":"failed","error":format!("{error:#}")}),
    };
    let usage = SessionCatalog::new(runtime.agent.workdir())
        .and_then(|catalog| events.persist_usage(&catalog, runtime.agent.session_id()));
    if let Err(error) = usage {
        eprintln!("[desktop] saving usage: {error:#}");
    }
    // Always restore the Agent to the host, even when a UI-only snapshot
    // fails (for example because the usage ledger is temporarily unreadable).
    let snapshot = desktop_host::snapshot(&runtime.agent);
    let last = events.drain();
    if !last.is_empty() {
        let _ = channel.send(json!({"kind":"batch","events":last}));
    }
    if let Ok(ref snapshot) = snapshot {
        let _ = channel.send(json!({"kind":"snapshot","snapshot":snapshot}));
    }
    let _ = channel.send(json!({"kind":"done","result":result}));
    {
        let mut host = state.0.lock().map_err(ui_error)?;
        host.runtime = Some(runtime);
        host.cancel = None;
        host.task_input = host.runtime.as_ref().map(|r| r.agent.task_input());
    }
    let snapshot = snapshot.map_err(ui_error)?;
    Ok(json!({"result":result,"snapshot":snapshot}))
}

fn reload_plugins(host: &mut Host) -> Result<()> {
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
    Ok(())
}

fn parse_setting(value: Value) -> Result<toml::Value> {
    match value {
        Value::String(text) => Ok(toml::Value::String(text)),
        Value::Bool(v) => Ok(toml::Value::Boolean(v)),
        Value::Number(n) if n.is_i64() => Ok(toml::Value::Integer(n.as_i64().unwrap())),
        Value::Number(n) => Ok(toml::Value::Float(n.as_f64().context("bad number")?)),
        _ => bail!("setting must be a string, number or boolean"),
    }
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

#[tauri::command]
async fn plugin_action(
    state: State<'_, DesktopState>,
    action: String,
    id: Option<String>,
    path: Option<String>,
    key: Option<String>,
    value: Option<Value>,
) -> std::result::Result<Value, String> {
    with_host(state, move |host| {
        require_idle(host)?;
        let catalog = PluginCatalog::discover(&host.project)?;
        let plugin = id.as_deref().unwrap_or("");
        let message = match action.as_str() {
            "enable" => catalog.set_enabled(plugin, true),
            "disable" => catalog.set_enabled(plugin, false),
            "install" => catalog.install(Path::new(
                path.as_deref().context("Plugin directory required")?,
            )),
            "uninstall" => catalog.uninstall(plugin),
            "set" => catalog.set_config_value(
                plugin,
                key.as_deref().context("Setting key required")?,
                value.map(parse_setting).transpose()?,
            ),
            "reload" => Ok("Reloading desktop plugins".to_owned()),
            _ => bail!("unknown plugin action"),
        }?;
        let reload = reload_plugins(host);
        let restart_required = reload.is_err();
        Ok(json!({
            "message":message,
            "restart_required":restart_required,
            "warning":reload.err().map(|e|e.to_string()),
            "plugins":PluginCatalog::discover(&host.project)?.summaries()?,
        }))
    })
    .await
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(DesktopState::new())
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            open_project,
            connect,
            save_profile,
            list_sessions,
            history_page,
            switch_session,
            new_session,
            rename_session,
            cancel_task,
            steer_task,
            queue_followup,
            send_message,
            plugin_action,
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
    fn simple_plugin_settings_preserve_typed_values() {
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
            events: Arc::new(DesktopEvents::default()),
            cancel: None,
            task_input: None,
        }
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
