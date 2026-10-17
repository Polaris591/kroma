//! B06 long-lived MCP contract: a *started* `tools/call` that is cancelled returns promptly and
//! does **not**, by itself, terminate or reap the long-lived server process — the server is reaped
//! only when the host is torn down. A wedged server whose discovery times out is reaped too. Neither
//! case may leave a residual server process behind.
//!
//! These are *result* invariants, not call-site assertions: the tests read the server's own pid from
//! `/proc` (start-time identity, zombie state) and `kill(pid, 0)` and require a genuine `ESRCH`, so a
//! temporary mutation that removes the final cleanup mechanism turns them red. The cancel case records
//! the two facts separately: the server process is still running, AND the poisoned transport refuses
//! the next call — the aliveness must never be read as "the call had no side effects".
//!
//! Platform boundary: the fixtures are `python3` servers in their own process group, so this file is
//! Unix-only. Linux gets the strongest evidence (`/proc/<pid>/stat` state + start-time identity);
//! where `/proc` is unavailable the tests fall back to the `kill(0)` liveness/`ESRCH` probe. Windows
//! is unsupported here (no `/proc`, no `kill(0)`); its Job-object containment/runtime stays B15.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kroma_core::config_file::McpServerConfig;
use kroma_core::env::ExecutionContext;
use kroma_core::extensions::{CustomToolContext, ExtensionId};
use kroma_core::mcp::{McpError, McpToolHost};
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::types::RunId;
use kroma_core::workspace::WorkspaceCap;
use tokio::sync::watch;

/// A server that reports its pid, answers the handshake, then holds a `slow` call open and writes an
/// "entered" marker so the test can prove the call reached execution (not merely the queue).
const ENTERED_SERVER: &str = r#"
import sys, json, os, time

with open(os.environ["B06_MCP_PID_FILE"], "w") as f:
    f.write(str(os.getpid()))

def respond(mid, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        respond(msg["id"], {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                            "serverInfo": {"name": "b06", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(msg["id"], {"tools": [
            {"name": "slow", "description": "holds the call open",
             "inputSchema": {"type": "object", "properties": {}}},
        ]})
    elif method == "tools/call":
        with open(os.environ["B06_MCP_ENTERED"], "w") as f:
            f.write(str(os.getpid()))
        time.sleep(300)
    elif "id" in msg:
        respond(msg["id"], {})
"#;

/// A wedged server: it reports its pid, then never answers `initialize`, so discovery times out.
const WEDGED_SERVER: &str = r#"
import sys, os, time

with open(os.environ["B06_MCP_PID_FILE"], "w") as f:
    f.write(str(os.getpid()))

for line in sys.stdin:
    time.sleep(300)
"#;

/// A unique private (0700) scratch directory under `TMPDIR`, so two concurrent fixtures can never
/// share a pid file.
fn unique_root(tag: &str) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let dir = std::env::temp_dir().join(format!("kroma-b06-ll-{}-{}", tag, uuid::Uuid::new_v4()));
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(&dir).expect("create private temp dir");
    dir
}

/// Bounded wait for a marker/pid file. The file appearing is the signal; the bound only keeps a
/// broken fixture from hanging the suite.
fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("file never appeared: {}", path.display());
}

fn wait_for_pid(path: &Path) -> i32 {
    wait_for_file(path);
    std::fs::read_to_string(path)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid file must hold a pid")
}

/// `/proc/<pid>/stat` as (state, starttime). `None` when the entry is gone or `/proc` is unavailable.
#[cfg(target_os = "linux")]
fn proc_stat(pid: i32) -> Option<(char, u64)> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field can contain spaces and parentheses; everything after the last ')' is stable.
    let rest = raw.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    // The first token here is field 4 (ppid); starttime is field 22, the 19th token.
    let starttime: u64 = fields.nth(18)?.parse().ok()?;
    Some((state, starttime))
}

#[cfg(not(target_os = "linux"))]
fn proc_stat(_pid: i32) -> Option<(char, u64)> {
    None
}

/// The server's process start time, captured once so a recycled pid cannot masquerade as the server.
fn sample_starttime(pid: i32) -> Option<u64> {
    proc_stat(pid).map(|(_, starttime)| starttime)
}

/// `kill(pid, 0)` errno: `None` while the pid is live, otherwise the errno.
fn kill0_errno(pid: i32) -> Option<i32> {
    if unsafe { libc::kill(pid, 0) } == 0 {
        None
    } else {
        std::io::Error::last_os_error().raw_os_error()
    }
}

/// True only for a genuinely *running* (not zombie, same identity) server. A zombie is not running,
/// and neither is a recycled pid.
fn running_confirmed(pid: i32, starttime: Option<u64>) -> bool {
    if kill0_errno(pid).is_some() {
        return false;
    }
    match proc_stat(pid) {
        Some((state, observed)) => {
            state != 'Z' && starttime.is_none_or(|expect| expect == observed)
        }
        // `/proc` unavailable: `kill(0) == 0` is the strongest portable evidence.
        None => true,
    }
}

/// True once the pid is genuinely gone: no `/proc` entry (or a recycled start time) AND an `ESRCH`
/// probe, so an unreaped zombie or a foreign `EPERM` process is never counted as reaped.
fn gone_confirmed(pid: i32, starttime: Option<u64>) -> bool {
    if let Some((_state, observed)) = proc_stat(pid) {
        return starttime.is_some_and(|expect| expect != observed);
    }
    matches!(kill0_errno(pid), Some(libc::ESRCH))
}

fn wait_until_gone(pid: i32, starttime: Option<u64>, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    loop {
        if gone_confirmed(pid, starttime) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// True only if the server is *still* running at the end of `window`. A `SIGKILL` that a buggy
/// cleanup would send on cancel can take a few scheduler ticks to take effect, so a single immediate
/// read can miss it; watching the whole window keeps the "cancel does not exit the server" check
/// sensitive without asserting anything about *when* a leaked kill would land.
fn still_running_after(pid: i32, starttime: Option<u64>, window: Duration) -> bool {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        if !running_confirmed(pid, starttime) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    running_confirmed(pid, starttime)
}

/// Best-effort cleanup so a failed assertion never leaves the fixture server behind. Disarmed once
/// the test has itself confirmed the server was reaped. The pid is the server group leader (the MCP
/// launch gives it its own group), so the whole group is swept.
struct PidGuard {
    pid: Option<i32>,
}

impl PidGuard {
    fn new(pid: i32) -> Self {
        Self { pid: Some(pid) }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

fn host_with_server(
    root: &Path,
    script: &str,
    timeout: Duration,
    env: Vec<(String, String)>,
) -> (ToolRegistry, String) {
    let server = root.join("server.py");
    std::fs::write(&server, script).expect("write fixture server");
    let config = McpServerConfig {
        name: "srv".to_string(),
        command: "python3".to_string(),
        args: vec![server.display().to_string()],
        env,
        read_only: false,
    };
    let host = McpToolHost::start(&[config], timeout).expect("start MCP host");
    let extension = host.into_extension("mcp");
    let registry = ToolRegistry::new(None);
    let id = registry
        .register_extension(extension)
        .expect("register MCP extension");
    (registry, id.as_str().to_string())
}

/// A started cancel returns promptly, and the long-lived server process is still running afterwards
/// (the second call is refused by the poisoned transport, not by the server's death) until host
/// teardown reaps it. This pins the current semantics: cancel abandons the call, it does not exit the
/// process; only unmount/shutdown does.
#[test]
fn started_cancel_keeps_the_server_running_until_host_teardown() {
    let root = unique_root("cancel");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("create workspace");
    let pid_file = root.join("server.pid");
    let entered = root.join("call_entered.marker");

    let (registry, id) = host_with_server(
        &root,
        ENTERED_SERVER,
        Duration::from_secs(10),
        vec![
            (
                "B06_MCP_PID_FILE".to_string(),
                pid_file.display().to_string(),
            ),
            ("B06_MCP_ENTERED".to_string(), entered.display().to_string()),
        ],
    );
    let pid = wait_for_pid(&pid_file);
    let starttime = sample_starttime(pid);
    let mut guard = PidGuard::new(pid);

    let dispatch = registry.extension_dispatch("mcp__srv__slow").unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let exec_ctx = ExecutionContext::new(Some(RunId::new()), None, Some(cancel_rx), None, None);

    let started = Instant::now();
    let out = std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            let ctx = CustomToolContext::with_exec_context(&workspace, Some(&exec_ctx));
            dispatch.handler.execute("{}", &ctx)
        });
        // Confirm the call reached execution on the server before cancelling: a queued-only cancel
        // is a different contract (`Cancelled`, not `StartedCancelled`).
        wait_for_file(&entered);
        cancel_tx.send(true).unwrap();
        handle.join().unwrap()
    });

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a started cancel must return promptly, took {:?}",
        started.elapsed()
    );
    assert!(
        !out.outcome.is_success(),
        "cancelled call must error: {out:?}"
    );
    assert!(
        out.content.contains("side-effects") || out.content.contains("unknown"),
        "a started cancel must report unknown execution state, got: {}",
        out.content
    );
    assert!(
        !out.content.contains("before it ran"),
        "the call had entered execution, so it must not be reported as never executed: {}",
        out.content
    );

    // Fact 1 (separately recorded): the process is still running — cancel does not exit/reap it.
    assert!(
        still_running_after(pid, starttime, Duration::from_millis(300)),
        "a started cancel must leave the long-lived server running (pid {pid})"
    );

    // Fact 2: the transport is poisoned, so the next call is refused even though the server lives.
    let next = dispatch
        .handler
        .execute("{}", &CustomToolContext::new(&workspace));
    assert!(
        !next.outcome.is_success(),
        "the poisoned client must refuse the next call: {next:?}"
    );
    assert!(
        next.content.contains("poisoned") || next.content.contains("IoError"),
        "the refusal must be the poisoned transport, got: {}",
        next.content
    );

    // Only host teardown reaps the server, and it must do so within a bounded window.
    let shutdown_started = Instant::now();
    registry
        .unregister_extension(&ExtensionId::new(id))
        .expect("unmount must not error");
    assert!(
        shutdown_started.elapsed() < Duration::from_secs(5),
        "host teardown must be bounded, took {:?}",
        shutdown_started.elapsed()
    );
    assert!(
        wait_until_gone(pid, starttime, Duration::from_secs(5)),
        "host teardown must reap the server (pid {pid})"
    );
    guard.disarm();
    let _ = std::fs::remove_dir_all(&root);
}

/// A wedged server whose discovery times out must be reaped, not merely abandoned: the bounded poll
/// requires a genuine `ESRCH`, so a leaked process (or an unreaped zombie) fails the test.
#[test]
fn discovery_timeout_reaps_the_wedged_server() {
    let root = unique_root("discovery");
    let pid_file = root.join("server.pid");
    let server = root.join("server.py");
    std::fs::write(&server, WEDGED_SERVER).expect("write wedged server");
    let config = McpServerConfig {
        name: "srv".to_string(),
        command: "python3".to_string(),
        args: vec![server.display().to_string()],
        env: vec![(
            "B06_MCP_PID_FILE".to_string(),
            pid_file.display().to_string(),
        )],
        read_only: false,
    };

    let started = Instant::now();
    let err = McpToolHost::start(&[config], Duration::from_millis(300))
        .err()
        .expect("a wedged server must fail discovery");
    assert!(
        matches!(err, McpError::Timeout(_)),
        "expected a bounded discovery timeout, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "discovery must be bounded, took {:?}",
        started.elapsed()
    );

    let pid = wait_for_pid(&pid_file);
    let starttime = sample_starttime(pid);
    let mut guard = PidGuard::new(pid);
    assert!(
        wait_until_gone(pid, starttime, Duration::from_secs(5)),
        "discovery timeout must reap the wedged server (pid {pid})"
    );
    guard.disarm();
    let _ = std::fs::remove_dir_all(&root);
}
