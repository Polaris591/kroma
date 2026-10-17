//! repl surface of the multi-surface runtime (split from `surface.rs`).

use super::*;
use std::collections::VecDeque;

/// Bracketed-paste markers a terminal wraps around a paste burst (`ESC[200~` / `ESC[201~`).
const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";
/// Maximum number of submitted prompts kept in the in-memory REPL history.
const REPL_HISTORY_LIMIT: usize = 100;
/// Number of `/history` entries listed when no count is given.
const REPL_HISTORY_DEFAULT_SHOWN: usize = 20;
/// Prompt shown while the REPL is still accumulating a multi-line submission.
const CONTINUATION_PROMPT: &str = "... ";

/// One complete user submission assembled by the REPL input loop.
///
/// `literal_prompt` is set for a bracketed paste, a `"""` block, or a backslash-continued
/// submission: that text is dispatched as a single prompt even when it begins with `/`.
struct Submission {
    text: String,
    literal_prompt: bool,
}

/// Outcome of resolving a `!N` / `!!` history recall.
enum HistoryRecall {
    /// The input was not a history recall and must be dispatched unchanged.
    NotRecall,
    /// The input named a history entry; dispatch this text instead.
    Text(String),
    /// The input named an out-of-range entry; print this error and execute nothing.
    Error(String),
}

/// P6 (`v1.5.2`): what the run is currently waiting on, rendered as one bounded status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitingState {
    /// Waiting on the provider to return a model response.
    Provider,
    /// A tool is running; carries the tool name.
    Tool(String),
    /// An operator approval is pending; carries the tool name.
    Approval(String),
}

/// True when `line` ends with an unescaped trailing backslash (an odd-length run of `\`).
fn ends_with_unescaped_backslash(line: &str) -> bool {
    line.chars().rev().take_while(|c| *c == '\\').count() % 2 == 1
}

/// Remove the single trailing continuation backslash from `line`.
fn strip_continuation_backslash(line: &str) -> &str {
    line.strip_suffix('\\').unwrap_or(line)
}

/// Split the bracketed-paste markers out of one input line.
///
/// Returns the line with any marker removed, whether a start marker was present, and whether an
/// end marker was present. A stray end marker is removed rather than forwarded to the model.
fn split_paste_markers(line: &str) -> (String, bool, bool) {
    let has_start = line.contains(BRACKETED_PASTE_START);
    let has_end = line.contains(BRACKETED_PASTE_END);
    if !has_start && !has_end {
        return (line.to_string(), false, false);
    }
    let cleaned = line
        .replace(BRACKETED_PASTE_START, "")
        .replace(BRACKETED_PASTE_END, "");
    (cleaned, has_start, has_end)
}

/// The waiting state implied by a live `kroma/event`, if any.
///
/// Only events that change what the run is *blocked on* map to a state; the streaming loop renders
/// a line only when the state actually changes, so nothing is printed per event type.
fn waiting_state_for_event(note: &JsonRpcNotification) -> Option<WaitingState> {
    if note.method != "kroma/event" {
        return None;
    }
    let event_type = note.params.get("event_type").and_then(Value::as_str)?;
    let key = event_type.to_ascii_lowercase().replace('_', "");
    let payload = note.params.get("payload")?;
    let tool_name = || {
        payload
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match key.as_str() {
        "modelrequeststarted" => Some(WaitingState::Provider),
        "toolstarted" => Some(WaitingState::Tool(tool_name())),
        "approvalrequested" => Some(WaitingState::Approval(tool_name())),
        _ => None,
    }
}

/// Render the waiting line only when `state` differs from `last`, updating `last` in place.
fn waiting_line_if_changed(
    state: WaitingState,
    last: &mut Option<WaitingState>,
    render: impl FnOnce(&WaitingState) -> String,
) -> Option<String> {
    if last.as_ref() == Some(&state) {
        return None;
    }
    let line = render(&state);
    *last = Some(state);
    Some(line)
}

/// Output of a single interactive REPL input line (`InteractiveRepl::handle_line`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplStepOutcome {
    pub output: String,
    pub should_exit: bool,
}

/// FLAGS cell of one `/tasks` row: `challenge(<reason>)`, `pending_merge`, both, or `-`.
///
/// `pending_merge` is tri-state on the wire: a JSON `true`/`false` is taken literally (a `false`
/// must not read as "the field is present"), any other non-null value counts as set, and a missing
/// or `null` field means unset.
pub fn format_task_flags(task: &Value) -> String {
    let challenge = task.get("challenge").and_then(Value::as_str);
    let pending_merge = match task.get("pending_merge") {
        Some(Value::Bool(b)) => *b,
        Some(v) => !v.is_null(),
        None => false,
    };
    match (challenge, pending_merge) {
        (Some(c), true) => format!("challenge({}) + merge", sanitize_terminal_text(c)),
        (Some(c), false) => format!("challenge({})", sanitize_terminal_text(c)),
        (None, true) => "pending_merge".to_string(),
        (None, false) => "-".to_string(),
    }
}

/// Stateless interactive REPL / TUI consumer (`kroma repl` / `kroma tui` / `--interactive`).
///
/// Per Blueprint v2.0 Gate 6 Invariant #3, `InteractiveRepl` holds zero private session state:
/// every slash command and prompt turn is dispatched as a JSON-RPC 2.0 request to [`RpcDispatcher`].
pub struct InteractiveRepl {
    dispatcher: RpcDispatcher,
    /// P6 (`v1.5.2`): in-memory prompt history (last [`REPL_HISTORY_LIMIT`] entries).
    ///
    /// Deliberately never persisted: prompt text stays in this process only.
    history: Mutex<VecDeque<String>>,
}

impl InteractiveRepl {
    pub fn new(dispatcher: RpcDispatcher) -> Self {
        Self {
            dispatcher,
            history: Mutex::new(VecDeque::new()),
        }
    }

    pub fn dispatcher(&self) -> &RpcDispatcher {
        &self.dispatcher
    }

    /// P6 (`v1.5.2`): render a [`WaitingState`] as a single bounded, sanitized status line.
    pub fn render_waiting_state(&self, state: &WaitingState) -> String {
        let renderer = self.dispatcher.theme_renderer();
        let line = match state {
            WaitingState::Provider => {
                renderer.status_line(StatusDot::Thinking, "waiting", "provider")
            }
            WaitingState::Tool(tool) => renderer.status_line(
                StatusDot::Tool,
                "waiting",
                &format!("tool {}", sanitize_terminal_text(tool)),
            ),
            WaitingState::Approval(tool) => renderer.status_line(
                StatusDot::Alert,
                "waiting",
                &format!("approval {}", sanitize_terminal_text(tool)),
            ),
        };
        crate::theme::truncate_display(&line, crate::theme::terminal_width())
    }

    /// Render the top status banner by querying `session.status` via [`RpcDispatcher`].
    pub async fn render_status_banner(&self) -> String {
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(0, "session.status", json!({})))
            .await;
        let result = resp.result.unwrap_or_else(|| json!({}));
        let session_id = result
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let ws = result
            .get("workspace_root")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let policy = result
            .get("policy_mode")
            .and_then(Value::as_str)
            .unwrap_or("ask");
        let turns = result
            .get("turn_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let rev = result
            .get("latest_revision_id")
            .and_then(Value::as_str)
            .unwrap_or("<none>");
        let in_tok = result
            .get("total_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cached_tok = result
            .get("total_cached_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let hit_pct = result
            .get("cache_hit_ratio")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            * 100.0;

        // P3 (`v1.5.0`): the gate line reflects the resolved completion gate reported by
        // `session.status`, instead of always claiming verification is armed.
        let gate = result.get("completion_gate");
        let gate_armed = gate
            .and_then(|g| g.get("armed"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let gate_command = gate
            .and_then(|g| g.get("command"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let (gate_dot, gate_detail) = if !gate_armed {
            (
                StatusDot::Tool,
                "completion verification disabled".to_string(),
            )
        } else if !gate_command.is_empty() {
            (
                StatusDot::Success,
                format!(
                    "completion verification armed: {}",
                    sanitize_terminal_text(gate_command)
                ),
            )
        } else {
            (
                StatusDot::Success,
                "completion verification armed (auto-detect)".to_string(),
            )
        };

        let active_model = self
            .dispatcher
            .active_model()
            .unwrap_or_else(|| "<unset>".to_string());
        let active_thinking = self
            .dispatcher
            .active_reasoning_effort()
            .unwrap_or_else(|| "off".to_string());
        let active_protocol = self
            .dispatcher
            .active_protocol()
            .unwrap_or_else(|| "auto".to_string());

        let renderer = self.dispatcher.theme_renderer();
        if renderer.theme() == Theme::Zen {
            // Design D (`v1.5.0`): borderless ultra-minimal header, session facts, and a one-line
            // hint. The facts line keeps the stable `model: / thinking: / protocol: / policy:`
            // labels that scripts and the existing surface-parity tests key on.
            let mut banner = renderer.banner(
                &active_model,
                &active_thinking,
                &active_protocol,
                hit_pct,
                rev,
                ws,
                session_id,
                policy,
                turns,
            );
            banner.push_str(&format!(
                "{}\n",
                renderer.status_line(
                    StatusDot::Tool,
                    "tokens",
                    &format!("in={} cached={} hit={:.1}%", in_tok, cached_tok, hit_pct)
                )
            ));
            banner.push_str(&format!(
                "{}\n",
                renderer.status_line(
                    StatusDot::Thinking,
                    "thinking",
                    &format!("effort={}", sanitize_terminal_text(&active_thinking))
                )
            ));
            banner.push_str(&format!(
                "{}\n",
                renderer.status_line(gate_dot, "gate", &gate_detail)
            ));
            banner.push_str(&format!(
                "{}\n",
                renderer.status_line(
                    StatusDot::Alert,
                    "keys",
                    "/theme /status /diff /model /thinking /race /undo"
                )
            ));
            banner.push_str(&format!(
                "{}\n",
                renderer.hairline(terminal_hairline_width())
            ));
            // P4 (`v1.5.0`): fit every chrome line to the real terminal width so nothing wraps.
            return crate::theme::fit_lines(&banner, crate::theme::terminal_width());
        }

        let classic = format!(
            "=== Kroma v{} Interactive REPL ===\n\
             workspace: {} | policy: {} | session: {}\n\
             model: {} | thinking: {} | protocol: {}\n\
             turns: {} | revision: {} | tokens: in={} (cached={}, hit={:.1}%)\n\
             Type a task prompt, or /help for all commands.\n\
             Key commands: /model  /thinking  /tasks  /accept  /reconcile  /cancel  /resume  /sessions  /new  /undo  /revisions  /graph  /race  /config  /theme  /status  /help  /quit\n",
            crate::version(),
            sanitize_terminal_text(ws),
            sanitize_terminal_text(policy),
            sanitize_terminal_text(session_id),
            sanitize_terminal_text(&active_model),
            sanitize_terminal_text(&active_thinking),
            sanitize_terminal_text(&active_protocol),
            turns,
            sanitize_terminal_text(rev),
            in_tok,
            cached_tok,
            hit_pct,
        );
        // P4 (`v1.5.0`): the classic layout is fixed-width too; fit it to the real terminal width.
        crate::theme::fit_lines(&classic, crate::theme::terminal_width())
    }

    /// Shared handler for `/model`, `/thinking`, `/protocol` and `/config`.
    async fn repl_config_command(
        &self,
        kind: &str,
        rest: &[&str],
    ) -> Result<ReplStepOutcome, String> {
        let save = rest.iter().any(|a| *a == "--save");
        let args: Vec<&str> = rest.iter().copied().filter(|a| *a != "--save").collect();
        let out = |text: String| {
            Ok(ReplStepOutcome {
                output: text,
                should_exit: false,
            })
        };

        match kind {
            "model" => {
                if args.is_empty() {
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "config.get", json!({})))
                        .await;
                    let val = resp.result.unwrap_or_else(|| json!({}));
                    let active = val
                        .get("active_model")
                        .and_then(Value::as_str)
                        .unwrap_or("<unset>");
                    return out(format!("model: {}\n", sanitize_terminal_text(active)));
                }
                let requested = args[0];
                // Mix-up guard: `/model high` is almost certainly a `/thinking` invocation.
                if looks_like_thinking_level(requested) {
                    return out(format!(
                        "hint: '{}' is a thinking level, not a model id.\n  did you mean: /thinking {} [--save]\n",
                        sanitize_terminal_text(requested),
                        sanitize_terminal_text(requested)
                    ));
                }
                let resp = self
                    .dispatcher
                    .handle_request(JsonRpcRequest::new(
                        1,
                        "config.set",
                        json!({ "model": requested, "save": save }),
                    ))
                    .await;
                match resp.result {
                    Some(val) => {
                        let model = val.get("model").and_then(Value::as_str).unwrap_or("-");
                        let protocol = val.get("protocol").and_then(Value::as_str).unwrap_or("-");
                        let persisted = val.get("persisted_to").and_then(Value::as_str);
                        out(format!(
                            "model -> {} (protocol: {}){}\n",
                            sanitize_terminal_text(model),
                            sanitize_terminal_text(protocol),
                            persisted
                                .map(|p| format!(" [saved to {}]", sanitize_terminal_text(p)))
                                .unwrap_or_default()
                        ))
                    }
                    None => out(format!(
                        "error: {}\n",
                        sanitize_terminal_text(
                            &resp
                                .error
                                .map(|e| e.message)
                                .unwrap_or_else(|| "config.set failed".to_string())
                        )
                    )),
                }
            }
            "thinking" => {
                if args.is_empty() {
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "config.get", json!({})))
                        .await;
                    let val = resp.result.unwrap_or_else(|| json!({}));
                    let level = val
                        .get("summary")
                        .and_then(Value::as_str)
                        .and_then(|summary| {
                            summary.lines().find_map(|line| {
                                line.strip_prefix("reasoning_effort:")
                                    .map(|v| v.trim().to_string())
                            })
                        })
                        .unwrap_or_else(|| "<unset>".to_string());
                    return out(format!("thinking: {}\n", sanitize_terminal_text(&level)));
                }
                let requested = args[0];
                if !looks_like_thinking_level(requested) {
                    return out(format!(
                        "hint: '{}' is not a thinking level (off|low|medium|high|xhigh).\n  did you mean: /model {} [--save]\n",
                        sanitize_terminal_text(requested),
                        sanitize_terminal_text(requested)
                    ));
                }
                let resp = self
                    .dispatcher
                    .handle_request(JsonRpcRequest::new(
                        1,
                        "config.set",
                        json!({ "reasoning_effort": requested, "save": save }),
                    ))
                    .await;
                match resp.result {
                    Some(val) => {
                        let level = val
                            .get("reasoning_effort")
                            .and_then(Value::as_str)
                            .unwrap_or("off");
                        let persisted = val.get("persisted_to").and_then(Value::as_str);
                        out(format!(
                            "thinking -> {}{}\n",
                            sanitize_terminal_text(level),
                            persisted
                                .map(|p| format!(" [saved to {}]", sanitize_terminal_text(p)))
                                .unwrap_or_default()
                        ))
                    }
                    None => out(format!(
                        "error: {}\n",
                        sanitize_terminal_text(
                            &resp
                                .error
                                .map(|e| e.message)
                                .unwrap_or_else(|| "config.set failed".to_string())
                        )
                    )),
                }
            }
            "protocol" => {
                if args.is_empty() {
                    let resp = self
                        .dispatcher
                        .handle_request(JsonRpcRequest::new(1, "config.get", json!({})))
                        .await;
                    let val = resp.result.unwrap_or_else(|| json!({}));
                    let active = val
                        .get("active_protocol")
                        .and_then(Value::as_str)
                        .unwrap_or("<unset>");
                    return out(format!("protocol: {}\n", sanitize_terminal_text(active)));
                }
                let resp = self
                    .dispatcher
                    .handle_request(JsonRpcRequest::new(
                        1,
                        "config.set",
                        json!({ "protocol": args[0], "save": save }),
                    ))
                    .await;
                match resp.result {
                    Some(val) => {
                        let protocol = val.get("protocol").and_then(Value::as_str).unwrap_or("-");
                        out(format!(
                            "protocol -> {}\n",
                            sanitize_terminal_text(protocol)
                        ))
                    }
                    None => out(format!(
                        "error: {}\n",
                        sanitize_terminal_text(
                            &resp
                                .error
                                .map(|e| e.message)
                                .unwrap_or_else(|| "config.set failed".to_string())
                        )
                    )),
                }
            }
            _ => {
                // `/config [show | set <key> <value> | profile <name>]`
                let sub = args.first().copied().unwrap_or("show");
                match sub {
                    "show" => {
                        let resp = self
                            .dispatcher
                            .handle_request(JsonRpcRequest::new(1, "config.get", json!({})))
                            .await;
                        let val = resp.result.unwrap_or_else(|| json!({}));
                        let summary = val
                            .get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or("<no layered configuration>");
                        let guard = val
                            .get("rejected_workspace_keys")
                            .and_then(Value::as_array)
                            .map(|list| {
                                list.iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        let mut text = sanitize_terminal_text(summary);
                        if !guard.is_empty() {
                            text.push_str(&format!(
                                "guard: ignored untrusted workspace keys: {}\n",
                                sanitize_terminal_text(&guard)
                            ));
                        }
                        out(text)
                    }
                    "set" => {
                        if args.len() < 3 {
                            return out(
                                "usage: /config set <key> <value>\n  keys: default_profile, model, base_url, api_key, protocol, reasoning_effort, policy, verify_command, max_subagents, projection_profile, adaptive_tournament, max_tokens\n"
                                    .to_string(),
                            );
                        }
                        let key = args[1];
                        let value = args[2..].join(" ");
                        let mut payload = json!({ "save": true });
                        if let Value::Object(ref mut map) = payload {
                            map.insert(key.to_string(), Value::String(value.clone()));
                        }
                        let resp = self
                            .dispatcher
                            .handle_request(JsonRpcRequest::new(1, "config.set", payload))
                            .await;
                        match resp.result {
                            Some(val) => out(format!(
                                "{} = {} [saved to {}]\n",
                                sanitize_terminal_text(key),
                                sanitize_terminal_text(&value),
                                sanitize_terminal_text(
                                    val.get("persisted_to")
                                        .and_then(Value::as_str)
                                        .unwrap_or("<unknown>")
                                )
                            )),
                            None => out(format!(
                                "error: {}\n",
                                sanitize_terminal_text(
                                    &resp
                                        .error
                                        .map(|e| e.message)
                                        .unwrap_or_else(|| "config.set failed".to_string())
                                )
                            )),
                        }
                    }
                    "profile" => {
                        if args.len() < 2 {
                            return out("usage: /config profile <name>\n".to_string());
                        }
                        let resp = self
                            .dispatcher
                            .handle_request(JsonRpcRequest::new(
                                1,
                                "config.set",
                                json!({ "profile": args[1], "save": save }),
                            ))
                            .await;
                        match resp.result {
                            Some(val) => out(format!(
                                "profile -> {} (model: {}, protocol: {})\n",
                                sanitize_terminal_text(args[1]),
                                sanitize_terminal_text(
                                    val.get("model").and_then(Value::as_str).unwrap_or("-")
                                ),
                                sanitize_terminal_text(
                                    val.get("protocol").and_then(Value::as_str).unwrap_or("-")
                                ),
                            )),
                            None => out(format!(
                                "error: {}\n",
                                sanitize_terminal_text(
                                    &resp
                                        .error
                                        .map(|e| e.message)
                                        .unwrap_or_else(|| "config.set failed".to_string())
                                )
                            )),
                        }
                    }
                    other => out(format!(
                        "unknown /config subcommand '{}'; use show | set <key> <value> | profile <name>\n",
                        sanitize_terminal_text(other)
                    )),
                }
            }
        }
    }

    /// `/sessions`: list recent session WALs.
    async fn repl_sessions_command(&self) -> Result<ReplStepOutcome, String> {
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(1, "sessions.list", json!({})))
            .await;
        let val = match resp.result {
            Some(v) => v,
            None => {
                return Ok(ReplStepOutcome {
                    output: format!(
                        "error: {}\n",
                        sanitize_terminal_text(
                            &resp
                                .error
                                .map(|e| e.message)
                                .unwrap_or_else(|| "sessions.list failed".to_string())
                        )
                    ),
                    should_exit: false,
                })
            }
        };
        let sessions = val
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if sessions.is_empty() {
            return Ok(ReplStepOutcome {
                output: "no sessions found.\n".to_string(),
                should_exit: false,
            });
        }
        let mut text = format!("sessions ({}):\n", sessions.len());
        for entry in sessions {
            let id = entry
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("-");
            let entries = entry.get("entries").and_then(Value::as_u64).unwrap_or(0);
            let turns = entry.get("turns").and_then(Value::as_u64).unwrap_or(0);
            let active = entry
                .get("active")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            text.push_str(&format!(
                "  {}{}  entries={} turns={}\n",
                if active { "* " } else { "  " },
                sanitize_terminal_text(id),
                entries,
                turns
            ));
        }
        text.push_str("use /resume <id|latest> to attach one.\n");
        Ok(ReplStepOutcome {
            output: text,
            should_exit: false,
        })
    }

    /// `/resume [<task_id|id|latest>]`: resume a durable task, or attach an existing session WAL.
    async fn repl_resume_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        let (is_task_keyword, raw_target) =
            if rest.first().copied() == Some("task") && rest.len() > 1 {
                (true, rest.get(1).copied())
            } else {
                (false, rest.first().copied())
            };

        let target = raw_target
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("latest");

        if target != "latest" {
            let is_explicit_task_prefix =
                target.starts_with("sub-") || target.starts_with("task-");
            let in_tasks = if is_task_keyword || is_explicit_task_prefix {
                true
            } else {
                let list_resp = self
                    .dispatcher
                    .handle_request(JsonRpcRequest::new(1, "tasks.list", json!({})))
                    .await;
                list_resp
                    .result
                    .as_ref()
                    .and_then(|r| r.get("tasks"))
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter().any(|t| {
                            t.get("task_id").and_then(Value::as_str) == Some(target)
                        })
                    })
                    .unwrap_or(false)
            };

            if in_tasks {
                let resp = self
                    .dispatcher
                    .handle_request(JsonRpcRequest::new(
                        1,
                        "tasks.control",
                        json!({ "action": "resume", "task_id": target }),
                    ))
                    .await;
                return match resp.result {
                    Some(val) => {
                        let tid = val.get("task_id").and_then(Value::as_str).unwrap_or(target);
                        let state = val.get("state").and_then(Value::as_str).unwrap_or("running");
                        Ok(ReplStepOutcome {
                            output: format!(
                                "resumed task {} (state: {})\n",
                                sanitize_terminal_text(tid),
                                sanitize_terminal_text(state),
                            ),
                            should_exit: false,
                        })
                    }
                    None => {
                        let msg = resp
                            .error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "tasks.control resume failed".to_string());
                        Ok(ReplStepOutcome {
                            output: format!("error: {}\n", sanitize_terminal_text(&msg)),
                            should_exit: false,
                        })
                    }
                };
            }
        }

        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "session.resume",
                json!({ "session_id": target }),
            ))
            .await;
        match resp.result {
            Some(val) => {
                let sid = val.get("session_id").and_then(Value::as_str).unwrap_or("-");
                let turns = val.get("turn_count").and_then(Value::as_u64).unwrap_or(0);
                Ok(ReplStepOutcome {
                    output: format!(
                        "resumed session {} ({} turns restored)\n",
                        sanitize_terminal_text(sid),
                        turns
                    ),
                    should_exit: false,
                })
            }
            None => Ok(ReplStepOutcome {
                output: format!(
                    "error: {}\n",
                    sanitize_terminal_text(
                        &resp
                            .error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "session.resume failed".to_string())
                    )
                ),
                should_exit: false,
            }),
        }
    }

    /// `/tasks`: list durable async subagent tasks for the active session.
    async fn repl_tasks_command(&self) -> Result<ReplStepOutcome, String> {
        let sid = self.dispatcher.session_id();
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "tasks.list",
                json!({ "session_id": sid }),
            ))
            .await;

        let val = match resp.result {
            Some(v) => v,
            None => {
                let msg = resp
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "tasks.list failed".to_string());
                return Ok(ReplStepOutcome {
                    output: format!("error: {}\n", sanitize_terminal_text(&msg)),
                    should_exit: false,
                });
            }
        };

        let tasks = val
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        if tasks.is_empty() {
            return Ok(ReplStepOutcome {
                output: "no tasks\n".to_string(),
                should_exit: false,
            });
        }

        let width = crate::theme::terminal_width();
        let mut out = format!("tasks ({}):\n", tasks.len());
        let hdr = format!(
            "  {:<12}  {:<8}  {:<10}  {:<16}  {:<6}  {}",
            "TASK ID", "ROLE", "STATE", "TOKENS", "TURN", "FLAGS"
        );
        out.push_str(&hdr);
        out.push('\n');

        for task in tasks.iter().take(crate::async_tasks::MAX_TASKS) {
            let task_id = task
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or("-");
            let role = task.get("role").and_then(Value::as_str).unwrap_or("-");
            let state = task.get("state").and_then(Value::as_str).unwrap_or("-");
            let spent = task.get("token_spent").and_then(Value::as_u64).unwrap_or(0);
            let alloc = task
                .get("token_allocation")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let tokens_str = format!("{}/{}", spent, alloc);
            let turn_str = task
                .get("updated_turn")
                .and_then(Value::as_u64)
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string());

            let flags_str = format_task_flags(task);

            let row = format!(
                "  {:<12}  {:<8}  {:<10}  {:<16}  {:<6}  {}",
                sanitize_terminal_text(task_id),
                sanitize_terminal_text(role),
                sanitize_terminal_text(state),
                sanitize_terminal_text(&tokens_str),
                sanitize_terminal_text(&turn_str),
                flags_str,
            );
            out.push_str(&row);
            out.push('\n');
        }

        let fitted = crate::theme::fit_lines(&out, width);
        let final_out = if !fitted.ends_with('\n') {
            format!("{}\n", fitted)
        } else {
            fitted
        };

        Ok(ReplStepOutcome {
            output: final_out,
            should_exit: false,
        })
    }

    /// `/accept <task_id> [verified_revision]`: mint an explicit local acceptance for a completed,
    /// gate-verified task. The accepting principal is filled by the host entry point, never by this
    /// text; a revision, when given, must be exactly the revision the gate verified.
    async fn repl_accept_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        let Some(task_id) = rest.first().map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            return Ok(ReplStepOutcome {
                output: "usage: /accept <task_id> [verified_revision]\n".to_string(),
                should_exit: false,
            });
        };
        let mut params = json!({ "action": "accept", "task_id": task_id });
        if let Some(revision) = rest.get(1).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            params["evidence_revision"] = json!(revision);
        }
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(1, "tasks.control", params))
            .await;
        Ok(match resp.result {
            Some(val) => {
                let rev = val
                    .get("evidence_revision")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                let artifacts = val
                    .get("artifact_digests")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                ReplStepOutcome {
                    output: format!(
                        "accepted task {} at revision {} ({} artifact digests)\n",
                        sanitize_terminal_text(task_id),
                        sanitize_terminal_text(rev),
                        artifacts,
                    ),
                    should_exit: false,
                }
            }
            None => ReplStepOutcome {
                output: format!(
                    "error: {}\n",
                    sanitize_terminal_text(
                        &resp
                            .error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "tasks.control accept failed".to_string())
                    )
                ),
                should_exit: false,
            },
        })
    }

    /// `/reconcile <task_id> unknown <reason...>`, `... verified <evidence...>`, or
    /// `... retry <new_task_id> <reason...>`: record an explicit disposition of a task whose outcome
    /// is unknown after a crash. Reconciling never clears the block on re-running that record.
    async fn repl_reconcile_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        const USAGE: &str = "usage: /reconcile <task_id> unknown <reason...> | /reconcile <task_id> verified <evidence...> | /reconcile <task_id> retry <new_task_id> <reason...>\n";
        let Some(task_id) = rest.first().map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            return Ok(ReplStepOutcome {
                output: USAGE.to_string(),
                should_exit: false,
            });
        };
        let Some(kind) = rest.get(1).map(|s| s.trim()) else {
            return Ok(ReplStepOutcome {
                output: USAGE.to_string(),
                should_exit: false,
            });
        };
        let (decision, reason_start) = match kind {
            "unknown" => (json!({ "kind": "acknowledged_unknown" }), 2),
            "verified" => {
                // The remaining text is both the external evidence and the audit reason.
                let evidence = rest.get(2..).map(|parts| parts.join(" ")).unwrap_or_default();
                let evidence = evidence.trim();
                if evidence.is_empty() {
                    return Ok(ReplStepOutcome {
                        output: USAGE.to_string(),
                        should_exit: false,
                    });
                }
                (
                    json!({ "kind": "externally_verified", "evidence": evidence }),
                    2,
                )
            }
            "retry" => {
                let Some(new_task_id) = rest.get(2).map(|s| s.trim()).filter(|s| !s.is_empty())
                else {
                    return Ok(ReplStepOutcome {
                        output: USAGE.to_string(),
                        should_exit: false,
                    });
                };
                (
                    json!({ "kind": "retry_as_new_attempt", "new_task_id": new_task_id }),
                    3,
                )
            }
            _ => {
                return Ok(ReplStepOutcome {
                    output: USAGE.to_string(),
                    should_exit: false,
                })
            }
        };
        let reason = rest
            .get(reason_start..)
            .map(|parts| parts.join(" "))
            .unwrap_or_default();
        let reason = reason.trim();
        if reason.is_empty() {
            return Ok(ReplStepOutcome {
                output: USAGE.to_string(),
                should_exit: false,
            });
        }
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "tasks.control",
                json!({
                    "action": "reconcile",
                    "task_id": task_id,
                    "decision": decision,
                    "reason": reason,
                }),
            ))
            .await;
        Ok(match resp.result {
            Some(val) => {
                let decision = val.get("decision").and_then(Value::as_str).unwrap_or("-");
                let superseded = val
                    .get("superseded_by")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                ReplStepOutcome {
                    output: format!(
                        "reconciled task {} ({}); superseded_by: {}\n",
                        sanitize_terminal_text(task_id),
                        sanitize_terminal_text(decision),
                        sanitize_terminal_text(superseded),
                    ),
                    should_exit: false,
                }
            }
            None => ReplStepOutcome {
                output: format!(
                    "error: {}\n",
                    sanitize_terminal_text(
                        &resp
                            .error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "tasks.control reconcile failed".to_string())
                    )
                ),
                should_exit: false,
            },
        })
    }

    /// `/cancel <task_id>`: cancel a durable async task.
    async fn repl_cancel_task_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        let task_id = match rest.first().copied().map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => id,
            None => {
                return Ok(ReplStepOutcome {
                    output: "usage: /cancel <task_id>\n".to_string(),
                    should_exit: false,
                })
            }
        };

        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "tasks.control",
                json!({ "action": "cancel", "task_id": task_id }),
            ))
            .await;

        match resp.result {
            Some(val) => {
                let tid = val.get("task_id").and_then(Value::as_str).unwrap_or(task_id);
                let state = val.get("state").and_then(Value::as_str).unwrap_or("unknown");
                Ok(ReplStepOutcome {
                    output: format!(
                        "task {} state: {}\n",
                        sanitize_terminal_text(tid),
                        sanitize_terminal_text(state),
                    ),
                    should_exit: false,
                })
            }
            None => {
                let msg = resp
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "tasks.control cancel failed".to_string());
                Ok(ReplStepOutcome {
                    output: format!("error: {}\n", sanitize_terminal_text(&msg)),
                    should_exit: false,
                })
            }
        }
    }

    /// `/new`: start a fresh session without exiting the REPL.
    async fn repl_new_session_command(&self) -> Result<ReplStepOutcome, String> {
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(1, "session.new", json!({})))
            .await;
        let sid = resp
            .result
            .and_then(|v| {
                v.get("session_id")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_default();
        Ok(ReplStepOutcome {
            output: format!("started new session {}\n", sanitize_terminal_text(&sid)),
            should_exit: false,
        })
    }

    /// `/race <N> <task>`: Best-of-N worktree tournament.
    async fn repl_race_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        if rest.len() < 2 {
            return Ok(ReplStepOutcome {
                output: "usage: /race <N> <task>   (N between 2 and 4)\n".to_string(),
                should_exit: false,
            });
        }
        let count = match rest[0].parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                return Ok(ReplStepOutcome {
                    output: format!(
                        "usage: /race <N> <task>   ('{}' is not a number)\n",
                        sanitize_terminal_text(rest[0])
                    ),
                    should_exit: false,
                })
            }
        };
        let task = rest[1..].join(" ");
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "session.tournament",
                json!({ "candidates": count, "task": task }),
            ))
            .await;
        match resp.result {
            Some(val) => {
                let winner = val
                    .get("winner")
                    .and_then(Value::as_str)
                    .unwrap_or("<none>");
                let merged = val
                    .get("merged_revision")
                    .and_then(Value::as_str)
                    .unwrap_or("<none>");
                let cleaned = val
                    .get("all_worktrees_cleaned_up")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut text = format!(
                    "tournament complete: winner={} merged_revision={} worktrees_cleaned_up={}\n",
                    sanitize_terminal_text(winner),
                    sanitize_terminal_text(merged),
                    cleaned
                );
                if let Some(list) = val.get("candidates").and_then(Value::as_array) {
                    for c in list {
                        text.push_str(&format!(
                            "  {} status={} gate_passed={} gate_status={} warnings={} changed_lines={} turns={}{}\n",
                            sanitize_terminal_text(
                                c.get("label").and_then(Value::as_str).unwrap_or("-")
                            ),
                            sanitize_terminal_text(
                                c.get("status").and_then(Value::as_str).unwrap_or("-")
                            ),
                            c.get("gate_passed").and_then(Value::as_bool).unwrap_or(false),
                            sanitize_terminal_text(
                                c.get("gate_status").and_then(Value::as_str).unwrap_or("-")
                            ),
                            c.get("predictive_warnings").and_then(Value::as_u64).unwrap_or(0),
                            c.get("total_changed_lines").and_then(Value::as_u64).unwrap_or(0),
                            c.get("turns_used").and_then(Value::as_u64).unwrap_or(0),
                            if c.get("winner").and_then(Value::as_bool).unwrap_or(false) {
                                "  <-- winner"
                            } else {
                                ""
                            }
                        ));
                    }
                }
                Ok(ReplStepOutcome {
                    output: text,
                    should_exit: false,
                })
            }
            None => Ok(ReplStepOutcome {
                output: format!(
                    "error: {}\n",
                    sanitize_terminal_text(
                        &resp
                            .error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "session.tournament failed".to_string())
                    )
                ),
                should_exit: false,
            }),
        }
    }

    /// Process one user input line (either a `/slash` command or a prompt turn) through
    /// [`RpcDispatcher`].
    pub async fn handle_line(&self, raw_line: &str) -> Result<ReplStepOutcome, String> {
        let line = raw_line.trim();
        if line.is_empty() {
            return Ok(ReplStepOutcome {
                output: String::new(),
                should_exit: false,
            });
        }

        if line.starts_with('/') {
            let mut parts = line.split_whitespace();
            let cmd = parts.next().unwrap_or("");
            let rest: Vec<&str> = parts.collect();
            return match cmd {
                "/help" | "/?" => self.repl_help_command().await,
                "/quit" | "/exit" => self.repl_quit_command().await,
                "/status" => self.repl_status_command().await,
                "/theme" | "/ui" => self.repl_theme_command(&rest).await,
                "/revisions" | "/diff" => self.repl_revisions_command(cmd).await,
                "/undo" => self.repl_undo_command(&rest).await,
                "/restore" => self.repl_restore_command(&rest).await,
                "/skills" => self.repl_skills_command(&rest).await,
                "/graph" => self.repl_graph_command(&rest).await,
                "/fork" => self.repl_fork_command(&rest).await,
                "/inspect" | "/evidence" => self.repl_inspect_command(&rest).await,
                "/model" => self.repl_config_command("model", &rest).await,
                "/thinking" => self.repl_config_command("thinking", &rest).await,
                "/protocol" => self.repl_config_command("protocol", &rest).await,
                "/config" => self.repl_config_command("config", &rest).await,
                "/sessions" => self.repl_sessions_command().await,
                "/tasks" => self.repl_tasks_command().await,
                "/experience" => {
                    if rest.len() > 2 {
                        return Ok(ReplStepOutcome { output: "experience refused: too many arguments; use an absolute path without spaces\n".into(), should_exit: false });
                    }
                    let result = self.dispatcher.experience_local_control(
                        rest.first().copied().unwrap_or("status"), rest.get(1).copied(),
                    );
                    Ok(ReplStepOutcome { output: format!("{}\n", sanitize_terminal_text(
                        &result.unwrap_or_else(|error| format!("experience refused: {error}")),
                    )), should_exit: false })
                }
                "/accept" => self.repl_accept_command(&rest).await,
                "/reconcile" => self.repl_reconcile_command(&rest).await,
                "/cancel" => self.repl_cancel_task_command(&rest).await,
                "/resume" => self.repl_resume_command(&rest).await,
                "/new" => self.repl_new_session_command().await,
                "/race" => self.repl_race_command(&rest).await,
                "/clear" => self.repl_clear_command().await,
                "/history" => self.repl_history_command(&rest).await,
                other => Ok(ReplStepOutcome {
                    output: format!(
                        "unknown command '{}'; type /help for available commands.\n",
                        sanitize_terminal_text(other)
                    ),
                    should_exit: false,
                }),
            };
        }

        self.handle_prompt_line(line).await
    }

    /// Dispatch a prompt turn to `session.prompt`, bypassing slash-command interpretation.
    ///
    /// `prompt` must already be trimmed. Used for ordinary prompts and for literal multi-line
    /// submissions (bracketed paste / `"""` block / backslash continuation), which must never be
    /// read as a slash command even when they begin with `/`.
    async fn handle_prompt_line(&self, prompt: &str) -> Result<ReplStepOutcome, String> {
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "session.prompt",
                json!({ "prompt": prompt }),
            ))
            .await;
        match (resp.result, resp.error) {
            (Some(val), _) => {
                let status = val
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let text = val.get("final_text").and_then(Value::as_str).unwrap_or("");
                let turns = val.get("turns_used").and_then(Value::as_u64).unwrap_or(0);
                let tools = val
                    .get("tool_calls_used")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let rev = val
                    .get("latest_revision_id")
                    .and_then(Value::as_str)
                    .unwrap_or("<none>");
                let in_tok = val
                    .get("total_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let out_tok = val
                    .get("total_output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let child_in_tok = val
                    .get("children_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let child_out_tok = val
                    .get("children_output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let hit_pct = val
                    .get("cache_hit_ratio")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    * 100.0;
                let modified: Vec<String> = val
                    .get("modified_files")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(sanitize_terminal_text)
                            .collect()
                    })
                    .unwrap_or_default();
                let mut out = String::new();
                if !text.is_empty() {
                    out.push_str(&sanitize_terminal_text(text));
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                if status != "completed" {
                    if let Some(reason) = val.get("terminal_reason").and_then(Value::as_str) {
                        out.push_str(&format!("error: {}\n", sanitize_terminal_text(reason)));
                    }
                }
                let terminal_reason = val
                    .get("terminal_reason")
                    .and_then(Value::as_str)
                    .unwrap_or(status);
                out.push_str(&format!(
                    "[status={} terminal_reason={} turns={} tools={} rev={}]\n",
                    sanitize_terminal_text(status),
                    sanitize_terminal_text(terminal_reason),
                    turns,
                    tools,
                    sanitize_terminal_text(rev),
                ));
                // `v1.5.0` Design D footer: a spectrum status dot, the files touched, and a
                // compact telemetry line (`self + child` token spend, cache hit, revision).
                let renderer = self.dispatcher.theme_renderer();
                if renderer.theme() == Theme::Zen {
                    let dot = match status {
                        "completed" => StatusDot::Success,
                        "cancelled" => StatusDot::Alert,
                        _ => StatusDot::Error,
                    };
                    out.push_str(&renderer.status_line(
                        dot,
                        "run",
                        &format!(
                            "status={} terminal_reason={} turns={} tools={}",
                            sanitize_terminal_text(status),
                            sanitize_terminal_text(terminal_reason),
                            turns,
                            tools
                        ),
                    ));
                    out.push('\n');
                    if !modified.is_empty() {
                        out.push_str(&renderer.status_line(
                            StatusDot::Success,
                            "edit",
                            &format!(
                                "{} file(s) changed: {}",
                                modified.len(),
                                modified.join(", ")
                            ),
                        ));
                        out.push('\n');
                    }
                    out.push_str(&renderer.telemetry_footer(
                        in_tok.saturating_add(out_tok),
                        child_in_tok.saturating_add(child_out_tok),
                        hit_pct,
                        rev,
                    ));
                    out.push('\n');
                }
                Ok(ReplStepOutcome {
                    output: out,
                    should_exit: false,
                })
            }
            (_, Some(err)) => Ok(ReplStepOutcome {
                output: format!("error: {}\n", sanitize_terminal_text(&err.message)),
                should_exit: false,
            }),
            _ => Err("empty response from session.prompt".to_string()),
        }
    }

    async fn repl_help_command(&self) -> Result<ReplStepOutcome, String> {
        Ok(ReplStepOutcome {
            output: concat!(
                "Available commands:\n",
                "  /help                       Show this command reference\n",
                "  /status                     Show active session, model, thinking, revision and token status\n",
                "  /model [<id>] [--save]      Show or hot-swap the active model (--save persists it)\n",
                "  /thinking [<lvl>] [--save]  Show or hot-swap reasoning effort (off|low|medium|high|xhigh)\n",
                "  /protocol [<name>]          Show or hot-swap the wire protocol (auto|chat-completions|\n",
                "                              anthropic-messages|openai-responses)\n",
                "  /config [show|set <k> <v>|profile <name>]  Inspect or persist layered configuration\n",
                "  /theme [zen|classic]        Show or switch the terminal theme (zen = Design D, default)\n",
                "  /sessions                   List recent sessions in the state directory\n",
                "  /tasks                      List durable async tasks for the active session\n",
                "  /experience status|list|load <file>|accept <id>|revoke <id>|on|off\n",
                "                              Local-only reviewed experience; off by default, process-local\n",
                "  /cancel <task_id>           Cancel a durable async task\n",
                "  /resume [<task_id|id>]      Resume a durable task, or attach an existing session WAL\n",
                "  /new | /clear               Start a fresh session in this workspace\n",
                "  /revisions                  List captured workspace revisions for this session\n",
                "  /undo [--force]             Restore workspace & session leaf to previous revision\n",
                "  /restore <rev> [--force]    Restore workspace to a specific revision ID\n",
                "  /diff                       Show files changed in the latest revision\n",
                "  /skills [<name>]            List workspace skills or activate <name>\n",
                "  /graph [<query>]            Rank files with Personalized PageRank over the symbol graph\n",
                "  /race <N> <task>            Best-of-N worktree tournament: race N workers, merge the best\n",
                "  /fork <turn>                Rewind/branch active session at <turn>\n",
                "  /inspect <id>               Recall raw tool evidence by source_entry_id\n",
                "  /history [N]                List the most recent N prompts (default 20); !N / !! re-run one\n",
                "  /quit | /exit               Shut down the interactive session\n"
            )
            .to_string(),
            should_exit: false,
        })
    }

    /// `/history [N]`: list the most recent `N` submitted prompts (default 20, capped at
    /// [`REPL_HISTORY_LIMIT`]), newest first. Entries are sanitized and width-fitted.
    async fn repl_history_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        let requested = match rest.first() {
            None => REPL_HISTORY_DEFAULT_SHOWN,
            Some(raw) => match raw.parse::<usize>() {
                Ok(n) if n >= 1 => n,
                _ => {
                    return Ok(ReplStepOutcome {
                        output: format!(
                            "usage: /history [N] (N >= 1; default {})\n",
                            REPL_HISTORY_DEFAULT_SHOWN
                        ),
                        should_exit: false,
                    });
                }
            },
        };
        let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let total = history.len();
        if total == 0 {
            return Ok(ReplStepOutcome {
                output: "history: no prompts recorded yet.\n".to_string(),
                should_exit: false,
            });
        }
        let shown = requested.min(total).min(REPL_HISTORY_LIMIT);
        let width = crate::theme::terminal_width();
        let mut out = format!(
            "history ({} of {} entr{}):\n",
            shown,
            total,
            if total == 1 { "y" } else { "ies" }
        );
        for (offset, entry) in history.iter().rev().take(shown).enumerate() {
            // `sanitize_diagnostic_label` keeps a multi-line entry on one bounded row.
            let row = format!("  {:>3}  {}", offset + 1, sanitize_diagnostic_label(entry));
            out.push_str(&crate::theme::truncate_display(&row, width));
            out.push('\n');
        }
        Ok(ReplStepOutcome {
            output: out,
            should_exit: false,
        })
    }

    /// Append one submitted prompt to the in-memory history, keeping the last
    /// [`REPL_HISTORY_LIMIT`] entries. Prompt text is never written to disk.
    fn record_history(&self, text: &str) {
        let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        if history.len() >= REPL_HISTORY_LIMIT {
            history.pop_front();
        }
        history.push_back(text.to_string());
    }

    /// Resolve `!N` (1-based, most recent first) or `!!` (the most recent prompt) against the
    /// in-memory history. Any other input is returned as [`HistoryRecall::NotRecall`].
    fn expand_history_recall(&self, input: &str) -> HistoryRecall {
        let trimmed = input.trim();
        if trimmed == "!!" {
            let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
            return match history.back() {
                Some(entry) => HistoryRecall::Text(entry.clone()),
                None => {
                    HistoryRecall::Error("error: no previous prompt to re-run.\n".to_string())
                }
            };
        }
        let Some(rest) = trimmed.strip_prefix('!') else {
            return HistoryRecall::NotRecall;
        };
        let Ok(index) = rest.parse::<usize>() else {
            return HistoryRecall::NotRecall;
        };
        let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        if index == 0 {
            return HistoryRecall::Error(
                "error: history index must be >= 1 (1 = most recent).\n".to_string(),
            );
        }
        if index > history.len() {
            return HistoryRecall::Error(format!(
                "error: history index {} out of range ({} entr{} recorded).\n",
                index,
                history.len(),
                if history.len() == 1 { "y" } else { "ies" }
            ));
        }
        match history.iter().rev().nth(index - 1) {
            Some(entry) => HistoryRecall::Text(entry.clone()),
            None => HistoryRecall::Error(
                "error: history index out of range; nothing was executed.\n".to_string(),
            ),
        }
    }

    async fn repl_quit_command(&self) -> Result<ReplStepOutcome, String> {
        let _ = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(1, "shutdown", json!({})))
            .await;
        Ok(ReplStepOutcome {
            output: "Session closed.\n".to_string(),
            should_exit: true,
        })
    }

    async fn repl_status_command(&self) -> Result<ReplStepOutcome, String> {
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(1, "session.status", json!({})))
            .await;
        let val = resp.result.ok_or_else(|| {
            resp.error
                .map(|e| e.message)
                .unwrap_or_else(|| "status failed".to_string())
        })?;
        let sid = val.get("session_id").and_then(Value::as_str).unwrap_or("-");
        let policy = val
            .get("policy_mode")
            .and_then(Value::as_str)
            .unwrap_or("-");
        let turns = val.get("turn_count").and_then(Value::as_u64).unwrap_or(0);
        let rev = val
            .get("latest_revision_id")
            .and_then(Value::as_str)
            .unwrap_or("<none>");
        let mut_count = val
            .get("mutations")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        let sub_count = val
            .get("subagents_spawned")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // `v1.5.0`: prefix the live view with a Design D status-dot line. The body keeps
        // its stable `key: value` lines so scripts and parity tests are unaffected.
        let renderer = self.dispatcher.theme_renderer();
        let mut out = String::new();
        if renderer.theme() == Theme::Zen {
            out.push_str(&renderer.status_line(
                StatusDot::Tool,
                "status",
                &format!(
                    "session={} policy={} turns={} rev={}",
                    sanitize_terminal_text(sid),
                    sanitize_terminal_text(policy),
                    turns,
                    crate::theme::rev_label(rev),
                ),
            ));
            out.push('\n');
        }
        out.push_str(&format!(
            "session_id: {}\npolicy: {}\nturn_count: {}\nlatest_revision: {}\nmutations: {}\nsubagents_spawned: {}\n",
            sanitize_terminal_text(sid),
            sanitize_terminal_text(policy),
            turns,
            sanitize_terminal_text(rev),
            mut_count,
            sub_count,
        ));
        Ok(ReplStepOutcome {
            output: out,
            should_exit: false,
        })
    }

    async fn repl_theme_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        // `v1.5.0`: show or switch the Design D / classic terminal theme.
        let renderer = self.dispatcher.theme_renderer();
        let Some(&requested) = rest.first() else {
            return Ok(ReplStepOutcome {
                output: format!(
                    "theme: {} (available: zen, classic)\n",
                    renderer.theme().as_str()
                ),
                should_exit: false,
            });
        };
        if Theme::parse(requested).is_err() {
            return Ok(ReplStepOutcome {
                output: format!(
                    "error: unknown theme '{}'; expected 'zen' or 'classic'\n",
                    sanitize_terminal_text(requested)
                ),
                should_exit: false,
            });
        }
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "config.set",
                json!({ "ui_theme": requested }),
            ))
            .await;
        match resp.result {
            Some(val) => {
                let theme = val
                    .get("ui_theme")
                    .and_then(Value::as_str)
                    .unwrap_or(requested);
                let renderer = self.dispatcher.theme_renderer();
                let mut out = String::new();
                if renderer.theme() == Theme::Zen {
                    out.push_str(&renderer.status_line(
                        StatusDot::Success,
                        "theme",
                        &format!("switched to {}", theme),
                    ));
                    out.push('\n');
                }
                out.push_str(&format!(
                    "theme -> {}{}\n",
                    sanitize_terminal_text(theme),
                    if theme == "zen" {
                        " (Design D: borderless, ultra-minimal)"
                    } else {
                        ""
                    }
                ));
                Ok(ReplStepOutcome {
                    output: out,
                    should_exit: false,
                })
            }
            None => Ok(ReplStepOutcome {
                output: format!(
                    "error: {}\n",
                    sanitize_terminal_text(
                        &resp
                            .error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "config.set failed".to_string())
                    )
                ),
                should_exit: false,
            }),
        }
    }

    async fn repl_revisions_command(&self, cmd: &str) -> Result<ReplStepOutcome, String> {
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(1, "revision.list", json!({})))
            .await;
        let val = resp.result.ok_or_else(|| {
            resp.error
                .map(|e| e.message)
                .unwrap_or_else(|| "revision.list failed".to_string())
        })?;
        let revs = val
            .get("revisions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if revs.is_empty() {
            return Ok(ReplStepOutcome {
                output: "no workspace revisions captured yet.\n".to_string(),
                should_exit: false,
            });
        }
        let mut out = String::new();
        if cmd == "/diff" {
            // `v1.5.0`: Design D header for the live diff view. The stable
            // `revisions (N captured):` body below is preserved for scripts.
            let renderer = self.dispatcher.theme_renderer();
            if renderer.theme() == Theme::Zen {
                out.push_str(&renderer.status_line(
                    StatusDot::Tool,
                    "diff",
                    &format!("{} workspace revision(s) captured", revs.len()),
                ));
                out.push('\n');
                out.push_str(&renderer.hairline(terminal_hairline_width()));
                out.push('\n');
            }
        }
        out.push_str(&format!("revisions ({} captured):\n", revs.len()));
        for r in &revs {
            let turn = r.get("turn").and_then(Value::as_u64).unwrap_or(0);
            let rid = r.get("revision_id").and_then(Value::as_str).unwrap_or("-");
            let files = r
                .get("tracked_file_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let is_cur = r
                .get("is_current")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let changed: Vec<String> = r
                .get("changed_paths")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(sanitize_terminal_text)
                        .collect()
                })
                .unwrap_or_default();
            let marker = if is_cur { " <- current" } else { "" };
            out.push_str(&format!(
                "  turn {}  {}  files={} changed=[{}]{}\n",
                turn,
                sanitize_terminal_text(rid),
                files,
                changed.join(", "),
                marker
            ));
        }
        Ok(ReplStepOutcome {
            output: out,
            should_exit: false,
        })
    }

    async fn repl_undo_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        let force = rest.iter().any(|&arg| arg == "--force" || arg == "-f");
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "revision.undo",
                json!({ "force": force }),
            ))
            .await;
        match (resp.result, resp.error) {
            (Some(val), _) => {
                let rev = val
                    .get("restored_revision")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                let turn = val.get("rewound_turn").and_then(Value::as_u64).unwrap_or(0);
                Ok(ReplStepOutcome {
                    output: format!(
                        "restored workspace to {} (rewound to turn {})\n",
                        sanitize_terminal_text(rev),
                        turn
                    ),
                    should_exit: false,
                })
            }
            (_, Some(err)) => Ok(ReplStepOutcome {
                output: format!("undo error: {}\n", sanitize_terminal_text(&err.message)),
                should_exit: false,
            }),
            _ => Err("unexpected empty response from revision.undo".to_string()),
        }
    }

    async fn repl_restore_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        let Some(&rev_id) = rest.first() else {
            return Ok(ReplStepOutcome {
                output: "usage: /restore <revision_id> [--force]\n".to_string(),
                should_exit: false,
            });
        };
        let force = rest
            .iter()
            .skip(1)
            .any(|&arg| arg == "--force" || arg == "-f");
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "revision.restore",
                json!({ "revision_id": rev_id, "force": force }),
            ))
            .await;
        match (resp.result, resp.error) {
            (Some(val), _) => {
                let rev = val
                    .get("restored_revision")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                Ok(ReplStepOutcome {
                    output: format!("restored workspace to {}\n", sanitize_terminal_text(rev)),
                    should_exit: false,
                })
            }
            (_, Some(err)) => Ok(ReplStepOutcome {
                output: format!("restore error: {}\n", sanitize_terminal_text(&err.message)),
                should_exit: false,
            }),
            _ => Err("unexpected empty response from revision.restore".to_string()),
        }
    }

    async fn repl_skills_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        if let Some(&skill_name) = rest.first() {
            let resp = self
                .dispatcher
                .handle_request(JsonRpcRequest::new(
                    1,
                    "skills.activate",
                    json!({ "name": skill_name }),
                ))
                .await;
            return match (resp.result, resp.error) {
                (Some(val), _) => {
                    let content = val.get("content").and_then(Value::as_str).unwrap_or("");
                    Ok(ReplStepOutcome {
                        output: format!(
                            "activated skill '{}':\n{}\n",
                            sanitize_terminal_text(skill_name),
                            sanitize_terminal_text(content)
                        ),
                        should_exit: false,
                    })
                }
                (_, Some(err)) => Ok(ReplStepOutcome {
                    output: format!("skill error: {}\n", sanitize_terminal_text(&err.message)),
                    should_exit: false,
                }),
                _ => Err("empty response from skills.activate".to_string()),
            };
        }
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(1, "skills.list", json!({})))
            .await;
        let val = resp.result.unwrap_or_else(|| json!({}));
        let skills = val
            .get("skills")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if skills.is_empty() {
            return Ok(ReplStepOutcome {
                output: "no skills discovered in .agents/skills/.\n".to_string(),
                should_exit: false,
            });
        }
        let mut out = format!("skills ({} discovered):\n", skills.len());
        for s in &skills {
            let name = s.get("name").and_then(Value::as_str).unwrap_or("-");
            let desc = s.get("description").and_then(Value::as_str).unwrap_or("");
            let act = s.get("activated").and_then(Value::as_bool).unwrap_or(false);
            let tag = if act { " [active]" } else { "" };
            out.push_str(&format!(
                "  {}{}: {}\n",
                sanitize_terminal_text(name),
                tag,
                sanitize_terminal_text(desc)
            ));
        }
        Ok(ReplStepOutcome {
            output: out,
            should_exit: false,
        })
    }

    async fn repl_graph_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        let query = rest.join(" ");
        let params = if query.trim().is_empty() {
            json!({})
        } else {
            json!({ "query": query })
        };
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(1, "workspace.graph", params))
            .await;
        let val = resp.result.ok_or_else(|| {
            resp.error
                .map(|e| e.message)
                .unwrap_or_else(|| "workspace.graph failed".to_string())
        })?;
        let total_files = val.get("total_files").and_then(Value::as_u64).unwrap_or(0);
        let total_symbols = val
            .get("total_symbols")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let total_edges = val.get("total_edges").and_then(Value::as_u64).unwrap_or(0);
        let ranked = val
            .get("ranked")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if ranked.is_empty() {
            return Ok(ReplStepOutcome {
                output: format!(
                    "repo graph: {} files, {} symbols, {} edges; no ranked files.\n",
                    total_files, total_symbols, total_edges
                ),
                should_exit: false,
            });
        }
        let mut out = format!(
            "repo graph: {} files, {} symbols, {} edges\n",
            total_files, total_symbols, total_edges
        );
        for (i, node) in ranked.iter().enumerate() {
            let path = node.get("path").and_then(Value::as_str).unwrap_or("-");
            let score = node.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            out.push_str(&format!(
                "  {} {} (score={:.4})\n",
                i + 1,
                sanitize_terminal_text(path),
                score
            ));
            if let Some(syms) = node.get("symbols").and_then(Value::as_array) {
                for s in syms {
                    let kind = s.get("kind").and_then(Value::as_str).unwrap_or("");
                    let sig = s.get("signature").and_then(Value::as_str).unwrap_or("");
                    out.push_str(&format!("      {} {}\n", kind, sanitize_terminal_text(sig)));
                }
            }
        }
        Ok(ReplStepOutcome {
            output: out,
            should_exit: false,
        })
    }

    async fn repl_fork_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        let Some(turn_str) = rest.first() else {
            return Ok(ReplStepOutcome {
                output: "usage: /fork <turn>\n".to_string(),
                should_exit: false,
            });
        };
        let Ok(turn) = turn_str.parse::<usize>() else {
            return Ok(ReplStepOutcome {
                output: "error: /fork expects a non-negative integer turn\n".to_string(),
                should_exit: false,
            });
        };
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "session.fork",
                json!({ "turn": turn }),
            ))
            .await;
        match (resp.result, resp.error) {
            (Some(val), _) => {
                let leaf = val.get("leaf_id").and_then(Value::as_str).unwrap_or("-");
                Ok(ReplStepOutcome {
                    output: format!(
                        "forked session at turn {} (leaf {})\n",
                        turn,
                        sanitize_terminal_text(leaf)
                    ),
                    should_exit: false,
                })
            }
            (_, Some(err)) => Ok(ReplStepOutcome {
                output: format!("fork error: {}\n", sanitize_terminal_text(&err.message)),
                should_exit: false,
            }),
            _ => Err("empty response from session.fork".to_string()),
        }
    }

    async fn repl_inspect_command(&self, rest: &[&str]) -> Result<ReplStepOutcome, String> {
        let sid = rest.first().copied().unwrap_or("latest");
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                1,
                "evidence.recall",
                json!({ "source_entry_id": sid }),
            ))
            .await;
        match (resp.result, resp.error) {
            (Some(val), _) => {
                let content = val.get("content").and_then(Value::as_str).unwrap_or("");
                Ok(ReplStepOutcome {
                    output: format!("{}\n", sanitize_terminal_text(content)),
                    should_exit: false,
                })
            }
            (_, Some(err)) => Ok(ReplStepOutcome {
                output: format!("evidence error: {}\n", sanitize_terminal_text(&err.message)),
                should_exit: false,
            }),
            _ => Err("empty response from evidence.recall".to_string()),
        }
    }

    async fn repl_clear_command(&self) -> Result<ReplStepOutcome, String> {
        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(1, "session.new", json!({})))
            .await;
        let sid = resp
            .result
            .and_then(|v| {
                v.get("session_id")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_default();
        Ok(ReplStepOutcome {
            output: format!("started new session {}\n", sanitize_terminal_text(&sid)),
            should_exit: false,
        })
    }

    /// `v1.5.0`: map a live `kroma/event` notification to a Design D status line.
    ///
    /// Returns `None` for notifications that belong to another session, are not `kroma/event`, or
    /// carry no terminal-worthy state. This is what makes tool start/complete, model turns, policy
    /// decisions, predictive checks, revision captures and subagent activity stream live.
    pub fn render_live_event(&self, note: &JsonRpcNotification) -> Option<String> {
        if note.method != "kroma/event" {
            return None;
        }
        let params = &note.params;
        let active_session = self.dispatcher.session_id();
        if let Some(session) = params.get("session_id").and_then(Value::as_str) {
            if !active_session.is_empty() && session != active_session {
                return None;
            }
        }
        // Serde serialises `EventPayload` with the variant name as the tag (`ToolStarted`); accept
        // the snake_case spelling too so hand-written emitters work as well.
        let event_type = params.get("event_type").and_then(Value::as_str)?;
        let event_key = event_type.to_ascii_lowercase().replace('_', "");
        let payload = params.get("payload").cloned().unwrap_or(Value::Null);
        let renderer = self.dispatcher.theme_renderer();
        if renderer.theme() != Theme::Zen {
            return None;
        }
        let str_of = |key: &str| {
            payload
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let num_of = |key: &str| payload.get(key).and_then(Value::as_u64).unwrap_or(0);

        let line = match event_key.as_str() {
            "toolstarted" => {
                let tool = str_of("tool_name");
                let dot = if matches!(tool.as_str(), "edit" | "write") {
                    StatusDot::Success
                } else {
                    StatusDot::Tool
                };
                let path = str_of("relative_path");
                renderer.status_line(dot, &tool, &path)
            }
            "toolcompleted" => {
                let tool = str_of("tool_name");
                let outcome = str_of("outcome");
                let ok = outcome == "success";
                let dot = if ok {
                    StatusDot::Success
                } else {
                    StatusDot::Error
                };
                let path = str_of("relative_path");
                let tier = payload
                    .get("edit_match_tier")
                    .and_then(Value::as_str)
                    .map(|t| format!(" [{}]", t))
                    .unwrap_or_default();
                let detail = format!(
                    "{}{} {} {}ms",
                    if ok { "\u{2713}" } else { "\u{2717}" },
                    tier,
                    path,
                    num_of("duration_ms")
                );
                renderer.status_line(dot, &tool, detail.trim())
            }
            "modelrequestcompleted" => {
                let hit = payload
                    .get("prefix_cache_hit_ratio")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    * 100.0;
                let detail = format!(
                    "turn {} · in {} (cached {}) / out {} · cache {:.1}% · {}ms",
                    num_of("turn"),
                    num_of("input_tokens"),
                    num_of("cached_input_tokens"),
                    num_of("output_tokens"),
                    hit,
                    num_of("latency_ms")
                );
                renderer.status_line(StatusDot::Thinking, "thinking", &detail)
            }
            "policydecision" => {
                let decision = str_of("decision");
                let dot = if decision.contains("deny") {
                    StatusDot::Alert
                } else {
                    StatusDot::Tool
                };
                renderer.status_line(
                    dot,
                    "policy",
                    &format!("{} → {}", str_of("tool_name"), decision),
                )
            }
            "predictivecheck" => {
                let status = str_of("status");
                let dot = if status == "ok" {
                    StatusDot::Success
                } else if status == "skipped" {
                    StatusDot::Tool
                } else {
                    StatusDot::Alert
                };
                renderer.status_line(
                    dot,
                    "check",
                    &format!(
                        "{} · {} issue(s) · {}ms",
                        str_of("path"),
                        num_of("issue_count"),
                        num_of("duration_ms")
                    ),
                )
            }
            "revisioncaptured" => {
                let changed = payload
                    .get("changed_paths")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                renderer.status_line(
                    StatusDot::Success,
                    "revision",
                    &format!(
                        "rev {} · {} file(s) · {}",
                        crate::theme::short_rev(&str_of("revision_id")),
                        changed,
                        str_of("trigger")
                    ),
                )
            }
            // P6 (`v1.5.2`): name the child task id, its role, and that it is running.
            "subagentspawned" => renderer.status_line(
                StatusDot::Thinking,
                "subagent",
                &format!(
                    "{} ({}) running · worktree={}",
                    str_of("subagent_id"),
                    str_of("role"),
                    payload
                        .get("isolated_worktree")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                ),
            ),
            // P6 (`v1.5.2`): name the same child id, its terminal state and tokens used.
            "subagentcompleted" => {
                let status = str_of("status");
                let dot = if status == "completed" {
                    StatusDot::Success
                } else {
                    StatusDot::Alert
                };
                renderer.status_line(
                    dot,
                    "subagent",
                    &format!(
                        "{} {} · {} turn(s) · {} tok · {}ms",
                        str_of("subagent_id"),
                        status,
                        num_of("turns_used"),
                        num_of("tokens_used"),
                        num_of("duration_ms")
                    ),
                )
            }
            // P6 (`v1.5.2`): name the child id, its merge status and whether the delta was merged.
            "worktreemerged" => {
                let status = str_of("status");
                let merged = status == "applied";
                let dot = if merged {
                    StatusDot::Success
                } else {
                    StatusDot::Alert
                };
                let written = payload
                    .get("written_files")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                let conflicts = payload
                    .get("conflict_paths")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                let conflict_suffix = if conflicts > 0 {
                    format!(" · {} conflict(s)", conflicts)
                } else {
                    String::new()
                };
                renderer.status_line(
                    dot,
                    "subagent",
                    &format!(
                        "{} merged={} · {} file(s){}",
                        str_of("subagent_id"),
                        if merged { "yes".to_string() } else { status },
                        written,
                        conflict_suffix
                    ),
                )
            }
            "approvalrequested" => renderer.status_line(
                StatusDot::Alert,
                "approval",
                &format!(
                    "{} awaiting decision on {}",
                    str_of("tool_name"),
                    str_of("surface")
                ),
            ),
            "loopguardtriggered" => renderer.status_line(
                StatusDot::Alert,
                "loop-guard",
                &format!("{} blocked ({})", str_of("tool_name"), str_of("guard_kind")),
            ),
            "runterminated" => {
                let status = str_of("status");
                let dot = if status.to_ascii_lowercase().contains("completed") {
                    StatusDot::Success
                } else {
                    StatusDot::Error
                };
                renderer.status_line(
                    dot,
                    "run",
                    &format!(
                        "run {} · {} turn(s) · {} tool call(s)",
                        status,
                        num_of("turns"),
                        num_of("total_tool_calls")
                    ),
                )
            }
            _ => return None,
        };
        // Every live line is bounded to the detected terminal width (`status_line` already
        // sanitizes its label and detail).
        Some(format!(
            "{}\n",
            crate::theme::truncate_display(&line, crate::theme::terminal_width())
        ))
    }

    /// `v1.5.0`: run one prompt turn while streaming Design D status lines for its live events.
    ///
    /// The turn executes on a worker task so this task can drain `kroma/event` notifications and
    /// print them as they happen; the final [`ReplStepOutcome`] is returned to the caller, which
    /// owns the summary and telemetry footer.
    ///
    /// P1 (`v1.5.0`): this task also owns the *only* stdin reader while the turn is in flight. When
    /// the policy engine raises `approval/requested` (emitted live, before the synchronous policy
    /// wait) the prompt is rendered immediately, one line is read from the same reader, and the
    /// decision is delivered through `approval.respond`. The operator's `y`/`n` therefore answers
    /// the approval instead of being re-read as the next task.
    pub async fn run_turn_streaming<R, W>(
        &self,
        prompt: &str,
        reader: &mut R,
        writer: &mut W,
    ) -> Result<ReplStepOutcome, String>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        self.run_turn_streaming_inner(prompt, reader, writer, false)
            .await
    }

    /// Shared streaming-turn implementation.
    ///
    /// `force_prompt` dispatches `prompt` through [`InteractiveRepl::handle_prompt_line`] even when
    /// it begins with `/`, so a bracketed paste or `"""` block is never read as a slash command.
    async fn run_turn_streaming_inner<R, W>(
        &self,
        prompt: &str,
        reader: &mut R,
        writer: &mut W,
        force_prompt: bool,
    ) -> Result<ReplStepOutcome, String>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut rx = self.dispatcher.subscribe_notifications();
        let turn_repl = InteractiveRepl::new(self.dispatcher.clone());
        let prompt_owned = prompt.to_string();
        let mut handle = tokio::spawn(async move {
            if force_prompt {
                turn_repl.handle_prompt_line(prompt_owned.trim()).await
            } else {
                turn_repl.handle_line(&prompt_owned).await
            }
        });

        // P6 (`v1.5.2`): the last waiting state rendered this turn. A waiting line is emitted only
        // when the state changes, so no line is printed per event type when nothing changed.
        let mut last_waiting: Option<WaitingState> = None;
        let step = loop {
            tokio::select! {
                biased;
                res = &mut handle => {
                    break res.map_err(|e| format!("REPL turn task failed: {}", e))??;
                }
                maybe = rx.recv() => {
                    match maybe {
                        Some(note) if note.method == "approval/requested" => {
                            // The run is now blocked on the operator's decision.
                            let state = WaitingState::Approval(
                                note.params
                                    .get("tool_name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            );
                            if let Some(line) = waiting_line_if_changed(state, &mut last_waiting, |s| {
                                self.render_waiting_state(s)
                            }) {
                                writer.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
                                writer.write_all(b"\n").await.map_err(|e| e.to_string())?;
                                writer.flush().await.map_err(|e| e.to_string())?;
                            }
                            self.answer_approval_request(&note, reader, writer, &mut rx).await?;
                        }
                        Some(note) => {
                            if let Some(state) = waiting_state_for_event(&note) {
                                if let Some(line) = waiting_line_if_changed(state, &mut last_waiting, |s| {
                                    self.render_waiting_state(s)
                                }) {
                                    writer.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
                                    writer.write_all(b"\n").await.map_err(|e| e.to_string())?;
                                    writer.flush().await.map_err(|e| e.to_string())?;
                                }
                            }
                            if let Some(line) = self.render_live_event(&note) {
                                writer.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
                                writer.flush().await.map_err(|e| e.to_string())?;
                            }
                        }
                        // The dispatcher holds the subscriber sender for the runtime's lifetime, so
                        // a closed channel cannot happen mid-turn; yield rather than busy-loop.
                        None => {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                }
            }
        };
        // Flush any events that were queued just before termination.
        while let Ok(note) = rx.try_recv() {
            if let Some(line) = self.render_live_event(&note) {
                writer
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        writer.flush().await.map_err(|e| e.to_string())?;
        Ok(step)
    }

    /// P1 (`v1.5.0`): render a live approval request and read the operator's decision from the same
    /// reader the input loop owns, then deliver it through `approval.respond`.
    ///
    /// The wait is interruptible: if the broker resolves the request out of band (timeout, `^C`
    /// cancel, disconnect) an `approval/resolved` notification aborts the read so the turn can
    /// finish and the prompt returns.
    async fn answer_approval_request<R, W>(
        &self,
        note: &JsonRpcNotification,
        reader: &mut R,
        writer: &mut W,
        rx: &mut mpsc::UnboundedReceiver<JsonRpcNotification>,
    ) -> Result<(), String>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let info: ApprovalPendingInfo = match serde_json::from_value(note.params.clone()) {
            Ok(info) => info,
            Err(_) => ApprovalPendingInfo {
                request_id: note
                    .params
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                tool_name: note
                    .params
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                effect_class: String::new(),
                is_sensitive_path: false,
                target_path: None,
                arguments_summary: String::new(),
                risk_reason: String::new(),
                surface: String::new(),
            },
        };
        let request_id = info.request_id.clone();
        let banner = format_approval_prompt(&info);
        writer
            .write_all(banner.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        writer.flush().await.map_err(|e| e.to_string())?;

        let decision = loop {
            tokio::select! {
                line = read_bounded_jsonrpc_line(reader, MAX_RPC_FRAME_BYTES) => {
                    match line? {
                        Some(line) => break approval_decision_from_answer(&line).to_string(),
                        // EOF while an approval is pending: fail closed rather than hang.
                        None => break "deny".to_string(),
                    }
                }
                resolved = rx.recv() => {
                    match resolved {
                        Some(note) => {
                            let is_this = note.method == "approval/resolved"
                                && note.params.get("request_id").and_then(Value::as_str)
                                    == Some(request_id.as_str());
                            if is_this {
                                let why = note
                                    .params
                                    .get("decision")
                                    .and_then(Value::as_str)
                                    .unwrap_or("deny");
                                writer
                                    .write_all(
                                        format!(
                                            "\n● approval  resolved without an answer: {}\n",
                                            sanitize_terminal_text(why)
                                        )
                                        .as_bytes(),
                                    )
                                    .await
                                    .map_err(|e| e.to_string())?;
                                writer.flush().await.map_err(|e| e.to_string())?;
                                return Ok(());
                            }
                        }
                        // The dispatcher holds the subscriber sender for the runtime's lifetime, so
                        // a closed channel cannot happen mid-turn; yield rather than busy-loop.
                        None => {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                }
            }
        };

        let resp = self
            .dispatcher
            .handle_request(JsonRpcRequest::new(
                0,
                "approval.respond",
                json!({ "request_id": request_id, "decision": decision }),
            ))
            .await;
        let summary = match resp.error {
            Some(err) => sanitize_terminal_text(&err.message),
            None => decision.clone(),
        };
        writer
            .write_all(format!("● approval  {}\n", summary).as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        writer.flush().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Drive the interactive REPL loop over arbitrary async buffered reader and writer streams.
    pub async fn run_io<R, W>(&self, reader: R, writer: W) -> Result<(), String>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        self.run_io_with_initial_prompt(reader, writer, None).await
    }

    /// Drive the interactive REPL loop, optionally executing an initial `--prompt`/`--task` as Turn
    /// 1 before entering the interactive input loop.
    pub async fn run_io_with_initial_prompt<R, W>(
        &self,
        mut reader: R,
        mut writer: W,
        initial_prompt: Option<&str>,
    ) -> Result<(), String>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let banner = self.render_status_banner().await;
        writer
            .write_all(banner.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        writer.flush().await.map_err(|e| e.to_string())?;

        if let Some(prompt) = initial_prompt.map(str::trim).filter(|s| !s.is_empty()) {
            if !prompt.starts_with('/') {
                self.record_history(prompt);
            }
            let step = if prompt.starts_with('/') {
                self.handle_line(prompt).await?
            } else {
                // `v1.5.0`: stream Design D status lines for the live events of this turn.
                self.run_turn_streaming(prompt, &mut reader, &mut writer)
                    .await?
            };
            if !step.output.is_empty() {
                writer
                    .write_all(step.output.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
                writer.flush().await.map_err(|e| e.to_string())?;
            }
            if step.should_exit {
                return Ok(());
            }
        }

        loop {
            // P2 (`v1.5.0`): recompute the prefix every iteration so `/theme` and `/ui` switches take
            // effect on the very next prompt instead of staying stuck on the startup theme.
            let renderer = self.dispatcher.theme_renderer();
            let prompt_prefix = if renderer.theme() == Theme::Zen {
                renderer.prompt_prefix()
            } else {
                "kroma> ".to_string()
            };
            writer
                .write_all(prompt_prefix.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            writer.flush().await.map_err(|e| e.to_string())?;

            let Some(submission) = read_submission(&mut reader, &mut writer).await? else {
                break;
            };
            if submission.text.trim().is_empty() {
                continue;
            }

            // P6 (`v1.5.2`): `!N` (1-based, most recent first) / `!!` re-run a history entry. A
            // paste / block is always submitted literally and is never treated as a recall.
            let effective = if submission.literal_prompt {
                submission.text.clone()
            } else {
                match self.expand_history_recall(&submission.text) {
                    HistoryRecall::NotRecall => submission.text.clone(),
                    HistoryRecall::Text(text) => text,
                    HistoryRecall::Error(message) => {
                        writer
                            .write_all(message.as_bytes())
                            .await
                            .map_err(|e| e.to_string())?;
                        writer.flush().await.map_err(|e| e.to_string())?;
                        continue;
                    }
                }
            };
            if effective.trim().is_empty() {
                continue;
            }
            // Only task prompts are remembered; slash commands are not history entries. A *literal*
            // prompt (bracketed paste, `"""` block, backslash continuation) is also skipped when it
            // looks like a slash command: recalling it later via `!N`/`!!` would expand to text that
            // the recall path treats as a command, which would silently defeat the paste protection
            // that exists precisely so a pasted `/quit` is never executed.
            let is_command = !submission.literal_prompt && effective.trim_start().starts_with('/');
            let recallable = !effective.trim_start().starts_with('/');
            if !is_command && recallable {
                self.record_history(&effective);
            }

            let step = if is_command {
                self.handle_line(&effective).await?
            } else {
                self.run_turn_streaming_inner(
                    &effective,
                    &mut reader,
                    &mut writer,
                    submission.literal_prompt,
                )
                .await?
            };
            if !step.output.is_empty() {
                writer
                    .write_all(step.output.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
                writer.flush().await.map_err(|e| e.to_string())?;
            }
            if step.should_exit {
                break;
            }
        }
        Ok(())
    }
}

/// Read one complete user submission, honoring backslash continuation, `"""` heredoc blocks and
/// bracketed-paste bursts.
///
/// Returns `None` only when the stream ends with nothing collected. EOF while a submission is
/// still open submits whatever was collected: it never hangs and never drops the operator's text.
async fn read_submission<R, W>(reader: &mut R, writer: &mut W) -> Result<Option<Submission>, String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let Some(first) = read_bounded_jsonrpc_line(reader, MAX_RPC_FRAME_BYTES).await? else {
        return Ok(None);
    };

    // Bracketed paste: `ESC[200~ ... ESC[201~` is one literal input. The burst may span several
    // lines; no inner line is interpreted as a slash command.
    let (first, paste_start, paste_end) = split_paste_markers(&first);
    if paste_start {
        let mut collected = vec![first];
        if !paste_end {
            loop {
                let Some(next) = read_bounded_jsonrpc_line(reader, MAX_RPC_FRAME_BYTES).await?
                else {
                    break;
                };
                let (next, _, end) = split_paste_markers(&next);
                collected.push(next);
                if end {
                    break;
                }
            }
        }
        return Ok(Some(Submission {
            text: collected.join("\n"),
            literal_prompt: true,
        }));
    }

    // `"""` block: read until a line that is exactly `"""`, then strip the terminator.
    if first == "\"\"\"" {
        let mut collected: Vec<String> = Vec::new();
        loop {
            write_continuation_prompt(writer).await?;
            let Some(next) = read_bounded_jsonrpc_line(reader, MAX_RPC_FRAME_BYTES).await? else {
                if collected.is_empty() {
                    return Ok(None);
                }
                break;
            };
            if next == "\"\"\"" {
                break;
            }
            collected.push(next);
        }
        return Ok(Some(Submission {
            text: collected.join("\n"),
            literal_prompt: true,
        }));
    }

    // Backslash continuation: read until the first line not ending in an unescaped backslash.
    if ends_with_unescaped_backslash(&first) {
        let mut collected: Vec<String> = Vec::new();
        let mut line = first;
        loop {
            collected.push(strip_continuation_backslash(&line).to_string());
            write_continuation_prompt(writer).await?;
            let Some(next) = read_bounded_jsonrpc_line(reader, MAX_RPC_FRAME_BYTES).await? else {
                break;
            };
            line = next;
            if !ends_with_unescaped_backslash(&line) {
                collected.push(line);
                break;
            }
        }
        return Ok(Some(Submission {
            text: collected.join("\n"),
            literal_prompt: true,
        }));
    }

    Ok(Some(Submission {
        text: first,
        literal_prompt: false,
    }))
}

/// Write the short continuation prompt shown while a submission is being accumulated.
async fn write_continuation_prompt<W>(writer: &mut W) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
{
    writer
        .write_all(CONTINUATION_PROMPT.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    writer.flush().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod input_assembly_tests {
    use super::*;

    #[test]
    fn trailing_backslash_detection_honors_escaping() {
        assert!(ends_with_unescaped_backslash("line \\"));
        assert!(ends_with_unescaped_backslash("line\\\\\\"));
        assert!(!ends_with_unescaped_backslash("line"));
        assert!(!ends_with_unescaped_backslash("line\\\\"));
        assert_eq!(strip_continuation_backslash("line \\"), "line ");
    }

    #[test]
    fn paste_markers_are_split_and_stripped() {
        let (cleaned, start, end) = split_paste_markers("\x1b[200~hello\x1b[201~");
        assert_eq!(cleaned, "hello");
        assert!(start && end);

        // A stray end marker is removed, not forwarded.
        let (cleaned, start, end) = split_paste_markers("plain\x1b[201~");
        assert_eq!(cleaned, "plain");
        assert!(!start && end);

        let (cleaned, start, end) = split_paste_markers("untouched");
        assert_eq!(cleaned, "untouched");
        assert!(!start && !end);
    }

    #[test]
    fn waiting_line_is_emitted_only_on_state_change() {
        let render = |state: &WaitingState| format!("{state:?}");
        let mut last: Option<WaitingState> = None;

        assert!(waiting_line_if_changed(WaitingState::Provider, &mut last, render).is_some());
        assert!(waiting_line_if_changed(WaitingState::Provider, &mut last, render).is_none());
        assert!(
            waiting_line_if_changed(WaitingState::Tool("read".to_string()), &mut last, render)
                .is_some()
        );
        assert!(
            waiting_line_if_changed(WaitingState::Tool("read".to_string()), &mut last, render)
                .is_none()
        );
        assert!(waiting_line_if_changed(WaitingState::Provider, &mut last, render).is_some());
    }

    #[test]
    fn waiting_state_maps_from_live_events() {
        let note = |event_type: &str, payload: Value| {
            JsonRpcNotification::new(
                "kroma/event",
                json!({ "event_type": event_type, "payload": payload }),
            )
        };
        assert_eq!(
            waiting_state_for_event(&note("model_request_started", json!({}))),
            Some(WaitingState::Provider)
        );
        assert_eq!(
            waiting_state_for_event(&note("tool_started", json!({ "tool_name": "read" }))),
            Some(WaitingState::Tool("read".to_string()))
        );
        assert_eq!(
            waiting_state_for_event(&note("approval_requested", json!({ "tool_name": "bash" }))),
            Some(WaitingState::Approval("bash".to_string()))
        );
        // A non-blocking event does not change the waiting state.
        assert_eq!(
            waiting_state_for_event(&note("model_request_completed", json!({}))),
            None
        );
    }
}
