use crate::{reload_plugins, require_idle, ui_error, with_host, DesktopState, Host};
use anyhow::{bail, Context, Result};
use kroma::plugins::{PluginCatalog, PluginUiBus};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tauri::{AppHandle, Emitter, State};

pub fn stop_ui_forwarder(host: &mut Host) {
    if let Some(handle) = host.ui_forwarder.take() {
        handle.abort();
    }
}

pub fn start_ui_forwarder(host: &mut Host, ui_bus: Arc<PluginUiBus>) {
    stop_ui_forwarder(host);
    let Some(app) = host.app_handle.clone() else {
        return;
    };
    let mut rx = ui_bus.subscribe();
    let handle = tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let _ = app.emit("plugin_ui", json!(event));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let _ = app.emit(
                        "plugin_ui",
                        json!({"kind": "status_changed", "statuses": ui_bus.statuses()}),
                    );
                    let _ = app.emit(
                        "plugin_ui",
                        json!({"kind": "widgets_changed", "widgets": ui_bus.widget_values()}),
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    host.ui_forwarder = Some(handle);
}

pub fn sync_active_plugins(host: &mut Host) {
    host.active_plugins = host.runtime.as_ref().map(|runtime| {
        (
            runtime.plugins.clone(),
            runtime.agent.session_id().to_owned(),
            runtime.agent.archive_path().to_path_buf(),
        )
    });
}

pub(crate) fn validate_plugin_source(directory: &Path) -> Result<PathBuf> {
    let meta = fs::symlink_metadata(directory)
        .with_context(|| format!("插件目录不存在：{}", directory.display()))?;
    if meta.file_type().is_symlink() {
        bail!("插件目录不能是符号链接：{}", directory.display());
    }
    if !meta.is_dir() {
        bail!("插件源必须是一个文件夹：{}", directory.display());
    }
    let source = directory
        .canonicalize()
        .with_context(|| format!("无法打开插件目录：{}", directory.display()))?;
    let manifest = source.join("plugin.toml");
    let manifest_meta = fs::symlink_metadata(&manifest)
        .with_context(|| format!("该文件夹缺少 plugin.toml：{}", source.display()))?;
    if manifest_meta.file_type().is_symlink() {
        bail!("plugin.toml 不能是符号链接：{}", manifest.display());
    }
    if !manifest_meta.is_file() {
        bail!("plugin.toml 必须是普通文件：{}", manifest.display());
    }
    if manifest_meta.len() > 1024 * 1024 {
        bail!("plugin.toml 过大（上限 1 MiB）：{}", manifest.display());
    }
    let text = fs::read_to_string(&manifest)
        .with_context(|| format!("无法读取 plugin.toml：{}", manifest.display()))?;
    if text.trim().is_empty() {
        bail!("plugin.toml 为空，无法安装插件：{}", manifest.display());
    }
    text.parse::<toml::Value>().with_context(|| {
        format!(
            "plugin.toml 不是合法的 TOML，无法安装插件：{}",
            manifest.display()
        )
    })?;
    Ok(source)
}

pub(crate) fn parse_setting(value: Value) -> Result<toml::Value> {
    match value {
        Value::String(text) => Ok(toml::Value::String(text)),
        Value::Bool(v) => Ok(toml::Value::Boolean(v)),
        Value::Number(n) if n.is_i64() => Ok(toml::Value::Integer(n.as_i64().unwrap())),
        Value::Number(n) => Ok(toml::Value::Float(n.as_f64().context("bad number")?)),
        _ => bail!("setting must be a string, number or boolean"),
    }
}

pub fn plugin_presentation_value(plugins: &PluginCatalog) -> Value {
    json!({
        "tool_labels": plugins.ui_tool_labels().unwrap_or_default(),
        "activity_frames": plugins.ui_activity_frames().unwrap_or_default(),
        "statuses": plugins.ui_statuses(),
        "widgets": plugins.ui_widgets().iter().map(|w| w.to_json()).collect::<Vec<_>>(),
        "desktop_surfaces": plugins.ui_desktop_surfaces(),
        "live_panel": plugins.ui_bus().live_panel().map(|p| p.to_json()),
        "shortcuts": plugins.ui_shortcuts().unwrap_or_default().into_iter()
            .map(|(key, command)| json!({"key": key.to_string(), "command": command}))
            .collect::<Vec<_>>(),
        "panels": plugins.ui_panels().into_iter()
            .map(|(id, title, dynamic)| json!({"id": id, "title": title, "dynamic": dynamic}))
            .collect::<Vec<_>>(),
        "colors": plugins.ui_colors().unwrap_or_default(),
        "composer_max_rows": plugins.ui_layout().ok().flatten().and_then(|layout| layout.composer_max_rows),
    })
}

fn active_plugins_snapshot(
    state: &State<'_, DesktopState>,
) -> Result<(PluginCatalog, String, PathBuf)> {
    let host = state.0.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(runtime) = host.runtime.as_ref() {
        return Ok((
            runtime.plugins.clone(),
            runtime.agent.session_id().to_owned(),
            runtime.agent.archive_path().to_path_buf(),
        ));
    }
    host.active_plugins.clone().context("Connect a model first")
}

#[tauri::command]
pub async fn plugin_presentation(
    state: State<'_, DesktopState>,
) -> std::result::Result<Value, String> {
    with_host(state, |host| match host.runtime.as_ref() {
        Some(runtime) => Ok(plugin_presentation_value(&runtime.plugins)),
        None => match host.active_plugins.as_ref() {
            Some((plugins, _, _)) => Ok(plugin_presentation_value(plugins)),
            None => Ok(json!({
                "tool_labels": {}, "activity_frames": [], "statuses": [],
                "widgets": [], "desktop_surfaces": [], "live_panel": null,
                "shortcuts": [], "panels": [], "colors": {}, "composer_max_rows": null,
            })),
        },
    })
    .await
}

#[tauri::command]
pub async fn plugin_panel(
    state: State<'_, DesktopState>,
    id: String,
) -> std::result::Result<String, String> {
    with_host(state, move |host| {
        let runtime = host.runtime.as_ref().context("Connect a model first")?;
        runtime.plugins.ui_panel(&id)
    })
    .await
}

#[tauri::command]
pub async fn plugin_panel_view(
    state: State<'_, DesktopState>,
    id: String,
) -> std::result::Result<Value, String> {
    let (plugins, _, _) = active_plugins_snapshot(&state).map_err(ui_error)?;
    plugins.ui_panel_blocks_async(&id).await.map_err(ui_error)
}

#[tauri::command]
pub async fn plugin_desktop_bundle(
    state: State<'_, DesktopState>,
    id: String,
) -> std::result::Result<Value, String> {
    let (plugins, _, _) = active_plugins_snapshot(&state).map_err(ui_error)?;
    plugins.ui_desktop_bundle(&id).map_err(ui_error)
}

#[tauri::command]
pub async fn plugin_ui_action(
    state: State<'_, DesktopState>,
    plugin_id: String,
    widget_id: String,
    action_id: String,
    payload: Option<Value>,
) -> std::result::Result<Value, String> {
    let (plugins, session_id, archive) = active_plugins_snapshot(&state).map_err(ui_error)?;
    let (content, task) = plugins
        .ui_action_async(
            &plugin_id,
            &widget_id,
            &action_id,
            payload,
            &session_id,
            &archive,
        )
        .await
        .map_err(ui_error)?;
    Ok(json!({ "content": content, "task": task }))
}

#[tauri::command]
pub async fn plugin_ui_message(
    state: State<'_, DesktopState>,
    plugin_id: String,
    payload: Value,
) -> std::result::Result<Value, String> {
    let (plugins, session_id, archive) = active_plugins_snapshot(&state).map_err(ui_error)?;
    plugins
        .ui_message_async(&plugin_id, payload, &session_id, &archive)
        .await
        .map_err(ui_error)
}

#[tauri::command]
pub fn answer_question(
    state: State<'_, DesktopState>,
    id: u64,
    picked: Vec<String>,
    typed: Option<String>,
    dismissed: bool,
) -> std::result::Result<(), String> {
    let host = state.0.lock().map_err(ui_error)?;
    let answer = if dismissed {
        kroma::tools::AskAnswer::Dismissed
    } else {
        kroma::tools::AskAnswer::Answered {
            picked,
            typed: typed.filter(|text| !text.trim().is_empty()),
        }
    };
    if host.asker.answer(id, answer) {
        Ok(())
    } else {
        Err("That question is no longer open".into())
    }
}

#[tauri::command]
pub async fn plugin_action(
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
            "install" => {
                let source = validate_plugin_source(Path::new(
                    path.as_deref().context("Plugin directory required")?,
                ))?;
                catalog.install(&source)
            }
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
        sync_active_plugins(host);
        let restart_required = reload.is_err();
        Ok(json!({
            "message": message,
            "restart_required": restart_required,
            "warning": reload.err().map(|e| e.to_string()),
            "plugins": PluginCatalog::discover(&host.project)?.summaries()?,
        }))
    })
    .await
}

pub fn attach_app(state: &DesktopState, app: AppHandle) {
    if let Ok(mut host) = state.0.lock() {
        host.app_handle = Some(app);
    }
}
