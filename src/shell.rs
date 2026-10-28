use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{
    collections::HashSet,
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::process::Command;

// Keep a single command from flooding a model request. This is NOT a command
// output limit: every byte is captured, and large results stay on disk with
// a path the agent can read in slices. Context has a separate emergency
// projection for many medium-sized results or legacy inline outputs.
const INLINE_OUTPUT_BYTES: u64 = 64 * 1024;
// A log with thousands of tiny lines can be below the byte threshold but
// still flood the model with mostly repetitive evidence.
const INLINE_OUTPUT_LINES: usize = 2_000;
const OUTPUT_PREVIEW_BYTES: usize = 3072;
// Scan at most 8 MiB, distributed across very large captures. The full
// capture stays available even when a diagnostic falls outside the samples.
const DIAGNOSTIC_SCAN_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DIAGNOSTIC_LINES: usize = 16;
static CAPTURE_ID: AtomicU64 = AtomicU64::new(0);
const ORPHAN_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

pub type OutputCallback<'a> = dyn Fn(&str, &str) + Send + Sync + 'a;

// Every current session owns a disjoint subset of its workdir's captures.
// Previous Kroma captures without a session prefix are left readable.
fn capture_prefix(session: &Path) -> String {
    let hash = session
        .to_string_lossy()
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("session-{hash:016x}-")
}

fn referenced_captures(messages: &[crate::protocol::Message]) -> HashSet<PathBuf> {
    let mut used = HashSet::new();
    for message in messages {
        if message.role != "tool" {
            continue;
        }
        let Some(raw) = message.content.as_deref() else {
            continue;
        };
        let Ok(result) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        for field in ["stdout_file", "stderr_file"] {
            if let Some(path) = result.get(field).and_then(serde_json::Value::as_str) {
                used.insert(PathBuf::from(path));
            }
        }
    }
    used
}

fn mcp_image_prefix(session: &Path) -> String {
    format!(
        "mcp-image-{}-",
        session.file_stem().unwrap_or_default().to_string_lossy()
    )
}

fn referenced_mcp_images(messages: &[crate::protocol::Message]) -> HashSet<PathBuf> {
    let mut used = HashSet::new();
    for message in messages.iter().filter(|message| message.role == "tool") {
        let Ok(value) =
            serde_json::from_str::<serde_json::Value>(message.content.as_deref().unwrap_or(""))
        else {
            continue;
        };
        for item in value["content"].as_array().into_iter().flatten() {
            if item["type"] == "image_reference" {
                if let Some(path) = item["path"].as_str() {
                    used.insert(PathBuf::from(path));
                }
            }
        }
    }
    used
}

/// A forked conversation must own its large Shell captures. Otherwise
/// clearing the original conversation can invalidate the fork's file links.
/// Captures are immutable, so hard-link them when possible and copy otherwise.
pub(crate) fn clone_session_captures(
    workdir: &Path,
    source_session: &Path,
    destination_session: &Path,
    messages: &[crate::protocol::Message],
) -> Result<Vec<crate::protocol::Message>> {
    let dir = capture_dir(workdir)?;
    let source_prefix = capture_prefix(source_session);
    let destination_prefix = capture_prefix(destination_session);
    let source_images = mcp_image_prefix(source_session);
    let destination_images = mcp_image_prefix(destination_session);
    let mut cloned = messages.to_vec();
    for msg in &mut cloned {
        if msg.role != "tool" {
            continue;
        }
        let Some(content) = msg.content.as_deref() else {
            continue;
        };
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(content) else {
            continue;
        };
        if let Some(items) = value["content"].as_array_mut() {
            for item in items {
                if item["type"] != "image_reference" {
                    continue;
                }
                let Some(original) = item["path"].as_str() else {
                    continue;
                };
                let old = Path::new(original);
                let Some(suffix) = old
                    .parent()
                    .filter(|parent| *parent == source_session.parent().unwrap_or(Path::new("")))
                    .and_then(|_| old.file_name())
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix(&source_images))
                else {
                    continue;
                };
                let destination = destination_session
                    .parent()
                    .unwrap_or(Path::new(""))
                    .join(format!("{destination_images}{suffix}"));
                if !destination.exists() {
                    fs::hard_link(old, &destination)
                        .or_else(|_| fs::copy(old, &destination).map(|_| ()))
                        .with_context(|| {
                            format!("unable to preserve MCP image {}", old.display())
                        })?;
                }
                item["path"] =
                    serde_json::Value::String(destination.to_string_lossy().into_owned());
            }
        }
        let mut replacements = Vec::new();
        for field in ["stdout_file", "stderr_file"] {
            let Some(original) = value.get(field).and_then(|v| v.as_str()) else {
                continue;
            };
            let old = Path::new(original);
            let Some(suffix) = old
                .parent()
                .filter(|parent| *parent == dir)
                .and_then(|_| old.file_name())
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix(&source_prefix))
            else {
                continue;
            };
            let new = dir.join(format!("{destination_prefix}{suffix}"));
            if !new.exists() {
                fs::hard_link(old, &new)
                    .or_else(|_| fs::copy(old, &new).map(|_| ()))
                    .with_context(|| {
                        format!("unable to preserve Shell capture {}", old.display())
                    })?;
            }
            replacements.push((
                field,
                original.to_owned(),
                new.to_string_lossy().into_owned(),
            ));
        }
        if !replacements.is_empty() {
            for (field, old, new) in replacements {
                value[field] = serde_json::Value::String(new.clone());
                for preview_field in ["stdout", "stderr"] {
                    if let Some(preview) = value[preview_field].as_str() {
                        value[preview_field] =
                            serde_json::Value::String(preview.replace(&old, &new));
                    }
                }
            }
        }
        msg.content = Some(value.to_string());
    }
    Ok(cloned)
}

/// Clean only files that belong to this exact session. A non-referenced file
/// may be a crash orphan; keep it at least 24h before automatic deletion.
/// /clear can request immediate cleanup after the new checkpoint is durable.
pub fn cleanup_session_orphans(
    workdir: &Path,
    session: &Path,
    messages: &[crate::protocol::Message],
    clear: bool,
) -> Result<usize> {
    let root = capture_dir(workdir)?;
    let prefix = capture_prefix(session);
    let referenced = referenced_captures(messages);
    let mut removed = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".log") {
            continue;
        }
        let path = entry.path();
        if referenced.contains(&path) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|changed| changed.elapsed().ok())
            .is_some_and(|elapsed| elapsed >= ORPHAN_GRACE);
        if clear || stale {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    // MCP media is in the session directory, not the Shell capture folder.
    // Only this exact session's files may be cleaned; forked sessions have
    // their own hardlinks and are never made dangling by /clear.
    if let Some(parent) = session.parent() {
        let image_prefix = mcp_image_prefix(session);
        let referenced_images = referenced_mcp_images(messages);
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&image_prefix) {
                continue;
            }
            let path = entry.path();
            if referenced_images.contains(&path) {
                continue;
            }
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_file() {
                continue;
            }
            let stale = meta
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= ORPHAN_GRACE);
            if clear || stale {
                fs::remove_file(path)?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Only remove large capture files explicitly referenced by a session the
/// user has cleared. Unrelated files and still-referenced outputs are intact.
pub fn cleanup_cleared_history(
    workdir: &Path,
    session: &Path,
    messages: &[crate::protocol::Message],
) {
    let Ok(root) = capture_dir(workdir) else {
        return;
    };
    let prefix = capture_prefix(session);
    for path in referenced_captures(messages) {
        if path.parent() == Some(root.as_path())
            && path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with(&prefix) && name.ends_with(".log")
            })
        {
            let _ = fs::remove_file(&path);
        }
    }
    let image_prefix = mcp_image_prefix(session);
    for path in referenced_mcp_images(messages) {
        if path.parent() == session.parent()
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&image_prefix))
        {
            let _ = fs::remove_file(path);
        }
    }
}

// Temporary captures are removed if execution is cancelled or fails. Once
// read_capture succeeds, it either deletes a small capture or retains a large
// one referenced by the persisted tool result.
struct CaptureCleanup {
    paths: Vec<PathBuf>,
}

impl CaptureCleanup {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }

    fn disarm(&mut self) {
        self.paths.clear();
    }
}

impl Drop for CaptureCleanup {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

// On Unix, isolate each shell in its own process group. When a task future is
// cancelled, killing only the parent shell would leave spawned children alive.
#[cfg(unix)]
struct ProcessGroupGuard {
    pgid: i32,
    armed: bool,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed && self.pgid > 0 {
            // A new process group is created for the spawned child (pgid == pid).
            // Negative pid addresses that group, not Kroma's own process group.
            unsafe {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
        }
    }
}

#[derive(Serialize)]
pub struct ShellOutput {
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    stdout: String,
    stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr_file: Option<String>,
    stdout_bytes: u64,
    stderr_bytes: u64,
}

fn capture_dir(workdir: &Path) -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    let hash = workdir
        .to_string_lossy()
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    let dir = home
        .join(".kroma")
        .join("tool-outputs")
        .join(format!("{hash:016x}"));
    #[cfg(unix)]
    let existed = dir.exists();
    fs::create_dir_all(&dir).context("failed to create tool-output directory")?;
    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

fn capture_file(dir: &Path, kind: &str, session: Option<&Path>) -> Result<(PathBuf, File)> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let prefix = session.map(capture_prefix).unwrap_or_default();
    let path = dir.join(format!(
        "{prefix}{}-{now}-{}-{kind}.log",
        std::process::id(),
        CAPTURE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .context("failed to create capture file")?;
    Ok((path, file))
}

fn diagnostic_lines(text: &str, remaining: usize) -> Vec<String> {
    let mut evidence = Vec::new();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if [
            "error:",
            "error[",
            "failed",
            "panicked",
            "traceback",
            "exception",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern))
        {
            let snippet = line.chars().take(240).collect::<String>();
            if !evidence.contains(&snippet) {
                evidence.push(snippet);
            }
            if evidence.len() >= remaining {
                break;
            }
        }
    }
    evidence
}

fn archived_diagnostics(path: &Path, size: u64) -> Result<Vec<String>> {
    let mut file = File::open(path)?;
    let spans = if size <= DIAGNOSTIC_SCAN_BYTES {
        vec![(0, size)]
    } else {
        let window = DIAGNOSTIC_SCAN_BYTES / 4;
        vec![
            (0, window),
            (size / 3, window),
            (size * 2 / 3, window),
            (size - window, window),
        ]
    };
    let mut found = Vec::new();
    for (offset, window) in spans {
        if found.len() >= MAX_DIAGNOSTIC_LINES {
            break;
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; window.min(size - offset) as usize];
        file.read_exact(&mut bytes)?;
        let remaining = MAX_DIAGNOSTIC_LINES - found.len();
        for line in diagnostic_lines(&String::from_utf8_lossy(&bytes), remaining) {
            if !found.contains(&line) {
                found.push(line);
            }
        }
    }
    Ok(found)
}

fn read_capture(path: PathBuf) -> Result<(String, Option<String>, u64)> {
    let size = fs::metadata(&path)?.len();
    if size <= INLINE_OUTPUT_BYTES {
        let content = fs::read(&path).context("failed to read captured tool output")?;
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<_> = text.lines().collect();
        if lines.len() <= INLINE_OUTPUT_LINES {
            fs::remove_file(&path)?;
            return Ok((text.into_owned(), None, size));
        }
        // Preserve both beginning and end, plus a bounded selection of
        // diagnostic lines from the omitted middle. The actual bytes stay on
        // disk regardless of whether the preview contained the error.
        const EDGE_LINES: usize = 100;
        let evidence = diagnostic_lines(
            &lines[EDGE_LINES..lines.len() - EDGE_LINES].join("\n"),
            MAX_DIAGNOSTIC_LINES,
        );
        let mut preview = lines[..EDGE_LINES].join("\n");
        preview.push_str(&format!(
            "\n[... {} middle lines omitted; PREVIEW ONLY; full output stored at {}. Read relevant sections with shell. ...]",
            lines.len() - 2 * EDGE_LINES,
            path.display(),
        ));
        if !evidence.is_empty() {
            preview.push_str("\n[Diagnostic lines from omitted middle:]\n");
            preview.push_str(&evidence.join("\n"));
        }
        preview.push('\n');
        preview.push_str(&lines[lines.len() - EDGE_LINES..].join("\n"));
        return Ok((preview, Some(path.to_string_lossy().into_owned()), size));
    }
    let mut file = File::open(&path)?;
    let mut head = vec![0; OUTPUT_PREVIEW_BYTES];
    file.read_exact(&mut head)?;
    file.seek(SeekFrom::End(-(OUTPUT_PREVIEW_BYTES as i64)))?;
    let mut tail = vec![0; OUTPUT_PREVIEW_BYTES];
    file.read_exact(&mut tail)?;
    let mut content = format!(
        "{}\n[... {} bytes omitted from inline response; PREVIEW ONLY; full output stored at {}. Read that file in smaller slices with shell when needed. ...]\n{}",
        String::from_utf8_lossy(&head),
        size - OUTPUT_PREVIEW_BYTES as u64 * 2,
        path.display(),
        String::from_utf8_lossy(&tail)
    );
    let evidence = archived_diagnostics(&path, size)?;
    if !evidence.is_empty() {
        content.push_str("\n[Diagnostic lines sampled from complete output:]\n");
        content.push_str(&evidence.join("\n"));
    }
    Ok((content, Some(path.to_string_lossy().into_owned()), size))
}

#[cfg(test)]
pub async fn execute(command: &str, workdir: &Path) -> Result<ShellOutput> {
    execute_internal(command, workdir, None, None).await
}

pub async fn execute_for_session(
    command: &str,
    workdir: &Path,
    session: &Path,
) -> Result<ShellOutput> {
    execute_internal(command, workdir, Some(session), None).await
}

/// Optional progress is sent only to the host UI; the complete Shell result
/// and model-visible output are identical to execute_for_session().
pub async fn execute_for_session_with_progress(
    command: &str,
    workdir: &Path,
    session: &Path,
    on_output: Option<&OutputCallback<'_>>,
) -> Result<ShellOutput> {
    execute_internal(command, workdir, Some(session), on_output).await
}

fn preview_new_bytes(
    path: &Path,
    offset: &mut u64,
    label: &str,
    on_output: &OutputCallback<'_>,
) -> Result<()> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    if size <= *offset {
        return Ok(());
    }
    // Reading a tail is cheap even for multi-gigabyte command output. All
    // original bytes remain intact in the file-based capture.
    let previous = *offset;
    let start = size.saturating_sub(4096).max(previous);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((size - start) as usize);
    file.read_to_end(&mut bytes)?;
    *offset = size;
    let mut preview = String::new();
    if start > previous {
        preview.push_str("[... earlier live output omitted ...]\n");
    }
    preview.push_str(&String::from_utf8_lossy(&bytes));
    if !preview.is_empty() {
        on_output(label, &preview);
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Bash,
    /// Unix fallback when `/bin/bash` is missing.
    #[cfg_attr(windows, allow(dead_code))]
    PosixSh,
    /// Windows fallback when no Git Bash is installed.
    #[cfg_attr(not(windows), allow(dead_code))]
    Cmd,
}

/// The shell Kroma will actually use, and therefore the syntax the model may
/// write. Resolved per command so a new installation or `KROMA_SHELL` change is
/// picked up without restarting the agent.
struct ShellChoice {
    program: PathBuf,
    dialect: Dialect,
}

impl ShellChoice {
    fn bash(program: PathBuf) -> Self {
        Self {
            program,
            dialect: Dialect::Bash,
        }
    }

    #[cfg(windows)]
    fn cmd() -> Self {
        Self {
            program: PathBuf::from("cmd"),
            dialect: Dialect::Cmd,
        }
    }

    fn configure(&self, cmd: &mut Command) {
        if self.dialect == Dialect::Bash {
            // Bash otherwise reports only the last pipeline command's status:
            // 'cargo test | tail' can appear successful despite failed tests.
            cmd.arg("-o").arg("pipefail");
        }
        match self.dialect {
            Dialect::Cmd => {
                cmd.arg("/C");
            }
            Dialect::Bash | Dialect::PosixSh => {
                // A login shell re-reads the profile. On Windows a profile can
                // change directory away from the workdir, so only Unix keeps -l.
                #[cfg(not(windows))]
                cmd.arg("-l");
                cmd.arg("-c");
            }
        }
        #[cfg(windows)]
        if self.dialect == Dialect::Bash {
            // A non-login Bash does not add its own tool directories to PATH.
            let dirs = crate::winsys::bash_tool_dirs(&self.program);
            if !dirs.is_empty() {
                let mut paths = dirs;
                if let Some(existing) = env::var_os("PATH") {
                    paths.extend(env::split_paths(&existing));
                }
                if let Ok(joined) = env::join_paths(paths) {
                    cmd.env("PATH", joined);
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn resolve_shell() -> ShellChoice {
    if Path::new("/bin/bash").is_file() {
        ShellChoice::bash(PathBuf::from("/bin/bash"))
    } else {
        ShellChoice {
            program: PathBuf::from("sh"),
            dialect: Dialect::PosixSh,
        }
    }
}

#[cfg(windows)]
fn resolve_shell() -> ShellChoice {
    match windows_bash() {
        Some(bash) => ShellChoice::bash(bash),
        None => ShellChoice::cmd(),
    }
}

/// Git Bash is the only shell commonly installed on Windows that accepts the
/// POSIX syntax coding models emit, so it is preferred over `cmd`.
/// `KROMA_SHELL` overrides the choice: point it at a `bash.exe`, or at `cmd` to
/// force native `cmd` semantics.
#[cfg(windows)]
fn windows_bash() -> Option<PathBuf> {
    if let Some(forced) = env::var_os("KROMA_SHELL") {
        let forced = PathBuf::from(forced);
        let name = forced
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if name == "cmd" || name == "cmd.exe" {
            return None;
        }
        if (name == "bash" || name == "bash.exe") && forced.is_file() {
            return Some(forced);
        }
    }
    let mut candidates = Vec::new();
    for variable in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(root) = env::var_os(variable) {
            let git = PathBuf::from(root).join("Git");
            candidates.push(git.join("bin").join("bash.exe"));
            candidates.push(git.join("usr").join("bin").join("bash.exe"));
        }
    }
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    // bash.exe in System32 launches WSL, which cannot run Windows paths from a
    // Windows working directory, so it is never a candidate.
    let system_bash =
        env::var_os("SystemRoot").map(|root| PathBuf::from(root).join("System32").join("bash.exe"));
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join("bash.exe");
            let wsl_stub = system_bash.as_ref().is_some_and(|stub| {
                candidate
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&stub.to_string_lossy())
            });
            if !wsl_stub {
                candidates.push(candidate);
            }
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// Description of the `shell` tool. It names the dialect actually in use: a
/// model that writes Bash pipelines against `cmd` wastes turns on avoidable
/// syntax failures, and the reverse hides failures behind `pipefail`.
pub fn tool_description() -> &'static str {
    match resolve_shell().dialect {
        Dialect::Bash => {
            "Run a Bash command in the project working directory with pipefail enabled. Returns exit_code, stdout and stderr; oversized or line-heavy output is archived with a readable path and a preview, including notable errors when available. For validation, prefer the unfiltered command and its exit_code: piping test output to tail/grep can hide the named passing test even when exit_code is 0. Pipelines ending in head may report SIGPIPE (141) despite useful output. Combine independent inspections when useful; do not repeat successful checks without a change or missing evidence."
        }
        Dialect::PosixSh => {
            "Execute a POSIX sh command in the project working directory. Returns exit_code, stdout and stderr. Avoid Bash-specific syntax. Keep checks relevant and verify required tests."
        }
        Dialect::Cmd => {
            "Execute commands in the current working directory using Windows cmd (/C). Returns stdout, stderr and exit_code. Use cmd syntax (&&, &, %VAR%, dir) instead of Bash syntax; only run commands needed for the task."
        }
    }
}

async fn execute_internal(
    command: &str,
    workdir: &Path,
    session: Option<&Path>,
    on_output: Option<&OutputCallback<'_>>,
) -> Result<ShellOutput> {
    if command.trim().is_empty() {
        bail!("shell command cannot be empty");
    }

    // Coding models commonly use Bash features (pipefail, arrays, PIPESTATUS).
    // Use Bash when it is installed and reachable, and keep the resolved
    // dialect in the tool description so the model knows what it may write.
    let shell = resolve_shell();

    let dir = capture_dir(workdir)?;
    let (stdout_path, stdout_file) = capture_file(&dir, "stdout", session)?;
    let mut cleanup_stdout = CaptureCleanup::new(vec![stdout_path.clone()]);
    let (stderr_path, stderr_file) = capture_file(&dir, "stderr", session)?;
    let mut cleanup_stderr = CaptureCleanup::new(vec![stderr_path.clone()]);
    let mut cmd = Command::new(&shell.program);
    shell.configure(&mut cmd);
    cmd.arg(command)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }

    #[cfg(windows)]
    let (mut child, mut guard) = crate::winsys::ChildTreeGuard::spawn(&mut cmd)
        .with_context(|| format!("failed to launch contained {}", shell.program.display()))?;
    #[cfg(not(windows))]
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to launch {}", shell.program.display()))?;
    #[cfg(unix)]
    let pid = child.id().context("shell process has no PID")?;
    #[cfg(unix)]
    let mut guard = ProcessGroupGuard {
        pgid: pid as i32,
        armed: true,
    };
    let status = if let Some(on_output) = on_output {
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stdout_offset = 0;
        let mut stderr_offset = 0;
        let status = loop {
            tokio::select! {
                result = child.wait() => break result.with_context(|| format!("failed to run {}", shell.program.display()))?,
                _ = ticker.tick() => {
                    preview_new_bytes(&stdout_path, &mut stdout_offset, "stdout", on_output)?;
                    preview_new_bytes(&stderr_path, &mut stderr_offset, "stderr", on_output)?;
                }
            }
        };
        preview_new_bytes(&stdout_path, &mut stdout_offset, "stdout", on_output)?;
        preview_new_bytes(&stderr_path, &mut stderr_offset, "stderr", on_output)?;
        status
    } else {
        child
            .wait()
            .await
            .with_context(|| format!("failed to run {}", shell.program.display()))?
    };
    #[cfg(any(unix, windows))]
    guard.disarm();

    let (stdout, stdout_file, stdout_bytes) = read_capture(stdout_path)?;
    let (stderr, stderr_file, stderr_bytes) = read_capture(stderr_path)?;
    // Large output files have become part of the tool result and must remain
    // readable. Disarm the cancellation cleanup only for retained captures.
    cleanup_stdout.disarm();
    cleanup_stderr.disarm();
    Ok(ShellOutput {
        exit_code: status.code(),
        signal: {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                status.signal()
            }
            #[cfg(not(unix))]
            {
                None
            }
        },
        stdout,
        stderr,
        stdout_file,
        stderr_file,
        stdout_bytes,
        stderr_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Message;

    #[test]
    fn forked_mcp_image_remains_readable_after_clearing_source() -> Result<()> {
        let dir = env::temp_dir().join(format!(
            "kroma-mcp-owned-test-{}-{}",
            std::process::id(),
            CAPTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir)?;
        let source = dir.join("original.json");
        let branch = dir.join("branch.json");
        let source_image = dir.join("mcp-image-original-1-2.png");
        fs::write(&source_image, b"image-data")?;
        let history = vec![Message::tool(
            "mcp-1".into(),
            serde_json::json!({"content":[{
                "type":"image_reference", "mimeType":"image/png", "path":source_image
            }],"isError":false})
            .to_string(),
        )];
        let cloned = clone_session_captures(&dir, &source, &branch, &history)?;
        let result: serde_json::Value =
            serde_json::from_str(cloned[0].content.as_deref().unwrap())?;
        let copied = PathBuf::from(result["content"][0]["path"].as_str().unwrap());
        assert_ne!(copied, source_image);
        assert_eq!(fs::read(&copied)?, b"image-data");
        cleanup_cleared_history(&dir, &source, &history);
        assert!(!source_image.exists());
        assert_eq!(fs::read(&copied)?, b"image-data");
        cleanup_session_orphans(&dir, &branch, &[], true)?;
        assert!(!copied.exists());
        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn shell_emits_progress_before_completion_without_changing_result() -> Result<()> {
        use std::sync::{Arc, Mutex};
        let root = env::temp_dir().join(format!(
            "kroma-shell-progress-test-{}-{}",
            std::process::id(),
            CAPTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let capture = seen.clone();
        let callback = move |stream: &str, text: &str| {
            if stream == "stdout" {
                capture.lock().unwrap().push(text.to_owned());
            }
        };
        let output = execute_for_session_with_progress(
            "printf 'first\\n'; sleep 0.45; printf 'second\\n'",
            &root,
            &root.join("session.json"),
            Some(&callback),
        )
        .await?;
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("first\nsecond\n"));
        let progress = seen.lock().unwrap();
        assert!(
            progress.len() >= 2,
            "expected progress while Shell was still running: {progress:?}"
        );
        assert!(progress[0].contains("first"));
        assert!(progress.last().unwrap().contains("second"));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn session_orphan_cleanup_keeps_live_and_unrelated_captures() {
        let dir = env::temp_dir().join(format!(
            "kroma-owned-captures-{}-{}",
            std::process::id(),
            CAPTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let capture = capture_dir(&dir).unwrap();
        let a = dir.join("a-session.json");
        let b = dir.join("b-session.json");
        let live = capture.join(format!("{}live.log", capture_prefix(&a)));
        let orphan = capture.join(format!("{}orphan.log", capture_prefix(&a)));
        let other = capture.join(format!("{}other.log", capture_prefix(&b)));
        for path in [&live, &orphan, &other] {
            fs::write(path, b"full tool output").unwrap();
        }
        let history = vec![Message::tool(
            "tool-id".into(),
            serde_json::json!({
                "exit_code":0,
                "stdout_file": live.to_string_lossy(),
                "stdout": "preview",
                "stderr":""
            })
            .to_string(),
        )];
        assert_eq!(
            cleanup_session_orphans(&dir, &a, &history, false).unwrap(),
            0
        );
        assert_eq!(
            cleanup_session_orphans(&dir, &a, &history, true).unwrap(),
            1
        );
        assert!(live.is_file());
        assert!(!orphan.exists());
        assert!(other.is_file());
        cleanup_cleared_history(&dir, &a, &history);
        assert!(!live.exists());
        assert!(other.exists());
        fs::remove_file(other).unwrap();
        fs::remove_dir(capture).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn clearing_a_session_keeps_another_sessions_capture() {
        let dir = env::temp_dir().join(format!(
            "kroma-capture-owner-{}-{}",
            std::process::id(),
            CAPTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let owner = dir.join("owner.json");
        let foreign = dir.join("foreign.json");
        let output = execute_for_session("printf '%70000s' x", &dir, &owner)
            .await
            .unwrap();
        let saved = serde_json::to_value(&output).unwrap();
        let capture = PathBuf::from(saved["stdout_file"].as_str().unwrap());
        assert!(capture.is_file());
        // A generic tool may expose a readable reference to another session's log.
        let messages = vec![Message::tool("external".into(), saved.to_string())];
        cleanup_cleared_history(&dir, &foreign, &messages);
        assert!(capture.is_file(), "foreign capture must survive");
        cleanup_cleared_history(&dir, &owner, &messages);
        assert!(!capture.is_file(), "owner capture must be removed");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clear_deletes_only_referenced_large_tool_capture() {
        let dir = env::temp_dir().join(format!(
            "kroma-capture-clear-test-{}-{}",
            std::process::id(),
            CAPTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let capture = capture_dir(&dir).unwrap();
        let referenced = capture.join(format!(
            "{}referenced.log",
            capture_prefix(&dir.join("current.json"))
        ));
        let unrelated = capture.join("other.log");
        fs::write(&referenced, "referenced").unwrap();
        fs::write(&unrelated, "other").unwrap();
        let message = Message::tool(
            "id".into(),
            serde_json::json!({
                "exit_code": 0, "stdout_file": referenced.to_string_lossy(),
                "stdout": "preview", "stderr": ""
            })
            .to_string(),
        );
        cleanup_cleared_history(&dir, &dir.join("current.json"), &[message]);
        assert!(!referenced.exists());
        assert!(unrelated.exists());
        fs::remove_file(&unrelated).unwrap();
        fs::remove_dir(&capture).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancellation_cleanup_removes_only_uncommitted_capture() {
        let dir = std::env::temp_dir().join(format!(
            "kroma-cleanup-test-{}-{}",
            std::process::id(),
            CAPTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let partial = dir.join("partial.log");
        std::fs::write(&partial, b"some uncommitted bytes").unwrap();
        {
            let _guard = CaptureCleanup::new(vec![partial.clone()]);
        }
        assert!(!partial.exists());
        let retained = dir.join("retained.log");
        std::fs::write(&retained, b"saved session references this").unwrap();
        {
            let mut guard = CaptureCleanup::new(vec![retained.clone()]);
            guard.disarm();
        }
        assert!(retained.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn runs_shell_and_returns_output() {
        let cwd = std::env::current_dir().unwrap();
        let result = execute("printf 'hello'; printf 'warning' >&2", &cwd)
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout, "hello");
        assert_eq!(result.stderr, "warning");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn nonzero_exit_is_returned_to_model() {
        let cwd = std::env::current_dir().unwrap();
        let output = execute("printf 'failed' >&2; exit 7", &cwd).await.unwrap();
        assert_eq!(output.exit_code, Some(7));
        assert_eq!(output.stderr, "failed");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn failing_test_pipeline_is_not_reported_as_success() {
        if !Path::new("/bin/bash").is_file() {
            return;
        }
        let cwd = std::env::current_dir().unwrap();
        let output = execute("sh -c 'echo test-failed; exit 7' 2>&1 | tail -10", &cwd)
            .await
            .unwrap();
        assert_eq!(output.exit_code, Some(7));
        assert!(output.stdout.contains("test-failed"));
        let output = execute("printf 'ok\\n' | tail -1", &cwd).await.unwrap();
        assert_eq!(output.exit_code, Some(0));
        let output = execute("yes x | head -c 1000", &cwd).await.unwrap();
        assert_eq!(output.stdout.len(), 1000);
        assert_eq!(output.exit_code, Some(141), "head causes SIGPIPE upstream");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn keeps_complete_output() {
        let cwd = std::env::current_dir().unwrap();
        let output = execute("yes x | head -c 40000", &cwd).await.unwrap();
        assert!(output.stdout.contains("PREVIEW ONLY"));
        let path = output.stdout_file.expect("all original bytes are retained");
        assert_eq!(fs::read(&path).unwrap().len(), 40000);
        fs::remove_file(path).unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn output_limit_is_preview_not_command_truncation() {
        let cwd = std::env::current_dir().unwrap();
        let small = execute("python3 -c \"print('A'*60000)\"", &cwd)
            .await
            .unwrap();
        assert_eq!(small.exit_code, Some(0));
        assert!(small.stdout_file.is_none());
        assert_eq!(small.stdout.len(), 60001);
        let large = execute("python3 -c \"print('B'*100000)\"", &cwd)
            .await
            .unwrap();
        assert_eq!(large.exit_code, Some(0));
        assert!(large.stdout.contains("PREVIEW ONLY"));
        assert!(large.stdout.len() < 10000);
        let path = large.stdout_file.expect("complete output retained on disk");
        let full = fs::read(&path).unwrap();
        assert_eq!(full.len(), 100001);
        assert!(full.starts_with(b"BBBB"));
        fs::remove_file(path).unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn large_output_preserves_critical_error_from_the_middle() {
        let cwd = std::env::current_dir().unwrap();
        let output = execute(
            "python3 -c \"print('A'*40000); print('error: CENTRAL_FAILURE_17'); print('B'*40000)\"",
            &cwd,
        )
        .await
        .unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("CENTRAL_FAILURE_17"));
        assert!(output.stdout.contains("PREVIEW ONLY"));
        assert!(output.stdout.len() < 12_000);
        let path = output.stdout_file.expect("complete output archived");
        let original = fs::read_to_string(&path).unwrap();
        assert!(original.len() > 64 * 1024);
        assert!(original.contains("CENTRAL_FAILURE_17"));
        fs::remove_file(path).unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn thousands_of_short_lines_are_previewed_with_midstream_errors() {
        let cwd = std::env::current_dir().unwrap();
        let output = execute(
            "python3 -c \"for i in range(5000): print('error: FAILED_TEST_MIDDLE' if i == 2500 else 'samplelog')\"",
            &cwd,
        )
        .await
        .unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("PREVIEW ONLY"));
        assert!(output.stdout.contains("FAILED_TEST_MIDDLE"));
        assert!(output.stdout.contains("samplelog"));
        assert!(output.stdout.len() < 10_000);
        let path = output.stdout_file.expect("full line-heavy output retained");
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 5000);
        fs::remove_file(path).unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn oversized_stderr_keeps_original_failure_evidence() {
        let cwd = std::env::current_dir().unwrap();
        let output = execute(
            "python3 -c \"import sys; sys.stderr.write('ERR'*30000); sys.exit(13)\"",
            &cwd,
        )
        .await
        .unwrap();
        assert_eq!(output.exit_code, Some(13));
        assert!(output.stdout_file.is_none());
        assert!(output.stderr.contains("PREVIEW ONLY"));
        assert!(output.stderr.len() < 10_000);
        let path = output
            .stderr_file
            .expect("large stderr must remain available");
        let full = fs::read(&path).unwrap();
        assert_eq!(full.len(), 90_000);
        assert!(full.starts_with(b"ERRERR"));
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clearing_session_deletes_referenced_large_capture_only() {
        let cwd = std::env::current_dir().unwrap();
        let result = execute("yes x | head -c 1100000", &cwd).await.unwrap();
        let path = result
            .stdout_file
            .clone()
            .expect("large result stored on disk");
        assert!(Path::new(&path).is_file());
        let raw = serde_json::to_string(&result).unwrap();
        cleanup_cleared_history(
            &cwd,
            &cwd.join("test-session.json"),
            &[Message::tool("large".into(), raw)],
        );
        assert!(!Path::new(&path).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_shell_descendants() {
        use std::time::Duration;
        let test_dir = std::env::temp_dir().join(format!(
            "kroma-process-group-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&test_dir).unwrap();
        let marker = test_dir.join("orphan.txt");
        let cwd = test_dir.clone();
        let task = tokio::spawn(async move {
            execute("(sleep 1; printf orphan > orphan.txt) & wait", &cwd).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!task.is_finished(), "shell finished before cancellation");
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(1150)).await;
        assert!(!marker.exists(), "a descendant survived shell cancellation");
        std::fs::remove_dir_all(&test_dir).unwrap();
    }

    // The shell must start in the requested workdir on every platform: a login
    // profile that changes directory would silently redirect every relative
    // path the model writes.
    #[tokio::test]
    async fn shell_starts_in_the_requested_workdir() {
        let dir = std::env::temp_dir().join(format!(
            "kroma-shell-cwd-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        // `echo` is the one redirect syntax shared by Bash, sh and cmd, so the
        // probe does not depend on which dialect this host resolved.
        let probe = "echo probe > workdir-probe.txt";
        let output = execute(probe, &dir).await.unwrap();
        assert_eq!(output.exit_code, Some(0), "{}", output.stderr);
        let written = dir.join("workdir-probe.txt");
        assert!(written.exists(), "shell did not start in {}", dir.display());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_shell_reports_output_and_exit_code() {
        let cwd = std::env::current_dir().unwrap();
        let output = execute("echo kroma-windows-ok", &cwd).await.unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("kroma-windows-ok"));
        let failed = execute("exit 7", &cwd).await.unwrap();
        assert_eq!(failed.exit_code, Some(7));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_bash_reports_a_failing_pipeline() {
        // Only Bash has pipefail; cmd has no equivalent to check here.
        if resolve_shell().dialect != Dialect::Bash {
            return;
        }
        let cwd = std::env::current_dir().unwrap();
        let command = "sh -c 'echo test-failed; exit 7' 2>&1 | tail -10";
        let output = execute(command, &cwd).await.unwrap();
        assert_eq!(output.exit_code, Some(7));
        assert!(output.stdout.contains("test-failed"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_cancel_kills_shell_descendants() {
        if resolve_shell().dialect != Dialect::Bash {
            return;
        }
        let test_dir = std::env::temp_dir().join(format!(
            "kroma-job-object-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&test_dir).unwrap();
        let marker = test_dir.join("orphan.txt");
        let cwd = test_dir.clone();
        let task = tokio::spawn(async move {
            execute("(sleep 1; printf orphan > orphan.txt) & wait", &cwd).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!task.is_finished(), "shell finished before cancellation");
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(1150)).await;
        assert!(!marker.exists(), "a descendant survived shell cancellation");
        std::fs::remove_dir_all(&test_dir).unwrap();
    }
}
