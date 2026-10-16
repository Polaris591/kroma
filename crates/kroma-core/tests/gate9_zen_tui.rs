//! Gate 9 — `v1.5.0` Design D ("Kroma Zen") terminal presentation layer tests.
//!
//! Verifies:
//! 1. `zen_theme_is_default_and_the_banner_is_borderless`
//! 2. `theme_command_switches_between_zen_and_classic`
//! 3. `theme_command_rejects_unknown_theme`
//! 4. `config_get_and_set_report_and_swap_ui_theme`
//! 5. `zen_status_view_prefixes_status_dot_and_keeps_stable_fields`
//! 6. `zen_diff_view_renders_header_and_preserves_listing`
//! 7. `zen_prompt_turn_emits_telemetry_footer`
//! 8. `repl_prompt_uses_borderless_prefix`
//! 9. `theme_renderer_public_api_degrades_without_color`

use kroma_core::budget::BudgetConfig;
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use kroma_core::surface::{
    InteractiveRepl, JsonRpcNotification, JsonRpcRequest, KromaRuntimeConfig, RpcDispatcher,
};
use kroma_core::theme::{color_enabled_with, DiffKind, DiffLine, StatusDot, Theme, ThemeRenderer};
use kroma_core::tool::policy::PolicyMode;
use serde_json::json;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::sync::watch;
use uuid::Uuid;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-gate9-{}-{}", tag, Uuid::new_v4()));
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
    let cfg = KromaRuntimeConfig::new(&ws, &state, PolicyMode::FullAutonomous)
        .with_surface("repl", "tty");
    let dispatcher = RpcDispatcher::new(cfg, provider).unwrap();
    (InteractiveRepl::new(dispatcher), ws, root)
}

#[tokio::test]
async fn zen_theme_is_default_and_the_banner_is_borderless() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("zen-default", provider);

    assert_eq!(repl.dispatcher().active_theme(), Theme::Zen);
    let banner = repl.render_status_banner().await;

    // Design D chrome: brand diamond header, no ASCII boxes.
    assert!(banner.contains("◆ kroma 1.5.0"), "banner: {banner}");
    assert!(
        !banner.contains("==="),
        "banner must be borderless: {banner}"
    );
    // Stable facts for scripts and parity tests remain present.
    assert!(banner.contains("model:"), "banner: {banner}");
    assert!(banner.contains("thinking:"), "banner: {banner}");
    assert!(banner.contains("protocol:"), "banner: {banner}");
    assert!(
        banner.contains("policy: allow-write+allow-shell"),
        "banner: {banner}"
    );
    assert!(
        banner.contains(&format!(
            "Kroma v{} Interactive REPL",
            kroma_core::version()
        )),
        "banner: {banner}"
    );
    // Under `cargo test` stdout is not a TTY, so colour must be disabled and no ESC may leak.
    assert!(!banner.contains('\x1b'), "banner leaked ESC: {banner:?}");

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn theme_command_switches_between_zen_and_classic() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("theme-switch", provider);

    let show = repl.handle_line("/theme").await.unwrap();
    assert!(show.output.contains("theme: zen"), "got: {}", show.output);

    let classic = repl.handle_line("/theme classic").await.unwrap();
    assert!(
        classic.output.contains("theme -> classic"),
        "got: {}",
        classic.output
    );
    assert_eq!(repl.dispatcher().active_theme(), Theme::Classic);
    let banner = repl.render_status_banner().await;
    assert!(
        banner.contains("=== Kroma v1.5.0 Interactive REPL ==="),
        "classic banner: {banner}"
    );

    let zen = repl.handle_line("/ui zen").await.unwrap();
    assert!(zen.output.contains("theme -> zen"), "got: {}", zen.output);
    assert!(zen.output.contains("Design D"), "got: {}", zen.output);
    assert_eq!(repl.dispatcher().active_theme(), Theme::Zen);
    let banner = repl.render_status_banner().await;
    assert!(banner.contains("◆ kroma 1.5.0"), "zen banner: {banner}");
    assert!(
        !banner.contains("==="),
        "zen banner must be borderless: {banner}"
    );

    // `/help` advertises the new command.
    let help = repl.handle_line("/help").await.unwrap();
    assert!(help.output.contains("/theme"), "help: {}", help.output);

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn theme_command_rejects_unknown_theme() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("theme-bad", provider);

    let out = repl.handle_line("/theme neon").await.unwrap();
    assert!(out.output.contains("unknown theme"), "got: {}", out.output);
    assert_eq!(repl.dispatcher().active_theme(), Theme::Zen);

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn config_get_and_set_report_and_swap_ui_theme() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("theme-config", provider);

    let got = repl
        .dispatcher()
        .handle_request(JsonRpcRequest::new(1, "config.get", json!({})))
        .await;
    let val = got.result.unwrap();
    assert_eq!(val.get("ui_theme").and_then(|v| v.as_str()), Some("zen"));

    let set = repl
        .dispatcher()
        .handle_request(JsonRpcRequest::new(
            2,
            "config.set",
            json!({ "ui_theme": "classic" }),
        ))
        .await;
    let val = set.result.unwrap();
    assert_eq!(
        val.get("ui_theme").and_then(|v| v.as_str()),
        Some("classic")
    );
    assert_eq!(repl.dispatcher().active_theme(), Theme::Classic);

    let bad = repl
        .dispatcher()
        .handle_request(JsonRpcRequest::new(
            3,
            "config.set",
            json!({ "ui_theme": "neon" }),
        ))
        .await;
    assert!(bad.error.is_some(), "invalid ui_theme must be rejected");

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn zen_status_view_prefixes_status_dot_and_keeps_stable_fields() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("zen-status", provider);

    let out = repl.handle_line("/status").await.unwrap();
    // Design D prefix line.
    assert!(out.output.contains("● status"), "got: {}", out.output);
    // Stable body lines preserved for scripts and parity tests.
    assert!(out.output.contains("session_id:"), "got: {}", out.output);
    assert!(out.output.contains("turn_count: 0"), "got: {}", out.output);
    assert!(out.output.contains("mutations:"), "got: {}", out.output);

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn zen_diff_view_renders_header_and_preserves_listing() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("zen-diff", provider);

    let out = repl.handle_line("/diff").await.unwrap();
    // With no revisions captured yet the view is still honest and borderless.
    assert!(
        out.output.contains("no workspace revisions captured yet"),
        "got: {}",
        out.output
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn zen_prompt_turn_emits_telemetry_footer() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("Done.")));
    let (repl, _ws, root) = repl_with_provider("zen-footer", provider);

    let out = repl.handle_line("Say done").await.unwrap();
    assert!(out.output.contains("Done."), "got: {}", out.output);
    // Stable machine line preserved.
    assert!(
        out.output.contains("[status=completed"),
        "got: {}",
        out.output
    );
    // Design D telemetry footer: self/child token spend, cache hit and revision.
    assert!(out.output.contains("self "), "got: {}", out.output);
    // P2 (`v1.5.0`): `self` is input + output tokens, not just input.
    assert!(out.output.contains("self 160 tok"), "got: {}", out.output);
    assert!(out.output.contains("child "), "got: {}", out.output);
    assert!(out.output.contains("tok"), "got: {}", out.output);
    assert!(out.output.contains("cache "), "got: {}", out.output);
    assert!(out.output.contains("rev "), "got: {}", out.output);
    // Design D run status dot.
    assert!(out.output.contains("● run"), "got: {}", out.output);

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn repl_prompt_uses_borderless_prefix() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("zen-prompt", provider);

    let mut out_buf = Vec::new();
    let input = std::io::Cursor::new(b"/quit\n".to_vec());
    repl.run_io_with_initial_prompt(BufReader::new(input), &mut out_buf, None)
        .await
        .unwrap();
    let rendered = String::from_utf8(out_buf).unwrap();
    assert!(rendered.contains("› "), "rendered: {rendered:?}");
    assert!(!rendered.contains("kroma> "), "rendered: {rendered:?}");
    assert!(
        rendered.contains("Session closed."),
        "rendered: {rendered:?}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn theme_switch_updates_repl_prompt_prefix_mid_session() {
    // P2 (`v1.5.0`): the REPL prompt prefix must follow a `/theme` switch made mid-session.
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("zen-prompt-switch", provider);

    let mut out_buf = Vec::new();
    let input = std::io::Cursor::new(b"/theme classic\n/quit\n".to_vec());
    repl.run_io_with_initial_prompt(BufReader::new(input), &mut out_buf, None)
        .await
        .unwrap();
    let rendered = String::from_utf8(out_buf).unwrap();
    // The startup prompt uses the Zen prefix; the next prompt must switch to the classic one.
    assert!(rendered.contains("\u{203a} "), "rendered: {rendered:?}");
    assert!(rendered.contains("kroma> "), "rendered: {rendered:?}");

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn config_set_accepts_stringified_adaptive_tournament() {
    // P2 (`v1.5.0`): `/config set adaptive_tournament 3` sends the value as a JSON string; the
    // kernel must parse it rather than silently rejecting the whole request.
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("adaptive-str", provider);

    let set = repl
        .dispatcher()
        .handle_request(JsonRpcRequest::new(
            1,
            "config.set",
            json!({ "adaptive_tournament": "3", "save": false }),
        ))
        .await;
    let val = set
        .result
        .expect("a stringified adaptive_tournament must be accepted");
    assert_eq!(
        val.get("adaptive_tournament").and_then(|v| v.as_u64()),
        Some(3)
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn short_rev_and_header_reject_control_characters() {
    // P2 (`v1.5.0`): an untrusted revision string must never reach the terminal as raw control
    // bytes, whether it is rendered directly or through the header/footer helpers.
    let hostile = "#\u{1b}[31mred\u{202e}";
    let plain = ThemeRenderer::plain(Theme::Zen);
    let header = plain.header("m", "high", "responses", 1.0, hostile, "/ws");
    let footer = plain.telemetry_footer(1, 0, 1.0, hostile);
    assert!(!header.contains('\u{1b}'), "header leaked ESC: {header:?}");
    assert!(!footer.contains('\u{1b}'), "footer leaked ESC: {footer:?}");
    assert!(
        !header.contains('\u{202e}'),
        "header leaked bidi override: {header:?}"
    );
}

#[test]
fn live_events_render_design_d_status_lines() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("unused")));
    let (repl, _ws, root) = repl_with_provider("zen-live", provider);
    let sid = repl.dispatcher().session_id();

    let note = |event_type: &str, payload: serde_json::Value| {
        JsonRpcNotification::new(
            "kroma/event",
            json!({
                "session_id": sid,
                "event_type": event_type,
                "payload": payload,
            }),
        )
    };

    let started = repl
        .render_live_event(&note(
            "tool_started",
            json!({ "tool_name": "read", "relative_path": "src/main.rs" }),
        ))
        .unwrap();
    assert!(started.contains("● read"), "got: {started}");
    assert!(started.contains("src/main.rs"), "got: {started}");

    let edit_started = repl
        .render_live_event(&note(
            "tool_started",
            json!({ "tool_name": "edit", "relative_path": "src/main.rs" }),
        ))
        .unwrap();
    assert!(edit_started.contains("● edit"), "got: {edit_started}");

    let completed = repl
        .render_live_event(&note(
            "tool_completed",
            json!({
                "tool_name": "edit",
                "relative_path": "src/main.rs",
                "duration_ms": 7,
                "outcome": "success",
                "edit_match_tier": "tier1_exact"
            }),
        ))
        .unwrap();
    assert!(completed.contains("● edit"), "got: {completed}");
    assert!(completed.contains("tier1_exact"), "got: {completed}");
    assert!(completed.contains("7ms"), "got: {completed}");

    let model = repl
        .render_live_event(&note(
            "model_request_completed",
            json!({
                "turn": 1,
                "input_tokens": 1200,
                "cached_input_tokens": 1000,
                "output_tokens": 40,
                "prefix_cache_hit_ratio": 0.83,
                "latency_ms": 420
            }),
        ))
        .unwrap();
    assert!(model.contains("● thinking"), "got: {model}");
    assert!(model.contains("cache 83.0%"), "got: {model}");

    let denied = repl
        .render_live_event(&note(
            "policy_decision",
            json!({ "tool_name": "bash", "decision": "deny" }),
        ))
        .unwrap();
    assert!(denied.contains("● policy"), "got: {denied}");

    let check = repl
        .render_live_event(&note(
            "predictive_check",
            json!({ "path": "src/main.rs", "status": "ok", "issue_count": 0, "duration_ms": 1 }),
        ))
        .unwrap();
    assert!(check.contains("● check"), "got: {check}");

    let terminated = repl
        .render_live_event(&note(
            "run_terminated",
            json!({ "status": "completed", "turns": 2, "total_tool_calls": 3 }),
        ))
        .unwrap();
    assert!(terminated.contains("● run"), "got: {terminated}");
    assert!(
        !terminated.contains("● gate"),
        "a run-termination line must not be labelled as the completion gate: {terminated}"
    );

    // Events from another session are ignored, as are non-event notifications.
    let other = JsonRpcNotification::new(
        "kroma/event",
        json!({
            "session_id": "some-other-session",
            "event_type": "ToolStarted",
            "payload": { "tool_name": "read" }
        }),
    );
    assert!(repl.render_live_event(&other).is_none());
    let unrelated = JsonRpcNotification::new("approval/resolved", json!({}));
    assert!(repl.render_live_event(&unrelated).is_none());
    let unknown = repl.render_live_event(&note("surface_started", json!({})));
    assert!(unknown.is_none());

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn streaming_turn_emits_live_status_lines_before_summary() {
    let provider = FnProvider::new(|_| Ok(assistant_stop("Done streaming.")));
    let (repl, _ws, root) = repl_with_provider("zen-stream", provider);

    let mut out = Vec::new();
    let mut input = BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));
    let step = repl
        .run_turn_streaming("Say done", &mut input, &mut out)
        .await
        .unwrap();
    let rendered = String::from_utf8(out).unwrap();
    assert!(
        rendered.contains("●"),
        "expected live status lines: {rendered:?}"
    );
    assert!(
        rendered.contains("thinking") || rendered.contains("gate"),
        "{rendered:?}"
    );
    assert!(
        step.output.contains("Done streaming."),
        "got: {}",
        step.output
    );
    assert!(step.output.contains("self "), "got: {}", step.output);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn theme_renderer_public_api_degrades_without_color() {
    // Colour decision is a pure function of (NO_COLOR, TERM, is_tty).
    assert!(color_enabled_with(None, Some("xterm-256color"), true));
    assert!(!color_enabled_with(Some("1"), Some("xterm-256color"), true));
    assert!(!color_enabled_with(None, Some("dumb"), true));
    assert!(!color_enabled_with(None, None, false));

    let plain = ThemeRenderer::plain(Theme::Zen);
    let header = plain.header(
        "deepseek",
        "high",
        "responses",
        99.6,
        "0b92f1ab",
        "/opt/kroma",
    );
    assert!(header.starts_with("◆ kroma 1.5.0"), "header: {header}");
    assert!(!header.contains('\x1b'));
    assert_eq!(plain.prompt_prefix(), "› ");
    assert!(plain.hairline(6).chars().all(|c| c == '-'));

    let colored = ThemeRenderer::new(Theme::Zen, true);
    assert!(colored
        .header("m", "high", "responses", 1.0, "r", "/ws")
        .contains("\x1b[38;2;"));
    assert_eq!(colored.hairline(4).matches('━').count(), 4);

    // Status dots map to the four-colour spectrum.
    assert!(colored
        .status_line(StatusDot::Thinking, "thinking", "x")
        .contains("\x1b[38;2;168;85;247m"));
    assert!(colored
        .status_line(StatusDot::Tool, "read", "x")
        .contains("\x1b[38;2;56;189;248m"));
    assert!(colored
        .status_line(StatusDot::Success, "edit", "x")
        .contains("\x1b[38;2;52;211;153m"));
    assert!(colored
        .status_line(StatusDot::Alert, "policy", "x")
        .contains("\x1b[38;2;251;191;36m"));

    // Borderless inline diff preview.
    let lines = vec![
        DiffLine {
            kind: DiffKind::Context,
            line_number: Some(1),
            text: "keep();".into(),
        },
        DiffLine {
            kind: DiffKind::Removed,
            line_number: Some(2),
            text: "old();".into(),
        },
        DiffLine {
            kind: DiffKind::Added,
            line_number: Some(2),
            text: "new();".into(),
        },
    ];
    let preview = colored.diff_preview(&lines);
    assert!(preview.contains("+    2 new();"), "preview: {preview:?}");
    assert!(preview.contains("-    2 old();"), "preview: {preview:?}");
    assert!(!preview.contains('╭') && !preview.contains('│'));

    // Unified-diff parsing.
    let parsed = ThemeRenderer::plain(Theme::Zen)
        .preview_from_unified_diff("@@ -1 +1 @@\n-old();\n+new();\n");
    assert!(parsed.contains("-      old();"), "parsed: {parsed:?}");
    assert!(parsed.contains("+      new();"), "parsed: {parsed:?}");

    // Theme parsing.
    assert_eq!(Theme::parse("zen").unwrap(), Theme::Zen);
    assert_eq!(Theme::parse("CLASSIC").unwrap(), Theme::Classic);
    assert!(Theme::parse("neon").is_err());
    assert_eq!(Theme::default(), Theme::Zen);
}
