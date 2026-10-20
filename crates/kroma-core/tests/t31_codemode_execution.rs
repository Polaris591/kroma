//! Codemode execution (R03, R12, R15).
//!
//! The tool had three defects, each pinned here through the real `ToolRuntime` dispatch path:
//!
//! 1. **R03 — the model/server call id was used as a host path.** `tool_call_id = "../../outside"`
//!    made the runtime write `state/codemode/../../outside.py` *on the host* before any sandbox
//!    existed. The program is now fed on stdin, so no id ever becomes a path and a refused or
//!    malformed call has no host side effect at all.
//! 2. **R12 — the program did not run under a real isolation backend.** The staged script lived in
//!    the trusted state directory, which is not mounted in the guest, and the guest root was the
//!    host path. The program now reads its source from stdin and its root from the guest view of the
//!    workspace.
//! 3. **R15 — the declared `timeout_sec` was dropped.** A one-second declaration ran a two-second
//!    program to completion. The timeout is parsed strictly and handed to the same executor `bash`
//!    uses.

use kroma_core::budget::BudgetConfig;
use kroma_core::env::LocalHostEnv;
use kroma_core::messages::ToolOutcome;
use kroma_core::sandbox::{ExecutionPolicy, SandboxSpec, SandboxTemplate};
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::tool::runtime::ToolRuntime;
use kroma_core::types::{ErrorCode, ToolCallId};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t31-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn runtime_for(
    ws: &std::path::Path,
    state: &std::path::Path,
    execution: ExecutionPolicy,
) -> ToolRuntime {
    let env = Arc::new(LocalHostEnv::new(ws, state.to_path_buf()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    ToolRuntime::new(
        ToolRegistry::default(),
        policy,
        env,
        BudgetConfig::default(),
    )
    .with_execution_policy(execution)
}

async fn run_codemode(
    runtime: &ToolRuntime,
    call_id: &str,
    args: serde_json::Value,
) -> (ToolOutcome, String, kroma_core::messages::ToolResultMeta) {
    let call = kroma_core::messages::ToolCall {
        call_id: ToolCallId::new(call_id),
        name: "codemode".to_string(),
        raw_arguments: args.to_string(),
    };
    let (_tx, rx) = watch::channel(false);
    let (message, _decision, _hooks) = runtime.execute_call(&call, 1, rx).await;
    (
        message.outcome,
        message.content.to_string(),
        message.meta.clone(),
    )
}

/// R03: an external id — relative traversal *or* an absolute path — must never become a host path.
/// The program itself is harmless; the assertion is that nothing was written outside the workspace.
#[tokio::test]
async fn a_model_supplied_call_id_cannot_write_outside_the_state_directory() {
    let root = scratch("call-id");
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let runtime = runtime_for(&ws, &state, ExecutionPolicy::host());

    for call_id in ["../../outside", "absolute-outside"] {
        let (outcome, content, _meta) = run_codemode(
            &runtime,
            call_id,
            serde_json::json!({ "code": "print('harmless')" }),
        )
        .await;
        assert_eq!(
            outcome,
            ToolOutcome::Success,
            "the program itself is harmless and must run: {content}"
        );
    }

    // The old implementation wrote `state/codemode/../../outside.py` (and, for an absolute id,
    // replaced the join's prefix entirely). No `.py` file may be written for a codemode call.
    let outside_state = state.join("outside.py");
    assert!(
        !outside_state.exists(),
        "the call id escaped the state directory: {} exists",
        outside_state.display()
    );
    let mut stray = Vec::new();
    collect_python_files(&root, &mut stray);
    assert!(
        stray.is_empty(),
        "a codemode call staged host script file(s) outside the workspace: {stray:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

fn collect_python_files(dir: &std::path::Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_python_files(&path, found);
        } else if path.extension().and_then(|e| e.to_str()) == Some("py") {
            found.push(path);
        }
    }
}

/// R12: the same program must work under the real isolation backend. Under `bwrap` the guest sees
/// the workspace at its mount point and nothing else, so a host script path or a host root makes the
/// program fail (`python3: can't open file .../state/codemode/...`).
#[tokio::test]
async fn codemode_runs_inside_the_isolation_backend_with_the_guest_root() {
    if SandboxSpec::bwrap("/tmp").probe().is_err() {
        panic!("bwrap is required to prove codemode's isolated execution and is not usable on this host");
    }
    let root = scratch("isolated");
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("data.txt"), "guest-visible-payload\n").unwrap();
    // A trusted-state file the guest must not be able to read: if the fix had mapped the state
    // directory in to make a script path work, this would be reachable from the program.
    std::fs::write(state.join("trusted-secret.txt"), "STATE_SECRET\n").unwrap();

    let runtime = runtime_for(
        &ws,
        &state,
        ExecutionPolicy::isolated(SandboxTemplate::bwrap()),
    );
    let (outcome, content, meta) = run_codemode(
        &runtime,
        "call_isolated",
        serde_json::json!({
            "code": "print(read('data.txt').strip())\nprint(ls('.'))"
        }),
    )
    .await;

    assert_eq!(
        outcome,
        ToolOutcome::Success,
        "a normal codemode program must run under bwrap: {content}"
    );
    assert!(
        content.contains("guest-visible-payload"),
        "the guest root must point at the mapped workspace: {content}"
    );
    assert_eq!(
        meta.execution_backend.as_deref(),
        Some("bwrap"),
        "the result must report the backend that actually ran the program"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// R15: `timeout_sec` is a real per-call limit. The declared one second must end a two-second
/// program, with the same timeout error `bash` produces.
#[tokio::test]
async fn codemode_enforces_its_declared_timeout() {
    let root = scratch("timeout");
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let runtime = runtime_for(&ws, &state, ExecutionPolicy::host());

    let started = Instant::now();
    let (outcome, content, meta) = run_codemode(
        &runtime,
        "call_timeout",
        serde_json::json!({
            "code": "import time\ntime.sleep(2)\nprint('finished')",
            "timeout_sec": 1
        }),
    )
    .await;
    let elapsed = started.elapsed();

    assert_eq!(
        outcome,
        ToolOutcome::Error {
            code: ErrorCode::ToolTimeout
        },
        "a declared one-second limit must time out, got {outcome:?}: {content}"
    );
    assert!(
        !content.contains("finished"),
        "the program must not reach its end: {content}"
    );
    assert!(
        elapsed < Duration::from_millis(1_800),
        "the call took {elapsed:?}; the declared timeout was not applied"
    );
    assert_eq!(meta.terminated_by.as_deref(), Some("Timeout"));

    let _ = std::fs::remove_dir_all(&root);
}

/// R15: the parser and the model-visible schema are one contract. An unknown field or a
/// non-positive timeout is refused, not silently ignored — and nothing runs.
#[tokio::test]
async fn codemode_refuses_arguments_outside_the_schema() {
    let root = scratch("strict");
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let runtime = runtime_for(&ws, &state, ExecutionPolicy::host());

    for args in [
        serde_json::json!({ "code": "print('ran')", "unknown_field": true }),
        serde_json::json!({ "code": "print('ran')", "timeout_sec": 0 }),
        serde_json::json!({ "code": "" }),
    ] {
        let (outcome, content, _meta) = run_codemode(&runtime, "call_strict", args.clone()).await;
        assert_eq!(
            outcome,
            ToolOutcome::Error {
                code: ErrorCode::InvalidArguments
            },
            "{args} must be refused, got {outcome:?}: {content}"
        );
        assert!(
            !content.contains("ran"),
            "a refused call must not execute the program: {content}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
