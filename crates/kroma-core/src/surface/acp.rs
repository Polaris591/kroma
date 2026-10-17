//! Agent Client Protocol (ACP) v1 adapter (F06, `v1.5.2`).
//!
//! Kroma's internal JSON-RPC API is dot-namespaced (`session.prompt`, `revision.list`,
//! `acp.initialize`, ...). It is *not* the Agent Client Protocol and is retained untouched for the
//! `serve` daemon and the REPL. A runtime built with [`KromaRuntimeConfig::with_acp_protocol`]
//! (the `kroma acp` stdio surface) additionally speaks the official ACP v1 method names so that a
//! standard client (Zed, JetBrains, ...) can connect:
//!
//! * `initialize` → `{ protocolVersion, agentCapabilities, agentInfo, authMethods }`
//! * `session/new` → `{ sessionId }`
//! * `session/prompt` → `{ stopReason }`, accepting `prompt: [ContentBlock]`
//! * `session/cancel` (notification) → cancels the in-flight prompt
//! * `session/update` (notification) → streams `agent_message_chunk` / `tool_call` /
//!   `tool_call_update`
//! * `session/request_permission` (agent → client request) → gates mutating/shell tools
//!
//! Capability negotiation is honest: the adapter advertises exactly what it implements, so it does
//! not claim `session/load`, image/audio/embedded-resource prompt blocks, or MCP servers.
//!
//! Protocol reference (v1 stable schema):
//! <https://github.com/agentclientprotocol/agent-client-protocol/blob/main/schema/v1/schema.json>.

use super::*;

/// Integer `protocolVersion` negotiated by this adapter. ACP only bumps this for breaking changes.
pub const ACP_V1_PROTOCOL_VERSION: u16 = 1;

/// Methods defined by ACP v1 that a client may send to the agent and this adapter answers directly.
///
/// `session/update` and `session/request_permission` are agent → client messages and are therefore
/// not listed here.
pub(crate) const ACP_CLIENT_METHODS: &[&str] = &[
    "initialize",
    "authenticate",
    "session/new",
    "session/prompt",
    "session/cancel",
    "session/load",
    "session/set_mode",
];

/// True when `method` is an ACP v1 client → agent method name.
pub(crate) fn is_acp_client_method(method: &str) -> bool {
    ACP_CLIENT_METHODS.contains(&method)
}

fn acp_invalid_params(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError::new(JSONRPC_INVALID_PARAMS, message)
}

/// The `agentCapabilities` object advertised by `initialize`. Every `true` here must correspond to
/// an implemented behaviour; currently none of the optional capabilities are claimed.
fn acp_agent_capabilities() -> Value {
    json!({
        "loadSession": false,
        "promptCapabilities": {
            "image": false,
            "audio": false,
            "embeddedContext": false
        },
        "mcpCapabilities": {
            "http": false,
            "sse": false
        },
        "sessionCapabilities": {}
    })
}

/// Map a Kroma tool name to an ACP `ToolKind`.
pub(crate) fn acp_tool_kind(tool_name: &str) -> &'static str {
    match tool_name {
        "read" | "ls" => "read",
        "edit" | "write" => "edit",
        "find" | "grep" | "code_graph" | "recall_evidence" => "search",
        "bash" => "execute",
        _ => "other",
    }
}

/// Human-readable title for a tool call; falls back to the bare tool name.
fn acp_tool_title(tool_name: &str, relative_path: Option<&str>) -> String {
    match relative_path {
        Some(path) if !path.is_empty() => format!("{} {}", tool_name, path),
        _ => tool_name.to_string(),
    }
}

/// Translate one Kroma diagnostic event into an ACP `session/update` payload, or `None` for events
/// ACP does not model.
pub(crate) fn acp_update_for_event(ev: &DiagnosticEvent, workspace_root: &Path) -> Option<Value> {
    match &ev.event {
        EventPayload::ToolStarted {
            call_id,
            tool_name,
            relative_path,
            ..
        } => {
            let mut update = json!({
                "sessionUpdate": "tool_call",
                "toolCallId": call_id.as_str(),
                "title": acp_tool_title(tool_name, relative_path.as_deref()),
                "name": tool_name,
                "kind": acp_tool_kind(tool_name),
                "status": "in_progress",
            });
            if let Some(rel) = relative_path {
                let absolute = workspace_root.join(rel).display().to_string();
                if let Value::Object(ref mut map) = update {
                    map.insert("locations".to_string(), json!([{ "path": absolute }]));
                }
            }
            Some(update)
        }
        EventPayload::ToolCompleted {
            call_id,
            tool_name,
            outcome,
            ..
        } => Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call_id.as_str(),
            "title": acp_tool_title(tool_name, None),
            "name": tool_name,
            "kind": acp_tool_kind(tool_name),
            "status": if outcome == "success" { "completed" } else { "failed" },
        })),
        _ => None,
    }
}

/// Build a `session/update` notification envelope.
pub(crate) fn acp_session_update(session_id: &str, update: Value) -> JsonRpcNotification {
    JsonRpcNotification::new(
        "session/update",
        json!({ "sessionId": session_id, "update": update }),
    )
}

/// Map a run result to a stable ACP `StopReason`.
///
/// `run_status` is the string returned by the internal prompt result; `terminal_reason` is its
/// `Debug` rendering. The stable v1 schema has no "error" stop reason, so a hard provider failure
/// is reported as `refusal` (the closest documented value) rather than a JSON-RPC error, because
/// the turn did run and may have produced side effects.
fn acp_stop_reason(run_status: &str, terminal_reason: &str) -> &'static str {
    match run_status {
        "completed" => "end_turn",
        "cancelled" => "cancelled",
        "budget_exhausted" => match terminal_reason {
            "TokenBudgetExhausted" | "ContextBudgetExceeded" => "max_tokens",
            _ => "max_turn_requests",
        },
        _ => "refusal",
    }
}

/// Flatten an ACP `prompt: [ContentBlock]` array into the single user-message string the internal
/// runner consumes.
///
/// Only the baseline block types (text and resource_link) are accepted, matching the advertised
/// `promptCapabilities`. Image/audio/embedded-resource blocks are rejected with `-32602` instead of
/// being silently dropped, because the client was told those capabilities are unsupported.
fn acp_prompt_text(prompt: &Value) -> Result<String, JsonRpcError> {
    let blocks = prompt
        .as_array()
        .ok_or_else(|| acp_invalid_params("'prompt' must be an array of content blocks"))?;
    if blocks.is_empty() {
        return Err(acp_invalid_params(
            "'prompt' must contain at least one content block",
        ));
    }
    let mut parts: Vec<String> = Vec::with_capacity(blocks.len());
    for (idx, block) in blocks.iter().enumerate() {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| acp_invalid_params(format!("prompt[{}] is missing 'type'", idx)))?;
        match block_type {
            "text" => {
                let text = block.get("text").and_then(Value::as_str).ok_or_else(|| {
                    acp_invalid_params(format!("prompt[{}].text must be a string", idx))
                })?;
                parts.push(text.to_string());
            }
            "resource_link" => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("resource");
                let uri = block.get("uri").and_then(Value::as_str).ok_or_else(|| {
                    acp_invalid_params(format!("prompt[{}].uri must be a string", idx))
                })?;
                parts.push(format!("[resource_link {} ({})]", name, uri));
            }
            other => {
                return Err(acp_invalid_params(format!(
                    "prompt[{}] content block type '{}' is not supported by this agent",
                    idx, other
                )));
            }
        }
    }
    let text = parts.join("\n\n");
    if text.trim().is_empty() {
        return Err(acp_invalid_params("'prompt' produced empty text"));
    }
    Ok(text)
}

impl RpcDispatcher {
    /// ACP `initialize`. Requires the client's integer `protocolVersion`; responds with the version
    /// this agent speaks (1) plus its capabilities and identity.
    pub(crate) async fn acp_initialize(&self, params: &Value) -> Result<Value, JsonRpcError> {
        params
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                acp_invalid_params("missing integer 'protocolVersion' parameter for initialize")
            })?;
        Ok(json!({
            "protocolVersion": ACP_V1_PROTOCOL_VERSION,
            "agentCapabilities": acp_agent_capabilities(),
            "agentInfo": {
                "name": "kroma",
                "title": "Kroma",
                "version": crate::version(),
            },
            // No authentication method is used on a locally launched stdio agent.
            "authMethods": []
        }))
    }

    /// ACP `session/new`. `cwd` and `mcpServers` are required by the v1 schema; the adapter creates a
    /// fresh session and returns its `sessionId`, then routes subsequent prompts to it.
    pub(crate) async fn acp_session_new(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or_else(|| acp_invalid_params("missing string 'cwd' parameter for session/new"))?;
        if !Path::new(cwd).is_absolute() {
            return Err(acp_invalid_params("'cwd' must be an absolute path"));
        }
        // The engine owns one workspace per runtime, so a session may only be created for that
        // workspace. Refuse a mismatch instead of silently operating on a different directory than
        // the client believes it selected.
        let requested = Path::new(cwd);
        let workspace = self.workspace_root();
        let same = match (requested.canonicalize(), workspace.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => {
                crate::workspace::strip_windows_verbatim_prefix(requested)
                    == crate::workspace::strip_windows_verbatim_prefix(workspace)
            }
        };
        if !same {
            return Err(acp_invalid_params(format!(
                "session cwd '{}' is not this agent's workspace '{}'",
                cwd,
                workspace.display()
            )));
        }
        let mcp = params.get("mcpServers").ok_or_else(|| {
            acp_invalid_params("missing 'mcpServers' array parameter for session/new")
        })?;
        let mcp = mcp
            .as_array()
            .ok_or_else(|| acp_invalid_params("'mcpServers' must be an array"))?;
        if !mcp.is_empty() {
            return Err(acp_invalid_params(
                "this agent does not support MCP servers; 'mcpServers' must be empty",
            ));
        }

        let created = self.rpc_session_new(&json!({})).await?;
        let session_id = created
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INTERNAL_ERROR,
                    "session/new did not return a session id",
                )
            })?
            .to_string();
        // Route permission requests to the session this connection just created.
        self.approval_broker().set_acp_session_id(&session_id);
        Ok(json!({ "sessionId": session_id }))
    }

    /// ACP `session/prompt`. Validates the `sessionId` route, flattens the prompt content blocks,
    /// runs the turn (streaming `session/update` notifications), and maps the outcome to a
    /// `stopReason`.
    pub(crate) async fn acp_session_prompt(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let session_id = self.acp_owned_session(params)?;
        let prompt = params
            .get("prompt")
            .ok_or_else(|| acp_invalid_params("missing 'prompt' parameter for session/prompt"))?;
        let prompt_text = acp_prompt_text(prompt)?;

        let result = self.run_session_prompt(prompt_text, params).await?;
        let status = result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("failed");
        let terminal = result
            .get("terminal_reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(text) = result.get("final_text").and_then(Value::as_str) {
            if !text.trim().is_empty() {
                self.emit_notification(acp_session_update(
                    &session_id,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": text }
                    }),
                ));
            }
        }
        Ok(json!({ "stopReason": acp_stop_reason(status, terminal) }))
    }

    /// ACP `session/cancel` (a notification): cancel the in-flight prompt of the routed session.
    pub(crate) async fn acp_session_cancel(&self, params: &Value) -> Result<(), JsonRpcError> {
        self.acp_owned_session(params)?;
        self.rpc_session_cancel().await?;
        Ok(())
    }

    /// Resolve the `sessionId` an ACP request names and refuse a misrouted request.
    pub(crate) fn acp_owned_session(&self, params: &Value) -> Result<String, JsonRpcError> {
        let requested = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| acp_invalid_params("missing non-empty 'sessionId' parameter"))?;
        let owned = self.session_id();
        if requested != owned {
            return Err(JsonRpcError::with_data(
                JSONRPC_INVALID_PARAMS,
                format!(
                    "this runtime owns session {} but the request targets {}",
                    owned, requested
                ),
                json!({ "sessionId": requested, "expectedSessionId": owned }),
            ));
        }
        Ok(requested.to_string())
    }
}
