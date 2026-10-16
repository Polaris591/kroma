//! Terminal-UX remediation tests (`v1.5.0`): the five confirmed audit defects.
//!
//! Verifies:
//! 1. `repl_answers_tool_approval_from_the_same_reader` — P1: the default `--policy ask` REPL
//!    renders the approval prompt live and reads the operator's `y` as the approval, not as the
//!    next task.
//! 2. `repl_denies_tool_approval_on_n` — P1 deny path.
//! 3. `approval_answer_maps_to_canonical_decisions` — P1 decision mapping.
//! 4. `cancel_active_run_aborts_provider_wait_and_reuses_session` — P2: `^C` reaches the dispatcher.
//! 5. `repl_cancel_returns_a_reusable_prompt` — P2 end-to-end through the REPL input loop.
//! 6. `gate_status_reports_disabled_and_armed` — P3: `session.status` gate truth + banner.
//! 7. `run_terminated_event_is_labelled_run_not_gate` — P3.
//! 8. `display_width_and_truncation_are_unicode_and_ansi_aware` — P4 helpers.
//! 9. `startup_banner_lines_fit_the_terminal_width` — P4 regression.
//! 10. `untrusted_diagnostic_labels_are_escaped` — P5.

use kroma_core::async_tasks::{
    ArtifactRef, AsyncTaskRegistry, NewTask, TaskFinish, TaskState, VerificationRecord,
};
use kroma_core::budget::BudgetConfig;
use kroma_core::completion_gate::WorkspaceCompletionGate;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall, TranscriptItem};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::subagents::SubagentSpec;
use kroma_core::surface::{
    approval_decision_from_answer, format_approval_prompt, format_task_flags,
    sanitize_diagnostic_label, ApprovalPendingInfo, InteractiveRepl, JsonRpcNotification,
    JsonRpcRequest, KromaRuntimeConfig, RpcDispatcher, WaitingState,
};
use kroma_core::theme::{
    display_width, terminal_width, terminal_width_from, truncate_display, wrap_segments,
};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::ToolCallId;
use serde_json::{json, Value};
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use uuid::Uuid;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-repl-ux-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn assistant_stop(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: Some(text.to_string()),
        tool_calls: Vec::new(),
        reasoning_blocks: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(TokenUsage {
            input_tokens: 120,
            cached_input_tokens: Some(90),
            output_tokens: 40,
            reasoning_tokens: None,
        }),
    }
}

fn assistant_write(path: &str, content: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_write_1"),
            name: "write".to_string(),
            raw_arguments: json!({ "path": path, "content": content }).to_string(),
        }],
        reasoning_blocks: Vec::new(),
        finish_reason: FinishReason::ToolCalls,
        usage: Some(TokenUsage {
            input_tokens: 160,
            cached_input_tokens: Some(120),
            output_tokens: 50,
            reasoning_tokens: None,
        }),
    }
}

struct FnProvider<F> {
    handler: F,
}

impl<F> FnProvider<F>
where
    F: Fn(ProviderRequest) -> Result<AssistantTurn, ProviderError> + Send + Sync + 'static,
{
    fn new(handler: F) -> Arc<Self> {
        Arc::new(Self { handler })
    }
}

impl<F> ModelProvider for FnProvider<F>
where
    F: Fn(ProviderRequest) -> Result<AssistantTurn, ProviderError> + Send + Sync + 'static,
{
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let turn = (self.handler)(req);
        Box::pin(async move {
            let turn = turn?;
            Ok(ProviderCompletion {
                turn,
                request_body_bytes: 128,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

/// Provider whose first call blocks until cancelled; later calls answer `PONG`.
struct SlowThenFastProvider {
    calls: AtomicUsize,
}

impl SlowThenFastProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }
}

impl ModelProvider for SlowThenFastProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if n == 0 {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(15)) => {
                            return Ok(ProviderCompletion {
                                turn: assistant_stop("late provider result"),
                                request_body_bytes: 128,
                                latency_ms: 1,
                                ttft_ms: Some(1),
                                attempts_used: 1,
                reported_model: None,
                            });
                        }
                        changed = cancel_rx.changed() => {
                            if changed.is_err() || *cancel_rx.borrow() {
                                return Err(ProviderError::Cancelled);
                            }
                        }
                    }
                }
            }
            Ok(ProviderCompletion {
                turn: assistant_stop("PONG"),
                request_body_bytes: 128,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

fn repl_with_provider<F>(
    tag: &str,
    provider: Arc<FnProvider<F>>,
) -> (InteractiveRepl, PathBuf, PathBuf)
where
    F: Fn(ProviderRequest) -> Result<AssistantTurn, ProviderError> + Send + Sync + 'static,
{
    let root = temp_root(tag);
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();
    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::Ask)
        .with_surface("repl", "tty")
        .with_approval_timeout(Duration::from_secs(5));
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    (InteractiveRepl::new(dispatcher), ws, root)
}

#[tokio::test]
async fn repl_answers_tool_approval_from_the_same_reader() {
    let provider = FnProvider::new(|_| Ok(assistant_write("approved.txt", "approved content\n")));
    let (repl, ws, root) = repl_with_provider("approval-allow", provider);

    let input = std::io::Cursor::new(b"please write approved.txt\ny\n/quit\n".to_vec());
    let mut out = Vec::new();
    repl.run_io(BufReader::new(input), &mut out)
        .await
        .expect("REPL loop must finish");
    let rendered = String::from_utf8_lossy(&out);

    assert!(
        rendered.contains("KROMA TOOL APPROVAL REQUIRED"),
        "the approval prompt must be rendered live: {rendered}"
    );
    assert!(
        rendered.contains("● approval  approve_once"),
        "the operator's `y` must approve once: {rendered}"
    );
    assert_eq!(
        fs::read_to_string(ws.join("approved.txt")).unwrap(),
        "approved content\n",
        "an approved write must reach the workspace"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn repl_denies_tool_approval_on_n() {
    let provider = FnProvider::new(|_| Ok(assistant_write("denied.txt", "should not exist\n")));
    let (repl, ws, root) = repl_with_provider("approval-deny", provider);

    let input = std::io::Cursor::new(b"please write denied.txt\nn\n/quit\n".to_vec());
    let mut out = Vec::new();
    repl.run_io(BufReader::new(input), &mut out)
        .await
        .expect("REPL loop must finish");
    let rendered = String::from_utf8_lossy(&out);

    assert!(
        rendered.contains("● approval  deny"),
        "the operator's `n` must deny: {rendered}"
    );
    assert!(
        !ws.join("denied.txt").exists(),
        "a denied write must never reach the workspace"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn approval_answer_maps_to_canonical_decisions() {
    assert_eq!(approval_decision_from_answer(" y \n"), "approve_once");
    assert_eq!(approval_decision_from_answer("YES"), "approve_once");
    assert_eq!(approval_decision_from_answer("A"), "approve_session");
    assert_eq!(approval_decision_from_answer("all"), "approve_session");
    assert_eq!(approval_decision_from_answer("n"), "deny");
    assert_eq!(approval_decision_from_answer(""), "deny");
    assert_eq!(approval_decision_from_answer("maybe"), "deny");
}

#[test]
fn approval_prompt_escapes_untrusted_arguments() {
    let info = ApprovalPendingInfo {
        request_id: "apr-0001".to_string(),
        tool_name: "bash".to_string(),
        effect_class: "side-effect".to_string(),
        is_sensitive_path: false,
        target_path: None,
        arguments_summary: "rm -rf /\x1b[2K\rEVIL".to_string(),
        risk_reason: "side-effect".to_string(),
        surface: "repl".to_string(),
    };
    let prompt = format_approval_prompt(&info);
    assert!(!prompt.contains('\x1b'), "ESC must be escaped: {prompt:?}");
    assert!(!prompt.contains('\r'), "CR must be escaped: {prompt:?}");
    assert!(prompt.contains("\\x1b"));
    assert!(prompt.contains("Choose: [y]"));
}

#[tokio::test]
async fn cancel_active_run_aborts_provider_wait_and_reuses_session() {
    let root = temp_root("cancel-dispatcher");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();
    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("repl", "tty");
    let dispatcher = RpcDispatcher::new(cfg, SlowThenFastProvider::new()).unwrap();

    let disp_for_prompt = dispatcher.clone();
    let prompt = tokio::spawn(async move {
        disp_for_prompt
            .handle_request(JsonRpcRequest::new(
                1,
                "session.prompt",
                json!({ "prompt": "slow task" }),
            ))
            .await
    });

    wait_until_in_flight(&dispatcher).await;

    assert!(
        dispatcher.cancel_active_run(),
        "cancel_active_run must report the in-flight run"
    );
    let resp = prompt.await.unwrap();
    assert!(resp.is_ok());
    assert_eq!(
        resp.result.unwrap()["status"],
        "cancelled",
        "the first `^C` must cancel the in-flight provider wait"
    );

    // The session must remain usable after a cancel.
    let next = dispatcher
        .handle_request(JsonRpcRequest::new(
            2,
            "session.prompt",
            json!({ "prompt": "PING" }),
        ))
        .await;
    assert!(next.is_ok());
    assert_eq!(next.result.unwrap()["status"], "completed");

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn repl_cancel_returns_a_reusable_prompt() {
    let root = temp_root("cancel-repl");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();
    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("repl", "tty");
    let dispatcher = RpcDispatcher::new(cfg, SlowThenFastProvider::new()).unwrap();
    let repl = InteractiveRepl::new(dispatcher.clone());

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task =
        tokio::spawn(async move { repl.run_io(BufReader::new(server_read), server_write).await });

    client.write_all(b"slow task\n").await.unwrap();
    client.flush().await.unwrap();
    wait_until_in_flight(&dispatcher).await;

    assert!(dispatcher.cancel_active_run(), "cancel must reach the run");

    client.write_all(b"PING\n").await.unwrap();
    client.write_all(b"/quit\n").await.unwrap();
    client.flush().await.unwrap();

    task.await.unwrap().expect("REPL loop must finish cleanly");
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    let rendered = String::from_utf8_lossy(&out);

    assert!(
        rendered.contains("cancelled"),
        "the cancelled turn must be reported and the prompt reused: {rendered}"
    );
    assert!(
        rendered.contains("PONG"),
        "the next task must run on the same reader: {rendered}"
    );

    let _ = fs::remove_dir_all(&root);
}

async fn wait_until_in_flight(dispatcher: &RpcDispatcher) {
    for _ in 0..400 {
        let resp = dispatcher
            .handle_request(JsonRpcRequest::new(99, "session.status", json!({})))
            .await;
        let in_flight = resp
            .result
            .as_ref()
            .and_then(|v| v.get("in_flight"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if in_flight {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the run never went in flight");
}

#[tokio::test]
async fn gate_status_reports_disabled_and_armed() {
    // Disabled: the default config's `AllowAllGate` must be reported as such.
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("gate-disabled", provider);
    let status = repl
        .dispatcher()
        .handle_request(JsonRpcRequest::new(1, "session.status", json!({})))
        .await
        .result
        .unwrap();
    assert_eq!(status["completion_gate"]["armed"], false);
    assert_eq!(status["completion_gate"]["kind"], "disabled");
    let banner = repl.render_status_banner().await;
    assert!(
        banner.contains("completion verification disabled"),
        "banner must not claim a disabled gate is armed: {banner}"
    );
    assert!(
        !banner.contains("gate  ") || !banner.contains("armed"),
        "banner must not claim 'armed' for a disabled gate: {banner}"
    );
    let _ = fs::remove_dir_all(&root);

    // Armed with an explicit command.
    let root = temp_root("gate-armed");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("repl", "tty")
        .with_completion_gate(Arc::new(WorkspaceCompletionGate::new(
            Some(vec!["cargo".to_string(), "test".to_string()]),
            Duration::from_secs(30),
        )));
    let repl = InteractiveRepl::new(RpcDispatcher::new(cfg, provider).unwrap());
    let status = repl
        .dispatcher()
        .handle_request(JsonRpcRequest::new(1, "session.status", json!({})))
        .await
        .result
        .unwrap();
    assert_eq!(status["completion_gate"]["armed"], true);
    assert_eq!(status["completion_gate"]["kind"], "command");
    assert_eq!(status["completion_gate"]["command"], "cargo test");
    let banner = repl.render_status_banner().await;
    assert!(
        banner.contains("completion verification armed: cargo test"),
        "banner must name the resolved verification command: {banner}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn run_terminated_event_is_labelled_run_not_gate() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("run-label", provider);
    let sid = repl.dispatcher().session_id();
    let note = JsonRpcNotification::new(
        "kroma/event",
        json!({
            "session_id": sid,
            "event_type": "run_terminated",
            "payload": { "status": "completed", "turns": 1, "total_tool_calls": 0 },
        }),
    );
    let line = repl.render_live_event(&note).unwrap();
    assert!(line.contains("● run"), "got: {line}");
    assert!(!line.contains("● gate"), "got: {line}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn display_width_and_truncation_are_unicode_and_ansi_aware() {
    assert_eq!(display_width("abc"), 3);
    assert_eq!(display_width("中文"), 4, "CJK glyphs occupy two cells");
    assert_eq!(
        display_width("\x1b[31mred\x1b[0m"),
        3,
        "escapes have zero width"
    );

    let truncated = truncate_display("中文中文中文", 5);
    assert!(display_width(&truncated) <= 5, "got {truncated:?}");
    assert!(truncated.ends_with('…'));

    let long = "a".repeat(200);
    let clipped = truncate_display(&long, 40);
    assert!(display_width(&clipped) <= 40);
    assert!(clipped.ends_with('…'));

    // No slicing panic on a multi-byte boundary.
    let _ = truncate_display("éééééé", 3);

    assert_eq!(terminal_width_from(Some(40), Some(100)), 40, "tty wins");
    assert_eq!(terminal_width_from(None, Some(30)), 30, "COLUMNS fallback");
    assert_eq!(terminal_width_from(None, None), 80, "portable default");
    assert_eq!(terminal_width_from(Some(0), None), 20, "clamped up");

    let lines = wrap_segments(&["model: x".to_string(), "thinking: y".to_string()], 12);
    assert!(lines.len() >= 2, "got {lines:?}");
    assert!(lines.iter().all(|l| display_width(l) <= 12));
}

#[tokio::test]
async fn startup_banner_lines_fit_the_terminal_width() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("banner-width", provider);

    let width = terminal_width();
    let banner = repl.render_status_banner().await;
    for line in banner.split('\n') {
        assert!(
            display_width(line) <= width,
            "banner line is {} cells wide (limit {width}): {line:?}",
            display_width(line)
        );
    }
    assert!(banner.contains("gate"), "banner: {banner}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn untrusted_diagnostic_labels_are_escaped() {
    let hostile = "bad\rkey\twith\x1b[31mESC\u{202e}override";
    let safe = sanitize_diagnostic_label(hostile);
    for forbidden in ['\x1b', '\r', '\t', '\n', '\u{202e}'] {
        assert!(
            !safe.contains(forbidden),
            "leaked {forbidden:?} in {safe:?}"
        );
    }
    assert!(safe.contains("\\x1b"), "got {safe:?}");
    assert!(safe.contains("\\r"), "got {safe:?}");
    assert!(safe.contains("\\t"), "got {safe:?}");
    assert_eq!(sanitize_diagnostic_label("plain-key"), "plain-key");
}

/// Provider that spawns a scout subagent on turn 0, completes the scout, then completes the parent.
struct SpawnSubagentProvider {
    parent_turns: AtomicUsize,
}

impl SpawnSubagentProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            parent_turns: AtomicUsize::new(0),
        })
    }
}

impl ModelProvider for SpawnSubagentProvider {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let is_child = req.system_prompt.contains("[SUBAGENT ROLE: scout")
            || req.system_prompt.contains("[SUBAGENT ROLE: worker");

        let turn = if is_child {
            assistant_stop("scout finished searching")
        } else {
            let n = self.parent_turns.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                AssistantTurn {
                    content: None,
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new("call_spawn_1"),
                        name: "spawn_subagent".to_string(),
                        raw_arguments: json!({
                            "role": "scout",
                            "task": "find the answers in the project",
                        })
                        .to_string(),
                    }],
                    reasoning_blocks: Vec::new(),
                    finish_reason: FinishReason::ToolCalls,
                    usage: Some(TokenUsage {
                        input_tokens: 100,
                        cached_input_tokens: None,
                        output_tokens: 50,
                        reasoning_tokens: None,
                    }),
                }
            } else {
                assistant_stop("parent received scout report and finished")
            }
        };

        Box::pin(async move {
            Ok(ProviderCompletion {
                turn,
                request_body_bytes: 128,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

#[tokio::test]
async fn repl_duplex_tasks_renders_spawned_task() {
    let root = temp_root("duplex-tasks");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();

    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("repl", "tty");
    let provider = SpawnSubagentProvider::new();
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let repl = InteractiveRepl::new(dispatcher.clone());

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task =
        tokio::spawn(async move { repl.run_io(BufReader::new(server_read), server_write).await });

    // 1. Initial /tasks with no tasks recorded -> prints clear 'no tasks' line
    client.write_all(b"/tasks\n").await.unwrap();
    // 2. Spawn a scout subagent through a prompt turn
    client.write_all(b"spawn a scout\n").await.unwrap();
    // 3. /tasks after spawn -> renders table with the subagent
    client.write_all(b"/tasks\n").await.unwrap();
    client.write_all(b"/quit\n").await.unwrap();
    client.flush().await.unwrap();

    task.await.unwrap().expect("REPL loop must finish cleanly");
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    let rendered = String::from_utf8_lossy(&out);

    assert!(
        rendered.contains("no tasks"),
        "initial /tasks must report no tasks: {rendered}"
    );
    assert!(
        rendered.contains("TASK ID") && rendered.contains("ROLE") && rendered.contains("STATE"),
        "table header missing: {rendered}"
    );
    assert!(
        rendered.contains("sub-0001"),
        "spawned task sub-0001 missing in /tasks: {rendered}"
    );
    assert!(
        rendered.contains("scout"),
        "role scout missing in /tasks: {rendered}"
    );
    assert!(
        rendered.contains("completed"),
        "state completed missing in /tasks: {rendered}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// P2 (`v1.5.2`): the `/tasks` FLAGS column must treat a JSON `pending_merge: false` as "not
/// pending". The previous disjunct (`is_some() && !is_null()`) read a literal `false` as truthy.
#[test]
fn repl_task_flags_do_not_render_pending_merge_for_json_false() {
    let false_flag = format_task_flags(&json!({ "task_id": "sub-0001", "pending_merge": false }));
    assert_eq!(
        false_flag, "-",
        "pending_merge:false must not render the flag, got '{false_flag}'"
    );

    let true_flag = format_task_flags(&json!({ "task_id": "sub-0001", "pending_merge": true }));
    assert_eq!(true_flag, "pending_merge");

    // Missing and explicit `null` are both unset.
    assert_eq!(format_task_flags(&json!({ "task_id": "sub-0001" })), "-");
    assert_eq!(
        format_task_flags(&json!({ "task_id": "sub-0001", "pending_merge": null })),
        "-"
    );

    // A challenge is rendered on its own when no merge is pending.
    assert_eq!(
        format_task_flags(&json!({ "challenge": "why?", "pending_merge": false })),
        "challenge(why?)"
    );
    assert_eq!(
        format_task_flags(&json!({ "challenge": "why?", "pending_merge": true })),
        "challenge(why?) + merge"
    );
}

#[tokio::test]
async fn repl_duplex_cancel_reports_real_state() {
    let root = temp_root("duplex-cancel");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();

    // Seed a suspended task in the registry before starting the dispatcher
    let tasks_dir = state.join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    let reg_path = tasks_dir.join("registry.json");
    {
        let mut reg = AsyncTaskRegistry::open(&reg_path).unwrap();
        let spec = SubagentSpec::worker("seeded work");
        reg.register(NewTask {
            task_id: "sub-9001".to_string(),
            session_id: "sess-seed".to_string(),
            owner_id: "owner".to_string(),
            workspace_root: ws.display().to_string(),
            role: "worker".to_string(),
            created_turn: 1,
            token_allocation: 1000,
            prompt: "work".to_string(),
            timeout_ms: 10000,
            spec_json: serde_json::to_string(&spec).unwrap(),
            parent_policy: serde_json::to_string(&PolicyMode::FullAutonomous).unwrap(),
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            session_path: "".to_string(),
        })
        .unwrap();
        reg.suspend("sub-9001", 1).unwrap();
    }

    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("repl", "tty");
    let provider = FnProvider::new(|_| Ok(assistant_stop("ok")));
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let repl = InteractiveRepl::new(dispatcher.clone());

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task =
        tokio::spawn(async move { repl.run_io(BufReader::new(server_read), server_write).await });

    // 1. Missing ID must reject with usage
    client.write_all(b"/cancel\n").await.unwrap();
    // 2. Cancel the suspended task -> reports real state "cancelled"
    client.write_all(b"/cancel sub-9001\n").await.unwrap();
    // 3. Cancel an already-cancelled task -> reports real state "cancelled"
    client.write_all(b"/cancel sub-9001\n").await.unwrap();
    client.write_all(b"/quit\n").await.unwrap();
    client.flush().await.unwrap();

    task.await.unwrap().expect("REPL loop must finish cleanly");
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    let rendered = String::from_utf8_lossy(&out);

    assert!(
        rendered.contains("usage: /cancel <task_id>"),
        "missing id must print usage: {rendered}"
    );
    assert!(
        rendered.contains("task sub-9001 state: cancelled"),
        "cancel must report real state cancelled: {rendered}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// B12: the local REPL entry can mint a trusted acceptance for a completed, verified task and
/// record an explicit reconcile decision for a crashed one. The principal is host-filled (never in
/// the command text), and refusals surface instead of being hidden.
#[tokio::test]
async fn repl_duplex_accept_and_reconcile_reach_the_trusted_local_entry() {
    let root = temp_root("duplex-accept");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();

    let tasks_dir = state.join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    let reg_path = tasks_dir.join("registry.json");
    {
        let mut reg = AsyncTaskRegistry::open(&reg_path).unwrap();
        let spec = SubagentSpec::worker("verified work");
        let seed = |task_id: &str| NewTask {
            task_id: task_id.to_string(),
            session_id: "sess-seed".to_string(),
            owner_id: "owner".to_string(),
            workspace_root: ws.display().to_string(),
            role: "worker".to_string(),
            created_turn: 1,
            token_allocation: 1000,
            prompt: "work".to_string(),
            timeout_ms: 10000,
            spec_json: serde_json::to_string(&spec).unwrap(),
            parent_policy: serde_json::to_string(&PolicyMode::FullAutonomous).unwrap(),
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            session_path: String::new(),
        };
        // 1. A completed task with a real passed verification: acceptable.
        reg.register(seed("sub-9101")).unwrap();
        reg.begin_execution("sub-9101", 1).unwrap();
        reg.settle_finish(
            "sub-9101",
            TaskFinish {
                state: TaskState::Completed,
                spent: 7,
                artifact: Some(
                    ArtifactRef::new("report", "reports/x", "report").with_sha256("abc123"),
                ),
                summary: "done".to_string(),
                turn: 2,
                keep_pending_merge: false,
                verification: Some(VerificationRecord {
                    gate: "post-merge".to_string(),
                    passed: true,
                    revision: Some("rev-1".to_string()),
                    parent_snapshot_revision: None,
                }),
            },
        )
        .unwrap();
        // 2. A crashed task with an unknown outcome: reconcile only.
        reg.register(seed("sub-9102")).unwrap();
        reg.begin_execution("sub-9102", 1).unwrap();
        assert_eq!(reg.recover_interrupted().unwrap(), 1);
    }

    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("repl", "tty");
    let provider = FnProvider::new(|_| Ok(assistant_stop("ok")));
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let repl = InteractiveRepl::new(dispatcher.clone());

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task =
        tokio::spawn(async move { repl.run_io(BufReader::new(server_read), server_write).await });

    client.write_all(b"/accept\n").await.unwrap();
    client.write_all(b"/accept sub-9101 rev-2\n").await.unwrap();
    client.write_all(b"/accept sub-9101 rev-1\n").await.unwrap();
    client
        .write_all(b"/reconcile sub-9101 unknown nope\n")
        .await
        .unwrap();
    client
        .write_all(b"/reconcile sub-9102 unknown reviewed the crash logs\n")
        .await
        .unwrap();
    client.write_all(b"/quit\n").await.unwrap();
    client.flush().await.unwrap();

    task.await.unwrap().expect("REPL loop must finish cleanly");
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    let rendered = String::from_utf8_lossy(&out);

    assert!(
        rendered.contains("usage: /accept <task_id> [verified_revision]"),
        "missing id must print usage: {rendered}"
    );
    assert!(
        rendered.contains("was verified at revision 'rev-1', not 'rev-2'"),
        "a mismatched revision must be refused: {rendered}"
    );
    assert!(
        rendered.contains("accepted task sub-9101 at revision rev-1 (1 artifact digests)"),
        "the local accept must reach the trusted entry: {rendered}"
    );
    assert!(
        rendered.contains("no unknown outcome"),
        "reconciling a completed task must be refused: {rendered}"
    );
    assert!(
        rendered.contains("reconciled task sub-9102 (acknowledged_unknown)"),
        "the local reconcile must reach the trusted entry: {rendered}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn repl_duplex_resume_surfaces_refusal() {
    let root = temp_root("duplex-resume");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();

    let tasks_dir = state.join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    let reg_path = tasks_dir.join("registry.json");
    {
        let mut reg = AsyncTaskRegistry::open(&reg_path).unwrap();
        let spec = SubagentSpec::worker("zero allocation work");
        reg.register(NewTask {
            task_id: "sub-9002".to_string(),
            session_id: "sess-seed".to_string(),
            owner_id: "owner".to_string(),
            workspace_root: ws.display().to_string(),
            role: "worker".to_string(),
            created_turn: 1,
            token_allocation: 100,
            prompt: "work".to_string(),
            timeout_ms: 10000,
            spec_json: serde_json::to_string(&spec).unwrap(),
            parent_policy: serde_json::to_string(&PolicyMode::FullAutonomous).unwrap(),
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::KnownEmpty,
            session_path: "".to_string(),
        })
        .unwrap();
        reg.settle_spend("sub-9002", 100).unwrap();
        reg.suspend("sub-9002", 1).unwrap();
    }

    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("repl", "tty");
    let provider = FnProvider::new(|_| Ok(assistant_stop("ok")));
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let repl = InteractiveRepl::new(dispatcher.clone());

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task =
        tokio::spawn(async move { repl.run_io(BufReader::new(server_read), server_write).await });

    // Attempt to resume the task with no token allocation remaining
    client.write_all(b"/resume sub-9002\n").await.unwrap();
    client.write_all(b"/quit\n").await.unwrap();
    client.flush().await.unwrap();

    task.await.unwrap().expect("REPL loop must finish cleanly");
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    let rendered = String::from_utf8_lossy(&out);

    assert!(
        rendered.contains("task 'sub-9002' has no token allocation remaining to resume"),
        "resume refusal must be surfaced verbatim: {rendered}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn repl_duplex_completion_line_names_terminal_reason() {
    let root = temp_root("duplex-terminal-reason");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();

    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("repl", "tty");
    let provider = FnProvider::new(|_| Ok(assistant_stop("Task finished successfully.")));
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    let repl = InteractiveRepl::new(dispatcher.clone());

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task =
        tokio::spawn(async move { repl.run_io(BufReader::new(server_read), server_write).await });

    client.write_all(b"run test prompt\n").await.unwrap();
    client.write_all(b"/quit\n").await.unwrap();
    client.flush().await.unwrap();

    task.await.unwrap().expect("REPL loop must finish cleanly");
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    let rendered = String::from_utf8_lossy(&out);

    assert!(
        rendered.contains("Task finished successfully."),
        "response text missing: {rendered}"
    );
    assert!(
        rendered.contains("[status=completed terminal_reason=ModelStop"),
        "completion line must explicitly state terminal_reason=ModelStop: {rendered}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Provider that records the last user message (the submitted prompt) of every request.
struct RecordingProvider {
    prompts: Mutex<Vec<String>>,
    reply: String,
}

impl RecordingProvider {
    fn new(reply: &str) -> Arc<Self> {
        Arc::new(Self {
            prompts: Mutex::new(Vec::new()),
            reply: reply.to_string(),
        })
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl ModelProvider for RecordingProvider {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        let prompt = req
            .messages
            .iter()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::User(user) => Some(user.content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        self.prompts.lock().unwrap().push(prompt);
        let turn = assistant_stop(&self.reply);
        Box::pin(async move {
            Ok(ProviderCompletion {
                turn,
                request_body_bytes: 128,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

fn repl_with_recording(
    tag: &str,
    provider: Arc<RecordingProvider>,
) -> (InteractiveRepl, PathBuf, PathBuf) {
    let root = temp_root(tag);
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();
    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("repl", "tty");
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    (InteractiveRepl::new(dispatcher), ws, root)
}

#[tokio::test]
async fn backslash_continuation_produces_one_prompt() {
    let provider = RecordingProvider::new("ok");
    let (repl, _ws, root) = repl_with_recording("backslash-continuation", provider.clone());

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task =
        tokio::spawn(async move { repl.run_io(BufReader::new(server_read), server_write).await });

    client
        .write_all(b"first line\\\nsecond line\n/quit\n")
        .await
        .unwrap();
    client.flush().await.unwrap();
    task.await.unwrap().expect("REPL loop must finish cleanly");

    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    let rendered = String::from_utf8_lossy(&out);

    assert!(
        rendered.contains("... "),
        "a continuation prompt must be shown: {rendered}"
    );
    assert_eq!(
        provider.prompts(),
        vec!["first line\nsecond line".to_string()],
        "a backslash continuation must produce exactly one two-line prompt"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn heredoc_block_accumulates_and_eof_submits_partial() {
    let provider = RecordingProvider::new("ok");
    let (repl, _ws, root) = repl_with_recording("heredoc-eof", provider.clone());

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task =
        tokio::spawn(async move { repl.run_io(BufReader::new(server_read), server_write).await });

    // Open a `"""` block, provide two lines, then EOF without the closing terminator.
    client.write_all(b"\"\"\"\nfirst\nsecond\n").await.unwrap();
    client.flush().await.unwrap();
    client.shutdown().await.unwrap();

    task.await.unwrap().expect("REPL loop must finish cleanly");
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    let rendered = String::from_utf8_lossy(&out);

    assert!(
        rendered.contains("... "),
        "the block must show a continuation prompt: {rendered}"
    );
    assert_eq!(
        provider.prompts(),
        vec!["first\nsecond".to_string()],
        "EOF while accumulating must submit the collected block, not hang or drop it"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn bracketed_paste_burst_is_one_prompt_and_inner_quit_is_not_executed() {
    let provider = RecordingProvider::new("ok");
    let (repl, _ws, root) = repl_with_recording("bracketed-paste", provider.clone());

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task =
        tokio::spawn(async move { repl.run_io(BufReader::new(server_read), server_write).await });

    // One pasted burst whose second line is `/quit`; the markers wrap the whole burst.
    client
        .write_all(b"\x1b[200~/quit\nsecond line\x1b[201~\n")
        .await
        .unwrap();
    // A command-looking literal prompt must not become recallable history: `!N`/`!!` would
    // otherwise expand it to text the recall path treats as a command, silently defeating the
    // paste protection. Nothing else has been submitted, so history must still be empty.
    client.write_all(b"/history\n").await.unwrap();
    // The real `/quit` must still work afterwards.
    client.write_all(b"/quit\n").await.unwrap();
    client.flush().await.unwrap();

    task.await.unwrap().expect("REPL loop must finish cleanly");
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    let rendered = String::from_utf8_lossy(&out);

    assert_eq!(
        provider.prompts(),
        vec!["/quit\nsecond line".to_string()],
        "a bracketed paste must be submitted as one literal prompt"
    );
    assert!(
        !rendered.contains("\x1b[200~") && !rendered.contains("\x1b[201~"),
        "paste markers must never be forwarded: {rendered:?}"
    );
    assert_eq!(
        rendered.matches("Session closed.").count(),
        1,
        "the inner `/quit` must not have shut the session down early: {rendered}"
    );
    assert!(
        rendered.contains("no prompts recorded yet"),
        "a command-looking pasted prompt must not be recorded as recallable history: {rendered}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn history_lists_and_recalls_entries() {
    let provider = RecordingProvider::new("ok");
    let (repl, _ws, root) = repl_with_recording("history", provider.clone());

    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task =
        tokio::spawn(async move { repl.run_io(BufReader::new(server_read), server_write).await });

    client.write_all(b"alpha task\nbeta task\n").await.unwrap();
    client.write_all(b"/history\n").await.unwrap();
    client.write_all(b"!1\n").await.unwrap();
    client.write_all(b"!!\n").await.unwrap();
    client.write_all(b"!99\n").await.unwrap();
    client.write_all(b"/quit\n").await.unwrap();
    client.flush().await.unwrap();

    task.await.unwrap().expect("REPL loop must finish cleanly");
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    let rendered = String::from_utf8_lossy(&out);

    assert!(
        rendered.contains("history ("),
        "`/history` must list entries: {rendered}"
    );
    assert!(rendered.contains("alpha task"), "{rendered}");
    assert!(rendered.contains("beta task"), "{rendered}");
    assert!(
        rendered.contains("out of range"),
        "an out-of-range index must error clearly: {rendered}"
    );
    assert_eq!(
        provider.prompts(),
        vec![
            "alpha task".to_string(),
            "beta task".to_string(),
            "beta task".to_string(),
            "beta task".to_string(),
        ],
        "`!1` and `!!` must re-run the most recent prompt; `!99` must execute nothing"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn subagent_live_lines_name_child_id_and_state() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("subagent-live", provider);
    let note = |event_type: &str, payload: Value| {
        JsonRpcNotification::new(
            "kroma/event",
            json!({ "event_type": event_type, "payload": payload }),
        )
    };

    let spawned = repl
        .render_live_event(&note(
            "subagent_spawned",
            json!({
                "subagent_id": "sub-0001",
                "role": "scout",
                "max_turns": 8,
                "isolated_worktree": false
            }),
        ))
        .unwrap();
    assert!(spawned.contains("sub-0001"), "{spawned}");
    assert!(spawned.contains("scout"), "{spawned}");
    assert!(spawned.contains("running"), "{spawned}");
    assert_eq!(spawned.matches('\n').count(), 1, "one bounded line: {spawned}");

    let completed = repl
        .render_live_event(&note(
            "subagent_completed",
            json!({
                "subagent_id": "sub-0001",
                "role": "scout",
                "status": "completed",
                "turns_used": 3,
                "tool_calls_used": 2,
                "tokens_used": 1200,
                "duration_ms": 42
            }),
        ))
        .unwrap();
    assert!(completed.contains("sub-0001"), "{completed}");
    assert!(completed.contains("completed"), "{completed}");
    assert!(completed.contains("1200"), "{completed}");
    assert_eq!(completed.matches('\n').count(), 1, "one bounded line: {completed}");

    let merged = repl
        .render_live_event(&note(
            "worktree_merged",
            json!({
                "subagent_id": "sub-0001",
                "status": "applied",
                "written_files": ["a.rs"],
                "removed_files": [],
                "conflict_paths": [],
                "revision_after": "abc"
            }),
        ))
        .unwrap();
    assert!(merged.contains("sub-0001"), "{merged}");
    assert!(merged.contains("merged="), "{merged}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn waiting_states_render_distinctly() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("waiting-states", provider);

    let provider_line = repl.render_waiting_state(&WaitingState::Provider);
    let tool_line = repl.render_waiting_state(&WaitingState::Tool("read".to_string()));
    let approval_line = repl.render_waiting_state(&WaitingState::Approval("bash".to_string()));

    assert!(provider_line.contains("provider"), "{provider_line}");
    assert!(tool_line.contains("read"), "{tool_line}");
    assert!(approval_line.contains("bash"), "{approval_line}");
    assert_ne!(provider_line, tool_line);
    assert_ne!(tool_line, approval_line);
    assert_ne!(provider_line, approval_line);

    for line in [&provider_line, &tool_line, &approval_line] {
        assert!(!line.contains('\n'), "must be a single line: {line:?}");
        assert!(
            display_width(line) <= terminal_width(),
            "must fit the terminal width: {line:?}"
        );
    }

    let _ = fs::remove_dir_all(&root);
}
