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
    let project = host.project.to_string_lossy().into_owned();
    let session = host.session.clone();
    let mut rx = ui_bus.subscribe();
    let handle = tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    // The renderer must only apply a plugin event to the
                    // conversation that produced it. The envelope carries the
                    // owner so a background turn cannot overwrite the visible
                    // composer, theme or widgets.
                    let _ = app.emit(
                        "plugin_ui",
                        json!({"project": project, "session": session, "event": event}),
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let _ = app.emit(
                        "plugin_ui",
                        json!({
                            "project": project,
                            "session": session,
                            "event": {"kind": "status_changed", "statuses": ui_bus.statuses()},
                        }),
                    );
                    let _ = app.emit(
                        "plugin_ui",
                        json!({
                            "project": project,
                            "session": session,
                            "event": {"kind": "widgets_changed", "widgets": ui_bus.widget_values()},
                        }),
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    host.ui_forwarder = Some(handle);
}

pub fn sync_active_plugins(host: &mut Host) {
    host.active_plugins = host
        .runtime
        .as_ref()
        .filter(|runtime| {
            runtime
                .capabilities()
                .any_enabled(kroma::capabilities::CapabilityGroup::Plugins)
        })
        .map(|runtime| {
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
    let layout = plugins.ui_layout().ok().flatten();
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
        "composer_max_rows": layout.as_ref().and_then(|layout| layout.composer_max_rows),
        "layout": {
            "content_width": layout.as_ref().and_then(|layout| layout.desktop_content_width),
            "sidebar": layout.as_ref().and_then(|layout| layout.desktop_sidebar.clone()),
            "density": layout.as_ref().and_then(|layout| layout.desktop_density.clone()),
        },
    })
}

fn active_plugins_snapshot(
    state: &State<'_, DesktopState>,
) -> Result<(PluginCatalog, String, PathBuf)> {
    let (_, host) = state
        .active_host()
        .map_err(|error| anyhow::anyhow!(error))?;
    let host = host.lock().map_err(|error| anyhow::anyhow!("{error}"))?;
    if let Some(runtime) = host.runtime.as_ref() {
        if !runtime
            .capabilities()
            .any_enabled(kroma::capabilities::CapabilityGroup::Plugins)
        {
            bail!("Plugins are disabled for this session");
        }
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
                "layout": {"content_width": null, "sidebar": null, "density": null},
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
    project: Option<String>,
    session: Option<String>,
) -> std::result::Result<(), String> {
    let key = crate::target_key(&state, project.as_deref(), session.as_deref())?;
    let host = state
        .host_for(&key)
        .context("That conversation is no longer open")
        .map_err(ui_error)?;
    let host = host.lock().map_err(ui_error)?;
    let answer = if dismissed {
        kroma::tools::AskAnswer::Dismissed
    } else {
        kroma::tools::AskAnswer::Answered {
            picked,
            typed: typed.filter(|text| !text.trim().is_empty()),
        }
    };
    if host.asker.answer(&key.session, id, answer) {
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
        if matches!(action.as_str(), "enable" | "disable") {
            anyhow::ensure!(
                catalog
                    .summaries()?
                    .iter()
                    .any(|item| item["id"].as_str() == Some(plugin)),
                "plugin '{plugin}' not found"
            );
            crate::set_capability_items_value(
                host,
                "defaults",
                kroma::capabilities::CapabilityGroup::Plugins,
                &std::collections::BTreeMap::from([(plugin.to_owned(), Some(action == "enable"))]),
            )?;
            return Ok(json!({
                "message": format!("Plugin '{plugin}' {}", if action == "enable" { "enabled" } else { "disabled" }),
                "restart_required": false,
                "warning": null,
                "plugins": PluginCatalog::discover(&host.project)?.summaries()?,
            }));
        }
        let message = match action.as_str() {
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
    if let Ok(mut registry) = state.0.lock() {
        registry.app_handle = Some(app.clone());
        for host in registry.hosts.values() {
            if let Ok(mut host) = host.lock() {
                host.app_handle = Some(app.clone());
            }
        }
    }
}
