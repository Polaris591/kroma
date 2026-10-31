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
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::Arc,
    time::Duration,
};

const MAX_MCP_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_MCP_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_MCP_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const TRUST_PROJECT_ENV: &str = "KROMA_TRUST_PROJECT_MCP";
const PROJECT_ENV_ALLOW: [&str; 12] = [
    "HOME",
    "USERPROFILE",
    "PATH",
    "TEMP",
    "TMP",
    "PWD",
    "KROMA_WORKDIR",
    "KROMA_PLUGIN_DIR",
    "KROMA_CONFIG_FILE",
    "XDG_CONFIG_HOME",
    "LOCALAPPDATA",
    "APPDATA",
];
const CHILD_ENV_ALLOW: [&str; 3] = ["KROMA_WORKDIR", "KROMA_PLUGIN_DIR", "KROMA_CONFIG_FILE"];
const HOST_SECRET_ENV: [&str; 12] = [
    "KROMA_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "DEEPSEEK_API_KEY",
    "EXA_API_KEY",
    "BRAVE_SEARCH_API_KEY",
    "TAVILY_API_KEY",
    "PARALLEL_API_KEY",
    "YDC_API_KEY",
    "FIRECRAWL_API_KEY",
];

fn is_host_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("KROMA_")
        || HOST_SECRET_ENV
            .iter()
            .any(|blocked| blocked.eq_ignore_ascii_case(&upper))
        || upper.starts_with("AWS_")
        || upper.starts_with("AZURE_")
        || upper.starts_with("GCP_")
        || upper.starts_with("GOOGLE_")
        || upper.starts_with("GITHUB_")
        || upper.starts_with("GH_")
        || upper.starts_with("SSH_")
        || upper.starts_with("DOCKER_")
        || upper.starts_with("NPM_")
        || upper.starts_with("CARGO_REGISTRY_")
        || upper.starts_with("DATABASE_URL")
        || upper.starts_with("REDIS_URL")
        || upper.starts_with("MONGODB_URI")
        || upper.ends_with("_KEY")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_PASSWORD")
        || upper.ends_with("_CREDENTIALS")
}

fn is_child_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("KROMA_") {
        return !CHILD_ENV_ALLOW
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&upper));
    }
    is_host_secret(name)
}

fn filter_child_environment(
    process: &mut Command,
    variables: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) {
    for (key, _) in variables {
        if is_child_secret(&key.to_string_lossy()) {
            process.env_remove(key);
        }
    }
}

fn project_env_allowed(name: &str) -> bool {
    if is_host_secret(name) {
        return false;
    }
    let upper = name.to_ascii_uppercase();
    upper.starts_with("MCP_")
        || PROJECT_ENV_ALLOW
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&upper))
}

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

fn fingerprint(bytes: &[u8]) -> String {
    sha256(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn trust_store_path() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .context("HOME/USERPROFILE unavailable; cannot store MCP trust decisions")?;
    Ok(PathBuf::from(home).join(".kroma").join("trusted-mcp.json"))
}

fn project_key(workdir: &Path) -> String {
    workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn load_trust_store() -> Result<BTreeMap<String, String>> {
    let path = trust_store_path()?;
    match fs::read(&path) {
        Ok(bytes) if bytes.len() as u64 > MAX_MCP_CONFIG_BYTES => {
            bail!("MCP trust store exceeds 1 MiB")
        }
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid MCP trust store {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => {
            Err(error).with_context(|| format!("reading MCP trust store {}", path.display()))
        }
    }
}

fn project_mcp_allowed(workdir: &Path, bytes: &[u8]) -> Result<bool> {
    if env::var(TRUST_PROJECT_ENV)
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
    {
        return Ok(true);
    }
    let store = load_trust_store()?;
    Ok(store
        .get(&project_key(workdir))
        .is_some_and(|approved| approved == &fingerprint(bytes)))
}

/// True when the project has no MCP config, or its current bytes are approved.
/// Hosts use this to show a trust prompt without executing anything.
pub fn project_mcp_trusted(workdir: &Path) -> Result<bool> {
    let config = workdir.join(".kroma").join("mcp.json");
    if !config.is_file() {
        return Ok(true);
    }
    let bytes = fs::read(&config)
        .with_context(|| format!("reading project MCP config {}", config.display()))?;
    if bytes.len() as u64 > MAX_MCP_CONFIG_BYTES {
        bail!("project MCP config exceeds 1 MiB");
    }
    project_mcp_allowed(workdir, &bytes)
}

/// Approve the current project's `.kroma/mcp.json`. The approval is bound to
/// the canonical project path and the exact file bytes; any later edit asks
/// for approval again. The trust store is private to the current user.
pub fn trust_project_mcp(workdir: &Path) -> Result<PathBuf> {
    let workdir = workdir
        .canonicalize()
        .with_context(|| format!("project path does not exist: {}", workdir.display()))?;
    let config = workdir.join(".kroma").join("mcp.json");
    let bytes = fs::read(&config)
        .with_context(|| format!("no project MCP config at {}", config.display()))?;
    if bytes.len() as u64 > MAX_MCP_CONFIG_BYTES {
        bail!("project MCP config exceeds 1 MiB");
    }
    let mut store = load_trust_store()?;
    store.insert(project_key(&workdir), fingerprint(&bytes));
    let path = trust_store_path()?;
    let parent = path.parent().context("MCP trust store has no parent")?;
    let temp = crate::private_file::save(
        parent,
        "trusted-mcp",
        "json",
        &serde_json::to_vec_pretty(&store)?,
    )?;
    if let Err(error) = crate::atomic_file::replace(&temp, &path) {
        let _ = fs::remove_file(temp);
        return Err(error);
    }
    Ok(config)
}

pub fn revoke_project_mcp(workdir: &Path) -> Result<()> {
    let workdir = workdir
        .canonicalize()
        .with_context(|| format!("project path does not exist: {}", workdir.display()))?;
    let mut store = load_trust_store()?;
    store.remove(&project_key(&workdir));
    let path = trust_store_path()?;
    let parent = path.parent().context("MCP trust store has no parent")?;
    let temp = crate::private_file::save(
        parent,
        "trusted-mcp",
        "json",
        &serde_json::to_vec_pretty(&store)?,
    )?;
    if let Err(error) = crate::atomic_file::replace(&temp, &path) {
        let _ = fs::remove_file(temp);
        return Err(error);
    }
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
    let (servers, _warnings) = configured_servers_with(workdir, None)?;
    Ok(servers)
}

fn configured_servers_with(
    workdir: &Path,
    trust_project: Option<bool>,
) -> Result<(BTreeMap<String, ServerConfig>, Vec<String>)> {
    let mut warnings = Vec::new();
    let paths: Vec<(PathBuf, bool)> = if let Some(override_path) = env::var_os("KROMA_MCP_CONFIG") {
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
        // Read the exact bytes once and make the trust decision and parser use
        // the same snapshot. Reopening the path after approval would allow a
        // project to swap the file between the hash check and execution.
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) if project => {
                // An untrusted repository must not be able to abort startup
                // by making its config unreadable or oversized.
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
        if bytes.len() as u64 > MAX_MCP_CONFIG_BYTES {
            if project {
                warnings.push(format!(
                    "project MCP config {} skipped: exceeds 1 MiB",
                    path.display()
                ));
                continue;
            }
            bail!("MCP config {} exceeds 1 MiB", path.display());
        }
        if project {
            let allowed = match trust_project {
                Some(value) => value,
                None => project_mcp_allowed(workdir, &bytes).unwrap_or(false),
            };
            if !allowed {
                warnings.push(format!(
                    "project MCP config {} not loaded: approve it with 'kroma trust-project' or set {TRUST_PROJECT_ENV}=1",
                    path.display()
                ));
                continue;
            }
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

fn expand_env(raw: &str, allow_host_secrets: bool) -> Result<String> {
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
        if !allow_host_secrets && is_host_secret(name) {
            bail!(
                "project MCP config may not expand host secret '{name}'; use ~/.kroma/mcp.json or an explicit trusted config"
            );
        }
        if !allow_host_secrets && !project_env_allowed(name) {
            bail!(
                "project MCP config may only expand MCP_* or basic path variables, not '{name}'; use a literal value or ~/.kroma/mcp.json"
            );
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

struct Connection {
    transport: Transport,
    version: String,
    next_id: u64,
    invalid: Option<String>,
    cancelled: Arc<AtomicBool>,
    request_timeout: Duration,
}

impl Connection {
    async fn connect(config: &ServerConfig, workdir: &Path) -> Result<Self> {
        let version = config.protocol_version.as_deref().unwrap_or(LEGACY_VERSION);
        if version != LEGACY_VERSION && version != MODERN_VERSION {
            bail!("protocolVersion must be {LEGACY_VERSION} or {MODERN_VERSION}");
        }
        let transport = match (&config.command, &config.url) {
            (Some(command), None) => {
                let mut process = Command::new(expand_env(command, !config.project)?);
                for arg in &config.args {
                    process.arg(expand_env(arg, !config.project)?);
                }
                filter_child_environment(&mut process, env::vars_os());
                if config.project && config.env.keys().any(|key| is_host_secret(key)) {
                    bail!("project MCP config may not set host secret environment variables");
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
                    process.env(key, expand_env(value, !config.project)?);
                }
                process.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(
                    if crate::diagnostics::quiet() {
                        Stdio::null()
                    } else {
                        Stdio::inherit()
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
                let url = expand_env(url, !config.project)?;
                let parsed = reqwest::Url::parse(&url).context("invalid MCP HTTP endpoint")?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("MCP URL must use HTTP or HTTPS");
                }
                let mut headers = HeaderMap::new();
                for (name, value) in &config.headers {
                    headers.insert(
                        HeaderName::from_bytes(name.as_bytes())?,
                        HeaderValue::from_str(&expand_env(value, !config.project)?)?,
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
            invalid: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            request_timeout: MCP_REQUEST_TIMEOUT,
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
        self.ensure_usable()?;
        let (id, message) = self.message(method, params, true)?;
        let timeout = self.request_timeout;
        let stdio = matches!(&self.transport, Transport::Stdio { .. });
        let response = match tokio::time::timeout(timeout, self.send(message, Some(id))).await {
            Ok(result) => {
                result?.with_context(|| format!("MCP {method} did not return a response"))?
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
    /// Non-fatal notices, such as a project config that needs approval.
    pub warnings: Vec<String>,
}

pub async fn register_configured(registry: &mut ToolRegistry, workdir: &Path) -> Result<usize> {
    Ok(register_configured_report(registry, workdir).await?.tools)
}

/// Test/integration seam: force-enable project MCP without touching the user's
/// trust store. Production hosts must use the normal trusted path.
pub async fn register_configured_allow_project(
    registry: &mut ToolRegistry,
    workdir: &Path,
) -> Result<usize> {
    Ok(
        register_configured_report_with(registry, workdir, Some(true))
            .await?
            .tools,
    )
}

pub async fn register_configured_report(
    registry: &mut ToolRegistry,
    workdir: &Path,
) -> Result<Registration> {
    register_configured_report_with(registry, workdir, None).await
}

async fn register_configured_report_with(
    registry: &mut ToolRegistry,
    workdir: &Path,
    trust_project: Option<bool>,
) -> Result<Registration> {
    let (config, warnings) = configured_servers_with(workdir, trust_project)?;
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
            invalid: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            request_timeout: MCP_REQUEST_TIMEOUT,
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
            fingerprint(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn project_configs_cannot_expand_secrets_or_arbitrary_environment() {
        let secret = expand_env("${AWS_SECRET_ACCESS_KEY}", false).unwrap_err();
        assert!(secret.to_string().contains("host secret"), "{secret:#}");
        let mcp_secret = expand_env("${MCP_API_TOKEN}", false).unwrap_err();
        assert!(
            mcp_secret.to_string().contains("host secret"),
            "{mcp_secret:#}"
        );
        let arbitrary = expand_env("${SOME_UNLISTED_VARIABLE}", false).unwrap_err();
        assert!(
            arbitrary.to_string().contains("may only expand"),
            "{arbitrary:#}"
        );
        // Trusted configs keep the existing escape hatch.
        assert!(expand_env("${PATH}", true).is_ok());
    }

    #[test]
    fn project_mcp_env_allowlist_is_narrow() {
        assert!(project_env_allowed("MCP_SERVER_URL"));
        assert!(project_env_allowed("PATH"));
        assert!(project_env_allowed("home"));
        assert!(!project_env_allowed("MCP_API_TOKEN"));
        assert!(!project_env_allowed("AWS_PROFILE"));
        assert!(!project_env_allowed("OPENAI_API_KEY"));
        assert!(!project_env_allowed("UNRELATED"));
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
            MockMode::NearLimit => "read line\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"data\":\"'\nhead -c 8300000 /dev/zero | tr '\0' x\nprintf '\"}}\n'".into(),
            MockMode::OverLimit => "read line\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"data\":\"'\nhead -c 8400000 /dev/zero | tr '\0' x\nprintf '\"}}\n'".into(),
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
    async fn child_environment_filters_sensitive_kroma_names() -> Result<()> {
        for name in [
            "KROMA_API_KEY",
            "KROMA_PROXY_TOKEN",
            "KROMA_CUSTOM_KEY",
            "KROMA_WEBHOOK_SECRET",
            "KROMA_MCP_PASSWORD",
            "UNKNOWN_TOKEN",
            "CUSTOM_SECRET",
        ] {
            assert!(is_child_secret(name), "{name} must be filtered");
        }
        for name in [
            "KROMA_WORKDIR",
            "KROMA_PLUGIN_DIR",
            "KROMA_CONFIG_FILE",
            "PATH",
        ] {
            assert!(!is_child_secret(name), "{name} is an explicit non-secret");
        }

        #[cfg(unix)]
        let mut command = Command::new("/bin/sh");
        #[cfg(unix)]
        command.args(["-c", "env"]);
        #[cfg(windows)]
        let mut command =
            Command::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
        #[cfg(windows)]
        command.args(["/C", "set"]);
        command.env_clear();
        let values = [
            ("KROMA_PROXY_TOKEN", "proxy-secret"),
            ("KROMA_CUSTOM_KEY", "custom-secret"),
            ("KROMA_WEBHOOK_SECRET", "webhook-secret"),
            ("KROMA_MCP_PASSWORD", "mcp-password"),
            ("UNKNOWN_TOKEN", "unknown-secret"),
            ("KROMA_WORKDIR", "safe-workdir"),
            ("MCP_SAFE_VALUE", "safe-value"),
        ];
        for (key, value) in values {
            command.env(key, value);
        }
        let fake: Vec<(std::ffi::OsString, std::ffi::OsString)> = values
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect();
        filter_child_environment(&mut command, fake.clone());
        // This mirrors the explicit global MCP config path: config.env is
        // applied after inherited values were filtered.
        command.env("KROMA_EXPLICIT_CONFIG", "explicit-allowed");
        let output = with_test_timeout(async move {
            command.output().await.context("spawning environment probe")
        })
        .await?;
        assert!(output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        for allowed in [
            "KROMA_WORKDIR=safe-workdir",
            "MCP_SAFE_VALUE=safe-value",
            "KROMA_EXPLICIT_CONFIG=explicit-allowed",
        ] {
            assert!(text.contains(allowed), "missing {allowed}: {text}");
        }
        for secret in [
            "KROMA_PROXY_TOKEN=proxy-secret",
            "KROMA_CUSTOM_KEY=custom-secret",
            "KROMA_WEBHOOK_SECRET=webhook-secret",
            "KROMA_MCP_PASSWORD=mcp-password",
            "UNKNOWN_TOKEN=unknown-secret",
        ] {
            assert!(!text.contains(secret), "leaked {secret}: {text}");
        }
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
                workdir: &dir,
                archive: &archive_a,
                on_output: None,
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
                        workdir: &waiting_dir,
                        archive: &archive,
                        on_output: None,
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
                workdir: &dir,
                archive: &archive,
                on_output: None,
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
}
