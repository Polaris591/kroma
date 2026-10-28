//! Minimal MCP tools client. Supports local JSON-RPC stdio and remote
//! Streamable HTTP, including legacy initialization-based and modern
//! per-request-metadata versions. Does not implement MCP resources, prompts,
//! sampling, elicitation, or an in-process plugin runtime.
use crate::{
    protocol::ToolCall,
    tools::{Tool, ToolContext, ToolFuture, ToolRegistry, ToolResult},
};
use anyhow::{bail, Context, Result};
use base64::Engine;
use futures_util::StreamExt;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE},
    Client, Response,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

const LEGACY_VERSION: &str = "2025-11-25";
const MODERN_VERSION: &str = "2026-07-28";
static MCP_MEDIA_ID: AtomicU64 = AtomicU64::new(0);

/// Large MCP image payloads should not become raw base64 text in the LLM
/// transcript. Retain the image on disk and return a useful reference.
fn archive_mcp_media(value: &mut Value, archive: &Path) -> Result<()> {
    let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for item in content {
        if item["type"] != "image" {
            continue;
        }
        let Some(encoded) = item["data"].as_str() else {
            continue;
        };
        let data = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("invalid base64 MCP image")?;
        let extension = match item["mimeType"].as_str() {
            Some("image/jpeg") => "jpg",
            Some("image/webp") => "webp",
            Some("image/gif") => "gif",
            _ => "png",
        };
        let dir = archive.parent().context("MCP session path has no parent")?;
        let owner = archive
            .file_stem()
            .context("MCP session has no filename")?
            .to_string_lossy();
        let file = dir.join(format!(
            "mcp-image-{}-{}-{}.{}",
            owner,
            std::process::id(),
            MCP_MEDIA_ID.fetch_add(1, Ordering::Relaxed),
            extension
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&file).context("unable to archive MCP image")?;
        use std::io::Write;
        output.write_all(&data)?;
        let mime = item["mimeType"].clone();
        *item = json!({
            "type": "image_reference",
            "mimeType": mime,
            "path": file,
            "note": "MCP image saved as a file; binary data was omitted from the text-only tool result"
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub protocol_version: Option<String>,
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, ServerConfig>,
}

pub fn configured_servers(workdir: &Path) -> Result<BTreeMap<String, ServerConfig>> {
    let paths = if let Some(override_path) = env::var_os("KROMA_MCP_CONFIG") {
        vec![PathBuf::from(override_path)]
    } else {
        let mut paths = Vec::new();
        if let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
            paths.push(PathBuf::from(home).join(".kroma/mcp.json"));
        }
        paths.push(workdir.join(".kroma/mcp.json"));
        paths
    };
    let mut servers = BTreeMap::new();
    for path in paths {
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading MCP config {}", path.display()))
            }
        };
        let parsed: ConfigFile = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid MCP config {}", path.display()))?;
        servers.extend(parsed.mcp_servers);
    }
    Ok(servers)
}

fn expand_env(raw: &str) -> Result<String> {
    let mut output = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let suffix = &rest[start + 2..];
        let end = suffix
            .find('}')
            .context("unterminated ${ENV_VAR} reference")?;
        let name = &suffix[..end];
        if name.is_empty() {
            bail!("empty environment variable reference");
        }
        output.push_str(
            &env::var(name).with_context(|| format!("environment variable {name} is not set"))?,
        );
        rest = &suffix[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

enum Transport {
    Stdio {
        _child: Box<Child>,
        stdin: ChildStdin,
        stdout: Lines<BufReader<ChildStdout>>,
    },
    Http {
        client: Client,
        url: String,
        headers: HeaderMap,
        session_id: Option<String>,
    },
}

struct Connection {
    transport: Transport,
    version: String,
    next_id: u64,
}

impl Connection {
    async fn connect(config: &ServerConfig, workdir: &Path) -> Result<Self> {
        let version = config.protocol_version.as_deref().unwrap_or(LEGACY_VERSION);
        if version != LEGACY_VERSION && version != MODERN_VERSION {
            bail!("protocolVersion must be {LEGACY_VERSION} or {MODERN_VERSION}");
        }
        let transport = match (&config.command, &config.url) {
            (Some(command), None) => {
                let mut process = Command::new(expand_env(command)?);
                for arg in &config.args {
                    process.arg(expand_env(arg)?);
                }
                process.current_dir(config.cwd.as_deref().map_or_else(
                    || workdir.to_path_buf(),
                    |cwd| {
                        if cwd.is_absolute() {
                            cwd.to_path_buf()
                        } else {
                            workdir.join(cwd)
                        }
                    },
                ));
                for (key, value) in &config.env {
                    process.env(key, expand_env(value)?);
                }
                process.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(
                    if crate::diagnostics::quiet() {
                        Stdio::null()
                    } else {
                        Stdio::inherit()
                    },
                );
                process.kill_on_drop(true);
                let mut child = process
                    .spawn()
                    .context("unable to launch MCP server process")?;
                let stdin = child.stdin.take().context("MCP server stdin unavailable")?;
                let stdout = child
                    .stdout
                    .take()
                    .context("MCP server stdout unavailable")?;
                Transport::Stdio {
                    _child: Box::new(child),
                    stdin,
                    stdout: BufReader::new(stdout).lines(),
                }
            }
            (None, Some(url)) => {
                let url = expand_env(url)?;
                let parsed = reqwest::Url::parse(&url).context("invalid MCP HTTP endpoint")?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("MCP URL must use HTTP or HTTPS");
                }
                let mut headers = HeaderMap::new();
                for (name, value) in &config.headers {
                    headers.insert(
                        HeaderName::from_bytes(name.as_bytes())?,
                        HeaderValue::from_str(&expand_env(value)?)?,
                    );
                }
                let client = Client::builder()
                    .connect_timeout(Duration::from_secs(15))
                    .build()?;
                Transport::Http {
                    client,
                    url,
                    headers,
                    session_id: None,
                }
            }
            _ => bail!("MCP server requires exactly one of command or url"),
        };
        let mut connection = Self {
            transport,
            version: version.into(),
            next_id: 0,
        };
        if connection.version != MODERN_VERSION {
            let result = connection
                .rpc(
                    "initialize",
                    json!({
                        "protocolVersion":LEGACY_VERSION,
                        "capabilities":{},
                        "clientInfo":{"name":"kroma","version":env!("CARGO_PKG_VERSION")}
                    }),
                )
                .await?;
            let chosen = result["protocolVersion"]
                .as_str()
                .context("MCP initialize omitted protocolVersion")?;
            if !matches!(
                chosen,
                "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
            ) {
                bail!("unsupported negotiated MCP protocol version {chosen}");
            }
            connection.version = chosen.to_owned();
            connection
                .notify("notifications/initialized", json!({}))
                .await?;
        }
        Ok(connection)
    }

    async fn list_tools(&mut self) -> Result<Vec<Value>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        // A repeated cursor means a broken server; avoid blocking Kroma's
        // startup forever. Normal servers return one page.
        let mut visited = std::collections::HashSet::new();
        loop {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |s| json!({"cursor":s}));
            let result = self.rpc("tools/list", params).await?;
            ensure_complete(&result)?;
            let page = result["tools"]
                .as_array()
                .context("MCP tools/list omitted tools array")?;
            tools.extend(page.iter().cloned());
            cursor = result["nextCursor"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if let Some(cursor) = cursor.as_deref() {
                if !visited.insert(cursor.to_owned()) {
                    bail!("MCP server repeated the tools/list cursor");
                }
            } else {
                break;
            }
        }
        Ok(tools)
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        let result = self
            .rpc("tools/call", json!({"name":name,"arguments":args}))
            .await?;
        ensure_complete(&result)?;
        Ok(result)
    }

    fn message(&mut self, method: &str, mut params: Value, request: bool) -> Result<(u64, Value)> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("MCP request ID overflow")?;
        if self.version == MODERN_VERSION && request {
            let params = params
                .as_object_mut()
                .context("MCP params must be an object")?;
            params.insert("_meta".into(),json!({
                "io.modelcontextprotocol/protocolVersion":MODERN_VERSION,
                "io.modelcontextprotocol/clientInfo":{"name":"kroma","version":env!("CARGO_PKG_VERSION")},
                "io.modelcontextprotocol/clientCapabilities":{}
            }));
        }
        let mut value = json!({"jsonrpc":"2.0","method":method,"params":params});
        if request {
            value["id"] = json!(self.next_id);
        }
        Ok((self.next_id, value))
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        let (id, message) = self.message(method, params, true)?;
        let response = self
            .send(message, Some(id))
            .await?
            .with_context(|| format!("MCP {method} did not return a response"))?;
        if let Some(error) = response.get("error") {
            bail!("MCP {method} failed: {error}");
        }
        response
            .get("result")
            .cloned()
            .context("MCP response missing result")
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let (_, message) = self.message(method, params, false)?;
        self.send(message, None).await?;
        Ok(())
    }

    async fn send(&mut self, message: Value, id: Option<u64>) -> Result<Option<Value>> {
        match &mut self.transport {
            Transport::Stdio { stdin, stdout, .. } => {
                let mut raw = serde_json::to_vec(&message)?;
                raw.push(b'\n');
                stdin
                    .write_all(&raw)
                    .await
                    .context("MCP stdio write failed")?;
                stdin.flush().await?;
                if let Some(id) = id {
                    loop {
                        let line = stdout
                            .next_line()
                            .await
                            .context("reading MCP stdio response")?
                            .context("MCP server closed stdout before responding")?;
                        let reply: Value = serde_json::from_str(&line)
                            .context("MCP server wrote non-JSON content to stdout")?;
                        if matches_id(&reply, id) {
                            return Ok(Some(reply));
                        }
                        if let (Some(other), Some(method)) =
                            (reply.get("id"), reply["method"].as_str())
                        {
                            // Older MCP servers may ask for roots/sampling.
                            // Advertise neither; answer unsupported requests
                            // instead of hanging until the original RPC ends.
                            if self.version != MODERN_VERSION {
                                let response = if method == "ping" {
                                    json!({"jsonrpc":"2.0","id":other,"result":{}})
                                } else {
                                    json!({"jsonrpc":"2.0","id":other,"error":{
                                        "code":-32601,"message":"Method not supported by Kroma"
                                    }})
                                };
                                let mut raw = serde_json::to_vec(&response)?;
                                raw.push(b'\n');
                                stdin.write_all(&raw).await?;
                                stdin.flush().await?;
                            }
                        }
                        // Ignore unrelated notifications/progress and stale
                        // responses to previously cancelled requests.
                    }
                }
                Ok(None)
            }
            Transport::Http {
                client,
                url,
                headers,
                session_id,
            } => {
                let method = message["method"].as_str().unwrap_or("");
                let mut request = client
                    .post(url.as_str())
                    .headers(headers.clone())
                    .header(ACCEPT, "application/json, text/event-stream")
                    .header(CONTENT_TYPE, "application/json")
                    .json(&message);
                if self.version == MODERN_VERSION {
                    request = request
                        .header("MCP-Protocol-Version", MODERN_VERSION)
                        .header("Mcp-Method", method);
                    if method == "tools/call" {
                        if let Some(name) = message["params"]["name"].as_str() {
                            request = request.header("Mcp-Name", name);
                        }
                    }
                } else if method != "initialize" {
                    request = request.header("MCP-Protocol-Version", self.version.as_str());
                }
                if let Some(session) = session_id.as_deref() {
                    request = request.header("Mcp-Session-Id", session);
                }
                let response = request.send().await.context("MCP HTTP request failed")?;
                if let Some(session) = response.headers().get("mcp-session-id") {
                    *session_id = Some(session.to_str()?.to_owned());
                }
                if !response.status().is_success() {
                    let status = response.status();
                    let excerpt = response.text().await.unwrap_or_default();
                    bail!(
                        "MCP HTTP {status}: {}",
                        excerpt.chars().take(600).collect::<String>()
                    );
                }
                if let Some(id) = id {
                    Ok(Some(read_http_response(response, id).await?))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

fn ensure_complete(result: &Value) -> Result<()> {
    match result["resultType"].as_str() {
        None | Some("complete") => Ok(()),
        Some("input_required") => bail!("MCP request requires client input (not supported yet)"),
        Some(other) => bail!("unsupported MCP resultType '{other}'"),
    }
}

fn matches_id(value: &Value, id: u64) -> bool {
    value["jsonrpc"] == "2.0"
        && value.get("method").is_none()
        && (value.get("result").is_some() ^ value.get("error").is_some())
        && value["id"].as_u64() == Some(id)
}

async fn read_http_response(response: Response, id: u64) -> Result<Value> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|s| s.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if content_type.starts_with("application/json") {
        let reply: Value = response.json().await.context("invalid MCP JSON response")?;
        if !matches_id(&reply, id) {
            bail!("MCP HTTP returned a mismatched JSON-RPC ID");
        }
        return Ok(reply);
    }
    if !content_type.starts_with("text/event-stream") {
        bail!("unexpected MCP HTTP content type '{content_type}'");
    }
    let mut stream = response.bytes_stream();
    let mut pending = Vec::<u8>::new();
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        pending.extend_from_slice(&chunk.context("MCP SSE stream failed")?);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=end).collect::<Vec<_>>();
            if let Some(reply) = process_sse_line(&line[..line.len() - 1], &mut data, id)? {
                return Ok(reply);
            }
        }
    }
    if !pending.is_empty() {
        if let Some(reply) = process_sse_line(&pending, &mut data, id)? {
            return Ok(reply);
        }
    }
    if let Some(reply) = process_sse_line(b"", &mut data, id)? {
        return Ok(reply);
    }
    bail!("MCP HTTP SSE stream ended without the matching JSON-RPC response")
}

fn process_sse_line(line: &[u8], data: &mut Vec<String>, id: u64) -> Result<Option<Value>> {
    let line = std::str::from_utf8(line)?.trim_end_matches('\r');
    if line.is_empty() {
        if data.is_empty() {
            return Ok(None);
        }
        let assembled = data.join("\n");
        data.clear();
        if assembled.trim().is_empty() {
            return Ok(None);
        }
        let reply: Value =
            serde_json::from_str(&assembled).context("invalid JSON in MCP SSE data")?;
        return Ok(matches_id(&reply, id).then_some(reply));
    }
    if let Some(value) = line.strip_prefix("data:") {
        data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
    }
    Ok(None)
}

fn tool_name(server: &str, remote: &str) -> Option<String> {
    let sanitize = |input: &str| {
        input
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    let name = format!("mcp__{}__{}", sanitize(server), sanitize(remote));
    (!server.is_empty() && !remote.is_empty() && name.len() <= 64).then_some(name)
}

struct McpTool {
    local_name: String,
    remote_name: String,
    description: String,
    schema: Value,
    connection: Arc<Mutex<Connection>>,
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.local_name
    }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{
            "name":self.local_name,
            "description":self.description,
            "parameters":self.schema
        }})
    }
    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            let outcome = async {
                let args: Value = serde_json::from_str(&call.function.arguments)
                    .context("invalid MCP tool arguments")?;
                if !args.is_object() {
                    bail!("MCP tool arguments must be an object");
                }
                // Tool operations may legitimately run for minutes. Unlike
                // startup, an active MCP call has no arbitrary hard deadline;
                // cancelling the Agent drops the pending request future.
                self.connection
                    .lock()
                    .await
                    .call_tool(&self.remote_name, args)
                    .await
            }
            .await;
            match outcome {
                Ok(mut value) => {
                    if let Err(error) = archive_mcp_media(&mut value, ctx.archive) {
                        return ToolResult::error(json!({"error": error.to_string()}).to_string());
                    }
                    let is_error = value
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if is_error {
                        ToolResult::error(value.to_string())
                    } else {
                        ToolResult::ok(value.to_string())
                    }
                }
                Err(error) => ToolResult::error(json!({"error":error.to_string()}).to_string()),
            }
        })
    }
}

#[derive(Default)]
pub struct Registration {
    pub tools: usize,
    pub unavailable: Vec<String>,
}

pub async fn register_configured(registry: &mut ToolRegistry, workdir: &Path) -> Result<usize> {
    Ok(register_configured_report(registry, workdir).await?.tools)
}

pub async fn register_configured_report(
    registry: &mut ToolRegistry,
    workdir: &Path,
) -> Result<Registration> {
    let config = configured_servers(workdir)?;
    let mut report = Registration::default();
    for (name, server) in config {
        // Keep startup finite on unavailable endpoints. Running tool calls
        // remain cancelable without an arbitrary elapsed-time cutoff.
        let connected = tokio::time::timeout(Duration::from_secs(15), async {
            let mut connection = Connection::connect(&server, workdir).await?;
            let tools = connection.list_tools().await?;
            Ok::<_, anyhow::Error>((connection, tools))
        })
        .await;
        let (connection, tools) = match connected {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                crate::runtime_log!("[mcp] server '{name}' unavailable: {error:#}");
                report.unavailable.push(format!("{name}: {error:#}"));
                continue;
            }
            Err(_) => {
                crate::runtime_log!("[mcp] server '{name}' timed out during startup");
                report.unavailable.push(format!("{name}: startup timeout"));
                continue;
            }
        };
        let connection = Arc::new(Mutex::new(connection));
        for metadata in tools {
            let Some(remote_name) = metadata["name"].as_str() else {
                crate::runtime_log!("[mcp] server '{name}' returned a tool without a name");
                continue;
            };
            let Some(local_name) = tool_name(&name, remote_name) else {
                crate::runtime_log!("[mcp] invalid or too long tool '{name}/{remote_name}'");
                continue;
            };
            let tool = McpTool {
                local_name,
                remote_name: remote_name.to_owned(),
                description: metadata["description"]
                    .as_str()
                    .unwrap_or("MCP tool")
                    .to_owned(),
                schema: metadata
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object","properties":{}})),
                connection: connection.clone(),
            };
            match registry.register(Arc::new(tool)) {
                Ok(()) => report.tools += 1,
                Err(error) => {
                    crate::runtime_log!(
                        "[mcp] skipped duplicate tool {name}/{remote_name}: {error:#}"
                    )
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_tool_results_keep_file_reference_instead_of_base64() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("kroma-mcp-image-test-{}", std::process::id()));
        fs::create_dir_all(&root)?;
        let mut result = json!({
            "content":[{"type":"text","text":"example"},{"type":"image","mimeType":"image/png","data":"aGVsbG8="}],
            "isError":false
        });
        archive_mcp_media(&mut result, &root.join("session.json"))?;
        assert_eq!(result["content"][0]["text"], "example");
        assert!(result["content"][1].get("data").is_none());
        let image = result["content"][1]["path"]
            .as_str()
            .context("image path")?;
        assert_eq!(fs::read(image)?, b"hello");
        fs::remove_file(image)?;
        fs::remove_dir(root)?;
        Ok(())
    }

    #[test]
    fn tool_names_are_stable_and_namespaced() {
        assert_eq!(
            tool_name("filesystem", "read_file"),
            Some("mcp__filesystem__read_file".into())
        );
        assert_eq!(
            tool_name("a.b", "get/item"),
            Some("mcp__a_b__get_item".into())
        );
        assert_eq!(tool_name("x", &"a".repeat(66)), None);
    }

    #[test]
    fn a_server_request_with_our_id_is_not_our_response() {
        let id = 7;
        assert!(matches_id(
            &json!({"jsonrpc":"2.0","id":id,"result":{}}),
            id
        ));
        assert!(matches_id(
            &json!({"jsonrpc":"2.0","id":id,"error":{"code":-1}}),
            id
        ));
        // Bidirectional JSON-RPC: the server may reuse our request ID.
        assert!(!matches_id(
            &json!({"jsonrpc":"2.0","id":id,"method":"ping","params":{}}),
            id
        ));
        assert!(!matches_id(&json!({"jsonrpc":"2.0","id":id}), id));
        assert!(!matches_id(
            &json!({"jsonrpc":"2.0","id":id,"result":{},"error":{}}),
            id
        ));
        assert!(!matches_id(
            &json!({"jsonrpc":"2.0","id":8,"result":{}}),
            id
        ));
    }

    #[test]
    fn modern_request_includes_inline_metadata() {
        let mut connection = Connection {
            transport: Transport::Http {
                client: Client::new(),
                url: "http://localhost".into(),
                headers: HeaderMap::new(),
                session_id: None,
            },
            version: MODERN_VERSION.into(),
            next_id: 0,
        };
        let (_, request) = connection.message("tools/list", json!({}), true).unwrap();
        assert_eq!(
            request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
            MODERN_VERSION
        );
        assert_eq!(
            request["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"],
            json!({})
        );
    }
}
