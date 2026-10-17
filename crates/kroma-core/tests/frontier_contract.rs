//! Frontier contract tests (`v1.5.2`, capability gaps #2–#6 from the review).
//!
//! One section per mechanism so a regression points at exactly one gap:
//!
//! * [`require_verified`] — a repair race must not treat a *skipped* verification as a win.
//! * counterfactual probe — opt-in, approved, budgeted, never auto-rollback.
//! * causal pruning / dead-end compression — optional, evidence- and pairing-preserving.
//! * bounded persistent async tasks — registration, artifacts, notifications, resume/challenge.
//! * skills / on-demand tools / MCP — `.claude` discovery, `paths` activation, MCP stdio adapter.

use kroma_core::budget::BudgetConfig;
use kroma_core::env::{CompletionGate, CompletionGateContext, GateOutcome};
use kroma_core::revision::SnapshotStore;
use kroma_core::subagents::{
    parse_tournament_request, run_worktree_tournament, ParentPolicyHooks, SubagentBackend,
    SubagentExecutionRequest, SubagentRunOutcome, TournamentCandidate, TournamentSpec,
};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::types::{RunStatus, TerminalReason};
use kroma_core::workspace::WorkspaceCap;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-frontier-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every candidate writes a real diff but the gate skips verification (nothing to run).
struct SkippedGate;

impl CompletionGate for SkippedGate {
    fn check<'a>(
        &'a self,
        _ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async move {
            Ok(GateOutcome::Skipped {
                reason: "no verification command configured".to_string(),
            })
        })
    }
}

struct ScriptedBackend {
    files: Vec<(String, String)>,
}

impl SubagentBackend for ScriptedBackend {
    fn run_subagent(
        &self,
        req: SubagentExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SubagentRunOutcome, String>> + Send + '_>> {
        let files = self.files.clone();
        let root = req.workspace.root().to_path_buf();
        Box::pin(async move {
            for (path, content) in files {
                let target = root.join(&path);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(&target, content).unwrap();
            }
            Ok(SubagentRunOutcome {
                status: RunStatus::Completed,
                terminal_reason: TerminalReason::ModelStop,
                summary: "scripted candidate completed".to_string(),
                relevant_files: Vec::new(),
                findings: Vec::new(),
                turns_used: 1,
                tool_calls_used: 1,
                tokens_used: 10,
                input_tokens: 6,
                cached_input_tokens: None,
                usage_basis: "provider_reported".to_string(),
                output_tokens: 4,
                duration_ms: 1,
                visible_tools: Vec::new(),
            })
        })
    }
}

fn baseline_workspace(root: &Path) -> (WorkspaceCap, SnapshotStore) {
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(
        ws.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();
    let store = SnapshotStore::new(root.join("store"));
    (workspace, store)
}

fn tournament_spec(root: &Path, candidates: usize, require_verified: bool) -> TournamentSpec {
    let mut spec = TournamentSpec {
        candidates: (0..candidates)
            .map(|i| TournamentCandidate::worker(format!("c{}", i + 1), "fix the bug"))
            .collect(),
        worktree_base: root.join("worktrees"),
        state_base: root.join("subagents"),
        allowed_write_globs: None,
        budget: BudgetConfig::default(),
        token_budget: None,
        policy_mode: PolicyMode::FullAutonomous,
        parent_policy_hooks: ParentPolicyHooks::KnownEmpty,
        workspace_skills: Vec::new(),
        predictive_check: false,
        cancel_rx: watch::channel(false).1,
        timeout_ms: 5_000,
        disallow_test_tampering: false,
        protected_paths: Vec::new(),
        require_verified,
    };
    spec.candidates.truncate(candidates);
    spec
}

// ---------------------------------------------------------------------------------------------
// Capability gap #5 — verify-before-win
// ---------------------------------------------------------------------------------------------

#[test]
fn strict_tournament_parses_require_verified_flag() {
    let parsed = parse_tournament_request(
        r#"{"task":"fix it","mode":"tournament","candidates":2,"require_verified":true}"#,
    )
    .unwrap()
    .expect("a tournament request");
    assert_eq!(parsed.require_verified, Some(true));

    let inherited =
        parse_tournament_request(r#"{"task":"fix it","tournament":true,"candidates":2}"#)
            .unwrap()
            .expect("a tournament request");
    assert_eq!(
        inherited.require_verified, None,
        "no explicit argument means the caller's strict default applies"
    );
}

#[test]
fn host_verification_requirement_cannot_be_downgraded_by_a_model() {
    use kroma_core::subagents::effective_tournament_require_verified;

    for request in [None, Some(true), Some(false)] {
        assert!(
            effective_tournament_require_verified(true, request),
            "a model must not relax strict host verification: {request:?}"
        );
    }
    assert!(!effective_tournament_require_verified(false, None));
    assert!(!effective_tournament_require_verified(false, Some(false)));
    assert!(effective_tournament_require_verified(false, Some(true)));
}

#[tokio::test]
async fn strict_tournament_rejects_a_skipped_verification_instead_of_merging_it() {
    let root = temp_root("strict-skip");
    let (workspace, store) = baseline_workspace(&root);
    let backend: Arc<dyn SubagentBackend> = Arc::new(ScriptedBackend {
        files: vec![(
            "lib.rs".to_string(),
            "pub fn add(a: i32, b: i32) -> i32 { a - b }\n".to_string(),
        )],
    });
    let gate: Arc<dyn CompletionGate> = Arc::new(SkippedGate);

    // Legacy policy: a skipped verification was accepted and merged (the review's complaint).
    let legacy = run_worktree_tournament(
        tournament_spec(&root, 2, false),
        &workspace,
        &store,
        Arc::clone(&backend),
        None,
        &gate,
    )
    .await
    .expect("tournament runs");
    assert!(
        legacy.winner.is_some(),
        "legacy policy merges a skipped-verification candidate"
    );

    // Strict repair policy: the same candidates must NOT merge, and no score may claim a pass.
    let strict = run_worktree_tournament(
        tournament_spec(&root, 2, true),
        &workspace,
        &store,
        backend,
        None,
        &gate,
    )
    .await
    .expect("tournament runs");
    assert!(
        strict.winner.is_none(),
        "strict policy refuses a skipped verification"
    );
    assert!(strict.parent_untouched());
    for score in &strict.scores {
        assert!(!score.gate_passed, "skipped must not report as passed");
        assert!(!score.eligible_to_win_with(true));
        assert!(
            score.gate_status.as_str() == "skipped",
            "status must stay `skipped`, got {}",
            score.gate_status.as_str()
        );
    }
    let report = strict.render_report();
    assert!(
        report.contains("skipped verification and were not treated as successes"),
        "report must call out unverified candidates: {report}"
    );
}

// ---------------------------------------------------------------------------------------------
// T01 (KR-01) — the host owns the verification requirement; the model may only tighten it
// ---------------------------------------------------------------------------------------------

#[test]
fn verification_policy_matrix_is_resolved_with_provenance() {
    use kroma_core::subagents::{resolve_tournament_verification, VerificationPolicySource as Src};

    // (host_required, model_requested, effective, provenance)
    let cases = [
        (true, None, true, Src::HostStrict),
        (true, Some(true), true, Src::HostStrict),
        (true, Some(false), true, Src::ModelDowngradeIgnored),
        (false, None, false, Src::HostRelaxed),
        (false, Some(false), false, Src::HostRelaxed),
        (false, Some(true), true, Src::ModelRequestedStrict),
    ];
    for (host, model, effective, source) in cases {
        let resolved = resolve_tournament_verification(host, model);
        assert_eq!(
            resolved.require_verified, effective,
            "host={host} model={model:?} must resolve to {effective}"
        );
        assert_eq!(
            resolved.source, source,
            "host={host} model={model:?} must be attributed to {source:?}"
        );
    }
    assert!(Src::ModelDowngradeIgnored.is_downgrade_attempt());
    assert!(!Src::HostStrict.is_downgrade_attempt());
}

#[test]
fn protected_paths_and_test_tampering_cannot_shrink_from_tool_or_rpc() {
    use kroma_core::subagents::{effective_disallow_test_tampering, merge_protected_globs};

    let host = vec!["oracle/**".to_string(), "tests/**".to_string()];
    // A caller that supplies nothing cannot clear the host set.
    assert_eq!(merge_protected_globs(&host, &[]), host);
    // A caller can only add to it.
    let merged = merge_protected_globs(&host, &["src/**.rs".to_string()]);
    for glob in &host {
        assert!(merged.contains(glob), "host glob {glob} disappeared");
    }
    assert!(merged.contains(&"src/**.rs".to_string()));
    // Duplicates collapse; order is stable.
    assert_eq!(
        merge_protected_globs(&["b".to_string()], &["a".to_string(), "b".to_string()]),
        vec!["a".to_string(), "b".to_string()]
    );
    // Host test-tampering protection cannot be switched off by a task-level flag.
    assert!(effective_disallow_test_tampering(true, false));
    assert!(effective_disallow_test_tampering(true, true));
    assert!(effective_disallow_test_tampering(false, true));
    assert!(!effective_disallow_test_tampering(false, false));
}

/// Gate that always errors: an infrastructure failure, not the candidate's fault.
struct InfrastructureErrorGate;

impl CompletionGate for InfrastructureErrorGate {
    fn check<'a>(
        &'a self,
        _ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async move { Err("verification harness crashed".to_string()) })
    }
}

/// Gate that rejects: the candidate's patch is wrong.
struct RejectingGate;

impl CompletionGate for RejectingGate {
    fn check<'a>(
        &'a self,
        _ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async move {
            Ok(GateOutcome::Rejected {
                feedback: "assertion failed".to_string(),
            })
        })
    }
}

/// Backend that never completes: the candidate did not reach verification.
struct FailingBackend;

impl SubagentBackend for FailingBackend {
    fn run_subagent(
        &self,
        _req: SubagentExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SubagentRunOutcome, String>> + Send + '_>> {
        Box::pin(async move { Err("child loop aborted".to_string()) })
    }
}

/// The verification state transition table: only a real pass may win under a strict host, and a
/// `Skipped` gate is the only non-pass a relaxed host may accept.
#[tokio::test]
async fn verification_state_transition_table_holds_for_every_terminal_status() {
    use kroma_core::subagents::GateVerificationStatus;

    struct Case {
        tag: &'static str,
        gate: Arc<dyn CompletionGate>,
        backend: Arc<dyn SubagentBackend>,
        expected: GateVerificationStatus,
        merges_when_relaxed: bool,
    }

    let cases = [
        Case {
            tag: "skipped",
            gate: Arc::new(SkippedGate),
            backend: Arc::new(ScriptedBackend {
                files: vec![(
                    "lib.rs".to_string(),
                    "pub fn add(a: i32, b: i32) -> i32 { a.wrapping_add(b) }\n".to_string(),
                )],
            }),
            expected: GateVerificationStatus::Skipped,
            merges_when_relaxed: true,
        },
        Case {
            tag: "failed",
            gate: Arc::new(RejectingGate),
            backend: Arc::new(ScriptedBackend {
                files: vec![(
                    "lib.rs".to_string(),
                    "pub fn add(a: i32, b: i32) -> i32 { a - b }\n".to_string(),
                )],
            }),
            expected: GateVerificationStatus::Failed,
            merges_when_relaxed: false,
        },
        Case {
            tag: "infrastructure-error",
            gate: Arc::new(InfrastructureErrorGate),
            backend: Arc::new(ScriptedBackend {
                files: vec![(
                    "lib.rs".to_string(),
                    "pub fn add(a: i32, b: i32) -> i32 { a.wrapping_add(b) }\n".to_string(),
                )],
            }),
            expected: GateVerificationStatus::InfrastructureError,
            merges_when_relaxed: false,
        },
        Case {
            tag: "not-run",
            gate: Arc::new(SkippedGate),
            backend: Arc::new(FailingBackend),
            expected: GateVerificationStatus::NotRun,
            merges_when_relaxed: false,
        },
    ];

    for case in cases {
        let root = temp_root(case.tag);
        let (workspace, store) = baseline_workspace(&root);
        let strict = run_worktree_tournament(
            tournament_spec(&root, 2, true),
            &workspace,
            &store,
            Arc::clone(&case.backend),
            None,
            &case.gate,
        )
        .await
        .expect("tournament runs");
        assert_eq!(
            strict.scores[0].gate_status, case.expected,
            "{}: terminal status must be preserved",
            case.tag
        );
        assert!(
            strict.winner.is_none(),
            "{}: a strict host must never merge a {}",
            case.tag,
            case.expected.as_str()
        );
        assert!(
            strict.parent_untouched(),
            "{}: the parent workspace must stay untouched",
            case.tag
        );

        let relaxed = run_worktree_tournament(
            tournament_spec(&root, 2, false),
            &workspace,
            &store,
            case.backend,
            None,
            &case.gate,
        )
        .await
        .expect("tournament runs");
        assert_eq!(
            relaxed.winner.is_some(),
            case.merges_when_relaxed,
            "{}: relaxed host merge expectation",
            case.tag
        );
        if !case.merges_when_relaxed {
            assert!(
                relaxed.parent_untouched(),
                "{}: a relaxed host still must not merge this candidate",
                case.tag
            );
        }
        let _ = fs::remove_dir_all(&root);
    }
}

/// Drive the *real* `spawn_subagent` tool through the agent loop so the assertion covers the tool
/// entry point, not just `run_worktree_tournament`. Returns the parent file afterwards.
async fn run_spawn_subagent_tournament(
    tag: &str,
    host_require_verified: bool,
    host_protected: Vec<String>,
    model_args: serde_json::Value,
) -> (String, String) {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::events::EventLogger;
    use kroma_core::messages::{AssistantTurn, FinishReason, ToolCall};
    use kroma_core::provider::FakeProvider;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine};
    use kroma_core::types::ToolCallId;

    let root = temp_root(tag);
    let ws_dir = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        ws_dir.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();

    let tool_turn = AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new("call_race"),
            name: "spawn_subagent".to_string(),
            raw_arguments: model_args.to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    };
    let stop_turn = AssistantTurn {
        content: Some("race finished".to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: None,
    };
    let provider = Arc::new(FakeProvider::new(vec![tool_turn, stop_turn]));

    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state_dir.join("sessions"), &ws_dir).unwrap();
    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws_dir, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig::default();
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("run a repair race", budget);
    let run_id = cfg.run_id;
    let logger = EventLogger::new(run_id, Some(state_dir.clone()), true).unwrap();
    let backend: Arc<dyn SubagentBackend> = Arc::new(ScriptedBackend {
        files: vec![(
            "lib.rs".to_string(),
            "pub fn add(a: i32, b: i32) -> i32 { a.wrapping_add(b) }\n".to_string(),
        )],
    });
    let (_tx, rx) = watch::channel(false);

    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .with_completion_gate(Arc::new(SkippedGate))
        .with_subagent_backend(backend)
        .with_tournament_require_verified(host_require_verified)
        .with_host_protected_paths(host_protected)
        .run(logger)
        .await;
    assert_eq!(
        summary.status,
        RunStatus::Completed,
        "{:?}",
        summary.terminal_reason
    );

    let parent = fs::read_to_string(ws_dir.join("lib.rs")).unwrap();
    let log = fs::read_to_string(state_dir.join(format!("{}.jsonl", run_id))).unwrap_or_default();
    let _ = fs::remove_dir_all(&root);
    (parent, log)
}

#[tokio::test]
async fn host_true_and_model_false_still_rejects_skipped_through_the_tool_entry() {
    let args = serde_json::json!({
        "mode": "tournament",
        "task": "fix add()",
        "candidates": 2,
        "require_verified": false,
    });
    let (parent, log) =
        run_spawn_subagent_tournament("t01-strict-false", true, Vec::new(), args).await;
    assert_eq!(
        parent, "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        "a model must not merge an unverified candidate into the parent workspace"
    );
    assert!(
        log.contains("model_downgrade_ignored"),
        "the refused downgrade must be audited; log:\n{log}"
    );
}

#[tokio::test]
async fn operator_only_unverified_mode_is_audited() {
    // No `require_verified` argument: the relaxed decision is the operator's, and it is logged.
    let args = serde_json::json!({
        "mode": "tournament",
        "task": "fix add()",
        "candidates": 2,
    });
    let (parent, log) = run_spawn_subagent_tournament("t01-relaxed", false, Vec::new(), args).await;
    assert_eq!(
        parent, "pub fn add(a: i32, b: i32) -> i32 { a.wrapping_add(b) }\n",
        "a relaxed host accepts a skipped gate, as configured by the operator"
    );
    assert!(
        log.contains("host_relaxed"),
        "the operator-owned relaxed policy must be audited; log:\n{log}"
    );
}

// ---------------------------------------------------------------------------------------------
// Capability gap #6 — skills (.claude + paths), on-demand tool schema, MCP stdio adapter
// ---------------------------------------------------------------------------------------------

#[test]
fn claude_skill_dir_and_paths_activation_are_supported() {
    use kroma_core::extensions::{discover_skills, execute_activate_skill};

    let root = temp_root("skills");
    let ws = root.join("ws");
    fs::create_dir_all(ws.join(".agents/skills/general")).unwrap();
    fs::create_dir_all(ws.join(".claude/skills/rust-audit")).unwrap();
    fs::create_dir_all(ws.join(".claude/skills/python-only")).unwrap();
    fs::write(
        ws.join(".agents/skills/general/SKILL.md"),
        "---\nname: general\ndescription: always applies\n---\n# general\n",
    )
    .unwrap();
    fs::write(
        ws.join(".claude/skills/rust-audit/SKILL.md"),
        "---\nname: rust-audit\ndescription: rust only\npaths:\n  - src/**/*.rs\n---\n# rust audit\n",
    )
    .unwrap();
    fs::write(
        ws.join(".claude/skills/python-only/SKILL.md"),
        "---\nname: python-only\ndescription: python only\npaths: [src/**/*.py]\n---\n# python\n",
    )
    .unwrap();
    let workspace = WorkspaceCap::new(&ws).unwrap();

    let skills = discover_skills(&workspace);
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["general", "python-only", "rust-audit"],
        "skills must be discovered from both .agents/skills and .claude/skills"
    );
    let rust = skills.iter().find(|s| s.name == "rust-audit").unwrap();
    assert_eq!(rust.paths, vec!["src/**/*.rs".to_string()]);
    assert!(rust.applies_to("src/lib.rs"));
    assert!(!rust.applies_to("src/main.py"));

    // Listing scoped to a Rust file hides the python-only skill.
    let rust_list = execute_activate_skill(&skills, r#"{"action":"list","path":"src/lib.rs"}"#);
    assert!(rust_list.content.contains("rust-audit"));
    assert!(!rust_list.content.contains("python-only"));

    // Activating a non-applicable skill is refused with the restriction explained.
    let denied = execute_activate_skill(&skills, r#"{"name":"python-only","path":"src/lib.rs"}"#);
    assert!(!denied.outcome.is_success());
    assert!(denied.content.contains("restricted to paths"));

    let ok = execute_activate_skill(&skills, r#"{"name":"rust-audit","path":"src/lib.rs"}"#);
    assert!(ok.outcome.is_success());
    assert!(ok.content.contains("untrusted_skill_context"));
}

#[test]
fn discover_tools_returns_policy_scoped_schemas_on_demand() {
    use kroma_core::extensions::execute_discover_tools;
    use kroma_core::tool::policy::PolicyMode;
    use kroma_core::tool::registry::ToolRegistry;

    let registry = ToolRegistry::default();
    let policy = PolicyMode::ReadOnly;
    let catalog: Vec<_> = registry
        .all_registered()
        .into_iter()
        .filter(|tool| registry.is_tool_enabled_for_policy(tool, policy))
        .collect();

    let out = execute_discover_tools(&catalog, r#"{"query":"grep","include_schema":true}"#);
    assert!(out.outcome.is_success());
    assert!(out.content.contains("grep"));
    assert!(out.content.contains("schema"));
    assert!(out.content.contains("tool_catalog"));

    // A read-only policy must not reveal `write`/`edit`/`bash`.
    let all = execute_discover_tools(&catalog, r#"{"include_schema":false}"#);
    assert!(!all.content.contains("- write:"));
    assert!(!all.content.contains("- edit:"));
    assert!(!all.content.contains("- bash:"));
}

#[tokio::test]
async fn mcp_stdio_adapter_handshakes_lists_and_calls_tools() {
    use kroma_core::mcp::{McpClient, MCP_PROTOCOL_VERSION};

    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp/echo_server.py");
    if !script.exists() {
        // A missing fixture is an explicit environment gap, not a silent pass.
        panic!("MCP fixture missing at {}", script.display());
    }
    let mut client = McpClient::spawn(
        "python3",
        &["-u".to_string(), script.display().to_string()],
        &[],
        std::time::Duration::from_secs(10),
    )
    .expect("spawn python MCP server");

    let init = client.initialize().await.expect("initialize handshake");
    assert_eq!(init.protocol_version, MCP_PROTOCOL_VERSION);
    assert_eq!(init.server_info["name"], "echo-server");

    let tools = client.list_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["echo", "boom"]);

    let registered =
        tools[0].to_registered_tool("echo-server", kroma_core::types::EffectClass::ReadOnly);
    assert_eq!(registered.definition.name, "mcp__echo-server__echo");
    assert!(registered.definition.parameters["properties"]["text"].is_object());

    let echoed = client
        .call_tool("echo", serde_json::json!({ "text": "hello mcp" }))
        .await
        .expect("tools/call echo");
    assert!(echoed.contains("hello mcp"));

    let boom = client
        .call_tool("boom", serde_json::json!({}))
        .await
        .expect("tools/call boom returns a result");
    assert!(boom.contains("kaboom"));

    // An unknown tool is a JSON-RPC error, surfaced as `Err`, never a silent empty success.
    let err = client
        .call_tool("nope", serde_json::json!({}))
        .await
        .expect_err("unknown tool must error");
    assert!(err.to_string().contains("unknown tool"), "{err}");

    client.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Capability gap #2 — optional isolated counterfactual probe
// ---------------------------------------------------------------------------------------------

/// Gate that records the workspace root of every check, rejects the first one, approves the rest.
struct RecordingRejectOnceGate {
    roots: std::sync::Mutex<Vec<PathBuf>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl RecordingRejectOnceGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            roots: std::sync::Mutex::new(Vec::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn probed_isolated_worktree(&self) -> bool {
        self.roots
            .lock()
            .unwrap()
            .iter()
            .any(|root| root.to_string_lossy().contains("counterfactual-"))
    }
}

impl CompletionGate for RecordingRejectOnceGate {
    fn check<'a>(
        &'a self,
        ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        self.roots.lock().unwrap().push(ctx.workspace_root.clone());
        let first = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                Ok(GateOutcome::Rejected {
                    feedback: "verification command `cargo check` exited with 101.\nSTDERR:\nerror: boom\n  --> src/a.rs:2:5\n".to_string(),
                })
            } else {
                Ok(GateOutcome::Approved)
            }
        })
    }
}

/// Build a runner that edits `src/a.rs` once then stops, so the gate sees a real mutation with a
/// distinct `revision_before` to probe.
fn probe_provider() -> Arc<kroma_core::provider::FakeProvider> {
    use kroma_core::messages::{AssistantTurn, FinishReason, ToolCall};
    use kroma_core::types::ToolCallId;
    let tool_turn = |id: &str, args: &str| AssistantTurn {
        content: None,
        reasoning_blocks: Vec::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new(id),
            name: "edit".to_string(),
            raw_arguments: args.to_string(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    };
    let stop_turn = |text: &str| AssistantTurn {
        content: Some(text.to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: None,
    };
    Arc::new(kroma_core::provider::FakeProvider::new(vec![
        tool_turn(
            "call_a",
            r#"{"path":"src/a.rs","edits":[{"old_text":"    1","new_text":"    42"}]}"#,
        ),
        stop_turn("done"),
        stop_turn("done again"),
    ]))
}

async fn run_probe_scenario(
    tag: &str,
    config: kroma_core::attribution::CounterfactualProbeConfig,
    approval: Arc<dyn kroma_core::attribution::ProbeApproval>,
) -> (PathBuf, Arc<RecordingRejectOnceGate>) {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::events::EventLogger;
    use kroma_core::revision::SnapshotStore;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine};

    let root = temp_root(tag);
    let ws_dir = root.join("ws");
    let state_dir = root.join("state");
    fs::create_dir_all(ws_dir.join("src")).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(ws_dir.join("src/a.rs"), "pub fn a() -> u32 {\n    1\n}\n").unwrap();

    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let journal = SessionJournal::create(state_dir.join("sessions"), &ws_dir).unwrap();
    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws_dir, state_dir.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig::default();
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("edit a then stop", budget);
    let logger = EventLogger::new(cfg.run_id, Some(state_dir.clone()), false).unwrap();
    let gate = RecordingRejectOnceGate::new();
    let (_tx, rx) = watch::channel(false);

    let (summary, _logger) = AgentRunner::new(cfg, probe_provider(), runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .with_completion_gate(gate.clone())
        .with_counterfactual_probe(config, approval)
        .run(logger)
        .await;
    assert_eq!(
        summary.status,
        RunStatus::Completed,
        "{:?}",
        summary.terminal_reason
    );
    (ws_dir, gate)
}

#[tokio::test]
async fn counterfactual_probe_is_opt_in_approved_and_never_rolls_back() {
    use kroma_core::attribution::{AllowProbes, CounterfactualProbeConfig, DenyProbes};

    // Disabled (the default): no probe, and the recovery run leaves the edit in place.
    let (ws_disabled, gate_disabled) = run_probe_scenario(
        "probe-off",
        CounterfactualProbeConfig::disabled(),
        Arc::new(DenyProbes),
    )
    .await;
    assert!(!gate_disabled.probed_isolated_worktree());
    assert!(fs::read_to_string(ws_disabled.join("src/a.rs"))
        .unwrap()
        .contains("42"));

    // Enabled but the approval seam denies: still no probe.
    let (_, gate_denied) = run_probe_scenario(
        "probe-denied",
        CounterfactualProbeConfig::enabled_with(2),
        Arc::new(DenyProbes),
    )
    .await;
    assert!(!gate_denied.probed_isolated_worktree());

    // Enabled and approved: exactly one probe runs in an isolated worktree, and the live file is
    // NOT reverted (the probe must never roll the user's workspace back).
    let (ws_enabled, gate_enabled) = run_probe_scenario(
        "probe-on",
        CounterfactualProbeConfig::enabled_with(2),
        Arc::new(AllowProbes),
    )
    .await;
    assert!(
        gate_enabled.probed_isolated_worktree(),
        "the approved probe must run inside the isolated counterfactual worktree"
    );
    assert!(
        fs::read_to_string(ws_enabled.join("src/a.rs"))
            .unwrap()
            .contains("42"),
        "the live workspace must be untouched by the probe"
    );
}

// ---------------------------------------------------------------------------------------------
// Capability gap #3 — causal citation value + dead-end negative-sample compression
// ---------------------------------------------------------------------------------------------

fn cp_tool_result(
    call_id: &str,
    tool: &str,
    path: Option<&str>,
    outcome: kroma_core::messages::ToolOutcome,
    body: String,
) -> kroma_core::messages::TranscriptItem {
    use kroma_core::messages::{ToolResultMessage, ToolResultMeta, TranscriptItem};
    use kroma_core::types::ToolCallId;
    TranscriptItem::ToolResult(ToolResultMessage {
        call_id: ToolCallId::new(call_id),
        tool_name: tool.to_string(),
        outcome,
        content: Arc::from(body),
        meta: ToolResultMeta {
            relative_path: path.map(str::to_string),
            ..Default::default()
        },
    })
}

fn cp_assistant(
    call_ids: &[(&str, &str)],
    content: Option<&str>,
) -> kroma_core::messages::TranscriptItem {
    use kroma_core::messages::{AssistantTurn, FinishReason, ToolCall, TranscriptItem};
    use kroma_core::types::ToolCallId;
    TranscriptItem::Assistant(AssistantTurn {
        content: content.map(str::to_string),
        reasoning_blocks: Vec::new(),
        tool_calls: call_ids
            .iter()
            .map(|(id, name)| ToolCall {
                call_id: ToolCallId::new(*id),
                name: (*name).to_string(),
                raw_arguments: format!(r#"{{"path":"src/stable.rs","n":{}}}"#, id.len()),
            })
            .collect(),
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    })
}

/// A transcript where a large read of `src/stable.rs` is later explicitly cited by the assistant,
/// and a large *failed* bash attempt is a dead end.
fn causal_pruning_transcript() -> Vec<kroma_core::messages::TranscriptItem> {
    use kroma_core::messages::{ToolOutcome, TranscriptItem, UserMessage};
    // > MIN_COMPACTABLE_TOOL_BYTES so it *is* a compaction candidate, but <= CAUSAL_PIN_MAX_BYTES
    // so the citation pin can keep it without breaching the budget.
    let mut stable = String::from("[File: src/stable.rs | Lines: 1-40 of 40]\n");
    for i in 1..=40 {
        stable.push_str(&format!(
            "{:>6}\t    // stable line {} UNIQUE_STABLE_MARKER\n",
            i, i
        ));
    }
    let mut failed = String::from("[Command exit_code=101 | duration_ms=9]\n");
    for i in 1..=200 {
        failed.push_str(&format!("error: dead end symptom line {}\n", i));
    }
    let mut items = vec![TranscriptItem::User(UserMessage {
        content: "investigate".to_string(),
    })];
    // Turn 1: read the stable file, then run a failing command.
    items.push(cp_assistant(
        &[("call_stable", "read"), ("call_dead", "bash")],
        None,
    ));
    items.push(cp_tool_result(
        "call_stable",
        "read",
        Some("src/stable.rs"),
        ToolOutcome::Success,
        stable,
    ));
    items.push(cp_tool_result(
        "call_dead",
        "bash",
        None,
        ToolOutcome::Error {
            code: kroma_core::types::ErrorCode::IoError,
        },
        failed,
    ));
    // Filler turns push the first turn outside the protected tail window.
    for turn in 0..7 {
        let id = format!("call_fill_{}", turn);
        items.push(cp_assistant(&[(id.as_str(), "read")], None));
        items.push(cp_tool_result(
            &id,
            "read",
            Some(&format!("src/fill_{}.rs", turn)),
            ToolOutcome::Success,
            format!("     1\tpub fn fill_{}() -> usize {{ {} }}\n", turn, turn),
        ));
    }
    // Final turn explicitly cites the stable file (causal value signal).
    items.push(cp_assistant(
        &[],
        Some("The fix belongs in src/stable.rs based on what we read."),
    ));
    items
}

fn causal_pruning_budget() -> BudgetConfig {
    BudgetConfig {
        context_high_watermark_bytes: 8 * 1024,
        context_low_watermark_bytes: 4 * 1024,
        context_tail_window_bytes: 512,
        context_min_tail_turns: 1,
        ..Default::default()
    }
}

#[test]
fn causal_pruning_is_optional_and_preserves_evidence_cache_and_pairing_invariants() {
    use kroma_core::context_projector::{
        verify_tool_call_pairing, CompactionReason, ContextProjector,
    };

    // Default (pruning off): the failed attempt is compacted with the ordinary outline stub, and no
    // dead-end accounting is produced.
    let mut canonical = causal_pruning_transcript();
    let mut plain = ContextProjector::new();
    let projected_plain = plain
        .project(&mut canonical, &causal_pruning_budget(), 2048, None, 9)
        .expect("projection succeeds");
    assert_eq!(
        plain.stats().dead_end_compactions,
        0,
        "dead-end compression must be off by default"
    );
    let plain_dead = projected_plain
        .messages
        .iter()
        .find_map(|item| match item {
            kroma_core::messages::TranscriptItem::ToolResult(tr) if tr.tool_name == "bash" => {
                Some(tr.content.to_string())
            }
            _ => None,
        })
        .expect("the bash result");
    assert!(!plain_dead.contains("negative lesson"));

    // Enabled: the failed attempt becomes a bounded negative-lesson stub, the cited file stays
    // pinned, and the tool-call pairing / evidence invariants still hold.
    let mut canonical = causal_pruning_transcript();
    let mut pruned = ContextProjector::new().with_causal_pruning(true);
    assert!(pruned.causal_pruning());
    let projected = pruned
        .project(&mut canonical, &causal_pruning_budget(), 2048, None, 9)
        .expect("projection succeeds");

    assert!(
        pruned.stats().dead_end_compactions >= 1,
        "the failed attempt must be compressed into a negative lesson"
    );
    let dead = projected
        .messages
        .iter()
        .find_map(|item| match item {
            kroma_core::messages::TranscriptItem::ToolResult(tr) if tr.tool_name == "bash" => {
                Some(tr.content.to_string())
            }
            _ => None,
        })
        .expect("the bash result");
    assert!(dead.contains("Dead-end"), "{dead}");
    assert!(dead.contains("negative lesson"), "{dead}");
    assert!(
        dead.contains("source_entry_id="),
        "the raw failure must stay recoverable: {dead}"
    );
    let link = pruned
        .evidence_links()
        .iter()
        .find(|l| l.reason == CompactionReason::DeadEnd)
        .expect("a DeadEnd evidence link");
    assert!(
        pruned.evidence_lookup(&link.source_entry_id).is_some(),
        "raw dead-end output must be retrievable"
    );

    // The cited file stays unpinned-free: its raw content survives because the assistant cited it.
    let stable_survives = projected.messages.iter().any(|item| match item {
        kroma_core::messages::TranscriptItem::ToolResult(tr)
            if tr.tool_name == "read"
                && tr.meta.relative_path.as_deref() == Some("src/stable.rs") =>
        {
            tr.content.contains("UNIQUE_STABLE_MARKER")
        }
        _ => false,
    });
    assert!(
        stable_survives,
        "a file a later assistant turn cites must stay pinned"
    );

    // Pairing invariant survives the compression.
    assert!(verify_tool_call_pairing(&projected.messages));
    assert_eq!(
        kroma_core::context_projector::validate_tool_call_pairing(&projected.messages),
        Ok(())
    );
}

// ---------------------------------------------------------------------------------------------
// Bench ablation switches (capability gaps #2/#3/#5)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn bench_arms_expose_the_frontier_ablation_switches() {
    use kroma_core::bench::{
        builtin_smoke_suite, run_paired_benchmark, BenchRunOptions, BenchStrategyConfig,
        SyntheticBenchExecutor,
    };

    let root = temp_root("bench-ablation");
    let tasks = builtin_smoke_suite();
    assert!(
        !tasks.is_empty(),
        "the builtin smoke suite must not be empty"
    );

    let base = BenchStrategyConfig::single_agent();
    let mut causal_on = base.clone();
    causal_on.name = "causal_on".to_string();
    causal_on.causal_pruning = true;
    causal_on.counterfactual_probe = true;
    let mut strict_off = base.clone();
    strict_off.name = "strict_off".to_string();
    strict_off.require_verified = false;
    let mut deferred = base.clone();
    deferred.name = "on_demand".to_string();
    deferred.on_demand_tools = true;
    let arms = vec![base, causal_on, strict_off, deferred];

    let report = run_paired_benchmark(
        "ablation",
        &tasks,
        &arms,
        50_000,
        &root.join("scratch"),
        &SyntheticBenchExecutor,
        &BenchRunOptions::new(1, 11, false),
    )
    .await;

    let envelope = report.ablation_envelope();
    let lookup = |name: &str| -> (bool, bool, bool, bool) {
        let entry = envelope
            .iter()
            .find(|(arm, _, _, _, _)| arm == name)
            .unwrap_or_else(|| panic!("arm {} missing from the ablation envelope", name));
        (entry.1, entry.2, entry.3, entry.4)
    };
    assert_eq!(lookup("single_agent"), (false, false, true, false));
    assert_eq!(lookup("causal_on"), (true, true, true, false));
    assert_eq!(lookup("strict_off"), (false, false, false, false));
    assert_eq!(lookup("on_demand"), (false, false, true, true));

    let rendered = report.render();
    assert!(rendered.contains("ablation switches:"));
    assert!(rendered.contains("causal_pruning=on"));
    assert!(rendered.contains("require_verified=off"));
}

// ---------------------------------------------------------------------------------------------
// Capability gap #4 — durable async task control plane over RPC
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn rpc_exposes_the_durable_task_control_plane() {
    use kroma_core::provider::FakeProvider;
    use kroma_core::surface::{JsonRpcRequest, KromaRuntimeConfig, RpcDispatcher};

    let root = temp_root("tasks-rpc");
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::write(ws.join("README.md"), "# demo\n").unwrap();

    assert!(
        RpcDispatcher::supported_methods().contains(&"tasks.list"),
        "the control plane must advertise tasks.list"
    );
    assert!(RpcDispatcher::supported_methods().contains(&"tasks.control"));

    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("test", "stdio");
    let dispatcher = RpcDispatcher::new(cfg, Arc::new(FakeProvider::new(Vec::new()))).unwrap();

    let listed = dispatcher
        .handle_request(JsonRpcRequest::new(1, "tasks.list", serde_json::json!({})))
        .await;
    let body = serde_json::to_value(&listed).unwrap();
    assert!(body["error"].is_null(), "tasks.list must succeed: {body}");
    assert_eq!(body["result"]["count"], 0);

    // Controlling an unknown task is an explicit invalid-params error, never a silent success.
    let controlled = dispatcher
        .handle_request(JsonRpcRequest::new(
            2,
            "tasks.control",
            serde_json::json!({"action": "cancel", "task_id": "nope"}),
        ))
        .await;
    let body = serde_json::to_value(&controlled).unwrap();
    assert!(!body["error"].is_null(), "unknown task must error: {body}");
}

// ---------------------------------------------------------------------------------------------
// Capability gap #6 — on-demand tool schema mode
// ---------------------------------------------------------------------------------------------

/// Records the tool names of the first request, then stops.
struct ToolRecordingProvider {
    seen: std::sync::Mutex<Vec<String>>,
}

impl kroma_core::provider::ModelProvider for ToolRecordingProvider {
    fn complete<'a>(
        &'a self,
        req: kroma_core::provider::ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        kroma_core::provider::ProviderCompletion,
                        kroma_core::provider::ProviderError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        {
            let mut seen = self.seen.lock().unwrap();
            if seen.is_empty() {
                *seen = req.tools.iter().map(|t| t.name.clone()).collect();
            }
        }
        Box::pin(async move {
            use kroma_core::messages::{AssistantTurn, FinishReason};
            Ok(kroma_core::provider::ProviderCompletion {
                turn: AssistantTurn {
                    content: Some("done".to_string()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
                request_body_bytes: 64,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}

async fn first_request_tools(on_demand: bool) -> Vec<String> {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::env::LocalHostEnv;
    use kroma_core::events::EventLogger;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine};

    let root = temp_root(if on_demand {
        "ondemand-on"
    } else {
        "ondemand-off"
    });
    let ws = root.join("ws");
    let state = root.join("state");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&state).unwrap();

    let env = Arc::new(LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig::default();
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("say done", budget).with_on_demand_tools(on_demand);
    let logger = EventLogger::new(cfg.run_id, Some(state), false).unwrap();
    let provider = Arc::new(ToolRecordingProvider {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let (_tx, rx) = watch::channel(false);
    let (summary, _logger) = AgentRunner::new(cfg, provider.clone(), runtime, rx)
        .run(logger)
        .await;
    assert_eq!(summary.status, RunStatus::Completed);
    let tools = provider.seen.lock().unwrap().clone();
    tools
}

#[tokio::test]
async fn on_demand_tool_mode_pins_only_the_core_schema() {
    assert!(kroma_core::agent_loop::is_core_tool("read"));
    assert!(kroma_core::agent_loop::is_core_tool("discover_tools"));
    assert!(!kroma_core::agent_loop::is_core_tool("spawn_subagent"));

    let full = first_request_tools(false).await;
    assert!(
        full.contains(&"spawn_subagent".to_string()),
        "default mode pins every policy-permitted schema: {full:?}"
    );

    let deferred = first_request_tools(true).await;
    assert!(deferred.contains(&"read".to_string()));
    assert!(deferred.contains(&"discover_tools".to_string()));
    assert!(
        !deferred.contains(&"spawn_subagent".to_string()),
        "on-demand mode must not pin non-core schemas: {deferred:?}"
    );
    assert!(
        deferred.len() < full.len(),
        "on-demand mode must shrink the initial schema set ({} vs {})",
        deferred.len(),
        full.len()
    );
}

// ---- B11b F1: caller-side preflight precedes worktree materialization --------------------------

/// A backend that must never be reached: proves a refused delegation never dispatched a candidate.
struct PanicBackend;

impl SubagentBackend for PanicBackend {
    fn run_subagent(
        &self,
        _req: SubagentExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SubagentRunOutcome, String>> + Send + '_>> {
        Box::pin(async move { panic!("a refused delegation must never reach the backend") })
    }
}

/// B11b F1: a tournament whose parent has fail-closed policy hooks is refused *before* any
/// candidate worktree is materialized, so the rejection leaves no side effect.
#[tokio::test]
async fn tournament_with_parent_policy_hooks_is_refused_before_materializing() {
    let root = temp_root("hooks-preflight");
    let (workspace, store) = baseline_workspace(&root);
    let mut spec = tournament_spec(&root, 2, false);
    spec.parent_policy_hooks = ParentPolicyHooks::Required {
        hook_ids: vec!["b11b-deny-src-writes".to_string()],
    };
    let backend: Arc<dyn SubagentBackend> = Arc::new(PanicBackend);
    let gate: Arc<dyn CompletionGate> = Arc::new(SkippedGate);

    let err = run_worktree_tournament(spec, &workspace, &store, backend, None, &gate)
        .await
        .expect_err("a parent policy hook must refuse the tournament");
    assert!(err.contains("unsupported delegation"), "got: {err}");
    assert!(
        !root.join("worktrees").exists(),
        "no candidate worktree may be materialized for a refused delegation"
    );
}

/// B11b F1: a tournament whose parent mode cannot auto-authorize a `worker` is refused before
/// materialization too (the caller check, not only the backend one).
#[tokio::test]
async fn tournament_with_a_non_auto_writing_parent_is_refused_before_materializing() {
    let root = temp_root("decay-preflight");
    let (workspace, store) = baseline_workspace(&root);
    let mut spec = tournament_spec(&root, 2, false);
    spec.policy_mode = PolicyMode::Ask;
    let backend: Arc<dyn SubagentBackend> = Arc::new(PanicBackend);
    let gate: Arc<dyn CompletionGate> = Arc::new(SkippedGate);

    let err = run_worktree_tournament(spec, &workspace, &store, backend, None, &gate)
        .await
        .expect_err("an Ask parent must not race writing workers");
    assert!(err.contains("worker") && err.contains("ask"), "got: {err}");
    assert!(
        !root.join("worktrees").exists(),
        "no candidate worktree may be materialized for a refused delegation"
    );
}
