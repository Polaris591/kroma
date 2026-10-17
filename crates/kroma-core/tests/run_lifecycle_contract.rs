//! F01 / F02 / F04 / F09 (`v1.5.1`) regression contract for the run lifecycle:
//!
//! * F01 — a dispatched `bash` (or side-effect extension) that *failed* still gets a mutation
//!   snapshot; a pre-dispatch refusal must not fabricate one.
//! * F02 — every completion-gate outcome (Approved / Skipped / Rejected / Err) captures whatever
//!   the gate wrote before the run acts on it.
//! * F04 — the run's absolute deadline bounds the provider request, `bash` and verification, and
//!   a terminated `bash` cleans up its process group instead of leaking it.
//! * F09 — a provider's internal retry loop only receives the *remaining* request attempts.
//!
//! These are the fixed versions of the review reproductions (which asserted the buggy behaviour).

use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use kroma_core::budget::BudgetConfig;
use kroma_core::completion_gate::WorkspaceCompletionGate;
use kroma_core::env::{CompletionGate, CompletionGateContext, GateOutcome, LocalHostEnv};
use kroma_core::events::EventLogger;
use kroma_core::messages::{AssistantTurn, FinishReason, ToolCall};
use kroma_core::provider::chat_completions::{ChatCompletionsClient, ChatCompletionsConfig};
use kroma_core::provider::{
    FakeProvider, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};
use kroma_core::revision::{RevisionId, SnapshotStore};
use kroma_core::session::SessionJournal;
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::tool::runtime::decision_dispatched;
use kroma_core::types::{RunStatus, TerminalReason, ToolCallId};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

fn stop() -> AssistantTurn {
    AssistantTurn {
        content: Some("done".into()),
        reasoning_blocks: vec![],
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: None,
    }
}

fn tool(name: &str, args: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        reasoning_blocks: vec![],
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("one"),
            name: name.into(),
            raw_arguments: args.into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }
}

fn temp_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kroma-run-lifecycle-{}-{}",
        label,
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Load the newest revision recorded on the session's active branch.
fn latest_revision(
    store: &SnapshotStore,
    journal: &SessionJournal,
) -> kroma_core::revision::WorkspaceRevision {
    let records = journal.revision_records();
    let last = records.last().expect("at least the baseline revision");
    store
        .load_revision(&RevisionId::new(last.revision_id.clone()))
        .unwrap()
}

fn wait_for_process_exit(pid: i32) {
    for _ in 0..100 {
        if !std::path::Path::new(&format!("/proc/{}", pid)).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// F01: dispatched-but-failed side effects are captured; refusals are not.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_bash_mutation_is_snapshotted() {
    let root = temp_root("failed-bash");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "before").unwrap();
    let state = root.join("state");
    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let wal = journal.path().to_owned();

    let budget = BudgetConfig {
        max_model_turns: 1,
        ..Default::default()
    };
    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::FullAutonomous,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let cfg = AgentRunConfig::new("repro", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let provider = Arc::new(FakeProvider::new(vec![tool(
        "bash",
        r#"{"command":"printf after > a.txt; exit 1"}"#,
    )]));

    let (summary, _) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store.clone())
        .run(logger)
        .await;

    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "after");
    assert_eq!(summary.terminal_reason, TerminalReason::MaxTurns);
    assert!(
        summary.modified_files.iter().any(|p| p == "a.txt"),
        "a failed but dispatched bash that wrote a file must be reported as modified: {:?}",
        summary.modified_files
    );
    let journal = SessionJournal::open(&wal).unwrap();
    let records = journal.revision_records();
    assert_eq!(
        records.len(),
        2,
        "baseline + the failed bash's mutation must both be recorded"
    );
    assert!(records
        .last()
        .unwrap()
        .changed_paths
        .iter()
        .any(|p| p == "a.txt"));
    let rev = latest_revision(&store, &journal);
    assert!(rev.tracked_files.contains_key("a.txt"));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn cancelled_bash_mutation_is_snapshotted() {
    let root = temp_root("cancelled-bash");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "before").unwrap();
    let state = root.join("state");
    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let wal = journal.path().to_owned();

    let budget = BudgetConfig {
        max_model_turns: 5,
        ..Default::default()
    };
    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::FullAutonomous,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let cfg = AgentRunConfig::new("repro", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = tx.send(true);
    });
    let provider = Arc::new(FakeProvider::new(vec![tool(
        "bash",
        r#"{"command":"printf after > a.txt; sleep 30"}"#,
    )]));

    let (summary, _) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store.clone())
        .run(logger)
        .await;

    assert_eq!(summary.status, RunStatus::Cancelled);
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "after");
    let journal = SessionJournal::open(&wal).unwrap();
    assert_eq!(journal.revision_records().len(), 2);
    assert!(latest_revision(&store, &journal)
        .tracked_files
        .contains_key("a.txt"));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn timed_out_bash_mutation_is_snapshotted() {
    let root = temp_root("timed-out-bash");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "before").unwrap();
    let state = root.join("state");
    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let wal = journal.path().to_owned();

    let budget = BudgetConfig {
        max_model_turns: 1,
        ..Default::default()
    };
    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::FullAutonomous,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let cfg = AgentRunConfig::new("repro", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let provider = Arc::new(FakeProvider::new(vec![tool(
        "bash",
        r#"{"command":"printf after > a.txt; sleep 30","timeout_sec":1}"#,
    )]));

    let (summary, _) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store.clone())
        .run(logger)
        .await;

    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "after");
    let journal = SessionJournal::open(&wal).unwrap();
    assert_eq!(journal.revision_records().len(), 2);
    assert!(latest_revision(&store, &journal)
        .tracked_files
        .contains_key("a.txt"));
    assert_eq!(summary.terminal_reason, TerminalReason::MaxTurns);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn denied_bash_does_not_fabricate_a_mutation() {
    let root = temp_root("denied-bash");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state = root.join("state");
    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let wal = journal.path().to_owned();

    let budget = BudgetConfig {
        max_model_turns: 1,
        ..Default::default()
    };
    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    // ReadOnly hides/denies `bash`, so the call is refused before dispatch.
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::ReadOnly,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let cfg = AgentRunConfig::new("repro", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let provider = Arc::new(FakeProvider::new(vec![tool(
        "bash",
        r#"{"command":"printf after > a.txt"}"#,
    )]));

    let (summary, _) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store.clone())
        .run(logger)
        .await;

    assert!(!ws.join("a.txt").exists(), "a denied bash must not run");
    assert!(summary.modified_files.is_empty());
    let journal = SessionJournal::open(&wal).unwrap();
    assert_eq!(
        journal.revision_records().len(),
        1,
        "a refused call must not append a mutation revision"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn decision_dispatch_classification_matches_refusals() {
    assert!(decision_dispatched("allow"));
    assert!(decision_dispatched("ask_approved"));
    assert!(decision_dispatched("allow_evidence"));
    assert!(!decision_dispatched("deny_policy_hidden"));
    assert!(!decision_dispatched("deny:policy"));
    assert!(!decision_dispatched("ask_denied"));
    assert!(!decision_dispatched("ask_cancelled"));
    assert!(!decision_dispatched("toctou_stale"));
    assert!(!decision_dispatched("deadline_exceeded"));
}

// ---------------------------------------------------------------------------
// F02: every completion-gate outcome is captured before it is acted on.
// ---------------------------------------------------------------------------

struct WritingGate {
    outcome: GateOutcome,
    error: Option<String>,
}

impl WritingGate {
    fn new(outcome: GateOutcome) -> Self {
        Self {
            outcome,
            error: None,
        }
    }
    fn error(message: &str) -> Self {
        Self {
            outcome: GateOutcome::Approved,
            error: Some(message.to_string()),
        }
    }
}

impl CompletionGate for WritingGate {
    fn check<'a>(
        &'a self,
        ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async move {
            std::fs::write(
                ctx.workspace_root.join("generated.txt"),
                "written by the gate",
            )
            .unwrap();
            match &self.error {
                Some(err) => Err(err.clone()),
                None => Ok(self.outcome.clone()),
            }
        })
    }
}

async fn run_with_gate(
    gate: Arc<dyn CompletionGate>,
) -> (kroma_core::agent_loop::RunSummary, bool, bool, bool) {
    let root = temp_root("gate");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state = root.join("state");
    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let wal = journal.path().to_owned();

    let budget = BudgetConfig::default();
    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::AllowWrite,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let cfg = AgentRunConfig::new("gate", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let provider = Arc::new(FakeProvider::new(vec![
        tool("write", r#"{"path":"a.txt","content":"edit"}"#),
        stop(),
    ]));

    let (summary, _) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store.clone())
        .with_completion_gate(gate)
        .with_completion_gate_max_retries(1)
        .run(logger)
        .await;

    let journal = SessionJournal::open(&wal).unwrap();
    let rev = latest_revision(&store, &journal);
    let has_generated = rev.tracked_files.contains_key("generated.txt");
    let reported = summary.modified_files.iter().any(|p| p == "generated.txt");
    let has_a = rev.tracked_files.contains_key("a.txt");
    let _ = std::fs::remove_dir_all(&root);
    (summary, has_generated, reported, has_a)
}

#[tokio::test]
async fn rejected_gate_mutation_is_snapshotted() {
    let gate = Arc::new(WritingGate::new(GateOutcome::Rejected {
        feedback: "intentional failure".into(),
    }));
    let (summary, has_generated, reported, has_a) = run_with_gate(gate).await;
    assert!(has_a, "the model's own write must be tracked");
    assert!(
        has_generated,
        "a rejected gate that wrote a file must still be captured"
    );
    assert!(
        reported,
        "summary.modified_files must include the gate's write"
    );
    assert!(matches!(
        summary.terminal_reason,
        TerminalReason::ProviderProtocolError(_)
    ));
}

#[tokio::test]
async fn approved_gate_mutation_is_snapshotted() {
    let gate = Arc::new(WritingGate::new(GateOutcome::Approved));
    let (summary, has_generated, reported, _) = run_with_gate(gate).await;
    assert!(has_generated);
    assert!(reported);
    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);
}

#[tokio::test]
async fn errored_gate_mutation_is_snapshotted() {
    let gate = Arc::new(WritingGate::error("gate exploded"));
    let (summary, has_generated, reported, _) = run_with_gate(gate).await;
    assert!(has_generated);
    assert!(reported);
    assert!(matches!(
        summary.terminal_reason,
        TerminalReason::ProviderProtocolError(_)
    ));
}

// ---------------------------------------------------------------------------
// F04: the run deadline is a shared, absolute bound.
// ---------------------------------------------------------------------------

struct SlowProvider;

impl ModelProvider for SlowProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok(ProviderCompletion {
                turn: stop(),
                request_body_bytes: 0,
                latency_ms: 150,
                ttft_ms: None,
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

#[tokio::test]
async fn run_deadline_bounds_a_slow_provider_request() {
    let root = temp_root("deadline-provider");
    let state = root.join("state");
    let budget = BudgetConfig {
        max_run_duration: Duration::from_millis(25),
        ..Default::default()
    };
    let env = Arc::new(LocalHostEnv::new(&root, state.clone()).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::ReadOnly,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let cfg = AgentRunConfig::new("repro", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let start = Instant::now();
    let (summary, _) = AgentRunner::new(cfg, Arc::new(SlowProvider), runtime, rx)
        .run(logger)
        .await;
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(120),
        "the request must be cut at the deadline, took {:?}",
        elapsed
    );
    assert_eq!(summary.status, RunStatus::BudgetExhausted);
    assert_eq!(summary.terminal_reason, TerminalReason::RunDeadlineExceeded);

    let _ = std::fs::remove_dir_all(&root);
}

struct DelayedToolProvider {
    delay: Duration,
    turn: AssistantTurn,
}

impl ModelProvider for DelayedToolProvider {
    fn complete<'a>(
        &'a self,
        _req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            Ok(ProviderCompletion {
                turn: self.turn.clone(),
                request_body_bytes: 0,
                latency_ms: self.delay.as_millis() as u64,
                ttft_ms: None,
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

#[tokio::test]
async fn run_deadline_terminates_bash_and_its_process_group() {
    let root = temp_root("deadline-bash");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state = root.join("state");
    // The provider burns half the budget before `bash` starts, so only ~500ms of the run deadline
    // remains. `execute_bash`'s own ceiling (`max_run_duration`, 1s) would let it run past the
    // deadline; only the shared absolute deadline can stop it at the right instant.
    let budget = BudgetConfig {
        max_run_duration: Duration::from_millis(1000),
        max_model_turns: 5,
        ..Default::default()
    };
    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::FullAutonomous,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let cfg = AgentRunConfig::new("deadline bash", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let provider = Arc::new(DelayedToolProvider {
        delay: Duration::from_millis(500),
        turn: tool("bash", r#"{"command":"echo $$ > pid.txt; sleep 30"}"#),
    });

    let start = Instant::now();
    let (summary, _) = AgentRunner::new(cfg, provider, runtime, rx)
        .run(logger)
        .await;
    let elapsed = start.elapsed();

    assert_eq!(summary.terminal_reason, TerminalReason::RunDeadlineExceeded);
    let bash_output = summary
        .transcript
        .iter()
        .find_map(|item| match item {
            kroma_core::messages::TranscriptItem::ToolResult(tr) if tr.tool_name == "bash" => {
                Some(tr.content.to_string())
            }
            _ => None,
        })
        .expect("the bash tool result is in the transcript");
    assert!(
        bash_output.contains("Run deadline reached"),
        "bash must be stopped by the shared run deadline, not its own ceiling: {}",
        bash_output
    );
    assert!(
        elapsed < Duration::from_millis(1400),
        "bash must not run to its own 1s ceiling after the deadline passed, took {:?}",
        elapsed
    );

    let pid: i32 = std::fs::read_to_string(ws.join("pid.txt"))
        .expect("the shell wrote its pid before sleeping")
        .trim()
        .parse()
        .unwrap();
    wait_for_process_exit(pid);
    assert!(
        !std::path::Path::new(&format!("/proc/{}", pid)).exists(),
        "the terminated bash process (pid {}) must not survive the deadline",
        pid
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn run_deadline_bounds_and_cleans_up_verification() {
    let root = temp_root("deadline-gate");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state = root.join("state");
    let budget = BudgetConfig {
        max_run_duration: Duration::from_millis(300),
        max_model_turns: 5,
        ..Default::default()
    };
    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::AllowWrite,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let gate = Arc::new(WorkspaceCompletionGate::new(
        Some(vec![
            "sh".into(),
            "-c".into(),
            "sleep 5; echo done > gate_marker.txt; exit 1".into(),
        ]),
        Duration::from_secs(30),
    ));
    let cfg = AgentRunConfig::new("deadline gate", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let provider = Arc::new(FakeProvider::new(vec![
        tool("write", r#"{"path":"a.txt","content":"edit"}"#),
        stop(),
    ]));

    let start = Instant::now();
    let (summary, _) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_completion_gate(gate)
        .run(logger)
        .await;
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "verification must be clamped to the run deadline, took {:?}",
        elapsed
    );
    assert_eq!(summary.terminal_reason, TerminalReason::RunDeadlineExceeded);
    assert!(
        !ws.join("gate_marker.txt").exists(),
        "the verification process group must be terminated before it can finish"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// F09: the provider retry loop only spends the remaining request attempts.
// ---------------------------------------------------------------------------

async fn read_request_body(sock: &mut TcpStream) -> Vec<u8> {
    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match tokio::time::timeout(Duration::from_secs(2), sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) => break,
        }
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&buf[..pos]);
            let content_length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if buf.len() >= pos + 4 + content_length {
                break;
            }
        }
        if buf.len() > 8 * 1024 * 1024 {
            break;
        }
    }
    buf
}

#[tokio::test]
async fn provider_retry_cannot_exceed_the_remaining_attempts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_server = Arc::clone(&hits);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let count = hits_server.fetch_add(1, Ordering::SeqCst) + 1;
            tokio::spawn(async move {
                let _ = read_request_body(&mut sock).await;
                let response = if count == 1 {
                    let body = r#"{"id":"cmpl-1","choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"ls","arguments":"{\"path\":\".\"}"}}]}}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else {
                    "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 14\r\nConnection: close\r\n\r\nrate limit hit".to_string()
                };
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });

    let client = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        api_key: "sk-test".into(),
        model: "gpt-test".into(),
        // Deliberately larger than the run-level cap: the provider must not use it.
        max_retries: 5,
        reasoning_effort: None,
    })
    .unwrap();

    let root = temp_root("retry-cap");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state = root.join("state");
    let budget = BudgetConfig {
        max_model_turns: 5,
        max_model_request_attempts: 3,
        ..Default::default()
    };
    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::ReadOnly,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let mut cfg = AgentRunConfig::new("retry cap", budget);
    cfg.stream = false;
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let (_tx, rx) = watch::channel(false);

    let (summary, _) = AgentRunner::new(cfg, Arc::new(client), runtime, rx)
        .run(logger)
        .await;

    let total_hits = hits.load(Ordering::SeqCst);
    assert!(
        total_hits <= 3,
        "the provider must not send more than max_model_request_attempts HTTP requests, sent {}",
        total_hits
    );
    assert_eq!(
        total_hits, 3,
        "the first turn uses one request and the second spends the two remaining"
    );
    assert!(
        matches!(
            summary.terminal_reason,
            TerminalReason::ProviderProtocolError(_)
        ),
        "unexpected terminal reason: {:?}",
        summary.terminal_reason
    );

    let _ = std::fs::remove_dir_all(&root);
}
