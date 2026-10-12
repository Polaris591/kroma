use harness_core::agent_loop::build_default_runtime;
use harness_core::budget::BudgetConfig;
use harness_core::env::LocalHostEnv;
use harness_core::messages::{ToolCall, ToolOutcome};
use harness_core::tool::output_buffer::StreamLineCollector;
use harness_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use harness_core::tools::bash::execute_bash;
use harness_core::types::{ErrorCode, ToolCallId};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

#[test]
fn test_stream_line_collector_3_byte_incomplete_utf8_across_chunks() {
    let mut collector = StreamLineCollector::new(2048);
    let mut collected = String::new();

    // "你好，世界！\n" in UTF-8: each Chinese char is 3 bytes.
    // Feed 1 byte or 2 bytes at a time to split every 3-byte UTF-8 character across chunk boundaries!
    let text = "你好，世界！Rust 核心系统测试\n";
    for chunk in text.as_bytes().chunks(2) {
        collector.push_chunk(chunk, |seg| collected.push_str(seg));
    }
    collector.finish(|seg| collected.push_str(seg));

    assert_eq!(collected, text);
    assert!(!collected.contains('\u{FFFD}'));

    // Verify HeadTailBuffer head_sealed prevents ASCII segments from jumping ahead of multi-byte UTF-8 at head_cap boundary
    use harness_core::tool::output_buffer::HeadTailBuffer;
    let mut ht = HeadTailBuffer::new(10, 50, 100, 1024, None);
    ht.push_str("123456789"); // 9 bytes in head_buf (1 byte remaining)
    ht.push_str("中\n");      // 3-byte UTF-8 char doesn't fit in 1 remaining byte -> seals head_buf and goes to tail
    ht.push_str("A\n");       // 2-byte ASCII must NOT jump into head_buf ahead of "中\n"!
    let (out, truncated) = ht.finalize();
    assert!(!truncated);
    assert_eq!(out, "123456789中\nA\n");
}

#[tokio::test]
async fn test_bash_16kib_head_48kib_rolling_tail_and_4mib_spill_file() {
    let tmp_ws = std::env::temp_dir().join(format!("harness-bash-ht-{}", Uuid::new_v4()));
    let tmp_state = std::env::temp_dir().join(format!("harness-state-ht-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();
    fs::create_dir_all(&tmp_state).unwrap();

    // Generate ~300 KiB of output where line 1 is "HEAD_START_MARKER_12345"
    // and the very last line is "FAILED: assertion X == Y"
    let cmd_json = serde_json::json!({
        "command": "echo 'HEAD_START_MARKER_12345'; i=0; while [ $i -lt 4500 ]; do echo \"compiling module_number_${i} with detailed progress output padding 0123456789abcdef\"; i=$((i+1)); done; echo 'FAILED: assertion X == Y'; exit 101"
    })
    .to_string();

    let budget = BudgetConfig::default();
    let (_tx, rx) = watch::channel(false);
    let call_id = ToolCallId::new("call_300k");

    let out = execute_bash(
        &tmp_ws,
        &tmp_state,
        &call_id,
        &cmd_json,
        &budget,
        rx,
        Duration::from_millis(200),
    )
    .await;

    assert_eq!(out.meta.exit_code, Some(101));
    assert!(out.meta.truncated);
    assert!(
        out.content.contains("HEAD_START_MARKER_12345"),
        "16 KiB Head must be preserved"
    );
    assert!(
        out.content.contains("Omitted"),
        "Middle omission notice with spill file path must be present"
    );
    assert!(
        out.content.contains("FAILED: assertion X == Y"),
        "48 KiB Rolling Tail must preserve the final failure line"
    );

    let spill_file = tmp_state.join("tool-call_300k.log");
    assert!(spill_file.exists(), "Spill log file must be written");
    let spill_meta = fs::metadata(&spill_file).unwrap();
    assert_eq!(spill_meta.permissions().mode() & 0o777, 0o600);
    assert!(spill_meta.len() > 250 * 1024);

    let _ = fs::remove_dir_all(&tmp_ws);
    let _ = fs::remove_dir_all(&tmp_state);
}

#[tokio::test]
async fn test_bash_200ms_pipe_drain_timeout_prevents_grandchild_deadlock() {
    let tmp_ws = std::env::temp_dir().join(format!("harness-bash-drain-{}", Uuid::new_v4()));
    let tmp_state = std::env::temp_dir().join(format!("harness-state-drain-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();
    fs::create_dir_all(&tmp_state).unwrap();

    // Spawn a background grandchild process that inherits stdout/stderr FDs and sleeps for 30 seconds,
    // while the main shell prints "MAIN_PROCESS_DONE" and exits immediately.
    // Without the 200ms pipe drain hard timeout, Tokio would wait 30s for pipe EOF!
    let cmd_json = serde_json::json!({
        "command": "(sleep 30 &); echo 'MAIN_PROCESS_DONE'; exit 0"
    })
    .to_string();

    let budget = BudgetConfig::default();
    let (_tx, rx) = watch::channel(false);
    let start = Instant::now();

    let out = execute_bash(
        &tmp_ws,
        &tmp_state,
        &ToolCallId::new("call_drain"),
        &cmd_json,
        &budget,
        rx,
        Duration::from_millis(200),
    )
    .await;

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "200ms drain timeout must prevent waiting 30s for grandchild pipe EOF, took {:?}",
        elapsed
    );
    assert!(out.outcome.is_success());
    assert!(out.content.contains("MAIN_PROCESS_DONE"));

    let _ = fs::remove_dir_all(&tmp_ws);
    let _ = fs::remove_dir_all(&tmp_state);
}

#[tokio::test]
async fn test_bash_process_group_cancellation_flood_cap_env_sanitization_and_zero_spawn_on_deny() {
    let tmp_ws = std::env::temp_dir().join(format!("harness-bash-sup-{}", Uuid::new_v4()));
    let tmp_state = std::env::temp_dir().join(format!("harness-state-sup-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();
    fs::create_dir_all(&tmp_state).unwrap();

    // 1. Sensitive environment variable blacklist stripping
    std::env::set_var("HARNESS_API_KEY", "super_secret_harness_key");
    std::env::set_var("OPENAI_API_KEY", "super_secret_openai_key");
    std::env::set_var("MY_CUSTOM_SECRET", "top_secret_val");
    std::env::set_var("SAFE_BUILD_FLAG", "keep_this_value");

    let budget = BudgetConfig::default();
    let (_tx, rx) = watch::channel(false);
    let env_out = execute_bash(
        &tmp_ws,
        &tmp_state,
        &ToolCallId::new("call_env"),
        r#"{"command":"env"}"#,
        &budget,
        rx,
        Duration::from_millis(100),
    )
    .await;
    assert!(env_out.outcome.is_success());
    assert!(!env_out.content.contains("super_secret_harness_key"));
    assert!(!env_out.content.contains("super_secret_openai_key"));
    assert!(!env_out.content.contains("top_secret_val"));
    assert!(env_out.content.contains("SAFE_BUILD_FLAG=keep_this_value"));

    // 2. OutputFloodCap (set low cap in budget for fast test) kills process group immediately
    let mut flood_budget = BudgetConfig::default();
    flood_budget.max_process_output_observed_bytes = 128 * 1024; // 128 KiB flood cap for fast test
    flood_budget.bash_spill_bytes = 64 * 1024;
    let (_tx2, rx2) = watch::channel(false);
    let flood_out = execute_bash(
        &tmp_ws,
        &tmp_state,
        &ToolCallId::new("call_flood"),
        r#"{"command":"yes 'flood_line_0123456789abcdef0123456789abcdef'"}"#,
        &flood_budget,
        rx2,
        Duration::from_millis(100),
    )
    .await;
    assert_eq!(
        flood_out.outcome,
        ToolOutcome::Error {
            code: ErrorCode::OutputFloodCap
        }
    );
    assert_eq!(
        flood_out.meta.terminated_by.as_deref(),
        Some("OutputFloodCap")
    );

    // 3. Process group cancellation kills child and grandchild processes cleanly
    let pid_file = tmp_ws.join("sub_pid.txt");
    let cancel_cmd = serde_json::json!({
        "command": format!("sleep 60 & echo $! > {}; wait", pid_file.display())
    })
    .to_string();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = cancel_tx.send(true);
    });
    let cancel_out = execute_bash(
        &tmp_ws,
        &tmp_state,
        &ToolCallId::new("call_cancel"),
        &cancel_cmd,
        &budget,
        cancel_rx,
        Duration::from_millis(100),
    )
    .await;
    assert_eq!(
        cancel_out.outcome,
        ToolOutcome::Error {
            code: ErrorCode::NotExecutedCancelled
        }
    );
    if let Ok(pid_str) = fs::read_to_string(&pid_file) {
        if let Ok(sub_pid) = pid_str.trim().parse::<i32>() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let alive = unsafe { libc::kill(sub_pid, 0) == 0 };
            assert!(!alive, "Subprocess PID {} must be terminated with process group", sub_pid);
        }
    }

    // 4. Unauthorized `Ask` in non-TTY performs zero spawns
    let marker = tmp_ws.join("must_not_spawn.txt");
    let env = Arc::new(LocalHostEnv::new(&tmp_ws, &tmp_state).unwrap());
    let policy = PolicyEngine::new(PolicyMode::Ask, Arc::new(NonInteractiveApprovalHandler));
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let (_tx3, rx3) = watch::channel(false);
    let (deny_msg, _) = runtime
        .execute_call(
            &ToolCall {
                call_id: ToolCallId::new("call_unauth"),
                name: "bash".into(),
                raw_arguments: serde_json::json!({
                    "command": format!("touch {}", marker.display())
                })
                .to_string(),
            },
            rx3,
        )
        .await;
    assert_eq!(
        deny_msg.outcome,
        ToolOutcome::Error {
            code: ErrorCode::PolicyDenied
        }
    );
    assert!(!marker.exists(), "Unauthorized bash must have 0 spawns");

    // 5. NormalExit background process sweep: `sh -c "sleep 30 >/dev/null 2>&1 & echo $!; exit 0"`
    //    must kill the background grandchild PID via `-pgid` cleanup before returning!
    let (_tx5, rx5) = watch::channel(false);
    let bg_out = execute_bash(
        &tmp_ws,
        &tmp_state,
        &ToolCallId::new("call_normal_exit_bg"),
        r#"{"command":"sleep 30 >/dev/null 2>&1 & echo $!; exit 0"}"#,
        &budget,
        rx5,
        Duration::from_millis(100),
    )
    .await;
    assert!(bg_out.outcome.is_success());
    let bg_pid: i32 = bg_out
        .content
        .lines()
        .skip(1)
        .find_map(|l| l.trim().parse::<i32>().ok())
        .expect("should parse background sleep PID from stdout");
    assert!(bg_pid > 0);
    let bg_alive = unsafe { libc::kill(bg_pid, 0) == 0 };
    assert!(
        !bg_alive,
        "Background grandchild PID {} must be killed by NormalExit process group cleanup",
        bg_pid
    );

    let _ = fs::remove_dir_all(&tmp_ws);
    let _ = fs::remove_dir_all(&tmp_state);
}
