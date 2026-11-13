//! File-oriented Desktop commands: draft attachment checks, queue attachment
//! snapshots, "save as" for stored attachments and full session backups.
//!
//! Every command here validates its own inputs, does its disk work on the
//! blocking pool and never holds the Agent mutex while reading or writing a
//! large file.
use crate::{
    attachment_root, ensure_attachment_path, ensure_history_attachment, home, refresh_capabilities,
    require_idle, ui_error, view, DesktopState, MAX_IMAGE_BYTES,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use tauri::State;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DRAFT_ATTACHMENTS: usize = 8;

/// Read at most `limit + 1` bytes so a growing file cannot allocate unbounded.
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn image_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || (bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP")
}

/// A user-chosen destination. The native dialog confirms overwrite; this only
/// refuses paths that would replace Kroma's own local data or a symlink the
/// person did not choose as a file.
fn ensure_user_destination_with(
    raw: &str,
    source: Option<&Path>,
    internal_root: Option<PathBuf>,
) -> Result<PathBuf> {
    let raw = Path::new(raw);
    if !raw.is_absolute() {
        bail!("请选择有效的保存位置");
    }
    let name = raw.file_name().context("请选择文件名")?;
    let parent = raw
        .parent()
        .context("请选择文件的保存目录")?
        .canonicalize()
        .with_context(|| format!("保存目录不存在：{}", raw.parent().unwrap_or(raw).display()))?;
    let target = parent.join(name);
    if internal_root.is_some_and(|root| {
        let canonical = root.canonicalize().unwrap_or(root);
        target.starts_with(canonical)
    }) {
        bail!("不能覆盖 Kroma 的本地数据（配置、会话或附件）");
    }
    if let Some(source) = source {
        if source == target {
            bail!("源文件和目标位置相同");
        }
    }
    if target.exists() {
        let metadata = fs::symlink_metadata(&target)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            bail!("目标不是可覆盖的普通文件，请选择新的文件名");
        }
        if let Some(source) = source {
            if source.canonicalize().ok().as_deref() == target.canonicalize().ok().as_deref() {
                bail!("源文件和目标位置相同");
            }
        }
    }
    Ok(target)
}

fn ensure_user_destination(raw: &str, source: Option<&Path>) -> Result<PathBuf> {
    let internal = home()
        .ok()
        .map(|home| home.join(".kroma"))
        .and_then(|path| path.canonicalize().ok());
    ensure_user_destination_with(raw, source, internal)
}

/// Write bytes without ever exposing a half-written destination.
fn write_atomic(destination: &Path, bytes: &[u8]) -> Result<()> {
    let parent = destination.parent().context("保存目录不存在")?;
    let name = destination
        .file_name()
        .context("请选择文件名")?
        .to_string_lossy();
    let temp = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .context("无法创建临时文件")?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // Core keeps the swap atomic on both platforms; a failed replace must
        // leave the existing destination untouched.
        kroma::replace_file_atomically(&temp, destination)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

fn state_of(path: &Path) -> &'static str {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return "missing";
    };
    if !metadata.file_type().is_file() {
        return "not_file";
    }
    let Ok(bytes) = read_bounded(path, 16) else {
        return "missing";
    };
    let image = image_magic(&bytes);
    if image {
        if metadata.len() > MAX_IMAGE_BYTES as u64 {
            return "image_too_large";
        }
    } else if metadata.len() > MAX_FILE_BYTES {
        return "too_large";
    }
    "ok"
}

/// A bounded existence/size check for drafts restored after a restart. It
/// returns no file content and never follows a directory, so a stale draft
/// label cannot become a general file reader.
#[tauri::command]
pub async fn inspect_attachments(paths: Vec<String>) -> std::result::Result<Value, String> {
    let paths = paths
        .into_iter()
        .take(MAX_DRAFT_ATTACHMENTS)
        .collect::<Vec<_>>();
    tauri::async_runtime::spawn_blocking(move || {
        let entries = paths
            .into_iter()
            .map(|path| {
                let candidate = Path::new(&path);
                let state = if candidate.is_absolute() {
                    state_of(candidate)
                } else {
                    "missing"
                };
                json!({"path":path,"state":state})
            })
            .collect::<Vec<_>>();
        json!({"entries":entries})
    })
    .await
    .map_err(ui_error)
}

/// Copy queued files into the private attachment store at queue time, so a
/// later change to the source file cannot alter an already queued task. The
/// running task keeps its Agent; only the project path is read briefly.
#[tauri::command]
pub async fn prepare_followup_attachments(
    state: State<'_, DesktopState>,
    paths: Vec<String>,
) -> std::result::Result<Value, String> {
    if paths.is_empty() {
        return Ok(json!({"items": []}));
    }
    let project = state.project()?;
    let sources = paths
        .into_iter()
        .take(MAX_DRAFT_ATTACHMENTS)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    tauri::async_runtime::spawn_blocking(move || -> Result<Value> {
        let mut items = Vec::new();
        for source in sources {
            let full = ensure_attachment_path(&source)?;
            items.push(kroma::host_attachments::import_file(&full, &project)?);
        }
        Ok(json!({"items":items}))
    })
    .await
    .map_err(ui_error)?
    .map_err(ui_error)
}

/// Copy one stored history attachment to a destination the person picked.
#[tauri::command]
pub async fn save_attachment_as(
    source: String,
    destination: String,
) -> std::result::Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<()> {
        let origin = ensure_history_attachment(&attachment_root()?, Path::new(&source))?;
        let metadata = fs::metadata(&origin)?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            bail!("附件必须是 32 MiB 以内的普通文件");
        }
        let bytes = read_bounded(&origin, MAX_FILE_BYTES)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            bail!("附件超过 32 MiB，无法另存");
        }
        if image_magic(&bytes) && bytes.len() > MAX_IMAGE_BYTES {
            bail!("图片超过 8 MiB，无法另存");
        }
        let target = ensure_user_destination(&destination, Some(&origin))?;
        write_atomic(&target, &bytes)
    })
    .await
    .map_err(ui_error)?
    .map_err(ui_error)
}

fn backup_destination(raw: &str) -> Result<PathBuf> {
    ensure_user_destination(raw, None)
}

/// Write the complete conversation, its context state and its attachments to
/// one local file. Nothing is written when any referenced attachment is
/// missing, so a successful export is always a complete one.
#[tauri::command]
pub async fn export_session_backup(
    state: State<'_, DesktopState>,
    id: String,
    path: String,
) -> std::result::Result<Value, String> {
    let project = {
        let (_, host) = state.active_host()?;
        let host = host.lock().map_err(ui_error)?;
        require_idle(&host).map_err(ui_error)?;
        host.project.clone()
    };
    tauri::async_runtime::spawn_blocking(move || -> Result<Value> {
        let destination = backup_destination(&path)?;
        let root = kroma::session_backup::default_attachment_root(&project);
        let report = kroma::session_backup::export_session(&project, &id, &root, &destination)?;
        Ok(json!({
            "title": report.title,
            "messages": report.messages,
            "attachments": report.attachments,
            "external_references": report.external_references,
        }))
    })
    .await
    .map_err(ui_error)?
    .map_err(ui_error)
}

/// Restore a backup into the current workspace as a new conversation.
///
/// The package is fully validated and its files are written before the Agent
/// is touched. A failure removes everything this call created.
#[tauri::command]
pub async fn import_session_backup(
    state: State<'_, DesktopState>,
    path: String,
) -> std::result::Result<Value, String> {
    let project = {
        let (_, host) = state.active_host()?;
        let host = host.lock().map_err(ui_error)?;
        require_idle(&host).map_err(ui_error)?;
        host.project.clone()
    };
    let root = kroma::session_backup::default_attachment_root(&project);
    let imported = tauri::async_runtime::spawn_blocking(move || {
        kroma::session_backup::import_session(&project, &root, Path::new(&path))
    })
    .await
    .map_err(ui_error)?
    .map_err(ui_error)?;
    let id = imported.id.clone();
    let summary = json!({
        "id": imported.id,
        "title": imported.title,
        "messages": imported.messages,
        "attachments": imported.attachments,
    });
    let outcome = crate::with_host(state, move |host| {
        // Everything is committed to disk already. From here the result may
        // only say whether this window could open the new conversation.
        let warning = if let Err(error) = require_idle(host) {
            format!(
                "会话已导入为 {id}，但当前有任务在运行，暂未切换；任务结束后可在会话列表打开（{error}）"
            )
        } else if host.runtime.is_none() {
            "已导入会话，但尚未连接模型；连接后可在会话列表打开".to_owned()
        } else {
            let runtime = host.runtime.as_mut().expect("runtime checked above");
            match runtime.agent.resume_session(&id) {
                Ok(()) => {
                    runtime.plugins.clear_session_ui();
                    // The Agent now belongs to the imported conversation. Move
                    // the host identity too; `with_host` reconciles the
                    // registry key after this closure releases the lock.
                    host.session = id.clone();
                    host.connect_epoch = host.connect_epoch.wrapping_add(1);
                    refresh_capabilities(host)
                        .err()
                        .map(|error| format!("会话已导入，但刷新会话能力失败：{error:#}"))
                        .unwrap_or_default()
                }
                Err(error) => format!("会话已导入为 {id}，但打开它失败：{error:#}"),
            }
        };
        let mut next = view(host).with_context(|| {
            format!("会话已导入为 {id}（可在会话列表找到），但刷新界面失败")
        })?;
        if let Some(object) = next.as_object_mut() {
            object.insert("imported".to_owned(), summary);
            if !warning.is_empty() {
                object.insert("open_warning".to_owned(), json!(warning));
            }
        }
        Ok(next)
    })
    .await?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kroma-desktop-files-{}-{tag}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn restored_draft_state_distinguishes_missing_size_and_file_kind() -> Result<()> {
        let dir = temp_dir("inspect");
        let plain = dir.join("note.txt");
        fs::write(&plain, b"hello")?;
        assert_eq!(state_of(&plain), "ok");
        assert_eq!(state_of(&dir.join("absent.txt")), "missing");
        assert_eq!(state_of(&dir), "not_file");
        let large = dir.join("large.bin");
        fs::File::create(&large)?.set_len(MAX_FILE_BYTES + 1)?;
        assert_eq!(state_of(&large), "too_large");
        let image = dir.join("large.png");
        fs::write(&image, b"\x89PNG\r\n\x1a\n")?;
        fs::OpenOptions::new()
            .write(true)
            .open(&image)?
            .set_len(MAX_IMAGE_BYTES as u64 + 1)?;
        assert_eq!(state_of(&image), "image_too_large");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn save_destination_never_replaces_kroma_internal_data() -> Result<()> {
        let dir = temp_dir("destination");
        let plain = dir.join("note.txt");
        fs::write(&plain, b"data")?;
        assert_eq!(
            ensure_user_destination(plain.to_str().unwrap(), None)?,
            plain.canonicalize()?
        );
        let internal = temp_dir("internal-store");
        let blocked = internal.join("escape.bin");
        assert!(ensure_user_destination_with(
            blocked.to_str().unwrap(),
            None,
            Some(internal.clone())
        )
        .is_err());
        fs::remove_dir_all(internal)?;
        assert!(ensure_user_destination("relative.bin", None).is_err());
        assert!(
            ensure_user_destination(dir.join("missing/out.bin").to_str().unwrap(), None).is_err()
        );
        assert!(ensure_user_destination(plain.to_str().unwrap(), Some(&plain)).is_err());
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
