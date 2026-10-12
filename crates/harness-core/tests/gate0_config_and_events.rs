use harness_core::budget::{BudgetConfig, ConfigError};
use harness_core::events::{EventLogger, EventPayload};
use harness_core::tool::policy::PolicyMode;
use harness_core::tool::registry::ToolRegistry;
use harness_core::types::{EditDiagKind, EditMatchTier, RunId, RunStatus, TerminalReason, ToolCallId};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use uuid::Uuid;

#[test]
fn test_budget_config_defaults_and_validation() {
    let cfg = BudgetConfig::default();
    assert_eq!(cfg.max_model_turns, 50);
    assert_eq!(cfg.max_model_request_attempts, 60);
    assert_eq!(cfg.max_tool_calls, 80);
    assert_eq!(cfg.max_tool_calls_per_response, 16);
    assert_eq!(cfg.context_high_watermark_bytes, 1536 * 1024);
    assert_eq!(cfg.context_low_watermark_bytes, 768 * 1024);
    assert_eq!(cfg.max_total_tool_output_bytes, 4 * 1024 * 1024);
    assert_eq!(cfg.bash_head_bytes, 16 * 1024);
    assert_eq!(cfg.bash_tail_bytes, 48 * 1024);
    assert!(cfg.validate().is_ok());

    // Zero value rejected
    let mut bad_zero = cfg.clone();
    bad_zero.max_model_turns = 0;
    assert_eq!(
        bad_zero.validate(),
        Err(ConfigError::ZeroValue {
            field: "max_model_turns"
        })
    );

    // Zero context window tokens rejected
    let mut bad_tok = cfg.clone();
    bad_tok.model_context_window_tokens = Some(0);
    assert_eq!(
        bad_tok.validate(),
        Err(ConfigError::ZeroValue {
            field: "model_context_window_tokens"
        })
    );

    // Overflow rejected
    let mut bad_overflow = cfg.clone();
    bad_overflow.bash_head_bytes = usize::MAX;
    bad_overflow.bash_tail_bytes = 1024;
    assert_eq!(
        bad_overflow.validate(),
        Err(ConfigError::Overflow {
            field: "bash_head_bytes + bash_tail_bytes"
        })
    );

    // Invalid watermark ordering rejected
    let mut bad_wm = cfg.clone();
    bad_wm.context_low_watermark_bytes = bad_wm.context_high_watermark_bytes;
    assert!(matches!(
        bad_wm.validate(),
        Err(ConfigError::InvalidWatermarks { .. })
    ));
}

#[test]
fn test_tool_registry_ascii_order_and_policy_visibility() {
    let reg = ToolRegistry::default();
    let all_defs = reg.visible_definitions(PolicyMode::Ask);
    let names: Vec<&str> = all_defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["bash", "edit", "find", "grep", "ls", "read", "write"]
    );

    // Under ReadOnly policy, edit/write/bash are physically hidden from schema AND rejected by dispatcher
    let ro_defs = reg.visible_definitions(PolicyMode::ReadOnly);
    let ro_names: Vec<&str> = ro_defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(ro_names, vec!["find", "grep", "ls", "read"]);
    assert!(reg.lookup_for_dispatch("edit", PolicyMode::ReadOnly).is_err());
    assert!(reg.lookup_for_dispatch("write", PolicyMode::ReadOnly).is_err());
    assert!(reg.lookup_for_dispatch("bash", PolicyMode::ReadOnly).is_err());
    assert!(reg.lookup_for_dispatch("read", PolicyMode::ReadOnly).is_ok());

    // --tool-filter ablation switch
    let filtered = ToolRegistry::new(Some(vec![
        "write".into(),
        "read".into(),
        "edit".into(),
        "bash".into(),
    ]));
    let f_defs = filtered.visible_definitions(PolicyMode::Ask);
    let f_names: Vec<&str> = f_defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(f_names, vec!["bash", "edit", "read", "write"]);
    assert!(filtered.lookup_for_dispatch("grep", PolicyMode::Ask).is_err());

    // Non-interactive PolicyEngine hides tools that would be automatically denied:
    // - Non-interactive Ask: hides bash, edit, write -> only find, grep, ls, read
    // - Non-interactive AllowWrite: hides bash -> edit, find, grep, ls, read, write
    // - Non-interactive AllowShell: hides edit, write -> bash, find, grep, ls, read
    // - Non-interactive FullAutonomous: exposes all 7 tools
    use harness_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine};
    use std::sync::Arc;

    let pe_ask = PolicyEngine::new(PolicyMode::Ask, Arc::new(NonInteractiveApprovalHandler));
    let ask_non_tty: Vec<String> = reg
        .visible_definitions_for_engine(&pe_ask)
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(ask_non_tty, vec!["find", "grep", "ls", "read"]);

    let pe_aw = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let aw_non_tty: Vec<String> = reg
        .visible_definitions_for_engine(&pe_aw)
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(
        aw_non_tty,
        vec!["edit", "find", "grep", "ls", "read", "write"]
    );

    let pe_as = PolicyEngine::new(
        PolicyMode::AllowShell,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let as_non_tty: Vec<String> = reg
        .visible_definitions_for_engine(&pe_as)
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(as_non_tty, vec!["bash", "find", "grep", "ls", "read"]);

    let pe_full = PolicyEngine::new(
        PolicyMode::FullAutonomous,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let full_non_tty: Vec<String> = reg
        .visible_definitions_for_engine(&pe_full)
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(
        full_non_tty,
        vec!["bash", "edit", "find", "grep", "ls", "read", "write"]
    );
}

#[test]
fn test_event_logger_0600_permissions_and_resilient_degradation() {
    let tmp_dir = std::env::temp_dir().join(format!("harness-test-events-{}", Uuid::new_v4()));
    let run_id = RunId::new();

    let mut logger = EventLogger::new(run_id, Some(tmp_dir.clone()), false).unwrap();
    logger
        .log(EventPayload::RunStarted {
            prompt_hash: "0123456789abcdef".into(),
            tool_schema_hash: "fedcba9876543210".into(),
            static_prefix_bytes: 1800,
            policy: "ask".into(),
            budget_config: BudgetConfig::default(),
        })
        .unwrap();
    logger
        .log(EventPayload::ToolCompleted {
            call_id: ToolCallId::new("call_1"),
            tool_name: "edit".into(),
            relative_path: Some("src/lib.rs".into()),
            duration_ms: 4,
            truncated: false,
            exit_code: None,
            outcome: "success".into(),
            edit_match_tier: Some(EditMatchTier::Tier2TrailingWhitespace),
            diag_kind: Some(EditDiagKind::IndentationMismatch),
            compaction_reread_count: 1,
        })
        .unwrap();

    let meta = fs::metadata(logger.log_path()).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert!(!logger.is_degraded());

    // Test resilient degradation when state directory is not writable (e.g. points inside a regular file)
    let blocker_file = tmp_dir.join("blocker_file");
    fs::write(&blocker_file, b"not a directory").unwrap();
    let impossible_dir = blocker_file.join("subdir");

    let mut degraded_logger =
        EventLogger::new(RunId::new(), Some(impossible_dir.clone()), false).unwrap();
    assert!(degraded_logger.is_degraded());
    // Logging continues without panic or error in non-strict mode
    assert!(degraded_logger
        .log(EventPayload::RunTerminated {
            status: RunStatus::Completed,
            terminal_reason: TerminalReason::ModelStop,
            turns: 1,
            total_tool_calls: 0,
            total_input_tokens: 100,
            total_cached_input_tokens: 80,
            total_output_tokens: 20,
            cache_hit_ratio: 0.8,
            compaction_reread_count: 0,
            duration_ms: Duration::from_millis(10).as_millis() as u64,
            log_degraded: true,
            dropped_events: 1,
        })
        .is_ok());
    assert_eq!(degraded_logger.dropped_events(), 1);

    // In --strict-log mode, opening an impossible log path fails closed immediately
    let strict_res = EventLogger::new(RunId::new(), Some(impossible_dir), true);
    assert!(strict_res.is_err());

    let _ = fs::remove_dir_all(&tmp_dir);
}
