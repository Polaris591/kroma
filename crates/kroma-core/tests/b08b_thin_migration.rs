//! B08b contract: the remaining host-side spawns (`bash`, the completion gate, and the synchronous
//! command hook) launch through the shared seam (`ProcessLaunch`), keeping their original
//! environment policy, stdio, and exit-code/output semantics; and a directly dropped long-lived MCP
//! client sweeps its server's descendants instead of leaking them.

#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use kroma_core::budget::BudgetConfig;
use kroma_core::completion_gate::WorkspaceCompletionGate;
use kroma_core::env::{CompletionGate, CompletionGateContext, GateOutcome};
use kroma_core::extensions::{CommandHookRunner, HookContext, HookEvent};
#[cfg(unix)]
use kroma_core::mcp::McpClient;
use kroma_core::tools::bash::execute_bash;
use kroma_core::types::{RunId, ToolCallId};
use tokio::sync::watch;

const CANARY: &str = "KROMA_B08B_CANARY";

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-b08b-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Bounded wait for a pid file a fixture wrote. The file appearing is the signal; the bound only
/// stops a broken fixture from hanging the suite.
#[cfg(unix)]
fn wait_for_pid(path: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<i32>() {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("pid file never appeared: {}", path.display());
}

/// True while `pid` still exists (including as an unreaped zombie), matching the existing test
/// helper in `mcp_config.rs`.
#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(unix)]
fn wait_until_dead(pid: i32, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// `bash` keeps its exact original environment (credential scrub only, no `CI` / `NO_COLOR`
/// baseline) and its exit code / output contract across the shared launch seam.
#[tokio::test]
async fn bash_keeps_its_exit_code_output_and_sanitized_env() {
    std::env::set_var(CANARY, "leak_canary");
    std::env::set_var("B08B_PUBLIC_VALUE", "kept");
    let ws = scratch_dir("bash");
    let state = scratch_dir("bash-state");
    let args = serde_json::json!({
        "command": "printf 'BASH_OUT|%s|%s|CI=%s' \"$KROMA_B08B_CANARY\" \"$B08B_PUBLIC_VALUE\" \"${CI:-unset}\"; exit 3"
    })
    .to_string();

    let (_tx, rx) = watch::channel(false);
    let out = execute_bash(
        &ws,
        &state,
        &ToolCallId::new("b08b_bash"),
        &args,
        &BudgetConfig::default(),
        rx,
        Duration::from_millis(200),
        &kroma_core::sandbox::ExecutionPolicy::host(),
    )
    .await;

    assert_eq!(
        out.meta.exit_code,
        Some(3),
        "exit code must survive the shared launch: {out:?}"
    );
    assert!(
        out.content.contains("BASH_OUT||kept|CI=unset"),
        "bash must keep the credential scrub, keep non-secret variables, and gain no CI baseline: {:?}",
        out.content
    );

    let _ = std::fs::remove_dir_all(&ws);
    let _ = std::fs::remove_dir_all(&state);
}

/// The completion gate keeps its explicit `CI` / `NO_COLOR` baseline and its credential scrub.
#[tokio::test]
async fn completion_gate_keeps_ci_baseline_and_scrubbed_env() {
    std::env::set_var(CANARY, "leak_canary");
    let ws = scratch_dir("gate");
    let env_out = ws.join("gate-env.txt");
    let gate = WorkspaceCompletionGate::new(
        Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "printf '%s|%s|%s' \"$CI\" \"$NO_COLOR\" \"$KROMA_B08B_CANARY\" > {}",
                env_out.display()
            ),
        ]),
        Duration::from_secs(10),
    );
    let ctx = CompletionGateContext {
        run_id: RunId::new(),
        workspace_root: ws.clone(),
        modified_files: vec!["src/lib.rs".to_string()],
        turns_used: 1,
        deadline: None,
    };

    let outcome = gate.check(&ctx).await.expect("gate must run");
    assert_eq!(outcome, GateOutcome::Approved, "gate outcome: {outcome:?}");
    assert_eq!(
        std::fs::read_to_string(&env_out).expect("gate wrote its environment"),
        "true|1|",
        "gate must keep CI=true / NO_COLOR=1 and scrub KROMA_*"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// The synchronous command hook keeps `KROMA_HOOK_EVENT`, gains no `CI` baseline, scrubs
/// credentials, and reports the exit code exactly as before.
#[test]
fn command_hook_keeps_event_env_and_exit_semantics() {
    std::env::set_var(CANARY, "leak_canary");
    let ws = scratch_dir("hook");
    let env_out = ws.join("hook-env.txt");
    let runner = CommandHookRunner::new(vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "printf '%s|%s|%s' \"$KROMA_HOOK_EVENT\" \"${{CI:-unset}}\" \"$KROMA_B08B_CANARY\" > {}",
            env_out.display()
        ),
    ]);
    let ctx = HookContext::new(1, &ws);

    let outcome = runner
        .run(HookEvent::PreToolUse, &ctx)
        .expect("hook must run");
    assert!(outcome.success, "hook outcome: {outcome:?}");
    assert_eq!(outcome.exit_code, Some(0));
    assert!(!outcome.timed_out);
    assert_eq!(
        std::fs::read_to_string(&env_out).expect("hook wrote its environment"),
        "pre_tool_use|unset|",
        "hook must keep KROMA_HOOK_EVENT, gain no CI baseline, and scrub KROMA_*"
    );

    // A non-zero hook still reports its exit code as a failure (allow/deny semantics unchanged).
    let failing = CommandHookRunner::new(vec![
        "sh".to_string(),
        "-c".to_string(),
        "exit 7".to_string(),
    ]);
    let failing_outcome = failing
        .run(HookEvent::PreToolUse, &ctx)
        .expect("hook must run");
    assert!(!failing_outcome.success);
    assert_eq!(failing_outcome.exit_code, Some(7));

    let _ = std::fs::remove_dir_all(&ws);
}

/// A client dropped without `shutdown` must sweep the server's whole process group: `kill_on_drop`
/// alone only stops the leader, leaving descendants behind. Unix-only (`sh`/`sleep`/`kill(0)`);
/// Windows containment stays B15.
#[cfg(unix)]
#[tokio::test]
async fn dropped_mcp_client_sweeps_server_descendants() {
    let dir = scratch_dir("mcp-drop");
    let pid_file = dir.join("grandchild.pid");
    let script = format!("sleep 60 & echo $! > {}; wait", pid_file.display());

    let client = McpClient::spawn(
        "sh",
        &["-c".to_string(), script],
        &[],
        Duration::from_secs(5),
    )
    .expect("mcp spawn");
    let grandchild = wait_for_pid(&pid_file);
    assert!(
        process_alive(grandchild),
        "the server descendant (pid {grandchild}) must be running before the drop"
    );

    drop(client);

    assert!(
        wait_until_dead(grandchild, Duration::from_secs(5)),
        "dropping the client must sweep the server's descendants (pid {grandchild})"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
