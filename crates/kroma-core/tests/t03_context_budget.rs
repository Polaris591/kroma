//! T03 (KR-03 / KR-07): the budget and the context window are decided by trusted sources, and the
//! whole request is checked before it is sent.
//!
//! Covered here:
//! * a workspace `.kroma.toml` may lower the host ceiling but never raise it (tokens, concurrency);
//! * capabilities are resolved with an explicit provenance, and an unknown model falls back to a
//!   conservative window instead of an invented spec;
//! * the estimator is conservative for CJK text and counts image payloads;
//! * a request that cannot fit the window is refused *before* transmission, with a structured
//!   event, and the tool schema is part of the calculation;
//! * a smaller window after a resume re-budgets instead of reusing the old watermarks.

use kroma_core::budget::{
    conservative_request_tokens, conservative_text_tokens, BudgetConfig, CapabilitySource,
    HostBudgetCeiling, ModelCapabilities, CONSERVATIVE_CONTEXT_WINDOW_TOKENS,
};
use kroma_core::config_file::{ConfigOverrides, KromaConfigFile, ResolvedConfig};
use kroma_core::messages::{AssistantTurn, FinishReason, TranscriptItem, UserMessage};
use kroma_core::types::TerminalReason;
use std::collections::BTreeMap;

fn resolve(user_toml: &str, workspace_toml: &str) -> ResolvedConfig {
    let user = KromaConfigFile::parse_str(user_toml).expect("user config parses");
    let workspace = KromaConfigFile::parse_str(workspace_toml).expect("workspace config parses");
    ResolvedConfig::resolve(
        &user,
        &workspace,
        &BTreeMap::new(),
        &ConfigOverrides::default(),
        None,
        std::path::PathBuf::from("/ws/.kroma.toml"),
    )
}

/// T03: a repository cannot raise its own token ceiling.
#[test]
fn workspace_cannot_raise_host_token_ceiling() {
    // Lowering is a legitimate preference.
    let lowered = resolve("max_tokens = 40000\n", "max_tokens = 10000\n");
    assert_eq!(lowered.max_tokens, Some(10000));
    assert!(lowered.clamped_workspace_keys.is_empty());

    // Raising is clamped to the host ceiling and reported.
    let raised = resolve("max_tokens = 40000\n", "max_tokens = 4000000\n");
    assert_eq!(
        raised.max_tokens,
        Some(40000),
        "a workspace must not be able to raise the host token ceiling"
    );
    assert!(
        raised
            .clamped_workspace_keys
            .iter()
            .any(|k| k.contains("max_tokens") && k.contains("4000000")),
        "the clamp must be visible: {:?}",
        raised.clamped_workspace_keys
    );

    // R04: with no owner ceiling, a workspace value is a preference that cannot authorize spend.
    // It is recorded and dropped, so `max_tokens` stays unset and the run has no envelope.
    let alone = resolve("", "max_tokens = 5000\n");
    assert_eq!(alone.max_tokens, None);
    assert_eq!(alone.owner_max_tokens, None);
    assert!(
        alone
            .clamped_workspace_keys
            .iter()
            .any(|k| k.contains("max_tokens") && k.contains("5000")),
        "a workspace-only spend authorization must be visible: {:?}",
        alone.clamped_workspace_keys
    );
}

/// R04: the trusted ceiling and its origin are kept apart from the effective cap, so only the
/// owner's declaration can mint a spend envelope.
#[test]
fn owner_ceiling_provenance_is_separate_from_the_workspace_preference() {
    use kroma_core::config_file::MaxTokensOrigin;

    let user_only = resolve("max_tokens = 40000\n", "max_tokens = 10000\n");
    assert_eq!(user_only.owner_max_tokens, Some(40000));
    assert_eq!(
        user_only.max_tokens_origin,
        Some(MaxTokensOrigin::UserConfig)
    );
    assert_eq!(user_only.max_tokens, Some(10000));

    let from_env = ResolvedConfig::resolve(
        &KromaConfigFile::default(),
        &KromaConfigFile::parse_str("max_tokens = 7000\n").unwrap(),
        &BTreeMap::from([("KROMA_MAX_TOKENS".to_string(), "9000".to_string())]),
        &ConfigOverrides::default(),
        None,
        std::path::PathBuf::from("/ws/.kroma.toml"),
    );
    assert_eq!(from_env.owner_max_tokens, Some(9000));
    assert_eq!(
        from_env.max_tokens_origin,
        Some(MaxTokensOrigin::Environment)
    );
    // An explicit per-invocation layer is the operator speaking now: a workspace can neither raise
    // nor quietly lower it.
    assert_eq!(from_env.max_tokens, Some(9000));
}

/// T03: and it cannot grant itself more parallel writers either.
#[test]
fn workspace_cannot_raise_host_concurrency() {
    let raised = resolve("max_subagents = 2\n", "max_subagents = 32\n");
    assert_eq!(raised.max_subagents, Some(2));
    assert!(
        raised
            .clamped_workspace_keys
            .iter()
            .any(|k| k.contains("max_subagents")),
        "the clamp must be visible: {:?}",
        raised.clamped_workspace_keys
    );

    let lowered = resolve("max_subagents = 8\n", "max_subagents = 1\n");
    assert_eq!(lowered.max_subagents, Some(1));
    assert!(lowered.clamped_workspace_keys.is_empty());
}

/// T03: capability resolution always says where the number came from.
#[test]
fn unknown_model_falls_back_conservatively_and_records_the_source() {
    let unknown = ModelCapabilities::resolve(None, None);
    assert_eq!(
        unknown.context_window_tokens,
        Some(CONSERVATIVE_CONTEXT_WINDOW_TOKENS)
    );
    assert_eq!(unknown.source, CapabilitySource::ConservativeFallback);
    assert_eq!(unknown.source.as_str(), "conservative_fallback");

    let configured = ModelCapabilities::resolve(Some(128_000), Some(8192));
    assert_eq!(configured.context_window_tokens, Some(128_000));
    assert_eq!(configured.max_output_tokens, Some(8192));
    assert_eq!(configured.source, CapabilitySource::Configured);
    // The output reservation never exceeds the provider cap.
    assert_eq!(configured.reserved_output_tokens(Some(99_999)), 8192);
    assert_eq!(configured.reserved_output_tokens(Some(1024)), 1024);

    let unbounded = ModelCapabilities::resolve(Some(0), None);
    assert_eq!(unbounded.context_window_tokens, None);
    assert_eq!(unbounded.source, CapabilitySource::Unbounded);
}

/// T03: `bytes / 4` is not a tokenizer — CJK must not be under-estimated, and images must count.
#[test]
fn cjk_and_image_budget_is_conservative() {
    let cjk = "你好世界，这是一段中文";
    let ascii = "abcdefghijklmnopqrstuvwxyz";
    let cjk_tokens = conservative_text_tokens(cjk);
    let naive = ((cjk.len() as u64) + 3) / 4;
    assert!(
        cjk_tokens > naive,
        "CJK must not be estimated with bytes/4: {cjk_tokens} vs {naive}"
    );
    // One token per character is the conservative ceiling for CJK.
    assert!(cjk_tokens >= cjk.chars().count() as u64);
    // ASCII stays on the familiar 4-bytes-per-token scale.
    assert_eq!(conservative_text_tokens(ascii), 7);

    // A request that carries an image payload is estimated larger than the same request without it.
    let mut with_image = vec![TranscriptItem::User(UserMessage {
        content: "look at this".to_string(),
    })];
    let text_only = conservative_request_tokens("system", &with_image, 0, 0);
    let meta = kroma_core::messages::ToolResultMeta {
        image_attachments: vec![kroma_core::messages::ImageAttachment {
            mime_type: "image/png".to_string(),
            base64_data: "A".repeat(4000).into(),
            width: Some(64),
            height: Some(64),
            byte_len: 3000,
        }],
        ..Default::default()
    };
    with_image.push(TranscriptItem::ToolResult(
        kroma_core::messages::ToolResultMessage {
            call_id: kroma_core::types::ToolCallId::new("call_img"),
            tool_name: "read".to_string(),
            outcome: kroma_core::messages::ToolOutcome::Success,
            content: "attached".into(),
            meta,
        },
    ));
    let with_image_tokens = conservative_request_tokens("system", &with_image, 0, 0);
    assert!(
        with_image_tokens > text_only + 900,
        "image payload bytes must be charged: {with_image_tokens} vs {text_only}"
    );
}

/// T03: the tool schema and the serialization overhead are part of the request estimate.
#[test]
fn task_contract_and_tool_schema_count_toward_context() {
    let items = vec![TranscriptItem::User(UserMessage {
        content: "hi".to_string(),
    })];
    let without = conservative_request_tokens("system", &items, 0, 0);
    let with_schema = conservative_request_tokens("system", &items, 40_000, 0);
    assert!(
        with_schema >= without + 9_000,
        "a 40 KB tool schema must add roughly 10k tokens: {without} -> {with_schema}"
    );
    let with_overhead = conservative_request_tokens("system", &items, 0, 4_000);
    assert!(with_overhead >= without + 900);
}

/// T03: a smaller window after a resume re-budgets instead of reusing the old watermarks.
#[test]
fn resume_and_switch_to_smaller_window_rebudgets() {
    let large = BudgetConfig {
        model_context_window_tokens: Some(200_000),
        ..Default::default()
    };
    let small = BudgetConfig {
        model_context_window_tokens: Some(16_000),
        ..Default::default()
    };
    let (large_high, large_low) = large.token_watermarks().expect("configured window");
    let (small_high, small_low) = small.token_watermarks().expect("configured window");
    assert!(small_high < large_high && small_low < large_low);
    assert_eq!(small_high, 12_000);
    assert_eq!(small_low, 7_200);

    // A run constructed with no window is re-budgeted to the conservative fallback, not left
    // unbounded: the resolved capability is what the projector uses.
    let fallback = ModelCapabilities::resolve(None, None);
    assert_eq!(
        fallback.context_window_tokens,
        Some(CONSERVATIVE_CONTEXT_WINDOW_TOKENS)
    );
    assert!(BudgetConfig {
        model_context_window_tokens: fallback.context_window_tokens,
        ..Default::default()
    }
    .token_watermarks()
    .is_some());
}

/// T03: the ceiling arithmetic itself.
#[test]
fn host_ceiling_clamps_and_reports() {
    let ceiling = HostBudgetCeiling {
        max_total_tokens: Some(1000),
        max_subagents: Some(3),
        context_window_tokens: Some(64_000),
    };
    assert_eq!(ceiling.clamp_tokens(Some(500)).effective, Some(500));
    assert!(!ceiling.clamp_tokens(Some(500)).clamped);
    assert_eq!(ceiling.clamp_tokens(Some(9999)).effective, Some(1000));
    assert!(ceiling.clamp_tokens(Some(9999)).clamped);
    assert_eq!(ceiling.clamp_subagents(Some(9)).effective, Some(3));
    assert_eq!(
        ceiling.clamp_window(Some(1_000_000)).effective,
        Some(64_000)
    );
}

// ---------------------------------------------------------------------------------------------
// Pre-send check
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_request_that_cannot_fit_the_window_is_never_sent() {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::events::EventLogger;
    use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
    use kroma_core::revision::SnapshotStore;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingProvider {
        calls: Arc<AtomicUsize>,
    }

    impl ModelProvider for CountingProvider {
        fn complete<'a>(
            &'a self,
            _req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(ProviderCompletion {
                    turn: AssistantTurn {
                        content: Some("should never be reached".to_string()),
                        reasoning_blocks: Vec::new(),
                        tool_calls: Vec::new(),
                        finish_reason: FinishReason::Stop,
                        usage: None,
                    },
                    request_body_bytes: 1,
                    latency_ms: 1,
                    ttft_ms: None,
                    attempts_used: 1,
                    reported_model: None,
                })
            })
        }
    }

    let root = std::env::temp_dir().join(format!("kroma-t03-window-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("lib.rs"), "pub fn a() {}\n").unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    });
    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    // 600 tokens: smaller than the default system prompt + tool schemas of a real run.
    let budget = BudgetConfig {
        model_context_window_tokens: Some(600),
        ..Default::default()
    };
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("do something", budget);
    let run_id = cfg.run_id;
    let logger = EventLogger::new(run_id, Some(state.clone()), true).unwrap();
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .run(logger)
        .await;

    assert_eq!(
        summary.terminal_reason,
        TerminalReason::ContextBudgetExceeded,
        "a request that cannot fit the window must be refused, not sent"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "zero provider calls: the check runs before transmission"
    );

    let log = std::fs::read_to_string(state.join(format!("{}.jsonl", run_id))).unwrap_or_default();
    assert!(
        log.contains("context_budget_exceeded"),
        "the refusal must be recorded structurally; log:\n{log}"
    );
    assert!(
        log.contains("configured"),
        "the window source must be recorded; log:\n{log}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Compaction before the first over-window request
// ---------------------------------------------------------------------------------------------

/// T03: on a `--resume` with no restored provider usage, a small window must compact *before* the
/// first request is transmitted, not after an over-window request has already gone out.
#[tokio::test]
async fn small_context_compacts_before_first_overflow_request() {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::events::EventLogger;
    use kroma_core::messages::{AssistantTurn, FinishReason, ToolCall};
    use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
    use kroma_core::revision::SnapshotStore;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
    use kroma_core::types::ToolCallId;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    /// Records the conservative token size of every request it is asked to send.
    struct RecordingProvider {
        turns: Mutex<std::collections::VecDeque<AssistantTurn>>,
        seen_tokens: Arc<Mutex<Vec<u64>>>,
    }

    impl ModelProvider for RecordingProvider {
        fn complete<'a>(
            &'a self,
            req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            let tools_bytes: usize = req
                .tools
                .iter()
                .map(|t| t.name.len() + t.description.len() + 256)
                .sum();
            self.seen_tokens
                .lock()
                .unwrap()
                .push(conservative_request_tokens(
                    &req.system_prompt,
                    &req.messages,
                    tools_bytes,
                    1024,
                ));
            let turn = self
                .turns
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted turn");
            Box::pin(async move {
                Ok(ProviderCompletion {
                    turn,
                    request_body_bytes: 1,
                    latency_ms: 1,
                    ttft_ms: None,
                    attempts_used: 1,
                    reported_model: None,
                })
            })
        }
    }

    let root = std::env::temp_dir().join(format!("kroma-t03-compact-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    // Sixteen distinct sources, each below the `read` output cap, so run 1 builds a history far
    // larger than the window used by the resumed run.
    const SOURCES: usize = 16;
    for idx in 0..SOURCES {
        let mut body = String::new();
        for line in 0..1_000 {
            body.push_str(&format!(
                "pub fn filler_{idx}_{line}() -> usize {{ {line} }}\n"
            ));
        }
        std::fs::write(ws.join(format!("filler_{idx}.rs")), body).unwrap();
    }

    let read_turns: Vec<AssistantTurn> = (0..SOURCES)
        .map(|idx| AssistantTurn {
            content: None,
            reasoning_blocks: Vec::new(),
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new(format!("call_read_{idx}")),
                name: "read".to_string(),
                raw_arguments: format!(r#"{{"path":"filler_{idx}.rs"}}"#),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        })
        .chain(std::iter::once(AssistantTurn {
            content: Some("done".to_string()),
            reasoning_blocks: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: None,
        }))
        .collect();

    let wide = BudgetConfig {
        // KR-B04: declare the wide window explicitly. The fallback path now compacts against the
        // conservative window, so leaving this unset would compact the fixture history and the
        // "history exceeds the window" precondition below would no longer hold.
        model_context_window_tokens: Some(1_000_000),
        max_model_turns: 32,
        max_tool_calls: 64,
        max_total_tool_output_bytes: 4 * 1024 * 1024,
        context_high_watermark_bytes: 4 * 1024 * 1024,
        context_low_watermark_bytes: 2 * 1024 * 1024,
        ..Default::default()
    };
    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::ReadOnly,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, wide.clone(), None);
    let cfg = AgentRunConfig::new("read the sources", wide.clone());
    let logger = EventLogger::new(cfg.run_id, Some(state.clone()), true).unwrap();
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let wal_path = journal.path().to_path_buf();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let (first, _logger) = AgentRunner::new(
        cfg,
        Arc::new(RecordingProvider {
            turns: Mutex::new(read_turns.into_iter().collect()),
            seen_tokens: Arc::new(Mutex::new(Vec::new())),
        }),
        runtime,
        rx,
    )
    .with_session_journal(journal)
    .run(logger)
    .await;
    assert_eq!(first.status, kroma_core::types::RunStatus::Completed);

    let history_bytes: usize = first
        .transcript
        .iter()
        .map(TranscriptItem::estimated_bytes)
        .sum();

    // The restored history is ~730 KB (~245k conservative tokens), so the first request of the
    // resumed run is over-window *before* compaction and fits after it. The window is deliberately
    // larger than Kroma's own static prefix: this test is about compaction, not about a window that
    // no prompt could ever fit.
    const WINDOW: u64 = 200_000;
    assert!(
        kroma_core::budget::conservative_bytes_tokens(history_bytes) > WINDOW,
        "the fixture history must exceed the window: {history_bytes} bytes"
    );

    let narrow = BudgetConfig {
        model_context_window_tokens: Some(WINDOW),
        max_model_turns: 16,
        max_tool_calls: 32,
        ..Default::default()
    };
    let resumed_journal = SessionJournal::open(&wal_path).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::ReadOnly,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, narrow.clone(), None);
    let cfg = AgentRunConfig::new("summarise", narrow);
    let run_id = cfg.run_id;
    let logger = EventLogger::new(run_id, Some(state.clone()), true).unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let (second, _logger) = AgentRunner::new(
        cfg,
        Arc::new(RecordingProvider {
            turns: Mutex::new(
                vec![AssistantTurn {
                    content: Some("summarised".to_string()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                }]
                .into_iter()
                .collect(),
            ),
            seen_tokens: Arc::clone(&seen),
        }),
        runtime,
        rx,
    )
    .with_session_journal(resumed_journal)
    .with_snapshot_store(Arc::new(SnapshotStore::new(root.join("snapshots"))))
    .run(logger)
    .await;

    let seen = seen.lock().unwrap().clone();
    assert!(
        !seen.is_empty(),
        "the resumed run must have sent a request (terminal={:?}, history_bytes={history_bytes})",
        second.terminal_reason
    );
    assert!(
        seen[0] <= WINDOW,
        "the first resumed request was over-window ({} tokens > {WINDOW}): compaction must run \
         before transmission",
        seen[0]
    );
    assert_eq!(second.status, kroma_core::types::RunStatus::Completed);

    let log = std::fs::read_to_string(state.join(format!("{}.jsonl", run_id))).unwrap_or_default();
    assert!(
        log.contains("ContextCompacted"),
        "compaction must be recorded structurally; log:\n{log}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// T03: a workspace may not select a model outside the operator's allow-list, and it may not
/// declare the allow-list either.
#[test]
fn workspace_model_outside_allowlist_is_refused() {
    let user = KromaConfigFile::parse_str(
        "allowed_models = \"cheap-model,mid-model\"\nmodel = \"mid-model\"\n",
    )
    .expect("user config parses");
    assert_eq!(
        user.allowed_models.as_deref(),
        Some("cheap-model,mid-model")
    );

    // Outside the allow-list: refused, and the operator's model is what runs.
    let outside = KromaConfigFile::parse_str("model = \"expensive-model\"\n").unwrap();
    let resolved = ResolvedConfig::resolve(
        &user,
        &outside,
        &BTreeMap::new(),
        &ConfigOverrides::default(),
        None,
        std::path::PathBuf::from("/ws/.kroma.toml"),
    );
    assert_eq!(resolved.model.as_deref(), Some("mid-model"));
    assert!(
        resolved
            .refused_workspace_keys
            .iter()
            .any(|k| k.contains("expensive-model")),
        "the refusal must be visible: {:?}",
        resolved.refused_workspace_keys
    );
    assert_eq!(
        resolved.allowed_models.as_deref(),
        Some(&["cheap-model".to_string(), "mid-model".to_string()][..])
    );

    // Inside the allow-list: honoured as before.
    let inside = KromaConfigFile::parse_str("model = \"cheap-model\"\n").unwrap();
    let resolved = ResolvedConfig::resolve(
        &user,
        &inside,
        &BTreeMap::new(),
        &ConfigOverrides::default(),
        None,
        std::path::PathBuf::from("/ws/.kroma.toml"),
    );
    assert_eq!(resolved.model.as_deref(), Some("cheap-model"));
    assert!(resolved.refused_workspace_keys.is_empty());

    // With no allow-list configured nothing is refused.
    let no_list = KromaConfigFile::parse_str("model = \"anything\"\n").unwrap();
    let resolved = ResolvedConfig::resolve(
        &KromaConfigFile::default(),
        &no_list,
        &BTreeMap::new(),
        &ConfigOverrides::default(),
        None,
        std::path::PathBuf::from("/ws/.kroma.toml"),
    );
    assert_eq!(resolved.model.as_deref(), Some("anything"));
    assert!(resolved.allowed_models.is_none());

    // The allow-list itself is operator-only: a workspace cannot widen it.
    let hostile = KromaConfigFile::parse_str("allowed_models = \"*\"\n").unwrap();
    let resolved = ResolvedConfig::resolve(
        &user,
        &hostile,
        &BTreeMap::new(),
        &ConfigOverrides::default(),
        None,
        std::path::PathBuf::from("/ws/.kroma.toml"),
    );
    assert!(
        resolved
            .rejected_workspace_keys
            .iter()
            .any(|k| k == "allowed_models"),
        "a workspace must not be able to declare the allow-list: {:?}",
        resolved.rejected_workspace_keys
    );
    assert_eq!(
        resolved.allowed_models.as_deref(),
        Some(&["cheap-model".to_string(), "mid-model".to_string()][..])
    );
}

/// T03: a live configuration change must not expand a grant that was already approved.
#[test]
fn live_reconfiguration_does_not_expand_existing_task_grant() {
    use kroma_core::subagents::{CanonicalTournamentBudget, CanonicalTournamentIntent};

    let intent_for = |budget: &BudgetConfig| CanonicalTournamentIntent {
        task: "fix the failing add()".to_string(),
        candidates: 2,
        strategies: vec!["smallest change".to_string(), "defensive fix".to_string()],
        allowed_paths: Some(vec!["src/**".to_string()]),
        max_turns: Some(6),
        timeout_ms: 30_000,
        disallow_test_tampering: true,
        protected_paths: vec!["tests/**".to_string()],
        require_verified: true,
        verification_policy_source: "host_strict".to_string(),
        budget: CanonicalTournamentBudget::from_config(budget),
        predictive_check: true,
    };

    let narrow = BudgetConfig {
        max_total_tokens: Some(1_000),
        max_model_turns: 4,
        ..Default::default()
    };
    let approved = intent_for(&narrow);
    let approved_digest = approved.digest();

    // The operator widens the ceiling while the approval is outstanding.
    let widened = BudgetConfig {
        max_total_tokens: Some(5_000),
        max_model_turns: 32,
        ..Default::default()
    };
    let live = intent_for(&widened);

    assert_ne!(
        live.digest(),
        approved_digest,
        "a widened live configuration must not silently produce the approved intent"
    );
    // The approved object still carries the numbers the human saw.
    assert_eq!(approved.budget.max_total_tokens, Some(1_000));
    assert_eq!(approved.budget.max_model_turns, 4);
    assert_eq!(live.budget.max_total_tokens, Some(5_000));

    // And the widened intent is not covered by the grant that was issued for the narrow one.
    let approver = std::sync::Arc::new(kroma_core::tool::policy::NonInteractiveApprovalHandler);
    let engine = kroma_core::tool::policy::PolicyEngine::new(
        kroma_core::tool::policy::PolicyMode::FullAutonomous,
        approver,
    );
    let scope = kroma_core::tool::policy::AuthScope {
        entry: kroma_core::tool::policy::AuthEntry::Tournament,
        tool_name: "spawn_subagent".to_string(),
        run_id: None,
        workspace_root: std::path::PathBuf::from("/ws"),
        revision: Some(kroma_core::revision::RevisionId::new(
            "r-00000000000000000000000000000003",
        )),
    };
    assert!(
        engine.consume_grant(&scope, &live.digest()).is_err(),
        "a grant approved for the narrow budget must not authorize the widened one"
    );
}

/// T03: the resolved configuration must *show* where a ceiling came from and what it clamped or
/// refused, so an operator never has to guess why a value did not apply.
#[test]
fn config_source_display_shows_the_ceiling_and_every_clamp() {
    let user = KromaConfigFile::parse_str(
        "allowed_models = \"cheap-model,mid-model\"\nmax_tokens = 40000\nmodel = \"mid-model\"\n",
    )
    .unwrap();
    let workspace =
        KromaConfigFile::parse_str("max_tokens = 4000000\nmodel = \"expensive-model\"\n").unwrap();
    let resolved = ResolvedConfig::resolve(
        &user,
        &workspace,
        &BTreeMap::new(),
        &ConfigOverrides::default(),
        None,
        std::path::PathBuf::from("/ws/.kroma.toml"),
    );
    let summary = resolved.redacted_summary();
    assert!(
        summary.contains("allowed_models:   cheap-model, mid-model"),
        "{summary}"
    );
    assert!(
        summary.contains("clamped:"),
        "the clamp must be visible: {summary}"
    );
    assert!(summary.contains("4000000"), "{summary}");
    assert!(
        summary.contains("refused:"),
        "the refusal must be visible: {summary}"
    );
    assert!(summary.contains("expensive-model"), "{summary}");

    // Unrestricted runs say so instead of printing nothing.
    let plain = ResolvedConfig::resolve(
        &KromaConfigFile::default(),
        &KromaConfigFile::default(),
        &BTreeMap::new(),
        &ConfigOverrides::default(),
        None,
        std::path::PathBuf::from("/ws/.kroma.toml"),
    );
    assert!(plain
        .redacted_summary()
        .contains("allowed_models:   (unrestricted)"));
}

// ---------------------------------------------------------------------------------------------
// KR-B04: an unconfigured model window must still enforce the hard pre-send check.
//
// Before the fix the whole-request window check ran only when the capability source was
// `Configured`. With no `model_context_window_tokens`, `ModelCapabilities::resolve` still yields
// `Some(CONSERVATIVE_CONTEXT_WINDOW_TOKENS)` but the source is `ConservativeFallback`, so the
// check was skipped entirely and an oversized request went to the provider.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn unconfigured_window_still_refuses_an_oversized_request() {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::events::EventLogger;
    use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
    use kroma_core::revision::SnapshotStore;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingProvider {
        calls: Arc<AtomicUsize>,
    }

    impl ModelProvider for CountingProvider {
        fn complete<'a>(
            &'a self,
            _req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(ProviderCompletion {
                    turn: AssistantTurn {
                        content: Some("should never be reached".to_string()),
                        reasoning_blocks: Vec::new(),
                        tool_calls: Vec::new(),
                        finish_reason: FinishReason::Stop,
                        usage: None,
                    },
                    request_body_bytes: 1,
                    latency_ms: 1,
                    ttft_ms: None,
                    attempts_used: 1,
                    reported_model: None,
                })
            })
        }
    }

    let root = std::env::temp_dir().join(format!("kroma-b04-window-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("lib.rs"), "pub fn a() {}\n").unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    });
    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    // No window configured: capability source is ConservativeFallback (32768 tokens).
    let budget = BudgetConfig::default();
    assert_eq!(
        kroma_core::budget::ModelCapabilities::resolve(None, None).source,
        kroma_core::budget::CapabilitySource::ConservativeFallback,
        "this test only makes sense on the fallback path"
    );

    // ~60000 CJK chars is ~180 KB and well past the 32768-token fallback window.
    let oversized_prompt = "中".repeat(60_000);
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new(&oversized_prompt, budget);
    let run_id = cfg.run_id;
    let logger = EventLogger::new(run_id, Some(state.clone()), true).unwrap();
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .run(logger)
        .await;

    assert_eq!(
        summary.terminal_reason,
        TerminalReason::ContextBudgetExceeded,
        "a fallback-window run must refuse an oversized request instead of sending it"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "zero provider calls: the hard window check must run before transmission"
    );

    let log = std::fs::read_to_string(state.join(format!("{}.jsonl", run_id))).unwrap_or_default();
    assert!(
        log.contains("context_budget_exceeded"),
        "the refusal must be recorded structurally; log:\n{log}"
    );
    assert!(
        log.contains("conservative_fallback"),
        "the fallback capability source must be recorded; log:\n{log}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// KR-B01: the tool set that is measured must be the tool set that is sent.
//
// Activated (discovered) tools were appended to the request *after* the estimate and the window
// check, so a run whose static schema fit the window could still transmit an over-window request
// once discovery contributed a large schema. This test activates a deliberately huge tool schema
// and requires the request to be refused before any provider call.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn activated_tool_schema_is_measured_before_the_request_is_sent() {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::events::EventLogger;
    use kroma_core::extensions::{
        CustomToolContext, ExtensionManifest, ExtensionTrustLevel, ScopedExtension,
        ScopedToolDefinition,
    };
    use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
    use kroma_core::revision::SnapshotStore;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::definition::{ToolDefinition, ToolMetadata};
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
    use kroma_core::types::EffectClass;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingProvider {
        calls: Arc<AtomicUsize>,
    }

    impl ModelProvider for CountingProvider {
        fn complete<'a>(
            &'a self,
            _req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(ProviderCompletion {
                    turn: AssistantTurn {
                        content: Some("should never be reached".to_string()),
                        reasoning_blocks: Vec::new(),
                        tool_calls: Vec::new(),
                        finish_reason: FinishReason::Stop,
                        usage: None,
                    },
                    request_body_bytes: 1,
                    latency_ms: 1,
                    ttft_ms: None,
                    attempts_used: 1,
                    reported_model: None,
                })
            })
        }
    }

    let root = std::env::temp_dir().join(format!("kroma-b01-tools-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("lib.rs"), "pub fn a() {}\n").unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    });
    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    // 20k tokens fits the static prompt + schemas comfortably but not an extra ~50k-token schema.
    let budget = BudgetConfig {
        model_context_window_tokens: Some(20_000),
        ..Default::default()
    };
    let runtime = build_default_runtime(env, policy, budget.clone(), None);

    let manifest = ExtensionManifest {
        name: "b01-big-schema".to_string(),
        version: "1.0.0".to_string(),
        api_version: "1.0.0".to_string(),
        capabilities: vec!["tools".to_string()],
        trust_level: ExtensionTrustLevel::OperatorTrusted,
        source: "test-fixture".to_string(),
        checksum: None,
    };
    let mut ext = ScopedExtension::new(manifest);
    ext.tools.push(ScopedToolDefinition {
        definition: ToolDefinition {
            name: "b01_big_schema_tool".to_string(),
            // ~200 KB of ASCII: roughly 50k conservative tokens on its own.
            description: "d".repeat(200_000),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }),
        },
        metadata: ToolMetadata {
            effect_class: EffectClass::ReadOnly,
            default_timeout_sec: 5,
            inline_output_cap_bytes: 1024,
        },
        handler: Arc::new(|_: &str, _: &CustomToolContext<'_>| {
            kroma_core::tools::ToolExecutionOutput::success(
                String::new(),
                kroma_core::messages::ToolResultMeta::default(),
            )
        }),
    });
    runtime
        .registry()
        .register_extension(ext)
        .expect("register big-schema extension");
    let activated = runtime.activate_tools(&["b01_big_schema_tool".to_string()]);
    assert!(
        activated.iter().any(|(_, r)| r.is_ok()),
        "the fixture tool must actually be activated: {activated:?}"
    );

    // On-demand mode keeps non-core schemas out of the initial prefix, so activation really
    // is the moment the tool set grows -- which is the sequence this test is about.
    let cfg = AgentRunConfig::new("do something small", budget).with_on_demand_tools(true);
    let run_id = cfg.run_id;
    let logger = EventLogger::new(run_id, Some(state.clone()), true).unwrap();
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let (summary, _logger) = AgentRunner::new(cfg, provider, runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .run(logger)
        .await;

    assert_eq!(
        summary.terminal_reason,
        TerminalReason::ContextBudgetExceeded,
        "an activated tool schema that pushes the request over the window must be refused"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "zero provider calls: the window check must see the activated tool set"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// KR-B02: the outbound request is sealed by identity.
//
// The runner recomputes the identity from the actual `ProviderRequest` and refuses to transmit a
// request that differs from the one it budgeted for. That seal is only meaningful if the identity
// is sensitive to every wire-relevant field, which is what this pins down.
// ---------------------------------------------------------------------------------------------

#[test]
fn request_identity_changes_when_the_tool_schema_changes() {
    use kroma_core::provenance::request_identity;
    use kroma_core::tool::definition::ToolDefinition;

    let base_tool = ToolDefinition {
        name: "probe".to_string(),
        description: "original".to_string(),
        parameters: serde_json::json!({"type": "object"}),
    };
    let base = request_identity("sys", std::slice::from_ref(&base_tool), &[]);

    let mut changed_description = base_tool.clone();
    changed_description.description = "original ".to_string();
    assert_ne!(
        base,
        request_identity("sys", &[changed_description], &[]),
        "a changed tool description must change the identity"
    );

    let mut changed_parameters = base_tool.clone();
    changed_parameters.parameters = serde_json::json!({"type": "object", "required": ["x"]});
    assert_ne!(
        base,
        request_identity("sys", &[changed_parameters], &[]),
        "changed tool parameters must change the identity"
    );

    let other = ToolDefinition {
        name: "a".to_string(),
        description: "x".to_string(),
        parameters: serde_json::json!({}),
    };
    assert_eq!(
        request_identity("s", &[base_tool.clone(), other.clone()], &[]),
        request_identity("s", &[other, base_tool], &[]),
        "reordering the same tool set must not change the identity"
    );
}

// ---------------------------------------------------------------------------------------------
// KR-B06: the audit log explains what was sent without storing what the user typed.
//
// `RunStarted` carries `prompt_hash`, never the prompt. This pins that down for the whole log: a
// marker placed in the task must not appear anywhere in the run's JSONL.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_event_log_does_not_contain_the_prompt_in_plaintext() {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::events::EventLogger;
    use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
    use kroma_core::revision::SnapshotStore;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    struct OneShotProvider;

    impl ModelProvider for OneShotProvider {
        fn complete<'a>(
            &'a self,
            _req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            Box::pin(async move {
                Ok(ProviderCompletion {
                    turn: AssistantTurn {
                        content: Some("done".to_string()),
                        reasoning_blocks: Vec::new(),
                        tool_calls: Vec::new(),
                        finish_reason: FinishReason::Stop,
                        usage: None,
                    },
                    request_body_bytes: 1,
                    latency_ms: 1,
                    ttft_ms: None,
                    attempts_used: 1,
                    reported_model: None,
                })
            })
        }
    }

    const MARKER: &str = "KR-B06-PLAINTEXT-MARKER-9f3a";

    let root = std::env::temp_dir().join(format!("kroma-b06-log-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::ReadOnly,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig {
        model_context_window_tokens: Some(200_000),
        ..Default::default()
    };
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new(format!("investigate {MARKER} please"), budget);
    let run_id = cfg.run_id;
    let logger = EventLogger::new(run_id, Some(state.clone()), true).unwrap();
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let (_summary, _logger) = AgentRunner::new(cfg, Arc::new(OneShotProvider), runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .run(logger)
        .await;

    let log = std::fs::read_to_string(state.join(format!("{}.jsonl", run_id))).unwrap_or_default();
    assert!(
        !log.is_empty(),
        "the run must produce a structured log to audit"
    );
    assert!(
        log.contains("RunStarted"),
        "the log must record that the run started; log:\n{log}"
    );
    assert!(
        !log.contains(MARKER),
        "the prompt must not be stored in plaintext; log:\n{log}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// KR-L05: a provider that answers as a different model must be recorded, not silently accepted.
//
// The task asks that both the executor and the model's actual capability be verifiable. The run
// dispatches to one model and the provider reports another; the answer is still useful, so the run
// continues, but the discrepancy lands in the structured log instead of vanishing.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_model_identity_mismatch_is_recorded_not_silently_accepted() {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::events::EventLogger;
    use kroma_core::provider::{
        DispatchBinding, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
    };
    use kroma_core::revision::SnapshotStore;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    struct MismatchProvider;

    impl ModelProvider for MismatchProvider {
        fn dispatch_binding(&self) -> Option<DispatchBinding> {
            Some(DispatchBinding {
                endpoint: "http://127.0.0.1:9".to_string(),
                model: "requested-model".to_string(),
            })
        }

        fn complete<'a>(
            &'a self,
            _req: ProviderRequest,
            _budget: &'a BudgetConfig,
            _cancel_rx: tokio::sync::watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>>
        {
            Box::pin(async move {
                Ok(ProviderCompletion {
                    turn: AssistantTurn {
                        content: Some("done".to_string()),
                        reasoning_blocks: Vec::new(),
                        tool_calls: Vec::new(),
                        finish_reason: FinishReason::Stop,
                        usage: None,
                    },
                    request_body_bytes: 1,
                    latency_ms: 1,
                    ttft_ms: None,
                    attempts_used: 1,
                    reported_model: Some("a-different-model".to_string()),
                })
            })
        }
    }

    let root = std::env::temp_dir().join(format!("kroma-l05-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::ReadOnly,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let budget = BudgetConfig {
        model_context_window_tokens: Some(200_000),
        ..Default::default()
    };
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("say done", budget);
    let run_id = cfg.run_id;
    let logger = EventLogger::new(run_id, Some(state.clone()), true).unwrap();
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let store = Arc::new(SnapshotStore::new(root.join("snapshots")));
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let (_summary, _logger) = AgentRunner::new(cfg, Arc::new(MismatchProvider), runtime, rx)
        .with_session_journal(journal)
        .with_snapshot_store(store)
        .run(logger)
        .await;

    let log = std::fs::read_to_string(state.join(format!("{}.jsonl", run_id))).unwrap_or_default();
    assert!(
        log.contains("ModelRequestCompleted"),
        "the completed request must be logged; log:\n{log}"
    );
    assert!(
        log.contains("\"model_identity_matches\":false"),
        "a provider answering as a different model must be recorded as a mismatch; log:\n{log}"
    );
    assert!(
        log.contains("a-different-model"),
        "the reported model must be kept so the mismatch is auditable"
    );

    let _ = std::fs::remove_dir_all(&root);
}
