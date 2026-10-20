//! Minimal, standard Model Context Protocol (MCP) stdio adapter (capability gap #6).
//!
//! The review found that MCP was only mentioned in the ACP capability handshake and never actually
//! implemented. This module implements the *transport and message layer* of the official MCP stdio
//! binding — newline-delimited JSON-RPC 2.0 over a child process's stdin/stdout — so a Kroma
//! session can talk to a real MCP server without a per-vendor shim:
//!
//! * [`McpClient::spawn`] launches a server process with piped stdio.
//! * [`McpClient::initialize`] performs the standard `initialize` handshake and reads the server's
//!   negotiated `protocolVersion` and capabilities.
//! * [`McpClient::list_tools`] issues `tools/list` and maps each descriptor into the same
//!   [`RegisteredTool`] shape the kernel registry uses, so an MCP tool can be exposed like any
//!   other scoped tool.
//! * [`McpClient::call_tool`] issues `tools/call` and returns the concatenated text content.
//!
//! Every read is bounded ([`MAX_MCP_MESSAGE_BYTES`]) and every response is awaited with a timeout,
//! so a hostile or wedged server cannot exhaust kernel memory or hang a turn.
//!
//! ## Call lifetime
//!
//! One host worker thread serializes every server call. Each request carries an absolute deadline
//! and a cancellation flag, so a call the caller has already abandoned (timeout, run cancellation,
//! or extension unmount) can never be executed later out of the queue. The worker's write *and* read
//! are bounded by that same deadline, so a server that stops reading its stdin cannot wedge the
//! worker, and shutdown has priority: it aborts in-flight I/O, reaps the children, and joins the
//! worker within a bounded window.

use crate::process_group::ChildProcessGroup;
use crate::process_launch::{EnvironmentPolicy, ProcessLaunch};
use crate::tool::definition::{RegisteredTool, ToolDefinition, ToolMetadata};
use crate::tools::ToolExecutionOutput;
use crate::types::{EffectClass, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::watch;

/// Hard ceiling on one JSON-RPC message read from (or written to) a server.
pub const MAX_MCP_MESSAGE_BYTES: usize = 1024 * 1024;
/// Maximum number of `tools/call` requests allowed to wait in the host worker queue while one
/// further call is executing. The queue is bounded so a burst of concurrent callers cannot grow it
/// without limit; the call beyond the bound is rejected before it reaches any server.
const MAX_MCP_QUEUE_DEPTH: usize = 16;
/// Conservative margin for the JSON-RPC envelope (method, id, `params` braces) around the queued
/// tool name and arguments, so a request admitted under `MAX_MCP_MESSAGE_BYTES -
/// MAX_MCP_REQUEST_ENVELOPE_BYTES` also fits the exact wire frame [`McpClient::write_message`]
/// enforces.
const MAX_MCP_REQUEST_ENVELOPE_BYTES: usize = 256;
/// The MCP revision this adapter speaks. A server may negotiate a different one back.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// T13 (KR-19): the versions this client actually implements. Deliberately a list of one: the
/// declared support range is 2024-11-05, and claiming more without testing it would be a lie in the
/// other direction. A server that answers with anything else is refused *before* the handshake
/// completes.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[MCP_PROTOCOL_VERSION];

/// T13: bounds for one `tools/list` walk. A hostile or broken server must not be able to make the
/// host allocate or loop without end.
pub const MAX_TOOL_PAGES: usize = 32;
pub const MAX_CATALOG_TOOLS: usize = 256;
pub const MAX_CATALOG_SCHEMA_BYTES: usize = 256 * 1024;
/// Default timeout awaiting a single server response.
pub const DEFAULT_MCP_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a caller waiting on a host reply re-checks for cancellation. Short enough that an
/// unmount is observed well inside the unmount drain window, without busy-spinning.
const CALL_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Grace between `SIGTERM` and `SIGKILL` when sweeping a server's process group on shutdown.
const SHUTDOWN_KILL_GRACE: Duration = Duration::from_millis(200);
/// Upper bound on the worker join during shutdown. The shutdown signal aborts in-flight I/O, so this
/// is a safety net rather than the expected path.
const SHUTDOWN_JOIN_BOUND: Duration = Duration::from_secs(2);
/// Prefix that namespaces an MCP tool inside the kernel registry.
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// Errors from the MCP client. Never swallowed; the caller decides how to degrade.
#[derive(Debug)]
pub enum McpError {
    Spawn(String),
    Io(String),
    Timeout(Duration),
    /// The call was cancelled (the caller gave up or the owning extension was unmounted) before it
    /// could run, so the server never saw it.
    Cancelled,
    /// F05: The call was cancelled after it was dispatched to the server; execution state is
    /// unknown and side-effects may have occurred.
    StartedCancelled,
    /// F05: The call timed out after it was dispatched to the server; execution state is
    /// unknown and side-effects may have occurred.
    StartedDeadline(Duration),
    MessageTooLarge(usize),
    /// The host worker queue is full: the call was admitted to no server and never ran. Retryable.
    Overloaded,
    /// The serialized request (tool name + arguments) exceeds the host admission cap, so it was
    /// never queued and never sent to any server.
    RequestTooLarge(usize),
    Protocol(String),
    Server {
        code: i64,
        message: String,
    },
    NoStdio,
    /// T13 (KR-19): the server negotiated a protocol version this client does not implement. The
    /// handshake stops before `notifications/initialized`, so the server is never told the session is
    /// live.
    UnsupportedProtocolVersion {
        server: String,
        supported: Vec<String>,
    },
    /// T13: the server did not declare a capability the call requires (`tools`). Asking anyway would
    /// be a protocol violation the server is free to answer however it likes.
    CapabilityMissing {
        capability: String,
    },
    /// T13: `tools/list` exceeded a catalog budget (pages, tools, or schema bytes).
    CatalogBudgetExceeded {
        reason: String,
    },
    /// T13: the server returned a cursor it had already returned — the pagination would never end.
    CursorCycle {
        cursor: String,
    },
    /// T13: two entries in the catalog share a name, so the mapping to host tools would be ambiguous.
    DuplicateToolName {
        name: String,
    },
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Spawn(msg) => write!(f, "failed to spawn MCP server: {}", msg),
            McpError::Io(msg) => write!(f, "MCP transport error: {}", msg),
            McpError::Timeout(d) => write!(f, "MCP server did not respond within {:?}", d),
            McpError::Cancelled => write!(f, "MCP call was cancelled before it ran"),
            McpError::StartedCancelled => write!(
                f,
                "MCP call was cancelled after being sent to server; execution state unknown and side-effects may have occurred"
            ),
            McpError::StartedDeadline(d) => write!(
                f,
                "MCP call timed out after {:?} while executing on server; execution state unknown and side-effects may have occurred",
                d
            ),
            McpError::MessageTooLarge(n) => {
                write!(f, "MCP message of {} bytes exceeds the cap", n)
            }
            McpError::Overloaded => write!(
                f,
                "MCP host queue is full (reason=mcp_queue_full, retryable); the call was not sent to any server and did not run"
            ),
            McpError::RequestTooLarge(n) => write!(
                f,
                "MCP request of {} bytes exceeds the admission cap; the call was not sent to any server and did not run",
                n
            ),
            McpError::Protocol(msg) => write!(f, "MCP protocol error: {}", msg),
            McpError::Server { code, message } => {
                write!(f, "MCP server error {}: {}", code, message)
            }
            McpError::NoStdio => write!(f, "MCP child did not expose piped stdio"),
            McpError::UnsupportedProtocolVersion { server, supported } => write!(
                f,
                "MCP server negotiated protocol version '{}', which this client does not support (supported: {})",
                server,
                supported.join(", ")
            ),
            McpError::CapabilityMissing { capability } => write!(
                f,
                "MCP server did not declare the '{}' capability; refusing to call it",
                capability
            ),
            McpError::CatalogBudgetExceeded { reason } => {
                write!(f, "MCP tool catalog exceeded its budget: {}", reason)
            }
            McpError::CursorCycle { cursor } => write!(
                f,
                "MCP tools/list returned a cursor it had already returned ('{}'); stopping",
                cursor
            ),
            McpError::DuplicateToolName { name } => write!(
                f,
                "MCP server listed tool '{}' twice; the catalog is ambiguous",
                name
            ),
        }
    }
}

impl std::error::Error for McpError {}

/// A tool descriptor parsed from `tools/list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

impl McpToolDescriptor {
    /// Namespace the tool name so two servers cannot collide, and mark it `Mutating` unless the
    /// caller opts a read-only server in. MCP does not carry an effect class, so the safe default is
    /// the stricter one.
    pub fn to_registered_tool(&self, server_id: &str, effect_class: EffectClass) -> RegisteredTool {
        let schema = if self.input_schema.is_null() {
            json!({ "type": "object", "properties": {} })
        } else {
            self.input_schema.clone()
        };
        RegisteredTool {
            definition: ToolDefinition {
                name: format!("{}{}__{}", MCP_TOOL_PREFIX, server_id, self.name),
                description: self.description.clone(),
                parameters: schema,
            },
            metadata: ToolMetadata {
                effect_class,
                default_timeout_sec: 60,
                inline_output_cap_bytes: 64 * 1024,
            },
        }
    }
}

/// The server's negotiated initialize result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpInitializeResult {
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(default, rename = "serverInfo")]
    pub server_info: Value,
}

/// A newline-delimited JSON-RPC 2.0 client over a child process's stdio.
pub struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Process group (Unix) / Job Object (Windows) so shutdown can sweep the whole server tree.
    group: ChildProcessGroup,
    next_id: i64,
    timeout: Duration,
    /// Set when an aborted or interrupted write left the byte stream in an unknown state. A poisoned
    /// client refuses further calls instead of writing into a corrupted request line.
    poisoned: bool,
    /// T13: capabilities the server declared during `initialize`. `None` until the handshake
    /// completes, which is why `list_tools` before `initialize` is a protocol error rather than a
    /// guess.
    server_capabilities: Option<Value>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("next_id", &self.next_id)
            .field("timeout", &self.timeout)
            .field("poisoned", &self.poisoned)
            .finish()
    }
}

impl McpClient {
    /// Spawn an MCP server process. `envs` is applied in addition to the inherited environment;
    /// credentials must be passed by the caller, never logged.
    pub fn spawn(
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        timeout: Duration,
    ) -> Result<Self, McpError> {
        if program.trim().is_empty() {
            return Err(McpError::Spawn(
                "program command cannot be empty".to_string(),
            ));
        }
        // F04: the shared launch seam purges inherited environment variables (API keys, tokens,
        // etc.) via the credential scrub, then applies the explicitly authorized server-specific
        // variables. `SanitizedWithCustomNoBaseline` preserves the long-lived server's inherited
        // environment apart from the scrub (no `CI`/`NO_COLOR` injection).
        let (mut child, group) = ProcessLaunch::new(program, args.to_vec())
            .env_policy(EnvironmentPolicy::SanitizedWithCustomNoBaseline(
                envs.to_vec(),
            ))
            .stdio(Stdio::piped(), Stdio::piped(), Stdio::null())
            // A dropped client must never leave an orphaned server process behind.
            .kill_on_drop(true)
            .spawn_tokio()
            .map_err(|e| McpError::Spawn(e.to_string()))?;
        let stdin = child.stdin.take().ok_or(McpError::NoStdio)?;
        let stdout = child.stdout.take().ok_or(McpError::NoStdio)?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            group,
            next_id: 1,
            timeout,
            poisoned: false,
            server_capabilities: None,
        })
    }

    fn next_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    async fn write_message(&mut self, value: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_vec(value).map_err(|e| McpError::Protocol(e.to_string()))?;
        ensure_wire_line_within_cap(line.len())?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .map_err(|e| McpError::Io(e.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| McpError::Io(e.to_string()))
    }

    /// Read one newline-delimited JSON message, bounded by [`MAX_MCP_MESSAGE_BYTES`].
    async fn read_message(&mut self) -> Result<Value, McpError> {
        if self.poisoned {
            return Err(McpError::Io(
                "MCP transport was poisoned by an interrupted request".to_string(),
            ));
        }
        match read_message_bounded(&mut self.stdout, MAX_MCP_MESSAGE_BYTES).await {
            Ok(value) => Ok(value),
            Err(e) => {
                self.poison();
                Err(e)
            }
        }
    }

    /// Mark the transport unusable. An interrupted write may have left a partial JSON-RPC line on
    /// the pipe, so a later request could be misparsed by the server; failing fast is safer than
    /// writing into a corrupted stream.
    fn poison(&mut self) {
        self.poisoned = true;
    }

    /// Bounded write. A write that does not finish before `deadline` poisons the client, because an
    /// interrupted line would corrupt every later request.
    async fn write_message_until(
        &mut self,
        value: &Value,
        deadline: Instant,
    ) -> Result<(), McpError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(McpError::Timeout(self.timeout));
        }
        match tokio::time::timeout(remaining, self.write_message(value)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                self.poison();
                Err(McpError::Timeout(self.timeout))
            }
        }
    }

    /// Send a request and await its matching response, ignoring unrelated notifications.
    ///
    /// Both the write and the read are bounded by `deadline`: a server that stops reading its stdin
    /// can no longer wedge the worker past the caller's timeout. If the write is interrupted the
    /// client is poisoned, because a partial line would corrupt every later request.
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, McpError> {
        if self.poisoned {
            return Err(McpError::Io(
                "MCP transport was poisoned by an interrupted request".to_string(),
            ));
        }
        let id = self.next_id();
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        // The write is *inside* the timeout. Previously it was unbounded, so stdin backpressure
        // from a server that stopped reading could block the worker forever.
        self.write_message_until(&request, deadline).await?;

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            self.poison();
            return Err(McpError::Timeout(self.timeout));
        }
        let response_res = tokio::time::timeout(remaining, async {
            loop {
                let message = self.read_message().await?;
                // A notification has no id; skip until the response for our request arrives.
                if message.get("id").and_then(Value::as_i64) == Some(id) {
                    return Ok::<Value, McpError>(message);
                }
            }
        })
        .await;
        let response = match response_res {
            Ok(res) => res?,
            Err(_) => {
                self.poison();
                return Err(McpError::Timeout(self.timeout));
            }
        };

        if let Some(error) = response.get("error") {
            return Err(McpError::Server {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown server error")
                    .to_string(),
            });
        }
        response.get("result").cloned().ok_or_else(|| {
            McpError::Protocol("response carried neither result nor error".to_string())
        })
    }

    /// Standard `initialize` handshake. Sends the client's protocol version and capabilities and
    /// returns what the server negotiated.
    pub async fn initialize(&mut self) -> Result<McpInitializeResult, McpError> {
        let deadline = Instant::now() + self.timeout;
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "kroma", "version": env!("CARGO_PKG_VERSION") },
                }),
                deadline,
            )
            .await?;
        let negotiated: McpInitializeResult =
            serde_json::from_value(result).map_err(|e| McpError::Protocol(e.to_string()))?;
        // T13 (KR-19): validate *before* telling the server the session is live. The 2024-11-05 spec
        // lets the server answer with a different version than the one requested, and a client that
        // ignores that mismatch will misinterpret every later frame.
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&negotiated.protocol_version.as_str()) {
            return Err(McpError::UnsupportedProtocolVersion {
                server: negotiated.protocol_version.clone(),
                supported: SUPPORTED_PROTOCOL_VERSIONS
                    .iter()
                    .map(|v| (*v).to_string())
                    .collect(),
            });
        }
        self.server_capabilities = Some(negotiated.capabilities.clone());
        // The spec requires a `notifications/initialized` notification once the handshake completes.
        let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        self.write_message_until(&notification, deadline).await?;
        Ok(negotiated)
    }

    /// Did the server declare `capability` during the handshake?
    ///
    /// A server that declared no capabilities at all is treated as not declaring any specific one:
    /// calling `tools/list` on it would be a protocol violation, and the caller gets an explicit error
    /// instead of a confusing empty catalog.
    pub fn declares_capability(&self, capability: &str) -> bool {
        match &self.server_capabilities {
            Some(caps) => caps.get(capability).is_some_and(|v| !v.is_null()),
            None => false,
        }
    }

    /// `tools/list`: return every tool the server exposes.
    pub async fn list_tools(&mut self) -> Result<Vec<McpToolDescriptor>, McpError> {
        // T13 (KR-19): `tools/list` is paginated by `nextCursor`. Following it is not optional — a
        // server with more tools than fit one page would otherwise look like a server with a truncated
        // catalog, and the missing tools would simply be uncallable.
        // A poisoned client refuses everything with the transport error: the byte stream is in an
        // unknown state, so a capability answer here would be a guess dressed up as a protocol fact.
        if !self.poisoned && !self.declares_capability("tools") {
            return Err(McpError::CapabilityMissing {
                capability: "tools".to_string(),
            });
        }
        let deadline = Instant::now() + self.timeout;
        let mut cursor: Option<String> = None;
        let mut seen_cursors: std::collections::BTreeSet<String> = Default::default();
        let mut names: std::collections::BTreeSet<String> = Default::default();
        let mut schema_bytes = 0usize;
        let mut tools: Vec<McpToolDescriptor> = Vec::new();

        for _page in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.request("tools/list", params, deadline).await?;
            let page: Vec<McpToolDescriptor> = serde_json::from_value(
                result
                    .get("tools")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new())),
            )
            .map_err(|e| McpError::Protocol(e.to_string()))?;
            for tool in page {
                if !names.insert(tool.name.clone()) {
                    return Err(McpError::DuplicateToolName { name: tool.name });
                }
                // The schema budget is measured on the serialized schema, because that is what ends up
                // in every request once the tool is visible.
                schema_bytes = schema_bytes.saturating_add(
                    serde_json::to_string(&tool.input_schema)
                        .map(|s| s.len())
                        .unwrap_or(0),
                );
                if schema_bytes > MAX_CATALOG_SCHEMA_BYTES {
                    return Err(McpError::CatalogBudgetExceeded {
                        reason: format!(
                            "tool schemas exceed {} bytes after {} tools",
                            MAX_CATALOG_SCHEMA_BYTES,
                            names.len()
                        ),
                    });
                }
                tools.push(tool);
                if tools.len() > MAX_CATALOG_TOOLS {
                    return Err(McpError::CatalogBudgetExceeded {
                        reason: format!("more than {} tools", MAX_CATALOG_TOOLS),
                    });
                }
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => {
                    if !seen_cursors.insert(next.to_string()) {
                        return Err(McpError::CursorCycle {
                            cursor: next.to_string(),
                        });
                    }
                    cursor = Some(next.to_string());
                }
                _ => return Ok(tools),
            }
        }
        Err(McpError::CatalogBudgetExceeded {
            reason: format!("more than {} pages", MAX_TOOL_PAGES),
        })
    }

    /// `tools/call`: invoke a tool and concatenate the textual content blocks it returns.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<String, McpError> {
        let deadline = Instant::now() + self.timeout;
        let res = self.call_tool_until(name, arguments, deadline).await?;
        Ok(res.content)
    }

    /// `tools/call` bounded by an absolute deadline. The host uses this so a queued or in-flight
    /// call can never outlive the caller's timeout. Returns structured [`McpCallResult`].
    pub async fn call_tool_until(
        &mut self,
        name: &str,
        arguments: Value,
        deadline: Instant,
    ) -> Result<McpCallResult, McpError> {
        let start = Instant::now();
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
                deadline,
            )
            .await
            .map_err(|e| match e {
                McpError::Timeout(_) => {
                    let elapsed = Instant::now().saturating_duration_since(start);
                    McpError::StartedDeadline(elapsed)
                }
                other => other,
            })?;
        Ok(McpCallResult::from_value(&result))
    }

    /// Terminate the child's whole process group and reap it, so no zombie or escaped grandchild is
    /// left behind.
    pub async fn shutdown(mut self) {
        let _ = self.stdin.shutdown().await;
        self.group
            .terminate_gracefully(&mut self.child, SHUTDOWN_KILL_GRACE)
            .await;
    }
}

/// A dropped client must not leave the server's descendants behind: `kill_on_drop` only stops the
/// leader, so the process group / Job Object is swept here as well.
///
/// The `id()` guard keeps this idempotent with [`McpClient::shutdown`]. `shutdown` sweeps the group
/// *before* it waits, and `wait` is the only place the leader is polled to completion (tokio's
/// `Child::id` returns `None` once the child has been polled to completion), so after a shutdown
/// there is nothing left to sweep. While `id()` is still `Some(pid)`, tokio has not reaped the
/// leader, so `pid` (which is also the pgid) cannot have been recycled and `kill(-pgid)` cannot
/// reach an unrelated process group. On Windows the job object's own `Drop` closes the handle with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so it is not closed manually here. A descendant that
/// called `setsid()` and left the group is not covered.
impl Drop for McpClient {
    fn drop(&mut self) {
        if self.child.id().is_some() {
            self.group.terminate_immediately();
        }
    }
}

/// Render an MCP `tools/call` result into text. A `content` array of text blocks is concatenated;
/// `isError` is surfaced explicitly. Structured content is serialized so nothing is silently lost.
pub fn render_tool_result(result: &Value) -> String {
    let mut out = String::new();
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        out.push_str("[mcp tool error]\n");
    }

    let has_structured = result
        .get("structuredContent")
        .is_some_and(|v| !v.is_null());

    let content_opt = result.get("content");
    match content_opt {
        Some(Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            out.push_str(text);
                            out.push('\n');
                        }
                    }
                    Some(other) => {
                        out.push_str(&format!("[{} content omitted]\n", other));
                    }
                    None => {
                        out.push_str(&format!("{}\n", item));
                    }
                }
            }
        }
        Some(Value::String(text)) => {
            out.push_str(text);
            out.push('\n');
        }
        Some(other) => {
            out.push_str(&format!("{}\n", other));
        }
        None => {
            // When content is absent, only dump the raw result if structuredContent
            // is also absent to prevent duplicate double-encoding.
            if !has_structured {
                out.push_str(&format!("{}\n", result));
            }
        }
    }

    if has_structured {
        if let Some(structured) = result.get("structuredContent") {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            let json_str =
                serde_json::to_string(structured).unwrap_or_else(|_| structured.to_string());
            out.push_str(&format!("[structuredContent]\n{}\n", json_str));
        }
    }

    out
}

/// Result of an MCP `tools/call` invocation preserving content, structured content, and error state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallResult {
    /// Rendered human- and model-facing text representation.
    pub content: String,
    /// Structured content payload if provided by the server.
    pub structured_content: Option<Value>,
    /// Whether the tool reported an execution error (`isError: true`).
    pub is_error: bool,
}

impl McpCallResult {
    pub fn from_value(value: &Value) -> Self {
        let is_error = value
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let content = render_tool_result(value);
        let structured_content = value.get("structuredContent").cloned();
        Self {
            content,
            structured_content,
            is_error,
        }
    }
}

/// Adapt an MCP error into a kernel [`ToolExecutionOutput`] error. A cancelled call is reported as
/// `NOT_EXECUTED_CANCELLED` only when the server never saw it. Once dispatched to the server,
/// cancellation or deadline aborts report execution status unknown and possible side-effects.
pub fn mcp_error_output(tool_name: &str, error: &McpError) -> ToolExecutionOutput {
    let code = match error {
        McpError::Cancelled => ErrorCode::NotExecutedCancelled,
        McpError::Overloaded => ErrorCode::NotExecutedBudgetExhausted,
        McpError::RequestTooLarge(_) => ErrorCode::InvalidArguments,
        McpError::StartedDeadline(_) => ErrorCode::ToolTimeout,
        McpError::Timeout(_) => ErrorCode::ToolTimeout,
        _ => ErrorCode::IoError,
    };
    ToolExecutionOutput::error(
        code,
        format!("MCP tool '{}' failed: {}", tool_name, error),
        Default::default(),
    )
}

// ---------------------------------------------------------------------------------------------
// Trusted-config integration: turn configured MCP servers into real, dispatchable kernel tools
// ---------------------------------------------------------------------------------------------

use crate::config_file::McpServerConfig;
use crate::env::ExecutionContext;
use crate::extensions::{
    CustomToolContext, CustomToolHandler, ExtensionManifest, InFlightTracker, ScopedExtension,
    ScopedToolDefinition,
};
use crate::mcp_http::McpHttpClient;
use crate::messages::ToolResultMeta;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

/// Call state linearizing execution between caller and worker via CAS.
pub const CALL_STATE_QUEUED: u8 = 0;
pub const CALL_STATE_STARTED: u8 = 1;
pub const CALL_STATE_CANCELLED_BEFORE_START: u8 = 2;
pub const CALL_STATE_DEADLINE_BEFORE_START: u8 = 3;

/// Bound on the number of tools exposed per server (the kernel registry is bounded too).
pub const MAX_MCP_TOOLS_PER_SERVER: usize = 64;
/// Bound on the total serialized schema bytes contributed by one server.
pub const MAX_MCP_SCHEMA_BYTES: usize = 256 * 1024;

/// One request handled by the MCP worker thread.
enum HostRequest {
    Call(HostCall),
    Shutdown,
}

/// The variable part of a `tools/call` request, serialized by reference so no `Value` is cloned.
#[derive(Serialize)]
struct QueuedRequestParams<'a> {
    name: &'a str,
    arguments: &'a Value,
}

/// Reject a request whose serialized tool name + arguments exceeds the admission cap.
///
/// A bounded sink stops the serialization as soon as the cap is crossed, so an oversized payload is
/// never fully materialized just to refuse it (the caller already owns the `Value`; this layer must
/// not allocate another unbounded copy). The exact wire frame is re-checked by
/// [`McpClient::write_message`].
fn ensure_request_within_cap(tool: &str, arguments: &Value) -> Result<(), McpError> {
    let limit = MAX_MCP_MESSAGE_BYTES - MAX_MCP_REQUEST_ENVELOPE_BYTES;
    let mut sink = BoundedSink::new(limit);
    let params = QueuedRequestParams {
        name: tool,
        arguments,
    };
    match serde_json::to_writer(&mut sink, &params) {
        Ok(()) => Ok(()),
        Err(_) if sink.exceeded() => Err(McpError::RequestTooLarge(sink.written())),
        Err(e) => Err(McpError::Protocol(format!(
            "failed to serialize MCP request: {}",
            e
        ))),
    }
}

/// A `std::io::Write` that counts bytes and fails once `limit` is crossed, so a bounded
/// serialization stops early instead of building the oversized output.
struct BoundedSink {
    limit: usize,
    written: usize,
    exceeded: bool,
}

impl BoundedSink {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            written: 0,
            exceeded: false,
        }
    }

    fn written(&self) -> usize {
        self.written
    }

    fn exceeded(&self) -> bool {
        self.exceeded
    }
}

impl std::io::Write for BoundedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written.saturating_add(buf.len()) > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "MCP request exceeds the admission cap",
            ));
        }
        self.written += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A single `tools/call` queued for the worker thread.
///
/// The deadline is computed when the caller enqueues the request, not when the worker dequeues it,
/// so a request that waited behind a slow call is rejected as expired instead of executing after its
/// caller already gave up. `cancel` carries the same "caller gave up" signal from the handler thread
/// into the worker's I/O loop.
pub struct HostCall {
    pub server: String,
    pub tool: String,
    pub arguments: Value,
    pub deadline: Instant,
    pub start_instant: Instant,
    pub cancel: watch::Receiver<bool>,
    pub state: Arc<AtomicU8>,
    pub reply: std_mpsc::Sender<Result<McpCallResult, McpError>>,
}

impl HostCall {
    /// True once the caller has abandoned this request. Checked *before* the worker starts the call,
    /// so an expired or cancelled queue entry is rejected instead of executed late.
    pub fn is_expired_or_cancelled(&self) -> bool {
        let st = self.state.load(Ordering::SeqCst);
        st == CALL_STATE_CANCELLED_BEFORE_START
            || st == CALL_STATE_DEADLINE_BEFORE_START
            || *self.cancel.borrow()
            || Instant::now() >= self.deadline
    }
}

/// Resolve as soon as `rx` is set to `true` (or its sender is dropped). Used inside the worker's
/// `select!` so a queued or in-flight call is aborted the moment its caller gives up or the host
/// shuts down.
async fn wait_cancelled(rx: &mut watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    let _ = rx.changed().await;
}

/// The thread-safe half of the MCP host: handlers hold this and send requests to the worker thread
/// that owns the client(s). Blocking here is safe because the MCP I/O runs on its own thread and
/// runtime, independent of the caller's executor.
struct McpHostInner {
    tx: std_mpsc::SyncSender<HostRequest>,
    timeout: Duration,
    /// Set to `true` to abort in-flight I/O and stop the worker, which then reaps every server.
    shutdown: watch::Sender<bool>,
    /// The worker's exit ack. The first caller of [`Self::shutdown_and_join`] takes it so shutdown
    /// has a bounded join and later callers cannot block on it.
    worker_done: Mutex<Option<std_mpsc::Receiver<()>>>,
}

impl McpHostInner {
    fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
        in_flight: Option<Arc<InFlightTracker>>,
        exec_context: Option<&ExecutionContext>,
    ) -> Result<McpCallResult, McpError> {
        if in_flight.as_ref().is_some_and(|t| t.is_cancelled())
            || exec_context.is_some_and(|ec| ec.is_cancelled())
        {
            return Err(McpError::Cancelled);
        }
        if exec_context.is_some_and(|ec| ec.is_deadline_exceeded()) {
            return Err(McpError::Timeout(Duration::ZERO));
        }

        let start_instant = Instant::now();
        let effective_deadline = match exec_context.and_then(|ec| ec.deadline) {
            Some(run_d) => {
                let clamped = run_d.clamp(self.timeout);
                Instant::now() + clamped
            }
            None => Instant::now() + self.timeout,
        };

        // Reject an oversized request before it is queued: a bounded sink serializes the variable
        // part (tool name + arguments) and stops early, so an oversized payload is never fully
        // materialized to decide that. This keeps each queued item bounded, and the depth cap below
        // keeps the whole queue bounded.
        ensure_request_within_cap(tool, &arguments)?;

        let (reply_tx, reply_rx) = std_mpsc::channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let state = Arc::new(AtomicU8::new(CALL_STATE_QUEUED));

        self.try_enqueue(HostRequest::Call(HostCall {
            server: server.to_string(),
            tool: tool.to_string(),
            arguments,
            deadline: effective_deadline,
            start_instant,
            cancel: cancel_rx,
            state: Arc::clone(&state),
            reply: reply_tx,
        }))?;

        loop {
            // 1. Cancellation check: linearize via CAS so caller only claims Cancelled if worker has NOT started
            if in_flight.as_ref().is_some_and(|t| t.is_cancelled())
                || exec_context.is_some_and(|ec| ec.is_cancelled())
            {
                match state.compare_exchange(
                    CALL_STATE_QUEUED,
                    CALL_STATE_CANCELLED_BEFORE_START,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => {
                        // Won the race before start: server never saw it, single settlement by caller
                        let _ = cancel_tx.send(true);
                        return Err(McpError::Cancelled);
                    }
                    Err(_) => {
                        // Worker already won CAS to STARTED! Signal worker to abort I/O and poison
                        let _ = cancel_tx.send(true);
                        match reply_rx
                            .recv_timeout(CALL_POLL_INTERVAL.max(Duration::from_millis(100)))
                        {
                            Ok(result) => return result,
                            Err(_) => return Err(McpError::StartedCancelled),
                        }
                    }
                }
            }

            // 2. Deadline check: linearize via CAS so caller only claims unstarted timeout if worker has NOT started
            let now = Instant::now();
            if now >= effective_deadline {
                match state.compare_exchange(
                    CALL_STATE_QUEUED,
                    CALL_STATE_DEADLINE_BEFORE_START,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => {
                        // Won the race before start: queued timeout without server dispatch
                        let _ = cancel_tx.send(true);
                        let elapsed = now.saturating_duration_since(start_instant);
                        return Err(McpError::Timeout(elapsed));
                    }
                    Err(_) => {
                        // Already started: deadline fired while executing on server
                        let _ = cancel_tx.send(true);
                        let elapsed = now.saturating_duration_since(start_instant);
                        match reply_rx
                            .recv_timeout(CALL_POLL_INTERVAL.max(Duration::from_millis(100)))
                        {
                            // The worker acknowledged the cancel *this deadline* just sent: the call
                            // was dispatched and then aborted by the deadline, so the true
                            // classification is a timeout. Reporting `StartedCancelled` here would
                            // map to `IoError` and make the same deadline report two different codes
                            // depending on whether the worker's ack beat the caller's grace window.
                            Ok(Err(McpError::StartedCancelled)) => {
                                return Err(McpError::StartedDeadline(elapsed))
                            }
                            Ok(result) => return result,
                            Err(_) => return Err(McpError::StartedDeadline(elapsed)),
                        }
                    }
                }
            }

            match reply_rx.recv_timeout(CALL_POLL_INTERVAL) {
                Ok(result) => return result,
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    // Loop to re-check cancel and deadline
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(McpError::Io("MCP host thread stopped".to_string()));
                }
            }
        }
    }

    /// Admit one request to the bounded worker queue without blocking.
    ///
    /// A full queue rejects the call before it is dispatched (the server never sees it, so the
    /// returned error is an explicit not-started one); a disconnected worker is a host-gone error.
    /// Never blocks, so a worker that is busy executing a long call cannot wedge a new caller.
    fn try_enqueue(&self, request: HostRequest) -> Result<(), McpError> {
        self.tx.try_send(request).map_err(|err| match err {
            std_mpsc::TrySendError::Full(_) => McpError::Overloaded,
            std_mpsc::TrySendError::Disconnected(_) => {
                McpError::Io("MCP host thread is gone".to_string())
            }
        })
    }

    /// Stop the worker and reap every child, bounded by `bound`.
    ///
    /// The shutdown signal aborts any in-flight I/O (so a server that stops reading cannot block
    /// us). The wake-up request is best-effort (`try_send`) because the queue may be full of calls
    /// the worker is still draining: when the queue is full the worker cannot be idle in `recv`, so
    /// it will observe the watch flag at the top of its loop (or abort the in-flight call through
    /// the same `select!`) and never needs the message to stop. Sending it must not block unmount or
    /// `Drop`.
    fn shutdown_and_join(&self, bound: Duration) {
        let _ = self.shutdown.send(true);
        let _ = self.tx.try_send(HostRequest::Shutdown);
        let rx = self
            .worker_done
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(rx) = rx {
            let _ = rx.recv_timeout(bound);
        }
    }
}

/// A discovered server's tools plus its effect class.
struct DiscoveredServer {
    name: String,
    tools: Vec<McpToolDescriptor>,
    read_only: bool,
}

/// One discovered tool together with the server it came from.
struct HostedTool {
    server: String,
    descriptor: McpToolDescriptor,
    read_only: bool,
}

/// The host workers support two *distinct* MCP revisions without wire-protocol guessing:
/// legacy stdio with initialize/capabilities, and 2026 stateless Streamable HTTP without a session.
enum HostedMcpClient {
    Stdio(McpClient),
    Http(Box<McpHttpClient>),
}

impl HostedMcpClient {
    async fn discover(&mut self) -> Result<Vec<McpToolDescriptor>, McpError> {
        match self {
            Self::Stdio(client) => {
                client.initialize().await?;
                client.list_tools().await
            }
            Self::Http(client) => client.list_tools().await,
        }
    }

    async fn call_tool_until(
        &mut self,
        name: &str,
        arguments: Value,
        deadline: Instant,
    ) -> Result<McpCallResult, McpError> {
        match self {
            Self::Stdio(client) => client.call_tool_until(name, arguments, deadline).await,
            Self::Http(client) => client.call_tool_until(name, arguments, deadline).await,
        }
    }

    fn poison(&mut self) {
        match self {
            Self::Stdio(client) => client.poison(),
            Self::Http(client) => client.poison(),
        }
    }

    async fn shutdown(self) {
        if let Self::Stdio(client) = self {
            client.shutdown().await;
        }
        // HTTP is stateless; dropping the active TCP/TLS request closes it on cancellation.
    }
}

fn new_hosted_client(
    server: &McpServerConfig,
    timeout: Duration,
) -> Result<HostedMcpClient, McpError> {
    if let Some(url) = server.http_url.as_deref() {
        let token = if let Some(variable) = server.bearer_token_env.as_deref() {
            if variable.is_empty()
                || !variable
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(McpError::Protocol(
                    "invalid MCP bearer token environment variable".into(),
                ));
            }
            Some(std::env::var(variable).map_err(|_| {
                McpError::Protocol(
                    "required MCP bearer token environment variable is not set".into(),
                )
            })?)
        } else {
            None
        };
        Ok(HostedMcpClient::Http(Box::new(McpHttpClient::new(
            url, token, timeout,
        )?)))
    } else {
        if server.bearer_token_env.is_some() {
            return Err(McpError::Protocol(
                "bearer_token_env is only valid for MCP HTTP endpoints".into(),
            ));
        }
        Ok(HostedMcpClient::Stdio(McpClient::spawn(
            &server.command,
            &server.args,
            &server.env,
            timeout,
        )?))
    }
}

/// A running MCP host. [`Self::into_extension`] consumes it into a kernel extension whose tools
/// proxy the configured servers.
pub struct McpToolHost {
    inner: Arc<McpHostInner>,
    descriptors: Vec<HostedTool>,
    /// T13 (KR-19): servers that started but were not registered because they do not declare the
    /// `tools` capability, with the reason. Recorded instead of silently dropped: a resource-only
    /// server is legitimate, but the operator should still be able to see why it contributes nothing.
    skipped_servers: Vec<(String, String)>,
}

impl McpToolHost {
    /// Spawn every configured server, complete the `initialize`/`tools/list` handshake, and return a
    /// host. A server that fails to start, negotiates an unsupported protocol version, or returns a
    /// malformed catalog fails the whole host (the operator explicitly configured it, so silently
    /// dropping it would hide a broken setup).
    ///
    /// The one exception is a server that simply does not declare the `tools` capability: it is
    /// skipped and recorded in [`Self::skipped_servers`], because a resource-only server is a valid
    /// thing to configure and calling `tools/list` on it would be a protocol violation.
    pub fn skipped_servers(&self) -> &[(String, String)] {
        &self.skipped_servers
    }
    pub fn start(servers: &[McpServerConfig], timeout: Duration) -> Result<Self, McpError> {
        Self::start_with_depth(servers, timeout, MAX_MCP_QUEUE_DEPTH)
    }

    /// [`Self::start`] with an explicit worker-queue depth, so the depth bound can be exercised with
    /// a small queue instead of many concurrent callers.
    fn start_with_depth(
        servers: &[McpServerConfig],
        timeout: Duration,
        queue_depth: usize,
    ) -> Result<Self, McpError> {
        if servers.is_empty() {
            return Err(McpError::Protocol("no MCP servers configured".to_string()));
        }
        for server in servers {
            let is_http = server.http_url.is_some();
            let has_command = !server.command.trim().is_empty();
            if is_http == has_command {
                return Err(McpError::Protocol(format!(
                    "MCP server '{}' must configure exactly one of command or http_url",
                    server.name
                )));
            }
            if is_http && (!server.args.is_empty() || !server.env.is_empty()) {
                return Err(McpError::Protocol(
                    "MCP HTTP server cannot also configure stdio args or env".into(),
                ));
            }
        }
        // A bounded queue: at most `queue_depth` calls wait while one further call executes. `0`
        // would be a rendezvous channel, so clamp to at least one.
        let (tx, rx) = std_mpsc::sync_channel::<HostRequest>(queue_depth.max(1));
        let (ready_tx, ready_rx) =
            std_mpsc::channel::<Result<(Vec<DiscoveredServer>, Vec<(String, String)>), McpError>>();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let (done_tx, done_rx) = std_mpsc::channel::<()>();
        let server_count = servers.len();
        let servers: Vec<McpServerConfig> = servers.to_vec();
        std::thread::Builder::new()
            .name("kroma-mcp".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(McpError::Spawn(e.to_string())));
                        let _ = done_tx.send(());
                        return;
                    }
                };
                let mut clients: HashMap<String, HostedMcpClient> = HashMap::new();
                let mut discovered: Vec<DiscoveredServer> = Vec::new();
                let mut skipped: Vec<(String, String)> = Vec::new();
                for server in &servers {
                    // Tokio process pipes register with the reactor during spawn, before the
                    // async discovery future is polled. Enter this worker's runtime for creation.
                    let created = {
                        let _guard = rt.enter();
                        new_hosted_client(server, timeout)
                    };
                    let mut client = match created {
                        Ok(client) => client,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            for (_, client) in clients {
                                rt.block_on(client.shutdown());
                            }
                            let _ = done_tx.send(());
                            return;
                        }
                    };
                    let handshake = rt.block_on(client.discover());
                    match handshake {
                        Ok(tools) => {
                            clients.insert(server.name.clone(), client);
                            discovered.push(DiscoveredServer {
                                name: server.name.clone(),
                                tools,
                                read_only: server.read_only,
                            });
                        }
                        Err(e @ McpError::CapabilityMissing { .. }) => {
                            // A server without the `tools` capability contributes no tools; calling
                            // `tools/list` would be a protocol violation. Skip it, record why, and
                            // keep the rest of the host alive.
                            skipped.push((server.name.clone(), e.to_string()));
                            rt.block_on(client.shutdown());
                            continue;
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            // This server never made it into the map, so reap it here as well.
                            rt.block_on(client.shutdown());
                            for (_, client) in clients {
                                rt.block_on(client.shutdown());
                            }
                            let _ = done_tx.send(());
                            return;
                        }
                    }
                }
                let _ = ready_tx.send(Ok((discovered, skipped)));

                while let Ok(request) = rx.recv() {
                    // Shutdown has priority over anything still queued behind it.
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    match request {
                        HostRequest::Call(call) => {
                            let HostCall {
                                server,
                                tool,
                                arguments,
                                deadline,
                                start_instant,
                                mut cancel,
                                state,
                                reply,
                            } = call;

                            // Linearize start via CAS:
                            // Caller only reports NotExecutedCancelled / unstarted Timeout if caller won CAS from QUEUED.
                            // Worker only executes and writes to server if worker won CAS QUEUED -> STARTED.
                            match state.compare_exchange(
                                CALL_STATE_QUEUED,
                                CALL_STATE_STARTED,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            ) {
                                Ok(_) => {
                                    let now = Instant::now();
                                    if *cancel.borrow() {
                                        if let Some(client) = clients.get_mut(&server) {
                                            client.poison();
                                        }
                                        let _ = reply.send(Err(McpError::StartedCancelled));
                                        continue;
                                    }
                                    if now >= deadline {
                                        if let Some(client) = clients.get_mut(&server) {
                                            client.poison();
                                        }
                                        let elapsed = now.saturating_duration_since(start_instant);
                                        let _ = reply.send(Err(McpError::StartedDeadline(elapsed)));
                                        continue;
                                    }

                                    let result = match clients.get_mut(&server) {
                                        Some(client) => {
                                            let mut aborted = false;
                                            let out = rt.block_on(async {
                                                tokio::select! {
                                                    biased;
                                                    _ = wait_cancelled(&mut cancel) => {
                                                        aborted = true;
                                                        Err(McpError::StartedCancelled)
                                                    }
                                                    _ = wait_cancelled(&mut shutdown_rx) => {
                                                        aborted = true;
                                                        Err(McpError::StartedCancelled)
                                                    }
                                                    r = client.call_tool_until(&tool, arguments, deadline) => r,
                                                }
                                            });
                                            if aborted {
                                                client.poison();
                                            }
                                            out
                                        }
                                        None => Err(McpError::Protocol(format!(
                                            "unknown MCP server '{}'",
                                            server
                                        ))),
                                    };
                                    let _ = reply.send(result);
                                }
                                Err(_) => {
                                    // Caller already transitioned to CANCELLED_BEFORE_START or DEADLINE_BEFORE_START.
                                    // Worker must not touch server. Caller already completed.
                                    continue;
                                }
                            }
                        }
                        HostRequest::Shutdown => break,
                    }
                }
                // Reject anything queued behind the shutdown so no waiter hangs on a reply.
                while let Ok(request) = rx.try_recv() {
                    if let HostRequest::Call(call) = request {
                        if call.state.compare_exchange(
                            CALL_STATE_QUEUED,
                            CALL_STATE_CANCELLED_BEFORE_START,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        ).is_ok() {
                            let _ = call.reply.send(Err(McpError::Cancelled));
                        }
                    }
                }
                for (_, client) in clients {
                    rt.block_on(client.shutdown());
                }
                let _ = done_tx.send(());
            })
            .map_err(|e| McpError::Spawn(e.to_string()))?;

        let discovery_timeout = timeout.saturating_mul(server_count as u32 + 1);
        let (discovered, skipped_servers) = match ready_rx.recv_timeout(discovery_timeout) {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                // Abort discovery and reap whatever was already spawned.
                let _ = shutdown_tx.send(true);
                let _ = tx.send(HostRequest::Shutdown);
                let _ = done_rx.recv_timeout(SHUTDOWN_JOIN_BOUND);
                return Err(McpError::Timeout(discovery_timeout));
            }
        };

        // Bound the schema budget: cap tools per server and total serialized schema bytes.
        let mut descriptors: Vec<HostedTool> = Vec::new();
        for server in discovered {
            let mut schema_bytes = 0usize;
            for descriptor in server.tools.into_iter().take(MAX_MCP_TOOLS_PER_SERVER) {
                let bytes = serde_json::to_string(&descriptor.input_schema)
                    .map(|s| s.len())
                    .unwrap_or(0);
                if schema_bytes.saturating_add(bytes) > MAX_MCP_SCHEMA_BYTES {
                    break;
                }
                schema_bytes = schema_bytes.saturating_add(bytes);
                descriptors.push(HostedTool {
                    server: server.name.clone(),
                    descriptor,
                    read_only: server.read_only,
                });
            }
        }

        Ok(Self {
            inner: Arc::new(McpHostInner {
                tx,
                timeout,
                shutdown: shutdown_tx,
                worker_done: Mutex::new(Some(done_rx)),
            }),
            descriptors,
            skipped_servers,
        })
    }

    /// Number of tools discovered across all servers.
    pub fn tool_count(&self) -> usize {
        self.descriptors.len()
    }

    /// Consume the host into a kernel extension whose tools proxy the configured servers. The
    /// extension's cleanup stops the worker thread and reaps the child processes.
    pub fn into_extension(self, extension_id: &str) -> ScopedExtension {
        let mut manifest = ExtensionManifest::operator_trusted(
            extension_id,
            "trusted config `[mcp.<name>]` / `--mcp-server`",
        );
        manifest.capabilities.push("tools".to_string());
        let mut extension = ScopedExtension::new(manifest);

        for tool in &self.descriptors {
            let server = tool.server.clone();
            let name = tool.descriptor.name.clone();
            let inner = Arc::clone(&self.inner);
            let effect_class = if tool.read_only {
                EffectClass::ReadOnly
            } else {
                EffectClass::Mutating
            };
            let registered = tool.descriptor.to_registered_tool(&server, effect_class);
            let handler: Arc<dyn CustomToolHandler> =
                Arc::new(move |raw_arguments: &str, ctx: &CustomToolContext<'_>| {
                    let arguments: Value = match serde_json::from_str(raw_arguments) {
                        Ok(v @ Value::Object(_)) => v,
                        Ok(Value::Null) => json!({}),
                        Ok(_) => {
                            return ToolExecutionOutput::error(
                                ErrorCode::InvalidArguments,
                                format!("MCP tool '{}' arguments must be a JSON object", name),
                                ToolResultMeta::default(),
                            );
                        }
                        Err(e) => {
                            return ToolExecutionOutput::error(
                                ErrorCode::InvalidArguments,
                                format!("invalid JSON arguments for MCP tool '{}': {}", name, e),
                                ToolResultMeta::default(),
                            );
                        }
                    };
                    match inner.call(
                        &server,
                        &name,
                        arguments,
                        ctx.in_flight.clone(),
                        ctx.exec_context,
                    ) {
                        Ok(call_res) => {
                            if call_res.is_error {
                                ToolExecutionOutput::error(
                                    ErrorCode::IoError,
                                    call_res.content,
                                    ToolResultMeta::default(),
                                )
                            } else {
                                ToolExecutionOutput::success(
                                    call_res.content,
                                    ToolResultMeta::default(),
                                )
                            }
                        }
                        Err(e) => mcp_error_output(&name, &e),
                    }
                });
            extension = extension.with_tool(ScopedToolDefinition::new(
                registered.definition,
                registered.metadata,
                handler,
            ));
        }

        let inner = Arc::clone(&self.inner);
        extension.cleanup = Some(Arc::new(move || {
            inner.shutdown_and_join(SHUTDOWN_JOIN_BOUND);
        }));
        extension
    }
}

/// Build the trusted MCP extension from configured servers, or `None` when none are configured.
///
/// This is the single loading helper shared by every surface — the `RpcDispatcher` (repl/acp/serve)
/// and the headless `kroma run` path — so `--mcp-server` / `[mcp.<name>]` cannot be accepted by one
/// entry point and silently ignored by another. A configured server that fails to start is a hard
/// error, never a silent drop.
pub fn trusted_mcp_extension(
    servers: &[McpServerConfig],
) -> Result<Option<(ScopedExtension, usize)>, String> {
    if servers.is_empty() {
        return Ok(None);
    }
    let host = McpToolHost::start(servers, DEFAULT_MCP_TIMEOUT).map_err(|e| e.to_string())?;
    let tool_count = host.tool_count();
    Ok(Some((host.into_extension("mcp"), tool_count)))
}

/// Read one newline-delimited JSON message from `reader`, bounded by `limit` bytes.
///
/// If incoming data before a newline exceeds `limit`, this immediately returns
/// [`McpError::MessageTooLarge`] without allocating beyond the limit or reading unbounded input.
async fn read_message_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Value, McpError> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| McpError::Io(e.to_string()))?;
        if available.is_empty() {
            return Err(McpError::Io("MCP server closed stdout".to_string()));
        }

        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let line_chunk_len = pos + 1;
            let total_len = buf.len() + line_chunk_len;
            if total_len > limit {
                let to_consume = (limit + 1).saturating_sub(buf.len()).min(line_chunk_len);
                reader.consume(to_consume);
                return Err(McpError::MessageTooLarge(total_len));
            }
            buf.extend_from_slice(&available[..line_chunk_len]);
            reader.consume(line_chunk_len);
            break;
        } else {
            let total_len = buf.len() + available.len();
            if total_len > limit {
                let to_consume = (limit + 1).saturating_sub(buf.len()).min(available.len());
                reader.consume(to_consume);
                return Err(McpError::MessageTooLarge(limit + 1));
            }
            buf.extend_from_slice(available);
            let consumed = available.len();
            reader.consume(consumed);
        }
    }

    let text = std::str::from_utf8(&buf)
        .map_err(|e| McpError::Protocol(format!("invalid UTF-8: {}", e)))?;
    serde_json::from_str(text.trim()).map_err(|e| McpError::Protocol(e.to_string()))
}

/// Enforce the one-line wire cap, counting the trailing newline exactly like the read side
/// ([`read_message_bounded`] counts the delimiter in its limit). A JSON body of
/// `MAX_MCP_MESSAGE_BYTES` bytes would become a `MAX + 1`-byte line once the delimiter is added, so
/// the largest admissible body is `MAX - 1`.
fn ensure_wire_line_within_cap(json_len: usize) -> Result<(), McpError> {
    let wire_len = json_len.saturating_add(1);
    if wire_len > MAX_MCP_MESSAGE_BYTES {
        Err(McpError::MessageTooLarge(wire_len))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod call_lifetime_tests {
    use super::*;

    /// Bounded wait for a pid file a test server wrote. Never sleeps as a correctness premise:
    /// the file appearing *is* the signal; the bound only stops a broken fixture from hanging.
    fn wait_for_pid(path: &std::path::Path) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    return pid;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("pid file never appeared: {}", path.display());
    }

    fn wait_until_dead(pid: i32, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        while Instant::now() < deadline {
            if !ChildProcessGroup::is_process_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kroma-b08b-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// The leader exited on its own but the handle was never polled, so `Child::id` still reports the
    /// pid. `Drop` must sweep the group in that state: the descendant would otherwise outlive the
    /// client, and the pid cannot have been recycled while it is unreaped.
    #[tokio::test]
    async fn drop_sweeps_the_group_after_the_leader_exited_unreaped() {
        let dir = scratch_dir("mcp-exit");
        let pid_file = dir.join("grandchild.pid");
        // The descendant must not inherit stdout, or the pipe would stay open past the leader's exit.
        let script = format!(
            "sleep 60 >/dev/null 2>&1 & echo $! > {}; exit 0",
            pid_file.display()
        );
        let mut client = McpClient::spawn(
            "sh",
            &["-c".to_string(), script],
            &[],
            Duration::from_secs(5),
        )
        .expect("mcp spawn");
        let grandchild = wait_for_pid(&pid_file);

        // EOF on the leader's stdout proves it exited; reading a pipe does not reap the child.
        let eof = tokio::time::timeout(Duration::from_secs(5), client.stdout.fill_buf())
            .await
            .expect("leader stdout must reach EOF")
            .expect("read leader stdout");
        assert!(eof.is_empty(), "leader must have exited and closed stdout");
        assert!(
            client.child.id().is_some(),
            "an unpolled child must still report its pid, so Drop cannot skip the sweep"
        );

        drop(client);
        assert!(
            wait_until_dead(grandchild, Duration::from_secs(5)),
            "Drop must sweep the descendant of an exited, unreaped leader (pid {grandchild})"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Once the leader has been reaped, `Child::id` is `None` and its pid (== pgid) may name a
    /// successor process group. `Drop` must then be a no-op, which also makes it idempotent with
    /// [`McpClient::shutdown`] (which sweeps the group *before* it waits).
    #[tokio::test]
    async fn drop_after_a_reaped_leader_does_not_sweep_a_recycled_group() {
        // A live unrelated group: spawned as its own group leader so its pid is a valid pgid that a
        // recycled id could collide with.
        let mut bystander_cmd = std::process::Command::new("sleep");
        bystander_cmd.arg("30");
        ChildProcessGroup::configure_std_command(&mut bystander_cmd);
        let mut bystander = bystander_cmd.spawn().expect("spawn bystander");
        let bystander_pid = bystander.id() as i32;
        let recycled_group = ChildProcessGroup::from_std_child(&bystander);
        assert_eq!(recycled_group.pgid(), bystander_pid);

        let mut client = McpClient::spawn(
            "sh",
            &["-c".to_string(), "exec sleep 30".to_string()],
            &[],
            Duration::from_secs(5),
        )
        .expect("mcp spawn");
        // Reproduce `shutdown`'s order exactly: sweep the group, then reap the leader.
        client.group.terminate_immediately();
        client.child.wait().await.expect("reap leader");
        assert!(
            client.child.id().is_none(),
            "a child polled to completion has no id"
        );

        client.group = recycled_group;
        drop(client);

        assert!(
            ChildProcessGroup::is_process_alive(bystander_pid),
            "a reaped leader must not let Drop sweep a pgid that now names another group"
        );
        let _ = bystander.kill();
        let _ = bystander.wait();
    }

    fn host_call(deadline: Instant, cancel: watch::Receiver<bool>) -> HostCall {
        let (reply, _reply_rx) = std_mpsc::channel();
        HostCall {
            server: "srv".to_string(),
            tool: "tool".to_string(),
            arguments: json!({}),
            deadline,
            start_instant: Instant::now(),
            cancel,
            state: Arc::new(AtomicU8::new(CALL_STATE_QUEUED)),
            reply,
        }
    }

    #[test]
    fn an_expired_queue_entry_is_rejected_before_execution() {
        let (_tx, rx) = watch::channel(false);
        let call = host_call(Instant::now() - Duration::from_millis(1), rx);
        assert!(
            call.is_expired_or_cancelled(),
            "a request whose deadline has passed must never be executed"
        );
    }

    #[test]
    fn a_cancelled_queue_entry_is_rejected_before_execution() {
        let (cancel_tx, rx) = watch::channel(false);
        let call = host_call(Instant::now() + Duration::from_secs(60), rx);
        assert!(!call.is_expired_or_cancelled());
        cancel_tx.send(true).unwrap();
        assert!(
            call.is_expired_or_cancelled(),
            "a cancelled request must never be executed"
        );
    }

    #[test]
    fn render_tool_result_preserves_content_and_structured_content() {
        // Success with both content and structuredContent
        let success_both = json!({
            "content": [{"type": "text", "text": "operation completed"}],
            "structuredContent": {"records_updated": 42, "status": "ok"}
        });
        let rendered_succ = render_tool_result(&success_both);
        assert!(rendered_succ.contains("operation completed\n"));
        assert!(rendered_succ
            .contains("[structuredContent]\n{\"records_updated\":42,\"status\":\"ok\"}\n"));
        assert!(!rendered_succ.contains("[mcp tool error]"));

        // Error with both content and structuredContent
        let error_both = json!({
            "isError": true,
            "content": [{"type": "text", "text": "database failed"}],
            "structuredContent": {"error_code": 1054, "sql_state": "42S22"}
        });
        let rendered_err = render_tool_result(&error_both);
        assert!(rendered_err.contains("[mcp tool error]\n"));
        assert!(rendered_err.contains("database failed\n"));
        assert!(rendered_err
            .contains("[structuredContent]\n{\"error_code\":1054,\"sql_state\":\"42S22\"}\n"));

        // Structured-only success (content field absent) - no double encoding
        let struct_only_succ = json!({
            "structuredContent": {"metric": 99.5}
        });
        let rendered_only_succ = render_tool_result(&struct_only_succ);
        assert_eq!(
            rendered_only_succ,
            "[structuredContent]\n{\"metric\":99.5}\n"
        );
        assert!(!rendered_only_succ.contains("\"structuredContent\":"));

        // Structured-only error (content field absent) - no double encoding
        let struct_only_err = json!({
            "isError": true,
            "structuredContent": {"fatal": true}
        });
        let rendered_only_err = render_tool_result(&struct_only_err);
        assert_eq!(
            rendered_only_err,
            "[mcp tool error]\n[structuredContent]\n{\"fatal\":true}\n"
        );

        // Content-only success
        let content_only = json!({
            "content": [{"type": "text", "text": "plain text only"}]
        });
        let rendered_content_only = render_tool_result(&content_only);
        assert_eq!(rendered_content_only, "plain text only\n");

        // Fallback when neither content nor structuredContent is present
        let empty_result = json!({"other_field": 123});
        let rendered_fallback = render_tool_result(&empty_result);
        assert!(rendered_fallback.contains("\"other_field\":123"));
    }

    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, ReadBuf};

    struct TrackingAsyncReader<R> {
        inner: R,
        bytes_read: usize,
    }

    impl<R: AsyncRead + Unpin> AsyncRead for TrackingAsyncReader<R> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let before = buf.filled().len();
            let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
            let after = buf.filled().len();
            self.bytes_read += after - before;
            poll
        }
    }

    #[tokio::test]
    async fn regression_oversized_no_newline_allocates_and_fails_closed() {
        use std::io::Cursor;
        let total_input_len = 2 * MAX_MCP_MESSAGE_BYTES;
        let payload = vec![b'x'; total_input_len];
        let tracking = TrackingAsyncReader {
            inner: Cursor::new(payload),
            bytes_read: 0,
        };
        // Use a 1-byte buffer to precisely assert that reading stops immediately
        // at the limit without consuming any further bytes from the producer.
        let mut reader = tokio::io::BufReader::with_capacity(1, tracking);

        let res = read_message_bounded(&mut reader, MAX_MCP_MESSAGE_BYTES).await;
        let err = res.expect_err("oversized message must error");
        match err {
            McpError::MessageTooLarge(n) => {
                let bytes_read = reader.get_ref().bytes_read;
                assert_eq!(
                    bytes_read,
                    MAX_MCP_MESSAGE_BYTES + 1,
                    "must stop reading immediately upon exceeding limit by 1 byte"
                );
                assert_eq!(
                    n,
                    MAX_MCP_MESSAGE_BYTES + 1,
                    "reported size must be bounded limit + 1"
                );
            }
            other => panic!("expected MessageTooLarge, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn boundary_n_minus_one_n_and_n_plus_one() {
        use std::io::Cursor;
        let limit = 40;

        // N - 1 = 39 bytes
        let pad_39 = "x".repeat(39 - 6 - 3);
        let msg_39 = format!("{{\"k\":\"{}\"}}\n", pad_39);
        assert_eq!(msg_39.len(), 39);
        let mut reader = tokio::io::BufReader::new(Cursor::new(msg_39.into_bytes()));
        let res = read_message_bounded(&mut reader, limit).await;
        assert!(res.is_ok(), "N - 1 boundary must succeed: {:?}", res);

        // N = 40 bytes
        let pad_40 = "x".repeat(40 - 6 - 3);
        let msg_40 = format!("{{\"k\":\"{}\"}}\n", pad_40);
        assert_eq!(msg_40.len(), 40);
        let mut reader = tokio::io::BufReader::new(Cursor::new(msg_40.into_bytes()));
        let res = read_message_bounded(&mut reader, limit).await;
        assert!(res.is_ok(), "N boundary must succeed: {:?}", res);

        // N + 1 = 41 bytes (with newline)
        let pad_41 = "x".repeat(41 - 6 - 3);
        let msg_41 = format!("{{\"k\":\"{}\"}}\n", pad_41);
        assert_eq!(msg_41.len(), 41);
        let mut reader = tokio::io::BufReader::new(Cursor::new(msg_41.into_bytes()));
        let res = read_message_bounded(&mut reader, limit).await;
        match res {
            Err(McpError::MessageTooLarge(n)) => assert_eq!(n, 41),
            other => panic!(
                "N + 1 boundary must fail with MessageTooLarge(41), got: {:?}",
                other
            ),
        }

        // N + 1 = 41 bytes (without newline)
        let msg_41_no_nl = "x".repeat(41);
        let mut reader = tokio::io::BufReader::new(Cursor::new(msg_41_no_nl.into_bytes()));
        let res = read_message_bounded(&mut reader, limit).await;
        match res {
            Err(McpError::MessageTooLarge(n)) => assert_eq!(n, 41),
            other => panic!(
                "N + 1 boundary without newline must fail with MessageTooLarge(41), got: {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn fragmented_oversize_stops_reading_immediately() {
        use std::io::Cursor;
        let limit = 50;
        let total_data = vec![b'a'; 500];
        let tracking = TrackingAsyncReader {
            inner: Cursor::new(total_data),
            bytes_read: 0,
        };
        // 10-byte buffer simulating fragmented TCP stream
        let mut reader = tokio::io::BufReader::with_capacity(10, tracking);
        let res = read_message_bounded(&mut reader, limit).await;
        match res {
            Err(McpError::MessageTooLarge(n)) => {
                let bytes_read = reader.get_ref().bytes_read;
                assert!(
                    bytes_read <= limit + 10,
                    "fragmented read must stop immediately, read {} bytes <= {}",
                    bytes_read,
                    limit + 10
                );
                assert_eq!(n, limit + 1);
            }
            other => panic!("expected MessageTooLarge, got: {:?}", other),
        }
    }

    struct InfiniteProducer;
    impl AsyncRead for InfiniteProducer {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let chunk = [b'z'; 128];
            let to_write = chunk.len().min(buf.remaining());
            buf.put_slice(&chunk[..to_write]);
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn no_newline_fast_producer_does_not_hang() {
        let mut reader = tokio::io::BufReader::with_capacity(1024, InfiniteProducer);
        let res = read_message_bounded(&mut reader, 2048).await;
        match res {
            Err(McpError::MessageTooLarge(n)) => assert_eq!(n, 2049),
            other => panic!("expected MessageTooLarge, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn eof_handling_empty_and_partial() {
        use std::io::Cursor;
        // Empty stream
        let mut reader = tokio::io::BufReader::new(Cursor::new(Vec::new()));
        let res = read_message_bounded(&mut reader, 1024).await;
        match res {
            Err(McpError::Io(msg)) => assert!(msg.contains("MCP server closed stdout")),
            other => panic!("expected Io error, got: {:?}", other),
        }

        // Partial before EOF
        let mut reader = tokio::io::BufReader::new(Cursor::new(b"{\"partial\": true".to_vec()));
        let res = read_message_bounded(&mut reader, 1024).await;
        match res {
            Err(McpError::Io(msg)) => assert!(msg.contains("MCP server closed stdout")),
            other => panic!("expected Io error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn invalid_utf8_rejected_as_protocol_error() {
        use std::io::Cursor;
        let mut invalid_bytes = b"{\"text\":\"".to_vec();
        invalid_bytes.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        invalid_bytes.extend_from_slice(b"\"}\n");
        let mut reader = tokio::io::BufReader::new(Cursor::new(invalid_bytes));
        let res = read_message_bounded(&mut reader, 1024).await;
        match res {
            Err(McpError::Protocol(msg)) => assert!(msg.contains("invalid UTF-8")),
            other => panic!(
                "expected Protocol error for invalid UTF-8, got: {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn subsequent_call_fails_closed_when_poisoned() {
        let mut client = McpClient {
            child: tokio::process::Command::new("true").spawn().unwrap(),
            stdin: tokio::process::Command::new("true")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap()
                .stdin
                .take()
                .unwrap(),
            stdout: tokio::io::BufReader::new(
                tokio::process::Command::new("true")
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
                    .stdout
                    .take()
                    .unwrap(),
            ),
            group: ChildProcessGroup::from_tokio_child(
                &tokio::process::Command::new("true").spawn().unwrap(),
            ),
            next_id: 1,
            timeout: Duration::from_secs(5),
            poisoned: false,
            server_capabilities: None,
        };

        // Initially not poisoned
        assert!(!client.poisoned);
        client.poison();
        assert!(client.poisoned);

        // read_message immediately fails closed
        let res = client.read_message().await;
        match res {
            Err(McpError::Io(msg)) => assert!(msg.contains("poisoned")),
            other => panic!("expected poisoned Io error, got: {:?}", other),
        }

        // request immediately fails closed without writing
        let res = client
            .request("test", json!({}), Instant::now() + Duration::from_secs(5))
            .await;
        match res {
            Err(McpError::Io(msg)) => assert!(msg.contains("poisoned")),
            other => panic!("expected poisoned Io error, got: {:?}", other),
        }
        client.shutdown().await;
    }

    #[tokio::test]
    async fn public_regression_invalid_utf8_in_valid_json_string_poisons_client() {
        let script = r#"
import sys
for line in sys.stdin:
    if not line.strip():
        continue
    # Valid JSON structure, but invalid UTF-8 bytes 0xff 0xfe in string value
    sys.stdout.buffer.write(b'{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"test\xff\xfe","version":"1"}}}\n')
    sys.stdout.buffer.flush()
"#;
        let mut client = McpClient::spawn(
            "python3",
            &["-c".to_string(), script.to_string()],
            &[],
            Duration::from_secs(5),
        )
        .expect("spawn mock server");

        let init_res = client.initialize().await;
        match init_res {
            Err(McpError::Protocol(msg)) => {
                assert!(
                    msg.contains("invalid UTF-8"),
                    "expected invalid UTF-8 error, got: {}",
                    msg
                );
            }
            other => panic!(
                "expected Protocol error with invalid UTF-8, got: {:?}",
                other
            ),
        }

        let next_res = client.list_tools().await;
        match next_res {
            Err(McpError::Io(msg)) => {
                assert!(
                    msg.contains("poisoned"),
                    "expected poisoned transport error, got: {}",
                    msg
                );
            }
            other => panic!(
                "expected poisoned Io error on subsequent request, got: {:?}",
                other
            ),
        }

        client.shutdown().await;
    }

    #[tokio::test]
    async fn public_regression_partial_read_timeout_poisons_next_request() {
        let script = r#"
import sys, time
line = sys.stdin.readline()
sys.stdout.write('{"jsonrpc":"2.0","id":1')
sys.stdout.flush()
time.sleep(10)
"#;
        let mut client = McpClient::spawn(
            "python3",
            &["-c".to_string(), script.to_string()],
            &[],
            Duration::from_millis(150),
        )
        .expect("spawn mock server");

        let init_res = client.initialize().await;
        match init_res {
            Err(McpError::Timeout(_)) => {}
            other => panic!("expected Timeout error, got: {:?}", other),
        }

        let next_res = client.list_tools().await;
        match next_res {
            Err(McpError::Io(msg)) => {
                assert!(
                    msg.contains("poisoned"),
                    "expected poisoned transport error, got: {}",
                    msg
                );
            }
            other => panic!(
                "expected poisoned Io error on subsequent request, got: {:?}",
                other
            ),
        }

        client.shutdown().await;
    }

    /// A `HostCall` plus the CAS state the caller would watch, so a test can prove a rejected call
    /// never advances past `QUEUED`.
    fn queued_call() -> (HostCall, Arc<AtomicU8>) {
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let state = Arc::new(AtomicU8::new(CALL_STATE_QUEUED));
        let (reply, _reply_rx) = std_mpsc::channel();
        let call = HostCall {
            server: "srv".to_string(),
            tool: "tool".to_string(),
            arguments: json!({}),
            deadline: Instant::now() + Duration::from_secs(60),
            start_instant: Instant::now(),
            cancel: cancel_rx,
            state: Arc::clone(&state),
            reply,
        };
        (call, state)
    }

    /// The worker queue admits exactly `MAX_MCP_QUEUE_DEPTH` waiting calls; the next is rejected as
    /// `Overloaded` before any dispatch and the caller's state stays `QUEUED` (never falsely
    /// `STARTED`).
    #[test]
    fn queue_admission_is_bounded_and_rejects_before_dispatch() {
        // The approved bound: 16 queued while one call is active.
        assert_eq!(MAX_MCP_QUEUE_DEPTH, 16);
        let (tx, _rx) = std_mpsc::sync_channel::<HostRequest>(MAX_MCP_QUEUE_DEPTH);
        let inner = McpHostInner {
            tx,
            timeout: Duration::from_secs(1),
            shutdown: watch::channel(false).0,
            worker_done: Mutex::new(None),
        };

        for _ in 0..MAX_MCP_QUEUE_DEPTH {
            let (call, state) = queued_call();
            inner
                .try_enqueue(HostRequest::Call(call))
                .expect("every call within the depth must be admitted");
            assert_eq!(state.load(Ordering::SeqCst), CALL_STATE_QUEUED);
        }

        let (call, state) = queued_call();
        let err = inner
            .try_enqueue(HostRequest::Call(call))
            .expect_err("the call past the depth must be rejected");
        assert!(matches!(err, McpError::Overloaded), "got {err:?}");
        assert_eq!(
            state.load(Ordering::SeqCst),
            CALL_STATE_QUEUED,
            "a rejected call must stay QUEUED, never STARTED"
        );
        assert!(
            err.to_string().contains("not sent to any server"),
            "an overloaded call must be explicit about never reaching a server: {err}"
        );
    }

    /// The bounded sink accepts exactly `limit` bytes and fails on the next one, so the admission
    /// check can never materialize an oversized payload.
    #[test]
    fn bounded_sink_stops_at_the_exact_limit() {
        use std::io::Write;
        let mut sink = BoundedSink::new(8);
        assert!(
            sink.write_all(b"xxxxxxxx").is_ok(),
            "exactly the limit fits"
        );
        assert!(!sink.exceeded());
        assert_eq!(sink.written(), 8);
        assert!(sink.write_all(b"x").is_err(), "one byte over must fail");
        assert!(sink.exceeded());
        assert_eq!(sink.written(), 8, "the rejected byte is not counted");
    }

    /// An oversized request is a distinct `RequestTooLarge`, never reported as a full queue, and is
    /// rejected before it is queued.
    #[test]
    fn oversized_request_is_rejected_before_queueing() {
        let small = json!({ "payload": "x".repeat(1024) });
        assert!(ensure_request_within_cap("tool", &small).is_ok());

        let huge = json!({ "payload": "x".repeat(MAX_MCP_MESSAGE_BYTES) });
        match ensure_request_within_cap("tool", &huge) {
            Err(McpError::RequestTooLarge(_)) => {}
            other => panic!("expected RequestTooLarge, got {other:?}"),
        }
    }

    /// The reserved `MAX_MCP_REQUEST_ENVELOPE_BYTES` margin must actually cover the real wire frame
    /// built around the admitted `params` — the JSON-RPC envelope (`jsonrpc`/`id`/`method`), the
    /// largest id this client can emit, `params` braces, the closing brace and the newline. If this
    /// ever fails, `ensure_request_within_cap` could admit a request that `write_message` then
    /// rejects, i.e. the admission cap and the wire cap would disagree.
    #[test]
    fn reserved_envelope_margin_covers_the_worst_case_wire_frame() {
        // `i64::MAX` is the largest id `next_id` can ever build (it saturates at `i64::MAX`).
        // Method names: every method this client writes, longest first. `tools/call` is the one the
        // admission path uses; the others share the same frame shape and must fit too.
        for method in [
            "notifications/initialized",
            "tools/call",
            "tools/list",
            "initialize",
        ] {
            let empty = json!({});
            let params = QueuedRequestParams {
                name: "t",
                arguments: &empty,
            };
            let params_len = serde_json::to_vec(&params)
                .expect("serialize queued params")
                .len();
            let request = json!({
                "jsonrpc": "2.0",
                "id": i64::MAX,
                "method": method,
                "params": params,
            });
            let wire_body_len = serde_json::to_vec(&request)
                .expect("serialize request")
                .len();
            let overhead = wire_body_len - params_len;
            assert!(
                overhead <= MAX_MCP_REQUEST_ENVELOPE_BYTES,
                "method {method} needs {overhead} bytes of envelope, above the reserved {MAX_MCP_REQUEST_ENVELOPE_BYTES}"
            );
            // The largest params payload admission can accept (`limit`), plus the real envelope,
            // plus the newline, must still fit the wire cap that `write_message` enforces.
            let admission_limit = MAX_MCP_MESSAGE_BYTES - MAX_MCP_REQUEST_ENVELOPE_BYTES;
            let worst_wire_len = admission_limit + overhead + 1;
            assert!(
                worst_wire_len <= MAX_MCP_MESSAGE_BYTES,
                "an admitted {method} request of {worst_wire_len} bytes would not fit the {MAX_MCP_MESSAGE_BYTES}-byte wire cap"
            );
        }
    }

    /// The write cap counts the trailing newline, symmetric with the read side. A body of
    /// `MAX - 1` bytes is the largest admissible line; a body of exactly `MAX` bytes is rejected as
    /// `MessageTooLarge(MAX + 1)` instead of writing a `MAX + 1`-byte line into the pipe.
    #[test]
    fn write_cap_counts_the_trailing_newline() {
        assert!(ensure_wire_line_within_cap(0).is_ok());
        assert!(ensure_wire_line_within_cap(MAX_MCP_MESSAGE_BYTES - 1).is_ok());
        match ensure_wire_line_within_cap(MAX_MCP_MESSAGE_BYTES) {
            Err(McpError::MessageTooLarge(n)) => assert_eq!(n, MAX_MCP_MESSAGE_BYTES + 1),
            other => panic!(
                "a body of exactly {MAX_MCP_MESSAGE_BYTES} bytes must be rejected once the newline is counted, got {other:?}"
            ),
        }
    }

    fn wait_for_marker(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("marker never appeared: {}", path.display());
    }

    const ADMISSION_SERVER: &str = r#"
import sys, json, os, time

with open(os.environ["B06_ADM_PID"], "w") as f:
    f.write(str(os.getpid()))

def respond(mid, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        respond(msg["id"], {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                            "serverInfo": {"name": "adm", "version": "1"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(msg["id"], {"tools": [
            {"name": "slow", "description": "blocking",
             "inputSchema": {"type": "object", "properties": {}}},
        ]})
    elif method == "tools/call":
        with open(os.environ["B06_ADM_EXECUTED"], "a") as f:
            f.write("call\n")
        with open(os.environ["B06_ADM_ENTERED"], "w") as f:
            f.write("1")
        time.sleep(300)
    elif "id" in msg:
        respond(msg["id"], {})
"#;

    /// With a 1-deep queue and the active call blocked on the server, the next call is rejected as
    /// `Overloaded` and never reaches the server; a queued call that is cancelled is never dispatched;
    /// and unmounting with the queue full still reaps the server within a bounded window.
    #[test]
    fn full_queue_rejects_new_call_and_shutdown_reaps_the_server() {
        let dir = scratch_dir("admission-full");
        let server = dir.join("server.py");
        std::fs::write(&server, ADMISSION_SERVER).expect("write fixture server");
        let pid_file = dir.join("server.pid");
        let entered = dir.join("entered.marker");
        let executed = dir.join("executed.count");
        let config = McpServerConfig {
            http_url: None,
            bearer_token_env: None,
            name: "srv".to_string(),
            command: "python3".to_string(),
            args: vec![server.display().to_string()],
            env: vec![
                ("B06_ADM_PID".to_string(), pid_file.display().to_string()),
                ("B06_ADM_ENTERED".to_string(), entered.display().to_string()),
                (
                    "B06_ADM_EXECUTED".to_string(),
                    executed.display().to_string(),
                ),
            ],
            read_only: false,
        };
        let host = McpToolHost::start_with_depth(&[config], Duration::from_secs(30), 1)
            .expect("start 1-deep host");
        let pid = wait_for_pid(&pid_file);
        let inner = Arc::clone(&host.inner);

        std::thread::scope(|scope| {
            // 1. The active call: the worker dequeues it and the server blocks, freeing the one slot.
            let blocker_inner = Arc::clone(&inner);
            let blocker =
                scope.spawn(move || blocker_inner.call("srv", "slow", json!({}), None, None));
            wait_for_marker(&entered);

            // 2. Fill the single queue slot with a call we will cancel while it waits.
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let queued_ctx = ExecutionContext::new(None, None, Some(cancel_rx), None, None);
            let queued_inner = Arc::clone(&inner);
            let queued = scope.spawn(move || {
                queued_inner.call("srv", "slow", json!({}), None, Some(&queued_ctx))
            });
            std::thread::sleep(Duration::from_millis(100));
            cancel_tx.send(true).unwrap();

            // 3. depth+1: rejected immediately, before any dispatch.
            let rejection_started = Instant::now();
            let err = inner
                .call("srv", "slow", json!({}), None, None)
                .expect_err("the call past the depth must be rejected");
            assert!(
                rejection_started.elapsed() < Duration::from_secs(1),
                "an overloaded call must fail fast, took {:?}",
                rejection_started.elapsed()
            );
            assert!(matches!(err, McpError::Overloaded), "got {err:?}");

            let queued_out = queued.join().unwrap();
            assert!(
                matches!(queued_out, Err(McpError::Cancelled)),
                "a cancelled queued call must return Cancelled, got {queued_out:?}"
            );

            // 4. Shutdown with a full queue must stay bounded and reap the blocked server.
            let shutdown_started = Instant::now();
            inner.shutdown_and_join(Duration::from_secs(5));
            assert!(
                shutdown_started.elapsed() < Duration::from_secs(5),
                "shutdown with a full queue must be bounded, took {:?}",
                shutdown_started.elapsed()
            );
            let blocker_out = blocker.join().unwrap();
            assert!(
                blocker_out.is_err(),
                "the blocked call must be aborted on shutdown"
            );
        });

        let executed_count = std::fs::read_to_string(&executed)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        assert_eq!(
            executed_count, 1,
            "only the active call may reach the server; the rejected/cancelled ones must not"
        );
        assert!(
            wait_until_dead(pid, Duration::from_secs(5)),
            "shutdown must reap the server (pid {pid})"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
