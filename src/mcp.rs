//! Minimal MCP tools client. Supports local JSON-RPC stdio and remote
//! Streamable HTTP, including legacy initialization-based and modern
//! per-request-metadata versions. Does not implement MCP resources, prompts,
//! sampling, elicitation, or an in-process plugin runtime.
use crate::{
    capabilities::{Capabilities, CapabilityGroup},
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
    collections::{BTreeMap, HashMap},
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_MCP_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_MCP_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_MCP_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Tool calls may legitimately run for minutes, but they must not be the only
/// MCP request without a deadline. `KROMA_MCP_CALL_SECS=0` removes it.
const MCP_CALL_TIMEOUT: Duration = Duration::from_secs(600);
const MCP_SEARCH_BUDGET: Duration = Duration::from_secs(30);
const MCP_IDLE_REAP_INTERVAL: Duration = Duration::from_secs(60);
const MCP_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

fn sha256(bytes: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state = [
        0x6a09e667_u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut data = bytes.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in data.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut output = [0_u8; 32];
    for (index, word) in state.iter().enumerate() {
        output[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    output
}

/// Stable content digest used wherever an approval is bound to code bytes.
pub(crate) fn content_digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut digest = String::with_capacity(64);
    for byte in sha256(bytes) {
        let _ = write!(&mut digest, "{byte:02x}");
    }
    digest
}

/// Read at most `limit + 1` bytes so an untrusted file cannot force a full
/// allocation before its size is rejected. The extra byte distinguishes an
/// exactly-at-limit file from an oversized one without a racy metadata pass.
fn read_bounded(path: &Path, limit: u64) -> std::io::Result<(Vec<u8>, bool)> {
    let bytes = crate::attachments::read_bounded(path, limit)?;
    let oversized = bytes.len() as u64 > limit;
    Ok((bytes, oversized))
}

/// Compatibility API: project MCP configuration no longer requires approval.
pub fn project_mcp_trusted(_workdir: &Path) -> Result<bool> {
    Ok(true)
}

/// Compatibility API: returns the config path without recording a trust decision.
pub fn trust_project_mcp(workdir: &Path) -> Result<PathBuf> {
    Ok(workdir.join(".kroma/mcp.json"))
}

/// Compatibility API: revocation no longer changes MCP loading.
pub fn revoke_project_mcp(_workdir: &Path) -> Result<()> {
    Ok(())
}

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
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
        if encoded.len() > MAX_MCP_IMAGE_BYTES {
            bail!("MCP image exceeds the 16 MiB encoded limit");
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("invalid base64 MCP image")?;
        if data.len() > MAX_MCP_IMAGE_BYTES {
            bail!("MCP image exceeds the 16 MiB decoded limit");
        }
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

/// An MCP result has already come back from the remote server. Failure to
/// archive a returned image must not turn a completed side effect into a
/// failed execution (which could cause an unsafe retry).
fn mcp_tool_result(mut value: Value, archive: &Path) -> ToolResult {
    let remote_error = value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Err(error) = archive_mcp_media(&mut value, archive) {
        let warning = format!(
            "Kroma could not archive an MCP image: {error:#}. The remote tool outcome is unchanged; inspect side effects before retrying."
        );
        crate::runtime_log!("[mcp] {warning}");
        // Never return raw image/base64 data to the model after a failed
        // archive, including when earlier images were saved successfully.
        if let Some(items) = value.get_mut("content").and_then(Value::as_array_mut) {
            for item in items.iter_mut() {
                if item["type"] == "image" {
                    *item =
                        json!({"type":"text","text":"[MCP image omitted: local archive failed]"});
                }
            }
            items.push(json!({"type":"text","text":warning}));
        } else if let Some(object) = value.as_object_mut() {
            object.insert("_kroma_warning".into(), json!(warning));
        }
    }
    if remote_error {
        ToolResult::error(value.to_string())
    } else {
        ToolResult::ok(value.to_string())
    }
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
    /// True when this entry came from `<project>/.kroma/mcp.json`.
    #[serde(skip)]
    pub project: bool,
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, ServerConfig>,
}

pub fn configured_servers(workdir: &Path) -> Result<BTreeMap<String, ServerConfig>> {
    let (servers, _warnings) = configured_servers_with(workdir, false)?;
    Ok(servers)
}

/// Display-safe inventory; never includes commands, URLs, env or headers.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ServerSummary {
    pub name: String,
    pub transport: &'static str,
    pub source: &'static str,
}

/// Lists configured servers without connecting or applying session selections.
pub fn configured_server_summaries(workdir: &Path) -> Result<Vec<ServerSummary>> {
    Ok(configured_servers(workdir)?
        .into_iter()
        .map(|(name, server)| ServerSummary {
            name,
            transport: match (&server.command, &server.url) {
                (Some(_), None) => "stdio",
                (None, Some(_)) => "http",
                _ => "invalid",
            },
            source: if server.project { "project" } else { "user" },
        })
        .collect())
}

fn configured_servers_with(
    workdir: &Path,
    project_only: bool,
) -> Result<(BTreeMap<String, ServerConfig>, Vec<String>)> {
    let mut warnings = Vec::new();
    let paths: Vec<(PathBuf, bool)> = if project_only {
        // Test/host seam: inspect exactly the project file, not the real user's
        // MCP servers.
        vec![(workdir.join(".kroma/mcp.json"), true)]
    } else if let Some(override_path) = env::var_os("KROMA_MCP_CONFIG") {
        // An explicit user-selected file is treated as user configuration.
        vec![(PathBuf::from(override_path), false)]
    } else {
        let mut paths = Vec::new();
        if let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
            paths.push((PathBuf::from(home).join(".kroma/mcp.json"), false));
        }
        paths.push((workdir.join(".kroma/mcp.json"), true));
        paths
    };
    let mut servers = BTreeMap::new();
    for (path, project) in paths {
        // Bound allocation before checking the size, then parse the same snapshot.
        let (bytes, oversized) = match read_bounded(&path, MAX_MCP_CONFIG_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) if project => {
                // A broken project config should not prevent startup.
                warnings.push(format!(
                    "project MCP config {} skipped: {error}",
                    path.display()
                ));
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading MCP config {}", path.display()))
            }
        };
        if oversized {
            if project {
                warnings.push(format!(
                    "project MCP config {} skipped: exceeds 1 MiB",
                    path.display()
                ));
                continue;
            }
            bail!("MCP config {} exceeds 1 MiB", path.display());
        }
        let parsed: ConfigFile = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid MCP config {}", path.display()))?;
        for (name, mut server) in parsed.mcp_servers {
            server.project = project;
            servers.insert(name, server);
        }
    }
    Ok((servers, warnings))
}

fn expand_env_for(raw: &str, project: bool) -> Result<String> {
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
        if project && crate::plugins::is_host_secret_env(name) {
            bail!("project MCP config cannot reference host secret environment variable {name}");
        }
        output.push_str(
            &env::var(name).with_context(|| format!("environment variable {name} is not set"))?,
        );
        rest = &suffix[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

#[cfg(unix)]
type StdioProcessGuard = crate::shell::ProcessGroupGuard;
#[cfg(windows)]
type StdioProcessGuard = crate::winsys::ChildTreeGuard;

enum Transport {
    Stdio {
        _child: Box<Child>,
        #[cfg(any(unix, windows))]
        _process_tree: Arc<StdioProcessGuard>,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
    },
    Http {
        client: Client,
        url: String,
        headers: HeaderMap,
        session_id: Option<String>,
    },
    Closed,
}

fn build_stdio_command(
    command: &str,
    config: &ServerConfig,
    workdir: &Path,
    host_env_keys: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Command> {
    let mut process = Command::new(expand_env_for(command, config.project)?);
    for arg in &config.args {
        process.arg(expand_env_for(arg, config.project)?);
    }
    let cwd = config.cwd.as_deref().map_or_else(
        || workdir.to_path_buf(),
        |cwd| {
            if cwd.is_absolute() {
                cwd.to_path_buf()
            } else {
                workdir.join(cwd)
            }
        },
    );
    process.current_dir(cwd);
    if config.project {
        for key in host_env_keys {
            if crate::plugins::is_host_secret_env(&key.to_string_lossy()) {
                process.env_remove(key);
            }
        }
    }
    for (key, value) in &config.env {
        process.env(key, expand_env_for(value, config.project)?);
    }
    Ok(process)
}

struct Connection {
    transport: Transport,
    version: String,
    next_id: u64,
    invalid: Option<String>,
    cancelled: Arc<AtomicBool>,
    request_timeout: Duration,
    // Reconnect only for the next call, never replay a cancelled request.
    reconnect: Option<(ServerConfig, PathBuf)>,
}

impl Connection {
    async fn connect(config: &ServerConfig, workdir: &Path) -> Result<Self> {
        let version = config.protocol_version.as_deref().unwrap_or(LEGACY_VERSION);
        if version != LEGACY_VERSION && version != MODERN_VERSION {
            bail!("protocolVersion must be {LEGACY_VERSION} or {MODERN_VERSION}");
        }
        let transport = match (&config.command, &config.url) {
            (Some(command), None) => {
                let mut process = build_stdio_command(
                    command,
                    config,
                    workdir,
                    env::vars_os().map(|(key, _)| key),
                )?;
                // Third-party server logs are noisy and can contain secrets.
                // Keep them out of Kroma's output unless explicitly requested.
                process.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(
                    if env::var_os("KROMA_MCP_STDERR").is_some_and(|value| value == "inherit") {
                        Stdio::inherit()
                    } else {
                        Stdio::null()
                    },
                );
                process.kill_on_drop(true);
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    process.as_std_mut().process_group(0);
                }
                #[cfg(windows)]
                let (mut child, process_tree) = crate::winsys::ChildTreeGuard::spawn(&mut process)
                    .context("unable to launch contained MCP server process")?;
                #[cfg(not(windows))]
                let mut child = process
                    .spawn()
                    .context("unable to launch MCP server process")?;
                #[cfg(unix)]
                let process_tree = {
                    let pid = child.id().context("MCP server process has no PID")?;
                    crate::shell::ProcessGroupGuard::new(pid as i32)
                };
                let stdin = child.stdin.take().context("MCP server stdin unavailable")?;
                let stdout = child
                    .stdout
                    .take()
                    .context("MCP server stdout unavailable")?;
                Transport::Stdio {
                    _child: Box::new(child),
                    #[cfg(any(unix, windows))]
                    _process_tree: Arc::new(process_tree),
                    stdin,
                    stdout: BufReader::new(stdout),
                }
            }
            (None, Some(url)) => {
                let url = expand_env_for(url, config.project)?;
                let parsed = reqwest::Url::parse(&url).context("invalid MCP HTTP endpoint")?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("MCP URL must use HTTP or HTTPS");
                }
                let mut headers = HeaderMap::new();
                for (name, value) in &config.headers {
                    headers.insert(
                        HeaderName::from_bytes(name.as_bytes())?,
                        HeaderValue::from_str(&expand_env_for(value, config.project)?)?,
                    );
                }
                let client = Client::builder()
                    .connect_timeout(Duration::from_secs(15))
                    // Never forward configured MCP auth headers to a redirect
                    // target with a different origin.
                    .redirect(reqwest::redirect::Policy::none())
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
            invalid: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            request_timeout: MCP_REQUEST_TIMEOUT,
            reconnect: Some((config.clone(), workdir.to_path_buf())),
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
        // Tool operations may legitimately run for minutes, so they get a
        // generous deadline of their own. Agent cancellation still drops this
        // future and the process guard closes the stdio tree.
        let result = self
            .rpc_with_timeout(
                "tools/call",
                json!({"name":name,"arguments":args}),
                mcp_call_timeout(),
            )
            .await?;
        ensure_complete(&result)?;
        if serde_json::to_vec(&result)?.len() > MAX_MCP_RESPONSE_BYTES {
            bail!("MCP tool result exceeds the 8 MiB limit");
        }
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

    fn ensure_usable(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            bail!("MCP stdio connection is unusable after a cancelled request");
        }
        if let Some(reason) = &self.invalid {
            bail!("MCP stdio connection is unusable after a previous failure: {reason}");
        }
        Ok(())
    }

    fn invalidate(&mut self, reason: impl Into<String>) {
        if self.invalid.is_none() {
            self.invalid = Some(reason.into());
            self.transport = Transport::Closed;
        }
    }

    fn is_invalid(&self) -> bool {
        self.invalid.is_some() || self.cancelled.load(Ordering::Acquire)
    }

    fn cancel_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    #[cfg(any(unix, windows))]
    fn process_tree(&self) -> Option<Arc<StdioProcessGuard>> {
        match &self.transport {
            Transport::Stdio { _process_tree, .. } => Some(Arc::clone(_process_tree)),
            Transport::Http { .. } | Transport::Closed => None,
        }
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.rpc_with_timeout(method, params, Some(self.request_timeout))
            .await
    }

    async fn rpc_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        self.ensure_usable()?;
        let (id, message) = self.message(method, params, true)?;
        let stdio = matches!(&self.transport, Transport::Stdio { .. });
        let response = match timeout {
            Some(timeout) => {
                match tokio::time::timeout(timeout, self.send(message, Some(id))).await {
                    Ok(result) => result
                        .with_context(|| mcp_transport_error_context(method))?
                        .with_context(|| format!("MCP {method} did not return a response"))?,
                    Err(_) => {
                        if stdio {
                            self.invalidate(format!(
                                "MCP {method} timed out after {}s",
                                timeout.as_secs()
                            ));
                        }
                        if method == "tools/call" {
                            bail!(
                                "MCP tools/call timed out after {}s; execution outcome is unknown: inspect state before retrying",
                                timeout.as_secs()
                            );
                        }
                        bail!("MCP {method} timed out after {}s", timeout.as_secs());
                    }
                }
            }
            None => self
                .send(message, Some(id))
                .await
                .with_context(|| mcp_transport_error_context(method))?
                .with_context(|| format!("MCP {method} did not return a response"))?,
        };
        if let Some(error) = response.get("error") {
            bail!("MCP {method} failed: {error}");
        }
        response
            .get("result")
            .cloned()
            .context("MCP response missing result")
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.ensure_usable()?;
        let (_, message) = self.message(method, params, false)?;
        let timeout = self.request_timeout;
        let stdio = matches!(&self.transport, Transport::Stdio { .. });
        match tokio::time::timeout(timeout, self.send(message, None)).await {
            Ok(result) => {
                result?;
            }
            Err(_) => {
                if stdio {
                    self.invalidate(format!(
                        "MCP {method} timed out after {}s",
                        timeout.as_secs()
                    ));
                }
                bail!("MCP {method} timed out after {}s", timeout.as_secs());
            }
        }
        Ok(())
    }

    async fn send(&mut self, message: Value, id: Option<u64>) -> Result<Option<Value>> {
        self.ensure_usable()?;
        let stdio = matches!(&self.transport, Transport::Stdio { .. });
        let result = self.send_inner(message, id).await;
        if let Err(error) = &result {
            if stdio {
                self.invalidate(format!("{error:#}"));
            }
        }
        result
    }

    async fn send_inner(&mut self, message: Value, id: Option<u64>) -> Result<Option<Value>> {
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
                        let line = read_stdio_line(stdout).await?;
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
                    let excerpt = read_limited_body(response, 64 * 1024)
                        .await
                        .map(|body| String::from_utf8_lossy(&body).into_owned())
                        .unwrap_or_default();
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
            Transport::Closed => bail!("MCP connection is closed"),
        }
    }
}

/// Wall-clock deadline for `tools/call`. Unset uses `MCP_CALL_TIMEOUT`; an
/// explicit `KROMA_MCP_CALL_SECS=0` disables it and leaves only the agent's own
/// idle guard as a bound.
fn mcp_call_timeout() -> Option<Duration> {
    match std::env::var("KROMA_MCP_CALL_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(Duration::from_secs(secs.min(86_400))),
            Err(_) => {
                crate::runtime_log!("[mcp] ignoring invalid KROMA_MCP_CALL_SECS={raw:?}");
                Some(MCP_CALL_TIMEOUT)
            }
        },
        Err(_) => Some(MCP_CALL_TIMEOUT),
    }
}

fn mcp_transport_error_context(method: &str) -> &'static str {
    if method == "tools/call" {
        "MCP tool response unavailable: execution outcome is unknown; inspect state before retrying"
    } else {
        "MCP request transport failed"
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

async fn read_limited_body(response: Response, limit: usize) -> Result<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("MCP HTTP body failed")?;
        if body.len().saturating_add(chunk.len()) > limit {
            bail!("MCP HTTP response exceeds the configured size limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn read_stdio_line(reader: &mut BufReader<ChildStdout>) -> Result<String> {
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .context("reading MCP stdio response")?;
        if available.is_empty() {
            bail!("MCP server closed stdout before responding");
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |at| at + 1);
        if line.len().saturating_add(take) > MAX_MCP_RESPONSE_BYTES {
            bail!("MCP stdio response exceeds the 8 MiB limit");
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            break;
        }
    }
    let mut text = String::from_utf8(line).context("MCP server wrote non-UTF-8 stdout")?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
}

async fn read_http_response(response: Response, id: u64) -> Result<Value> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|s| s.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if content_type.starts_with("application/json") {
        let body = read_limited_body(response, MAX_MCP_RESPONSE_BYTES).await?;
        let reply: Value = serde_json::from_slice(&body).context("invalid MCP JSON response")?;
        if !matches_id(&reply, id) {
            bail!(
                "the MCP server answered a different request than Kroma sent (JSON-RPC id mismatch). Restart the server, or update it."
            );
        }
        return Ok(reply);
    }
    if !content_type.starts_with("text/event-stream") {
        bail!(
            "the MCP server answered with '{content_type}' instead of JSON or an event stream. Check the server URL, and that the server speaks MCP over HTTP."
        );
    }
    let mut stream = response.bytes_stream();
    let mut pending = Vec::<u8>::new();
    let mut data = Vec::new();
    let mut total = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("MCP SSE stream failed")?;
        total = total.saturating_add(chunk.len());
        if total > MAX_MCP_RESPONSE_BYTES {
            bail!("MCP SSE response exceeds the 8 MiB limit");
        }
        if pending.len().saturating_add(chunk.len()) > MAX_MCP_RESPONSE_BYTES {
            bail!("MCP SSE frame exceeds the 8 MiB limit");
        }
        pending.extend_from_slice(&chunk);
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
        let value = value.strip_prefix(' ').unwrap_or(value);
        if data
            .iter()
            .map(String::len)
            .sum::<usize>()
            .saturating_add(value.len())
            > MAX_MCP_RESPONSE_BYTES
        {
            bail!("MCP SSE event exceeds the 8 MiB limit");
        }
        data.push(value.to_owned());
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

#[cfg(any(unix, windows))]
struct McpCancelGuard {
    tree: Arc<StdioProcessGuard>,
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

#[cfg(any(unix, windows))]
impl McpCancelGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(any(unix, windows))]
impl Drop for McpCancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
            self.tree.terminate();
        }
    }
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

    fn manages_timeout(&self) -> bool {
        true
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
                let mut connection = self.connection.lock().await;
                if connection.is_invalid() {
                    let (config, workdir) = connection
                        .reconnect
                        .clone()
                        .context("MCP server cannot be reconnected")?;
                    // Never replay the cancelled call: recover for this new call.
                    *connection = Connection::connect(&config, &workdir)
                        .await
                        .context("MCP server reconnect failed")?;
                }
                // A waiter that is cancelled before this point has no guard and
                // must not affect the call currently holding the shared lock.
                #[cfg(any(unix, windows))]
                let mut cancel = connection.process_tree().map(|tree| McpCancelGuard {
                    tree,
                    cancelled: connection.cancel_signal(),
                    armed: true,
                });
                // Tool operations may legitimately run for minutes. Unlike
                // startup, an active MCP call has no arbitrary hard deadline;
                // cancelling the Agent drops this future and the guard closes
                // the server process tree so a partially read stdio frame is
                // never reused by a later call.
                let result = connection.call_tool(&self.remote_name, args).await;
                #[cfg(any(unix, windows))]
                if let Some(cancel) = cancel.as_mut() {
                    if result.is_ok() || !connection.is_invalid() {
                        cancel.disarm();
                    }
                }
                result
            }
            .await;
            match outcome {
                Ok(value) => mcp_tool_result(value, ctx.archive),
                Err(error) => ToolResult::error(json!({"error":error.to_string()}).to_string()),
            }
        })
    }
}

#[derive(Default)]
pub struct Registration {
    pub tools: usize,
    pub unavailable: Vec<String>,
    /// Non-fatal notices, such as an oversized project config.
    pub warnings: Vec<String>,
}

pub async fn register_configured(registry: &mut ToolRegistry, workdir: &Path) -> Result<usize> {
    Ok(register_configured_report(registry, workdir).await?.tools)
}

/// Test/host seam: load only the project's MCP config.
pub async fn register_configured_project_only_report(
    registry: &mut ToolRegistry,
    workdir: &Path,
) -> Result<Registration> {
    register_configured_report_with(registry, workdir, true, &Capabilities::default()).await
}

pub async fn register_configured_allow_project(
    registry: &mut ToolRegistry,
    workdir: &Path,
) -> Result<usize> {
    Ok(
        register_configured_report_with(registry, workdir, true, &Capabilities::default())
            .await?
            .tools,
    )
}

pub async fn register_configured_report(
    registry: &mut ToolRegistry,
    workdir: &Path,
) -> Result<Registration> {
    register_configured_report_with(registry, workdir, false, &Capabilities::default()).await
}

async fn register_configured_report_with(
    registry: &mut ToolRegistry,
    workdir: &Path,
    project_only: bool,
    settings: &Capabilities,
) -> Result<Registration> {
    if env::var_os("KROMA_NO_MCP").is_some() || !settings.any_enabled(CapabilityGroup::Mcp) {
        return Ok(Registration::default());
    }
    let (mut config, mut warnings) = configured_servers_with(workdir, project_only)?;
    config.retain(|name, _| settings.enabled(CapabilityGroup::Mcp, name, true));
    filter_servers(&mut config, &mut warnings);
    let mut report = Registration {
        warnings,
        ..Registration::default()
    };
    // Probe independent servers concurrently, but register their tools in
    // config order so the model's tool schema/prompt prefix stays stable.
    // A broken server should cost at most one 15s startup timeout, not N.
    let connections =
        futures_util::future::join_all(config.into_iter().map(|(name, server)| async move {
            let connected = tokio::time::timeout(Duration::from_secs(15), async {
                let mut connection = Connection::connect(&server, workdir).await?;
                let tools = connection.list_tools().await?;
                Ok::<_, anyhow::Error>((connection, tools))
            })
            .await;
            (name, connected)
        }))
        .await;
    for (name, connected) in connections {
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

    fn capability_call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "capability-call".into(),
            kind: "function".into(),
            function: crate::provider::FunctionCall {
                name: name.into(),
                arguments: arguments.to_string(),
            },
        }
    }

    fn capability_context<'a>(dir: &'a Path, archive: &'a Path) -> ToolContext<'a> {
        ToolContext {
            model_connection: None,
            user_message: None,
            user_turn: None,
            user_images: None,
            previous_user_images: None,
            workdir: dir,
            archive,
            on_output: None,
            ask: None,
        }
    }

    #[tokio::test]
    async fn capability_filters_block_servers_and_lazy_call_alias_bypasses() -> Result<()> {
        let dir = scratch("capability-filter");
        fs::create_dir_all(dir.join(".kroma"))?;
        let script = dir.join("server.cjs");
        fs::write(
            &script,
            r#"
const fs = require('fs');
const tag = process.argv[2];
fs.writeFileSync(tag + '.started', 'yes');
require('readline').createInterface({input:process.stdin}).on('line', line => {
  const r = JSON.parse(line);
  if (r.id === undefined) return;
  let result = {};
  if (r.method === 'initialize') result = {protocolVersion:'2024-11-05',capabilities:{tools:{}},serverInfo:{name:'probe',version:'1'}};
  if (r.method === 'tools/list') result = {tools:['allowed','secret'].map(name => ({name,description:name,inputSchema:{type:'object',properties:{}}}))};
  if (r.method === 'tools/call') {
    fs.appendFileSync(tag + '.calls', r.params.name + '\n');
    result = {content:[{type:'text',text:r.params.name}],isError:false};
  }
  console.log(JSON.stringify({jsonrpc:'2.0',id:r.id,result}));
});
"#,
        )?;
        fs::write(
            dir.join(".kroma/mcp.json"),
            json!({"mcpServers": {
                "active": {"command":"node","args":[script,"active"]},
                "blocked": {"command":"node","args":[script,"blocked"]}
            }})
            .to_string(),
        )?;
        let mut settings = Capabilities {
            mcp: false,
            ..Default::default()
        };
        settings.items.mcp.insert("active".into(), true);
        settings.items.mcp.insert("blocked".into(), false);
        settings
            .items
            .tools
            .insert("mcp__active__secret".into(), false);
        let archive = dir.join("session.json");
        let ctx = capability_context(&dir, &archive);
        let mut lazy = ToolRegistry::new();
        register_configured_lazy_with(&mut lazy, &dir, true, &settings).await?;
        assert!(
            !dir.join("active.started").exists(),
            "lazy registration connected eagerly"
        );
        let search = lazy
            .execute(&capability_call("mcp_search", json!({})), ctx)
            .await;
        assert!(!search.is_error, "{}", search.content);
        let found: Value = serde_json::from_str(&search.content)?;
        assert!(found["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["tool"] == "allowed"));
        assert!(!search.content.contains("secret"));
        assert!(dir.join("active.started").exists());
        assert!(!dir.join("blocked.started").exists());
        for call in [
            capability_call("mcp_call", json!({"server":"active","tool":"secret"})),
            capability_call("mcp_call", json!({"server":"blocked","tool":"allowed"})),
            capability_call("mcp__active__secret", json!({})),
        ] {
            assert!(lazy.execute(&call, ctx).await.is_error);
        }
        assert!(!dir.join("active.calls").exists());
        assert!(!dir.join("blocked.started").exists());
        let allowed = lazy
            .execute(
                &capability_call("mcp_call", json!({"server":"active","tool":"allowed"})),
                ctx,
            )
            .await;
        assert!(!allowed.is_error, "{}", allowed.content);
        assert_eq!(fs::read_to_string(dir.join("active.calls"))?, "allowed\n");
        drop(lazy);
        let mut eager = ToolRegistry::new();
        register_configured_report_with(&mut eager, &dir, true, &settings).await?;
        eager.retain_enabled(&settings);
        let definitions = serde_json::to_string(&eager.definitions())?;
        assert!(definitions.contains("mcp__active__allowed"));
        assert!(!definitions.contains("secret"));
        assert!(!definitions.contains("blocked"));
        assert!(
            eager
                .execute(&capability_call("mcp__active__secret", json!({})), ctx)
                .await
                .is_error
        );
        assert!(
            !eager
                .execute(&capability_call("mcp__active__allowed", json!({})), ctx)
                .await
                .is_error
        );
        assert!(!dir.join("blocked.started").exists());
        drop(eager);
        tokio::time::sleep(Duration::from_millis(100)).await;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

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
    fn media_postprocessing_failure_does_not_invert_remote_outcome() -> Result<()> {
        let value = json!({
            "content": [
                {"type":"text","text":"side effect completed"},
                {"type":"image","mimeType":"image/png","data":"!invalid-base64!"}
            ],
            "isError": false
        });
        let outcome = mcp_tool_result(value.clone(), Path::new("session.json"));
        assert!(!outcome.is_error);
        let payload: Value = serde_json::from_str(&outcome.content)?;
        assert_eq!(payload["content"][0]["text"], "side effect completed");
        assert!(payload["content"][1].get("data").is_none());
        assert!(payload["content"][2]["text"]
            .as_str()
            .unwrap()
            .contains("outcome is unchanged"));
        assert!(!outcome.content.contains("!invalid-base64!"));

        let mut remote_failure = value;
        remote_failure["isError"] = json!(true);
        assert!(mcp_tool_result(remote_failure, Path::new("session.json")).is_error);
        Ok(())
    }

    #[tokio::test]
    async fn timed_out_tool_rpc_reports_unknown_outcome() -> Result<()> {
        let dir = scratch("call-timeout");
        let mut connection = with_test_timeout(connect_mock(&dir, MockMode::Timeout)).await?;
        let error = with_test_timeout(connection.rpc_with_timeout(
            "tools/call",
            json!({"name":"slow","arguments":{}}),
            Some(Duration::from_millis(150)),
        ))
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("outcome is unknown"),
            "{error:#}"
        );
        drop(connection);
        let _ = fs::remove_dir_all(dir);
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
            invalid: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            request_timeout: MCP_REQUEST_TIMEOUT,
            reconnect: None,
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

    #[test]
    fn fingerprint_is_sha256() {
        assert_eq!(
            content_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kroma-mcp-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bounded_read_stops_one_byte_past_the_limit() {
        let dir = scratch("bounded-read");
        let path = dir.join("oversized.json");
        fs::File::create(&path)
            .unwrap()
            .set_len(32 * 1024 * 1024)
            .unwrap();
        let (bytes, oversized) = read_bounded(&path, 1024).unwrap();
        assert!(oversized);
        assert_eq!(bytes.len(), 1025, "read only limit+1 bytes");
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    fn mock_server_script(dir: &Path, body: &str) -> (String, Vec<String>) {
        let path = dir.join("server.sh");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        ("sh".into(), vec![path.to_string_lossy().into_owned()])
    }

    #[cfg(windows)]
    fn mock_server_script(dir: &Path, body: &str) -> (String, Vec<String>) {
        let path = dir.join("server.ps1");
        fs::write(&path, body).unwrap();
        (
            "powershell.exe".into(),
            vec![
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                path.to_string_lossy().into_owned(),
            ],
        )
    }

    #[derive(Clone, Copy, Debug)]
    enum MockMode {
        Normal,
        NearLimit,
        OverLimit,
        InvalidJson,
        Eof,
        Timeout,
        ProcessTree,
        TreeTimeout,
        Serialized,
    }

    fn mock_config(dir: &Path, mode: MockMode) -> ServerConfig {
        let (command, args) = mock_server_script(dir, &mock_script_body(dir, mode));
        ServerConfig {
            command: Some(command),
            args,
            env: BTreeMap::new(),
            cwd: Some(dir.to_path_buf()),
            url: None,
            headers: BTreeMap::new(),
            protocol_version: Some(MODERN_VERSION.into()),
            project: false,
        }
    }

    #[cfg(unix)]
    fn mock_script_body(dir: &Path, mode: MockMode) -> String {
        match mode {
            MockMode::Normal => "read line\nprintf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":1}}'\nread line\nprintf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":2}}'".into(),
            MockMode::NearLimit => "read line\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"data\":\"'\nhead -c 8300000 /dev/zero | tr '\\0' x\nprintf '\"}}\n'".into(),
            MockMode::OverLimit => "read line\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"data\":\"'\nhead -c 8400000 /dev/zero | tr '\\0' x\nprintf '\"}}\n'".into(),
            MockMode::InvalidJson => "read line\nprintf '%s\n' 'not-json'".into(),
            MockMode::Eof => "read line\nexit 0".into(),
            MockMode::Timeout => "read line\nsleep 5".into(),
            MockMode::ProcessTree | MockMode::TreeTimeout => format!(
                "cd '{}'\n(sleep 2; printf survived > survived.txt) &\nprintf started > started.txt\nread line\nsleep 5",
                dir.display()
            ),
            MockMode::Serialized => format!(
                "cd '{}'\n(sleep 5; printf survived > survived.txt) &\nprintf started > started.txt\nread line\nwhile [ ! -f release.txt ]; do sleep 0.05; done\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"ok\":1}}}}'\nread line\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"ok\":2}}}}'\nsleep 2",
                dir.display()
            ),
        }
    }

    #[cfg(windows)]
    fn mock_script_body(dir: &Path, mode: MockMode) -> String {
        match mode {
            MockMode::Normal => "[Console]::In.ReadLine() | Out-Null\n[Console]::Out.WriteLine('{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":1}}')\n[Console]::In.ReadLine() | Out-Null\n[Console]::Out.WriteLine('{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":2}}')".into(),
            MockMode::NearLimit => "[Console]::In.ReadLine() | Out-Null\n$p = '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"data\":\"' + ('x' * 8300000) + '\"}}'\n[Console]::Out.WriteLine($p)".into(),
            MockMode::OverLimit => "[Console]::In.ReadLine() | Out-Null\n$p = '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"data\":\"' + ('x' * 8400000) + '\"}}'\n[Console]::Out.WriteLine($p)".into(),
            MockMode::InvalidJson => "[Console]::In.ReadLine() | Out-Null\n[Console]::Out.WriteLine('not-json')".into(),
            MockMode::Eof => "[Console]::In.ReadLine() | Out-Null\nexit 0".into(),
            MockMode::Timeout => "[Console]::In.ReadLine() | Out-Null\nStart-Sleep -Seconds 5".into(),
            MockMode::ProcessTree | MockMode::TreeTimeout => format!(
                "Set-Location '{}'\n$null = Start-Process -FilePath 'cmd.exe' -ArgumentList '/C','ping -n 3 127.0.0.1 >nul & echo survived>survived.txt' -NoNewWindow -PassThru\nSet-Content -Path 'started.txt' -Value 'started'\n[Console]::In.ReadLine() | Out-Null\nStart-Sleep -Seconds 5",
                dir.display()
            ),
            MockMode::Serialized => format!(
                "Set-Location '{}'\n$null = Start-Process -FilePath 'cmd.exe' -ArgumentList '/C','ping -n 6 127.0.0.1 >nul & echo survived>survived.txt' -NoNewWindow -PassThru\nSet-Content -Path 'started.txt' -Value 'started'\n[Console]::In.ReadLine() | Out-Null\nwhile (-not (Test-Path 'release.txt')) {{ Start-Sleep -Milliseconds 50 }}\n[Console]::Out.WriteLine('{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"ok\":1}}}}')\n[Console]::In.ReadLine() | Out-Null\n[Console]::Out.WriteLine('{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"ok\":2}}}}')\nStart-Sleep -Seconds 2",
                dir.display()
            ),
        }
    }

    async fn connect_mock(dir: &Path, mode: MockMode) -> Result<Connection> {
        let config = mock_config(dir, mode);
        Connection::connect(&config, dir).await
    }

    async fn with_test_timeout<F, T>(future: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::time::timeout(Duration::from_secs(20), future)
            .await
            .context("mock MCP test exceeded its hard timeout")?
    }

    #[tokio::test]
    async fn project_stdio_cwd_can_be_outside_the_project() -> Result<()> {
        let dir = scratch("project-cwd");
        let project = dir.join("project");
        let server = dir.join("server");
        fs::create_dir_all(&project)?;
        fs::create_dir_all(&server)?;
        let mut config = mock_config(&server, MockMode::Normal);
        config.project = true;
        for cwd in [PathBuf::from("../server"), server.clone()] {
            config.cwd = Some(cwd);
            let mut connection = with_test_timeout(Connection::connect(&config, &project)).await?;
            let result = with_test_timeout(connection.rpc("first", json!({}))).await?;
            assert_eq!(result["ok"], 1);
            let result = with_test_timeout(connection.rpc("second", json!({}))).await?;
            assert_eq!(result["ok"], 2);
        }
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[tokio::test]
    async fn stdio_normal_and_near_limit_responses_work() -> Result<()> {
        let dir = scratch("normal");
        let mut connection = with_test_timeout(connect_mock(&dir, MockMode::Normal)).await?;
        let first = with_test_timeout(connection.rpc("first", json!({}))).await?;
        assert_eq!(first["ok"], 1);
        let second = with_test_timeout(connection.rpc("second", json!({}))).await?;
        assert_eq!(second["ok"], 2);
        drop(connection);

        let near_dir = scratch("near-limit");
        let mut near = with_test_timeout(connect_mock(&near_dir, MockMode::NearLimit)).await?;
        let result = with_test_timeout(near.rpc("near", json!({}))).await?;
        assert_eq!(result["data"].as_str().unwrap().len(), 8_300_000);
        drop(near);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(near_dir);
        Ok(())
    }

    #[tokio::test]
    async fn stdio_overlimit_invalidates_without_reusing_stale_bytes() -> Result<()> {
        let dir = scratch("over-limit");
        let mut connection = with_test_timeout(connect_mock(&dir, MockMode::OverLimit)).await?;
        let error = with_test_timeout(connection.rpc("first", json!({})))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("8 MiB"), "{error:#}");
        let second = with_test_timeout(connection.rpc("second", json!({})))
            .await
            .unwrap_err();
        assert!(
            format!("{second:#}").contains("unusable"),
            "stale connection was reused: {second:#}"
        );
        drop(connection);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[tokio::test]
    async fn stdio_invalid_json_and_eof_invalidate_connection() -> Result<()> {
        for mode in [MockMode::InvalidJson, MockMode::Eof] {
            let dir = scratch("broken");
            let mut connection = with_test_timeout(connect_mock(&dir, mode)).await?;
            let error = with_test_timeout(connection.rpc("first", json!({})))
                .await
                .unwrap_err();
            assert!(!error.to_string().is_empty());
            let second = with_test_timeout(connection.rpc("second", json!({})))
                .await
                .unwrap_err();
            assert!(
                format!("{second:#}").contains("unusable"),
                "connection was reused after {mode:?}: {second:#}"
            );
            drop(connection);
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = fs::remove_dir_all(dir);
        }
        Ok(())
    }

    #[tokio::test]
    async fn stdio_timeout_invalidates_connection() -> Result<()> {
        let dir = scratch("timeout");
        let mut connection = with_test_timeout(connect_mock(&dir, MockMode::Timeout)).await?;
        connection.request_timeout = Duration::from_millis(250);
        let started = std::time::Instant::now();
        let error = with_test_timeout(connection.rpc("first", json!({})))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(2));
        let second = with_test_timeout(connection.rpc("second", json!({})))
            .await
            .unwrap_err();
        assert!(
            format!("{second:#}").contains("unusable"),
            "timed-out connection was reused: {second:#}"
        );
        drop(connection);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[tokio::test]
    async fn one_failed_mcp_server_does_not_break_independent_connection() -> Result<()> {
        let broken_dir = scratch("independent-broken");
        let mut broken =
            with_test_timeout(connect_mock(&broken_dir, MockMode::InvalidJson)).await?;
        assert!(with_test_timeout(broken.rpc("broken", json!({})))
            .await
            .is_err());
        drop(broken);

        let good_dir = scratch("independent-good");
        let mut good = with_test_timeout(connect_mock(&good_dir, MockMode::Normal)).await?;
        let result = with_test_timeout(good.rpc("good", json!({}))).await?;
        assert_eq!(result["ok"], 1);
        drop(good);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = fs::remove_dir_all(broken_dir);
        let _ = fs::remove_dir_all(good_dir);
        Ok(())
    }

    #[tokio::test]
    async fn mcp_timeout_cleans_up_descendant_processes() -> Result<()> {
        let dir = scratch("timeout-tree");
        let mut connection = with_test_timeout(connect_mock(&dir, MockMode::TreeTimeout)).await?;
        let started = dir.join("started.txt");
        tokio::time::timeout(Duration::from_secs(8), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("mock MCP descendant never started")?;
        connection.request_timeout = Duration::from_millis(250);
        let error = with_test_timeout(connection.rpc("timeout", json!({})))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("timed out"), "{error:#}");
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !dir.join("survived.txt").exists(),
            "MCP descendant survived timeout cleanup"
        );
        drop(connection);
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[tokio::test]
    async fn waiting_shared_tool_cancel_does_not_interrupt_active_call() -> Result<()> {
        let dir = scratch("shared-serialized");
        let tool = Arc::new(McpTool {
            local_name: "mcp__test__serialized".into(),
            remote_name: "serialized".into(),
            description: "serialized test tool".into(),
            schema: json!({"type":"object","properties":{}}),
            connection: Arc::new(tokio::sync::Mutex::new(
                with_test_timeout(connect_mock(&dir, MockMode::Serialized)).await?,
            )),
        });
        let call_a = ToolCall {
            id: "call-a".into(),
            kind: "function".into(),
            function: crate::provider::FunctionCall {
                name: "mcp__test__serialized".into(),
                arguments: "{}".into(),
            },
        };
        let archive_a = dir.join("session-a.json");
        let mut active = Box::pin(tool.execute(
            &call_a,
            ToolContext {
                model_connection: None,
                user_message: None,
                user_turn: None,
                user_images: None,
                previous_user_images: None,
                workdir: &dir,
                archive: &archive_a,
                on_output: None,
                ask: None,
            },
        ));
        // Let A enter the connection lock and send the request. The mock waits
        // for release.txt, so it cannot complete yet.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while tool.connection.try_lock().is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "active call did not acquire the shared connection"
            );
            tokio::select! {
                result = &mut active => panic!("active call completed before release: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }

        let waiting_tool = Arc::clone(&tool);
        let waiting_dir = dir.clone();
        let waiting = tokio::spawn(async move {
            let archive = waiting_dir.join("session-b.json");
            let call = ToolCall {
                id: "call-b".into(),
                kind: "function".into(),
                function: crate::provider::FunctionCall {
                    name: "mcp__test__serialized".into(),
                    arguments: "{}".into(),
                },
            };
            waiting_tool
                .execute(
                    &call,
                    ToolContext {
                        model_connection: None,
                        user_message: None,
                        user_turn: None,
                        user_images: None,
                        previous_user_images: None,
                        workdir: &waiting_dir,
                        archive: &archive,
                        on_output: None,
                        ask: None,
                    },
                )
                .await
        });
        // B must be blocked on the same mutex while A owns it. It has no
        // process-tree guard yet, so aborting B must not kill A's server.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !waiting.is_finished(),
            "B should still be waiting for the lock"
        );
        waiting.abort();
        let _ = waiting.await;

        fs::write(dir.join("release.txt"), "go")?;
        let first = tokio::time::timeout(Duration::from_secs(5), &mut active)
            .await
            .context("active call did not finish after releasing the mock")?;
        assert!(!first.is_error, "active call failed: {}", first.content);
        assert!(first.content.contains("\"ok\":1"), "{}", first.content);

        let mut connection = tokio::time::timeout(Duration::from_secs(2), tool.connection.lock())
            .await
            .context("connection remained locked after the active call")?;
        let second = with_test_timeout(connection.rpc("second", json!({}))).await?;
        assert_eq!(
            second["ok"], 2,
            "B cancellation damaged the shared connection"
        );
        drop(connection);
        drop(active);
        drop(tool);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[tokio::test]
    async fn new_mcp_call_recovers_an_invalid_connection_without_replaying_the_old_call(
    ) -> Result<()> {
        let dir = scratch("reconnect");
        let mut connection = with_test_timeout(connect_mock(&dir, MockMode::Normal)).await?;
        connection.invalidate("previous call was cancelled");
        let tool = McpTool {
            local_name: "mcp__test__first".into(),
            remote_name: "first".into(),
            description: "test".into(),
            schema: json!({"type":"object"}),
            connection: Arc::new(Mutex::new(connection)),
        };
        let call = ToolCall {
            id: "new-call".into(),
            kind: "function".into(),
            function: crate::provider::FunctionCall {
                name: "mcp__test__first".into(),
                arguments: "{}".into(),
            },
        };
        let output = with_test_timeout(async {
            Ok(tool
                .execute(
                    &call,
                    ToolContext {
                        model_connection: None,
                        user_message: None,
                        user_turn: None,
                        user_images: None,
                        previous_user_images: None,
                        workdir: &dir,
                        archive: &dir.join("session.json"),
                        on_output: None,
                        ask: None,
                    },
                )
                .await)
        })
        .await?;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(serde_json::from_str::<Value>(&output.content)?["ok"], 1);
        drop(tool);
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_mcp_tool_closes_descendant_process_tree() -> Result<()> {
        let dir = scratch("cancel-tree");
        let connection = Arc::new(tokio::sync::Mutex::new(
            with_test_timeout(connect_mock(&dir, MockMode::ProcessTree)).await?,
        ));
        let tool = McpTool {
            local_name: "mcp__test__slow".into(),
            remote_name: "slow".into(),
            description: "slow test tool".into(),
            schema: json!({"type":"object","properties":{}}),
            connection,
        };
        let call = ToolCall {
            id: "call".into(),
            kind: "function".into(),
            function: crate::provider::FunctionCall {
                name: "mcp__test__slow".into(),
                arguments: "{}".into(),
            },
        };
        let started = dir.join("started.txt");
        let archive = dir.join("session.json");
        let mut future = Box::pin(tool.execute(
            &call,
            ToolContext {
                model_connection: None,
                user_message: None,
                user_turn: None,
                user_images: None,
                previous_user_images: None,
                workdir: &dir,
                archive: &archive,
                on_output: None,
                ask: None,
            },
        ));
        tokio::select! {
            _ = async {
                while !started.exists() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            } => {}
            result = &mut future => panic!("MCP tool completed instead of staying pending: {result:?}"),
        }
        drop(future);
        let mut connection = tokio::time::timeout(Duration::from_secs(2), tool.connection.lock())
            .await
            .context("cancelled MCP tool retained the connection lock")?;
        assert!(
            connection.is_invalid(),
            "cancelled MCP tool did not invalidate the connection"
        );
        let first_after_cancel = with_test_timeout(connection.rpc("after-cancel", json!({}))).await;
        assert!(
            first_after_cancel.is_err(),
            "cancelled MCP connection still served a request: {first_after_cancel:?}"
        );
        let second_after_cancel = with_test_timeout(connection.rpc("still-cancelled", json!({})))
            .await
            .unwrap_err();
        assert!(
            format!("{second_after_cancel:#}").contains("unusable"),
            "cancelled MCP connection was reused: {second_after_cancel:#}"
        );
        drop(connection);
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !dir.join("survived.txt").exists(),
            "cancelled MCP tool left a descendant process alive"
        );
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[tokio::test]
    async fn mcp_drop_cleans_up_descendant_processes() -> Result<()> {
        let dir = scratch("process-tree");
        let connection = with_test_timeout(connect_mock(&dir, MockMode::ProcessTree)).await?;
        let started = dir.join("started.txt");
        tokio::time::timeout(Duration::from_secs(8), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("mock MCP descendant never started")?;
        drop(connection);
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !dir.join("survived.txt").exists(),
            "MCP descendant escaped process-tree cleanup"
        );
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[tokio::test]
    async fn an_idle_lazy_connection_is_pruned() -> Result<()> {
        let dir = scratch("lazy-idle");
        let connection = with_test_timeout(connect_mock(&dir, MockMode::Normal)).await?;
        let manager = LazyMcp {
            settings: Capabilities::default(),
            servers: BTreeMap::new(),
            workdir: dir.clone(),
            connections: tokio::sync::Mutex::new(HashMap::from([(
                "idle".to_owned(),
                CachedConnection {
                    connection: Arc::new(tokio::sync::Mutex::new(connection)),
                    last_used: Instant::now() - MCP_IDLE_TIMEOUT - Duration::from_secs(1),
                },
            )])),
            connect_locks: tokio::sync::Mutex::new(HashMap::new()),
        };
        manager.prune_idle(MCP_IDLE_TIMEOUT).await;
        assert!(manager.connections.lock().await.is_empty());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(name: &str) -> Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "kroma-mcp-config-{name}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(dir.join(".kroma"))?;
        Ok(dir)
    }

    #[test]
    fn project_config_loads_and_reloads_without_approval() -> Result<()> {
        let dir = scratch("reload")?;
        let path = dir.join(".kroma/mcp.json");
        // No entry-file scanning or env expansion is required to load config.
        fs::write(
            &path,
            br#"{"mcpServers":{"local":{"command":"${UNSET_MCP_ENTRY}","args":["missing.js"]}}}"#,
        )?;
        let (servers, warnings) = configured_servers_with(&dir, true)?;
        assert!(warnings.is_empty());
        assert!(servers["local"].project);
        assert_eq!(
            servers["local"].command.as_deref(),
            Some("${UNSET_MCP_ENTRY}")
        );
        fs::write(
            &path,
            br#"{"mcpServers":{"changed":{"url":"https://example.com/mcp"}}}"#,
        )?;
        let (servers, warnings) = configured_servers_with(&dir, true)?;
        assert!(warnings.is_empty());
        assert!(servers.contains_key("changed"));
        assert!(!servers.contains_key("local"));
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn legacy_trust_apis_do_not_change_loading_or_write_a_store() -> Result<()> {
        let dir = scratch("compat")?;
        let store = dir.join(".kroma/trusted-mcp.json");
        fs::write(&store, "not a trust store")?;
        assert!(project_mcp_trusted(&dir)?);
        assert_eq!(trust_project_mcp(&dir)?, dir.join(".kroma/mcp.json"));
        revoke_project_mcp(&dir)?;
        assert!(project_mcp_trusted(&dir)?);
        assert_eq!(fs::read_to_string(store)?, "not a trust store");
        assert!(!dir.join(".kroma/trusted-mcp.json.lock").exists());
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn missing_project_config_is_empty() -> Result<()> {
        let dir = scratch("missing")?;
        let (servers, warnings) = configured_servers_with(&dir, true)?;
        assert!(servers.is_empty());
        assert!(warnings.is_empty());
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn invalid_project_config_is_reported() -> Result<()> {
        let dir = scratch("invalid")?;
        let path = dir.join(".kroma/mcp.json");
        for content in ["{", r#"{"mcpServers":{"local":{"args":42}}}"#] {
            fs::write(&path, content)?;
            let error = configured_servers_with(&dir, true).unwrap_err();
            assert!(format!("{error:#}").contains("invalid MCP config"));
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn config_size_limit_is_bounded_and_keeps_exact_limit_valid() -> Result<()> {
        let dir = scratch("size")?;
        let path = dir.join(".kroma/mcp.json");
        let mut content = b"{\"mcpServers\":{}}".to_vec();
        content.resize(MAX_MCP_CONFIG_BYTES as usize, b' ');
        fs::write(&path, &content)?;
        let (servers, warnings) = configured_servers_with(&dir, true)?;
        assert!(servers.is_empty());
        assert!(warnings.is_empty());
        content.extend_from_slice(b"  ");
        fs::write(&path, content)?;
        let (bytes, oversized) = read_bounded(&path, MAX_MCP_CONFIG_BYTES)?;
        assert!(oversized);
        assert_eq!(bytes.len() as u64, MAX_MCP_CONFIG_BYTES + 1);
        let (servers, warnings) = configured_servers_with(&dir, true)?;
        assert!(servers.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("exceeds 1 MiB"));
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn project_stdio_server_strips_host_secrets_and_blocks_secret_expansion() -> Result<()> {
        let dir = scratch("project-secret-strip")?;
        let host_keys = [
            std::ffi::OsString::from("KROMA_API_KEY"),
            std::ffi::OsString::from("OPENAI_API_KEY"),
            std::ffi::OsString::from("AWS_SECRET_ACCESS_KEY"),
            std::ffi::OsString::from("PUBLIC_MCP_SETTING"),
        ];
        let project_server = ServerConfig {
            command: Some("node".into()),
            args: vec!["server.js".into()],
            env: BTreeMap::from([("SAFE_VAR".into(), "ok".into())]),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            protocol_version: None,
            project: true,
        };
        let cmd = build_stdio_command("node", &project_server, &dir, host_keys.clone())?;
        let envs: BTreeMap<String, Option<String>> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|val| val.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(envs.get("KROMA_API_KEY"), Some(&None));
        assert_eq!(envs.get("OPENAI_API_KEY"), Some(&None));
        assert_eq!(envs.get("AWS_SECRET_ACCESS_KEY"), Some(&None));
        assert!(!envs.contains_key("PUBLIC_MCP_SETTING"));
        assert_eq!(envs.get("SAFE_VAR").and_then(|v| v.as_deref()), Some("ok"));

        let user_server = ServerConfig {
            project: false,
            ..project_server.clone()
        };
        let user_cmd = build_stdio_command("node", &user_server, &dir, host_keys)?;
        let user_envs: BTreeMap<String, Option<String>> = user_cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|val| val.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(!user_envs.contains_key("KROMA_API_KEY"));
        assert!(!user_envs.contains_key("OPENAI_API_KEY"));

        let secret_ref = ServerConfig {
            env: BTreeMap::from([("LEAK".into(), "${OPENAI_API_KEY}".into())]),
            ..project_server.clone()
        };
        let error = build_stdio_command("node", &secret_ref, &dir, [])
            .expect_err("project MCP must refuse ${OPENAI_API_KEY}");
        assert!(
            error.to_string().contains("host secret"),
            "unexpected error: {error}"
        );
        let unset_err = expand_env_for("${UNSET_MCP_ENTRY}", true)
            .expect_err("non-secret missing env still reports missing env");
        assert!(unset_err.to_string().contains("UNSET_MCP_ENTRY"));

        fs::remove_dir_all(dir)?;
        Ok(())
    }
}

fn filter_servers(config: &mut BTreeMap<String, ServerConfig>, warnings: &mut Vec<String>) {
    let Some(selected) = env::var_os("KROMA_MCP_SERVERS") else {
        return;
    };
    let selected = selected.to_string_lossy();
    let names = selected
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if names.is_empty() {
        return;
    }
    for name in &names {
        if !config.contains_key(*name) {
            warnings.push(format!(
                "MCP server '{name}' was requested with --mcp but is not configured"
            ));
        }
    }
    config.retain(|name, _| names.contains(&name.as_str()));
}

struct CachedConnection {
    connection: Arc<tokio::sync::Mutex<Connection>>,
    last_used: Instant,
}

/// Lazily connected MCP servers, shared by the two discovery tools. No child
/// process starts until the model actually searches or calls a tool.
struct LazyMcp {
    settings: Capabilities,
    servers: BTreeMap<String, ServerConfig>,
    workdir: PathBuf,
    connections: tokio::sync::Mutex<HashMap<String, CachedConnection>>,
    connect_locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl LazyMcp {
    fn tool_enabled(&self, server: &str, remote: &str) -> bool {
        tool_name(server, remote).is_some_and(|name| {
            self.settings
                .items
                .tools
                .get(&name)
                .copied()
                .unwrap_or(true)
        })
    }
    async fn server_connect_lock(&self, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.connect_locks.lock().await;
        Arc::clone(
            locks
                .entry(name.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }
    async fn connection(&self, name: &str) -> Result<Arc<tokio::sync::Mutex<Connection>>> {
        let existing = {
            let mut connections = self.connections.lock().await;
            connections.get_mut(name).map(|cached| {
                cached.last_used = Instant::now();
                Arc::clone(&cached.connection)
            })
        };
        if let Some(connection) = existing {
            if !connection.lock().await.is_invalid() {
                return Ok(connection);
            }
            self.connections.lock().await.remove(name);
        }
        // Serialize cold-starts per server so concurrent calls never spawn
        // duplicate child processes while leaving other servers unblocked.
        let connect_lock = self.server_connect_lock(name).await;
        let _connect_guard = connect_lock.lock().await;
        let existing = {
            let mut connections = self.connections.lock().await;
            connections.get_mut(name).map(|cached| {
                cached.last_used = Instant::now();
                Arc::clone(&cached.connection)
            })
        };
        if let Some(connection) = existing {
            if !connection.lock().await.is_invalid() {
                return Ok(connection);
            }
            self.connections.lock().await.remove(name);
        }
        let config = self
            .servers
            .get(name)
            .with_context(|| format!("unknown MCP server '{name}'"))?;
        let connection = tokio::time::timeout(
            Duration::from_secs(15),
            Connection::connect(config, &self.workdir),
        )
        .await
        .with_context(|| format!("MCP server '{name}' startup timed out"))??;
        let connection = Arc::new(tokio::sync::Mutex::new(connection));
        let mut connections = self.connections.lock().await;
        connections.insert(
            name.to_owned(),
            CachedConnection {
                connection: Arc::clone(&connection),
                last_used: Instant::now(),
            },
        );
        Ok(connection)
    }

    async fn forget(&self, name: &str) {
        self.connections.lock().await.remove(name);
    }

    async fn prune_idle(&self, max_idle: Duration) {
        let mut connections = self.connections.lock().await;
        let now = Instant::now();
        connections.retain(|_, cached| now.duration_since(cached.last_used) < max_idle);
    }
}

struct McpSearch {
    manager: Arc<LazyMcp>,
    server_names: String,
}

// Discovery must return callable input schemas. Previously the agent saw only
// names/descriptions and had to guess required arguments for mcp_call. Bound
// the expansion so a single tool with a pathological schema cannot drown the
// context or make tool discovery unusable.
const MCP_DISCOVERY_MAX_BYTES: usize = 128 * 1024;
const MCP_DISCOVERY_SCHEMA_MAX_BYTES: usize = 16 * 1024;

fn discovered_mcp_tool(server: &str, metadata: &Value) -> Option<Value> {
    let remote = metadata["name"].as_str()?;
    let description = metadata["description"].as_str().unwrap_or("MCP tool");
    let mut entry = json!({"server":server,"tool":remote,"description":description});
    if let Some(schema) = metadata.get("inputSchema") {
        if serde_json::to_vec(schema)
            .is_ok_and(|bytes| bytes.len() <= MCP_DISCOVERY_SCHEMA_MAX_BYTES)
        {
            entry["input_schema"] = schema.clone();
        } else {
            entry["schema_omitted"] = json!("Tool input schema exceeds discovery limit (16 KiB)");
        }
    } else {
        entry["schema_omitted"] = json!("Server did not advertise inputSchema");
    }
    Some(entry)
}

impl Tool for McpSearch {
    fn name(&self) -> &str {
        "mcp_search"
    }

    fn may_mutate_worktree(&self) -> bool {
        false
    }

    fn definition(&self) -> Value {
        json!({"type":"function","function":{
            "name":"mcp_search",
            "description":format!(
                "Search MCP servers for tools. Call this before mcp_call. Available servers: {}. Set server to narrow the search, or query to match names and descriptions.",
                self.server_names
            ),
            "parameters":{"type":"object","properties":{
                "query":{"type":"string","description":"Words to match in tool names or descriptions"},
                "server":{"type":"string","description":"Only search this configured server"}
            },"additionalProperties":false}
        }})
    }

    fn execute<'a>(&'a self, call: &'a ToolCall, _ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: Value =
                serde_json::from_str(&call.function.arguments).unwrap_or_else(|_| json!({}));
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let only = args.get("server").and_then(Value::as_str);
            if let Some(name) = only {
                if !self.manager.servers.contains_key(name) {
                    return ToolResult::error(
                        json!({"error": format!("unknown MCP server '{name}'")}).to_string(),
                    );
                }
            }
            let mut found = Vec::new();
            let mut truncated = false;
            let mut result_bytes = 0usize;
            let deadline = tokio::time::Instant::now() + MCP_SEARCH_BUDGET;
            for name in self.manager.servers.keys() {
                if only.is_some_and(|only| only != name) {
                    continue;
                }
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    found.push(json!({"server":name,"error":"MCP search time budget exhausted"}));
                    truncated = true;
                    continue;
                }
                let connection =
                    match tokio::time::timeout(remaining, self.manager.connection(name)).await {
                        Ok(Ok(connection)) => connection,
                        Ok(Err(error)) => {
                            found.push(json!({"server":name,"error":format!("{error:#}")}));
                            continue;
                        }
                        Err(_) => {
                            found.push(
                                json!({"server":name,"error":"MCP search time budget exhausted"}),
                            );
                            truncated = true;
                            continue;
                        }
                    };
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let tools = tokio::time::timeout(remaining, async {
                    let mut connection = connection.lock().await;
                    #[cfg(any(unix, windows))]
                    let mut cancel = connection.process_tree().map(|tree| McpCancelGuard {
                        tree,
                        cancelled: connection.cancel_signal(),
                        armed: true,
                    });
                    let result = connection.list_tools().await;
                    #[cfg(any(unix, windows))]
                    if let Some(cancel) = cancel.as_mut() {
                        if result.is_ok() || !connection.is_invalid() {
                            cancel.disarm();
                        }
                    }
                    result
                })
                .await;
                let tools = match tools {
                    Ok(result) => result,
                    Err(_) => {
                        self.manager.forget(name).await;
                        found.push(json!({
                            "server": name,
                            "error": "MCP search time budget exhausted"
                        }));
                        truncated = true;
                        continue;
                    }
                };
                match tools {
                    Ok(tools) => {
                        for metadata in tools {
                            let Some(remote) = metadata["name"].as_str() else {
                                continue;
                            };
                            if !self.manager.tool_enabled(name, remote) {
                                continue;
                            }
                            let description =
                                metadata["description"].as_str().unwrap_or("MCP tool");
                            if !query.is_empty()
                                && !remote.to_ascii_lowercase().contains(&query)
                                && !description.to_ascii_lowercase().contains(&query)
                            {
                                continue;
                            }
                            if found.len() >= 50 {
                                truncated = true;
                                continue;
                            }
                            if let Some(entry) = discovered_mcp_tool(name, &metadata) {
                                let size =
                                    serde_json::to_vec(&entry).map_or(usize::MAX, |b| b.len());
                                if result_bytes.saturating_add(size) > MCP_DISCOVERY_MAX_BYTES {
                                    truncated = true;
                                    continue;
                                }
                                result_bytes += size;
                                found.push(entry);
                            }
                        }
                    }
                    Err(error) => {
                        self.manager.forget(name).await;
                        found.push(json!({"server":name,"error":format!("{error:#}")}));
                    }
                }
            }
            ToolResult::ok(
                json!({"tools":found,"truncated":truncated,"usage":"Call mcp_call with {server, tool, arguments}."})
                    .to_string(),
            )
        })
    }
}

struct McpCall {
    manager: Arc<LazyMcp>,
}

impl Tool for McpCall {
    fn name(&self) -> &str {
        "mcp_call"
    }

    fn manages_timeout(&self) -> bool {
        true
    }

    fn definition(&self) -> Value {
        json!({"type":"function","function":{
            "name":"mcp_call",
            "description":"Call a tool discovered with mcp_search. Arguments must be a JSON object.",
            "parameters":{"type":"object","required":["server","tool"],"properties":{
                "server":{"type":"string"},
                "tool":{"type":"string"},
                "arguments":{"type":"object","description":"Tool arguments as a JSON object"}
            },"additionalProperties":false}
        }})
    }

    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            let parsed: Value = match serde_json::from_str(&call.function.arguments) {
                Ok(value) => value,
                Err(error) => {
                    return ToolResult::error(
                        json!({"error":format!("invalid arguments: {error}")}).to_string(),
                    );
                }
            };
            let Some(server) = parsed.get("server").and_then(Value::as_str) else {
                return ToolResult::error(json!({"error":"server is required"}).to_string());
            };
            let Some(tool) = parsed.get("tool").and_then(Value::as_str) else {
                return ToolResult::error(json!({"error":"tool is required"}).to_string());
            };
            if !self.manager.tool_enabled(server, tool) {
                return ToolResult::error("MCP tool is disabled");
            }
            let arguments = parsed
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !arguments.is_object() {
                return ToolResult::error(
                    json!({"error":"arguments must be a JSON object"}).to_string(),
                );
            }
            let connection = match self.manager.connection(server).await {
                Ok(connection) => connection,
                Err(error) => {
                    return ToolResult::error(json!({"error":format!("{error:#}")}).to_string());
                }
            };
            let outcome = async {
                let mut connection = connection.lock().await;
                if connection.is_invalid() {
                    drop(connection);
                    self.manager.forget(server).await;
                    return Err(anyhow::anyhow!(
                        "MCP server '{server}' was disconnected; call mcp_search and retry"
                    ));
                }
                #[cfg(any(unix, windows))]
                let mut cancel = connection.process_tree().map(|tree| McpCancelGuard {
                    tree,
                    cancelled: connection.cancel_signal(),
                    armed: true,
                });
                let result = connection.call_tool(tool, arguments).await;
                #[cfg(any(unix, windows))]
                if let Some(cancel) = cancel.as_mut() {
                    if result.is_ok() || !connection.is_invalid() {
                        cancel.disarm();
                    }
                }
                result
            }
            .await;
            match outcome {
                Ok(value) => mcp_tool_result(value, ctx.archive),
                Err(error) => {
                    self.manager.forget(server).await;
                    ToolResult::error(json!({"error":format!("{error:#}")}).to_string())
                }
            }
        })
    }
}

pub async fn register_selected(
    registry: &mut ToolRegistry,
    workdir: &Path,
    settings: &Capabilities,
) -> Result<Registration> {
    if env::var_os("KROMA_MCP_EAGER").is_some() {
        register_configured_report_with(registry, workdir, false, settings).await
    } else {
        register_configured_lazy_with(registry, workdir, false, settings).await
    }
}

/// Register two small discovery tools instead of every remote schema. Server
/// processes start on the first `mcp_search` or `mcp_call`.
pub async fn register_configured_lazy(
    registry: &mut ToolRegistry,
    workdir: &Path,
) -> Result<Registration> {
    register_configured_lazy_with(registry, workdir, false, &Capabilities::default()).await
}

/// Compatibility test/host seam: load only the project config; no approval is needed.
pub async fn register_configured_lazy_allow_project(
    registry: &mut ToolRegistry,
    workdir: &Path,
) -> Result<Registration> {
    register_configured_lazy_with(registry, workdir, true, &Capabilities::default()).await
}

async fn register_configured_lazy_with(
    registry: &mut ToolRegistry,
    workdir: &Path,
    project_only: bool,
    settings: &Capabilities,
) -> Result<Registration> {
    if env::var_os("KROMA_NO_MCP").is_some() || !settings.any_enabled(CapabilityGroup::Mcp) {
        return Ok(Registration::default());
    }
    let (mut config, mut warnings) = configured_servers_with(workdir, project_only)?;
    config.retain(|name, _| settings.enabled(CapabilityGroup::Mcp, name, true));
    filter_servers(&mut config, &mut warnings);
    let mut report = Registration {
        warnings,
        ..Registration::default()
    };
    if config.is_empty() {
        return Ok(report);
    }
    let names = config.keys().cloned().collect::<Vec<_>>().join(", ");
    let manager = Arc::new(LazyMcp {
        settings: settings.clone(),
        servers: config,
        workdir: workdir.to_path_buf(),
        connections: tokio::sync::Mutex::new(HashMap::new()),
        connect_locks: tokio::sync::Mutex::new(HashMap::new()),
    });
    let weak = Arc::downgrade(&manager);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(MCP_IDLE_REAP_INTERVAL).await;
            let Some(manager) = weak.upgrade() else {
                break;
            };
            manager.prune_idle(MCP_IDLE_TIMEOUT).await;
        }
    });
    let search: Arc<dyn Tool> = Arc::new(McpSearch {
        manager: Arc::clone(&manager),
        server_names: names,
    });
    if registry.register(search).is_ok() {
        report.tools += 1;
    }
    let call: Arc<dyn Tool> = Arc::new(McpCall { manager });
    if registry.register(call).is_ok() {
        report.tools += 1;
    }
    Ok(report)
}
