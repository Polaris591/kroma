//! Production-entry contracts for adaptive escalation. Deterministic providers, real worktrees,
//! real development gate, final scorer, snapshots/WAL. No external model or paid calls.
use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use kroma_core::bench::{
    BenchExecutor, BenchStrategyConfig, BenchTask, ProviderBenchExecutor, ProviderBenchSettings,
};
use kroma_core::budget::BudgetConfig;
use kroma_core::env::{
    CompletionGate, CompletionGateContext, ExecutionEnv, GateOutcome, LocalHostEnv,
};
use kroma_core::events::{EventLogger, EventPayload};
use kroma_core::job_scope::JobScope;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall, TranscriptItem};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::revision::SnapshotStore;
use kroma_core::session::SessionJournal;
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::types::{TerminalReason, ToolCallId};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("at-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct StallProvider {
    requests: Mutex<Vec<ProviderRequest>>,
}
impl ModelProvider for StallProvider {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        cancel: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ProviderError::Cancelled);
            }
            let child = req.system_prompt.contains("SUBAGENT ROLE: worker");
            let wrote = req.messages.iter().any(|message| {
                matches!(message,
                TranscriptItem::ToolResult(result) if result.tool_name == "write")
            });
            let good = child
                && req.messages.iter().any(|message| {
                    matches!(message,
                TranscriptItem::User(user) if user.content.contains("CANDIDATE c1 STRATEGY"))
                });
            let usage = Some(TokenUsage {
                input_tokens: if child { 13 } else { 11 },
                cached_input_tokens: Some(if child { 7 } else { 4 }),
                cache_creation_input_tokens: None,
                output_tokens: if child { 3 } else { 2 },
                reasoning_tokens: None,
            });
            let turn = if wrote {
                AssistantTurn {
                    content: Some("done".into()),
                    reasoning_blocks: vec![],
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                    usage,
                }
            } else {
                AssistantTurn {
                    content: None,
                    reasoning_blocks: vec![],
                    tool_calls: vec![ToolCall {
                        call_id: ToolCallId::new(if child {
                            "child-write"
                        } else {
                            "parent-write"
                        }),
                        name: "write".into(),
                        raw_arguments: serde_json::json!({
                            "path":"src/calc.js", "content": if good {
                                "module.exports = (a,b) => a+b; // repaired\n"
                            } else if child {
                                "module.exports = (a,b) => a-b; // failed-candidate\n"
                            } else {
                                "module.exports = (a,b) => a-b; // parent-attempt\n"
                            }
                        })
                        .to_string(),
                    }],
                    finish_reason: FinishReason::ToolCalls,
                    usage,
                }
            };
            self.requests.lock().unwrap().push(req);
            Ok(ProviderCompletion {
                turn,
                request_body_bytes: 0,
                latency_ms: 0,
                ttft_ms: Some(0),
                attempts_used: 1,
                reported_model: Some("deterministic-checkpoint".into()),
            })
        })
    }
}

fn task() -> BenchTask {
    BenchTask {
        id:"stall-local-development-check".into(), prompt:"Repair addition in src/calc.js".into(),
        files:[
            ("src/calc.js".into(), "module.exports = (a,b) => a-b;\n".into()),
            ("package.json".into(), r#"{"scripts":{"test":"node test/check.js"}}"#.into()),
            ("test/check.js".into(), "const add=require('../src/calc.js'); require('assert').strictEqual(add(2,3),5);\n".into()),
        ].into_iter().collect(),
        expect_contains:vec![], expect_absent:vec![], regression_guard:false,
        // A genuinely separate final check. Its command/diagnostics must not reach the provider.
        verify_command:Some(vec!["node".into(),"-e".into(),
            "/* HELD_OUT_DO_NOT_DISCLOSE */ require('assert').strictEqual(require('./src/calc.js')(8,4),12)".into()]),
    }
}

#[tokio::test]
async fn provider_bench_arms_use_a_development_gate_without_leaking_the_final_scorer() {
    let root = Tree::new();
    let provider = Arc::new(StallProvider::default());
    let settings = ProviderBenchSettings::new("fixture-alias", "local")
        .with_policy_mode(PolicyMode::FullAutonomous)
        .with_max_model_turns(8)
        .with_max_run_duration(Duration::from_secs(30));
    let executor = ProviderBenchExecutor::new(provider.clone(), settings);
    let task = task();
    // The arm runs inside its job scope, exactly as `run_paired_benchmark` runs it.
    let adaptive_scope = JobScope::open(&root.0, "adaptive").unwrap();
    let adaptive = executor
        .execute(
            &task,
            &BenchStrategyConfig::adaptive_tournament(2),
            500_000,
            &adaptive_scope,
        )
        .await;
    adaptive_scope.keep();
    assert!(
        adaptive.resolved,
        "adaptive Bench must actually reach verified children: {adaptive:?}"
    );
    assert!(
        adaptive.children_tokens > 0,
        "arm name is not proof of a tournament"
    );
    assert_eq!(
        adaptive.children_input_tokens, 52,
        "both winner and failed candidate are charged"
    );
    assert_eq!(adaptive.children_output_tokens, 12);
    // Parse new telemetry as JSON so its absence is an assertion failure, not an API compile error.
    let json = serde_json::to_value(&adaptive).unwrap();
    assert_eq!(json["mechanism"]["adaptive_tournaments"], 1);
    assert_eq!(json["mechanism"]["candidate_runs"], 2);
    assert_eq!(json["mechanism"]["verified_candidates"], 1);
    assert_eq!(json["mechanism"]["merged_tournaments"], 1);
    {
        let requests = provider.requests.lock().unwrap();
        assert!(requests
            .iter()
            .any(|r| r.system_prompt.contains("SUBAGENT ROLE: worker")));
        for request in requests.iter() {
            assert!(
                !format!("{:?}", request.messages).contains("HELD_OUT_DO_NOT_DISCLOSE"),
                "final scorer must stay independent of development feedback"
            );
        }
    }
    let single_scope = JobScope::open(&root.0, "single").unwrap();
    let single = executor
        .execute(
            &task,
            &BenchStrategyConfig::single_agent(),
            500_000,
            &single_scope,
        )
        .await;
    single_scope.keep();
    assert!(
        !single.resolved,
        "a deliberately stalled single agent must not fabricate a fix"
    );
    assert_eq!(single.children_tokens, 0);
}

struct ContentGate;
impl CompletionGate for ContentGate {
    fn check<'a>(
        &'a self,
        ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async move {
            let text = std::fs::read_to_string(ctx.workspace_root.join("src/calc.js")).unwrap();
            Ok(if text.contains("=> a+b;") {
                GateOutcome::Approved
            } else {
                GateOutcome::Rejected {
                    feedback: "addition is still wrong".into(),
                }
            })
        })
    }
}

/// Run real child loops, then withhold their completed outcomes until the parent is cancelled.
/// This models a late completion without relying on timing or weakening the production merger.
struct DelayedCompletionBackend {
    inner: kroma_core::subagents::ProviderSubagentBackend,
    ready: tokio::sync::mpsc::Sender<()>,
    release: watch::Receiver<bool>,
}
impl kroma_core::subagents::SubagentBackend for DelayedCompletionBackend {
    fn run_subagent(
        &self,
        req: kroma_core::subagents::SubagentExecutionRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<kroma_core::subagents::SubagentRunOutcome, String>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let result = self.inner.run_subagent(req).await?;
            self.ready.send(()).await.map_err(|e| e.to_string())?;
            let mut release = self.release.clone();
            while !*release.borrow() {
                release.changed().await.map_err(|e| e.to_string())?;
            }
            Ok(result) // Deliberately late, even if parent cancellation has arrived.
        })
    }
}

#[tokio::test]
async fn cancelled_adaptive_run_does_not_merge_late_completed_candidate_bytes() {
    let root = Tree::new();
    let ws = root.0.join("ws");
    let state = root.0.join("state");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/calc.js"), "module.exports = (a,b) => a-b;\n").unwrap();
    let env = Arc::new(LocalHostEnv::new(&ws, &state).unwrap());
    let budget = BudgetConfig {
        max_model_turns: 8,
        max_total_tokens: Some(500_000),
        max_run_duration: Duration::from_secs(20),
        ..Default::default()
    };
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::FullAutonomous,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let cfg = AgentRunConfig::new("Repair addition", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state.clone()), false).unwrap();
    let provider = Arc::new(StallProvider::default());
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(2);
    let (release_tx, release_rx) = watch::channel(false);
    let backend = Arc::new(DelayedCompletionBackend {
        inner: kroma_core::subagents::ProviderSubagentBackend::new(provider.clone()),
        ready: ready_tx,
        release: release_rx,
    });
    let (cancel, rx) = watch::channel(false);
    let runner = AgentRunner::new(cfg, provider, runtime, rx)
        .with_completion_gate(Arc::new(ContentGate))
        .with_completion_gate_max_retries(4)
        .with_max_subagents(2)
        .with_adaptive_tournament_candidates(2)
        .with_subagent_backend(backend);
    let running = tokio::spawn(runner.run(logger));
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(3), ready_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(std::fs::read_to_string(ws.join("src/calc.js"))
        .unwrap()
        .contains("parent-attempt"));
    cancel.send(true).unwrap();
    release_tx.send(true).unwrap();
    let (summary, logger) = tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(summary.terminal_reason, TerminalReason::UserCancelled);
    assert!(
        std::fs::read_to_string(ws.join("src/calc.js"))
            .unwrap()
            .contains("parent-attempt"),
        "late candidate bytes must never overwrite the cancelled parent's tree"
    );
    assert_eq!(
        summary.children_input_tokens, 52,
        "cancelled tournament still accounts for completed children"
    );
    assert_eq!(summary.children_output_tokens, 12);
    assert!(logger.events().iter().any(|event| matches!(
        &event.event,
        EventPayload::TournamentCompleted {
            candidate_runs: 2,
            merged: false,
            ..
        }
    )));
    assert!(!logger.events().iter().any(|event| matches!(&event.event,
        EventPayload::WorktreeMerged { status,.. } if status=="applied")));
    assert!(std::fs::read_dir(state.join("worktrees"))
        .unwrap()
        .next()
        .is_none());
}

#[tokio::test]
async fn adaptive_runtime_records_completion_usage_and_merged_revision_with_undo() {
    let root = Tree::new();
    let ws = root.0.join("ws");
    let state = root.0.join("state");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/calc.js"), "module.exports = (a,b) => a-b;\n").unwrap();
    let env = Arc::new(LocalHostEnv::new(&ws, &state).unwrap());
    let budget = BudgetConfig {
        max_model_turns: 8,
        max_total_tokens: Some(500_000),
        max_run_duration: Duration::from_secs(20),
        ..Default::default()
    };
    let runtime = build_default_runtime(
        env.clone(),
        PolicyEngine::new(
            PolicyMode::FullAutonomous,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let wal = journal.path().to_path_buf();
    let store = Arc::new(SnapshotStore::new(state.join("snapshots")));
    let cfg = AgentRunConfig::new("Repair addition", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state.clone()), false).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, logger) = AgentRunner::new(cfg, Arc::new(StallProvider::default()), runtime, rx)
        .with_completion_gate(Arc::new(ContentGate))
        .with_completion_gate_max_retries(4)
        .with_max_subagents(2)
        .with_adaptive_tournament_candidates(2)
        .with_snapshot_store(store.clone())
        .with_session_journal(journal)
        .run(logger)
        .await;
    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);
    assert_eq!(summary.children_input_tokens, 52);
    assert_eq!(summary.children_output_tokens, 12);
    assert_eq!(
        summary.children_cached_input_tokens, 28,
        "do not lose child cached usage at the boundary"
    );
    assert_eq!(summary.usage_basis, "provider_reported");
    let spawned = logger
        .events()
        .iter()
        .filter(|e| matches!(&e.event, EventPayload::SubagentSpawned { .. }))
        .count();
    let completed: Vec<_> = logger
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            EventPayload::SubagentCompleted { tokens_used, .. } => Some(*tokens_used),
            _ => None,
        })
        .collect();
    assert_eq!(spawned, 2);
    assert_eq!(
        completed,
        vec![32, 32],
        "real candidates must have completion/usage evidence"
    );
    assert!(logger.events().iter().any(|e|matches!(&e.event,
        EventPayload::WorktreeMerged{status,revision_after,..} if status=="applied"&&revision_after.is_some())));
    let journal = SessionJournal::open(&wal).unwrap();
    let revisions = journal.revision_records();
    assert!(
        revisions.len() >= 3,
        "baseline, parent attempt and verified merged bytes must persist"
    );
    assert!(journal
        .latest_active_checkpoint()
        .unwrap()
        .mutations
        .iter()
        .any(|m| m.tool_name == "adaptive_tournament"));
    let before =
        kroma_core::revision::RevisionId::parse(&revisions[revisions.len() - 2].revision_id)
            .unwrap();
    let current =
        kroma_core::revision::RevisionId::parse(&revisions.last().unwrap().revision_id).unwrap();
    store
        .restore_revision(env.workspace(), &before, Some(&current), false)
        .unwrap();
    assert!(std::fs::read_to_string(ws.join("src/calc.js"))
        .unwrap()
        .contains("parent-attempt"));
}
