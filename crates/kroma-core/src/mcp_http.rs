//! MCP 2026-07-28 stateless Streamable HTTP adapter.
//!
//! Authored independently from the legacy 2024 stdio adapter. Each JSON-RPC request gets one
//! bounded HTTP POST (no initialize, GET session or client-side notifications). Server-controlled
//! catalog metadata cannot grant permissions: the trusted host sets every tool's effect class.
//! Transport caches are scoped to one configured server instance and authorization context, never
//! shared process-wide. Missing or zero ttlMs means no cache. Does not implement MRTR InputRequired.

use crate::budget::BudgetConfig;
use crate::mcp::{
    McpCallResult, McpError, McpToolDescriptor, MAX_CATALOG_SCHEMA_BYTES, MAX_CATALOG_TOOLS,
    MAX_MCP_MESSAGE_BYTES, MAX_TOOL_PAGES,
};
use crate::provider::endpoint::{validate_base_url, ValidatedEndpoint};
use crate::provider::transport::{read_full_body, send_request, HttpRequestSpec, TransportFailure};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub const MCP_HTTP_PROTOCOL: &str = "2026-07-28";
pub const MAX_CATALOG_TTL_MS: u64 = 1_800_000;
/// Defend against header amplification from server-supplied schema annotations and tool input.
pub const MAX_ANNOTATED_HEADERS: usize = 32;
pub const MAX_ANNOTATED_VALUE_BYTES: usize = 4 * 1024;
pub const MAX_TOTAL_ANNOTATED_HEADER_BYTES: usize = 16 * 1024;

/// The JSON-RPC gateway always uses the endpoint from a user-trusted config file.
/// Authentication is resolved from an *environment variable name*, never stored in the config.
pub struct McpHttpClient {
    endpoint: ValidatedEndpoint,
    bearer: Option<String>,
    timeout: Duration,
    next_id: i64,
    poisoned: bool,
    catalog: Option<CachedCatalog>,
    /// Header annotations are validated when the catalog is ingested. These definitions are
    /// private to this instance (authorization partition) and never upgraded by untrusted output.
    annotations: HashMap<String, Vec<HeaderBinding>>,
    registered_tools: HashMap<String, McpToolDescriptor>,
}

struct CachedCatalog {
    tools: Vec<McpToolDescriptor>,
    expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeaderBinding {
    name: String,
    path: Vec<String>,
    kind: &'static str,
}

impl McpHttpClient {
    pub fn new(url: &str, bearer: Option<String>, timeout: Duration) -> Result<Self, McpError> {
        let endpoint = validate_base_url(url)
            .map_err(|e| McpError::Protocol(format!("invalid MCP HTTPS endpoint: {}", e)))?;
        if endpoint.base_path.is_empty() {
            return Err(McpError::Protocol(
                "MCP HTTP endpoint must include an explicit path (for example /mcp)".into(),
            ));
        }
        if bearer.as_ref().is_some_and(|s| {
            s.is_empty() || s.len() > 8192 || s.bytes().any(|c| c < 32 || c == 127)
        }) {
            return Err(McpError::Protocol("invalid HTTP MCP credential".into()));
        }
        Ok(Self {
            endpoint,
            bearer,
            timeout,
            next_id: 1,
            poisoned: false,
            catalog: None,
            annotations: HashMap::new(),
            registered_tools: HashMap::new(),
        })
    }

    pub fn poison(&mut self) {
        self.poisoned = true;
        self.catalog = None;
    }

    async fn request(
        &mut self,
        method: &str,
        mut params: Value,
        deadline: Instant,
        extra_headers: &[(String, String)],
    ) -> Result<Value, McpError> {
        if self.poisoned {
            return Err(McpError::Io("MCP HTTP client is poisoned".into()));
        }
        if Instant::now() >= deadline {
            return Err(McpError::Timeout(self.timeout));
        }
        let map = params
            .as_object_mut()
            .ok_or_else(|| McpError::Protocol("MCP request params must be an object".into()))?;
        map.insert(
            "_meta".into(),
            json!({
                "io.modelcontextprotocol/protocolVersion": MCP_HTTP_PROTOCOL,
                "io.modelcontextprotocol/clientInfo": {"name": "Kroma", "version": "1.5.0"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }),
        );
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        let body =
            serde_json::to_string(&request).map_err(|e| McpError::Protocol(e.to_string()))?;
        if body.len() > MAX_MCP_MESSAGE_BYTES {
            return Err(McpError::RequestTooLarge(body.len()));
        }
        let mut headers = vec![
            ("MCP-Protocol-Version".into(), MCP_HTTP_PROTOCOL.into()),
            ("Mcp-Method".into(), method.into()),
        ];
        if let Some(name) = request["params"].get("name").and_then(Value::as_str) {
            headers.push(("Mcp-Name".into(), encode_header_value(name)));
        }
        if let Some(token) = &self.bearer {
            headers.push(("Authorization".into(), format!("Bearer {}", token)));
        }
        headers.extend(extra_headers.iter().cloned());
        let budget = BudgetConfig::default();
        let spec = HttpRequestSpec {
            method: "POST",
            path: &self.endpoint.base_path,
            headers: &headers,
            body: &body,
            accept: "application/json, text/event-stream",
        };
        let mut response = send_request(&self.endpoint, &spec, &budget, deadline)
            .await
            .map_err(|e| match e {
                TransportFailure::Transient(err) | TransportFailure::Fatal(err) => {
                    McpError::Io(err.to_string())
                }
            })?;
        if response.status != 200 {
            return Err(McpError::Protocol(format!(
                "MCP HTTP request rejected with status {}",
                response.status
            )));
        }
        if response
            .content_length
            .is_some_and(|len| len > MAX_MCP_MESSAGE_BYTES)
        {
            return Err(McpError::MessageTooLarge(
                response.content_length.unwrap_or(0),
            ));
        }
        let content_type = response
            .content_type
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        let is_sse = content_type.starts_with("text/event-stream");
        let is_json = content_type.starts_with("application/json");
        if !is_json && !is_sse {
            return Err(McpError::Protocol(
                "unsupported MCP HTTP response content-type".into(),
            ));
        }
        let bytes = read_full_body(&mut response, &budget, deadline, MAX_MCP_MESSAGE_BYTES)
            .await
            .map_err(|e| McpError::Io(e.to_string()))?;
        let message = if is_sse {
            parse_sse_final(&bytes, id)?
        } else {
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|e| McpError::Protocol(format!("malformed MCP JSON response: {}", e)))?
        };
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || message.get("id").and_then(Value::as_i64) != Some(id)
        {
            return Err(McpError::Protocol(
                "MCP HTTP JSON-RPC response id/version mismatch".into(),
            ));
        }
        if message.get("result").is_some() == message.get("error").is_some() {
            return Err(McpError::Protocol(
                "MCP HTTP JSON-RPC response must contain exactly one of result/error".into(),
            ));
        }
        if let Some(error) = message.get("error") {
            return Err(McpError::Server {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("server error")
                    .chars()
                    .take(250)
                    .collect(),
            });
        }
        let result = message
            .get("result")
            .ok_or_else(|| McpError::Protocol("MCP HTTP response lacks result".into()))?;
        if result.get("resultType").and_then(Value::as_str) == Some("input_required") {
            return Err(McpError::Protocol(
                "MCP InputRequired/MRTR is unsupported; cannot silently discard input requests"
                    .into(),
            ));
        }
        Ok(result.clone())
    }

    pub async fn list_tools(&mut self) -> Result<Vec<McpToolDescriptor>, McpError> {
        if let Some(c) = &self.catalog {
            if Instant::now() < c.expires_at {
                return Ok(c.tools.clone());
            }
        }
        self.catalog = None;
        let deadline = Instant::now() + self.timeout;
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        let mut seen_tools = HashSet::new();
        let mut collected = Vec::new();
        let mut new_headers = HashMap::new();
        let mut total_schema = 0usize;
        let mut ttl_ms: Option<u64> = None;
        let mut scope: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |v| json!({"cursor":v}));
            let page = self.request("tools/list", params, deadline, &[]).await?;
            let tools = page.get("tools").and_then(Value::as_array).ok_or_else(|| {
                McpError::Protocol("MCP HTTP tools/list missing tools array".into())
            })?;
            let page_ttl = match page.get("ttlMs") {
                None => 0,
                Some(value) => value
                    .as_u64()
                    .ok_or_else(|| {
                        McpError::Protocol(
                            "MCP HTTP catalog ttlMs must be a nonnegative integer".into(),
                        )
                    })?
                    .min(MAX_CATALOG_TTL_MS),
            };
            ttl_ms = Some(ttl_ms.unwrap_or(page_ttl).min(page_ttl));
            let page_scope = page
                .get("cacheScope")
                .and_then(Value::as_str)
                .unwrap_or("private");
            if !matches!(page_scope, "public" | "private") {
                return Err(McpError::Protocol(
                    "MCP HTTP unrecognized catalog cacheScope".into(),
                ));
            }
            if let Some(previous) = scope.as_deref() {
                if previous != page_scope {
                    ttl_ms = Some(0);
                }
            } else {
                scope = Some(page_scope.to_string());
            }
            for item in tools {
                // Only valid tools are discoverable: an invalid parameter-header annotation
                // would otherwise force unsafe omissions at dispatch time.
                let desc: McpToolDescriptor =
                    serde_json::from_value(item.clone()).map_err(|e| {
                        McpError::Protocol(format!("MCP tool descriptor invalid: {}", e))
                    })?;
                if desc.name.is_empty() || desc.name.len() > 256 {
                    return Err(McpError::Protocol("invalid MCP tool name".into()));
                }
                if !seen_tools.insert(desc.name.clone()) {
                    return Err(McpError::DuplicateToolName { name: desc.name });
                }
                let size = serde_json::to_vec(&desc)
                    .map_err(|e| McpError::Protocol(e.to_string()))?
                    .len();
                total_schema = total_schema.saturating_add(size);
                if total_schema > MAX_CATALOG_SCHEMA_BYTES || collected.len() >= MAX_CATALOG_TOOLS {
                    return Err(McpError::CatalogBudgetExceeded {
                        reason: format!(
                            "{} bytes or {} tools",
                            MAX_CATALOG_SCHEMA_BYTES, MAX_CATALOG_TOOLS
                        ),
                    });
                }
                match compile_headers(&desc.input_schema) {
                    Ok(headers) => {
                        new_headers.insert(desc.name.clone(), headers);
                        collected.push(desc);
                    }
                    Err(why) => {
                        eprintln!(
                            "[kroma:mcp-http] excluded invalid annotated tool (reason={})",
                            why
                        );
                    }
                }
            }
            let next_cursor = match page.get("nextCursor") {
                None | Some(Value::Null) => None,
                Some(Value::String(value)) => Some(value.as_str()),
                _ => {
                    return Err(McpError::Protocol(
                        "MCP HTTP catalog nextCursor must be a string".into(),
                    ))
                }
            };
            match next_cursor {
                Some(next) if !next.is_empty() => {
                    if next.len() > 1024 || !seen_cursors.insert(next.to_string()) {
                        return Err(McpError::CursorCycle {
                            cursor: "duplicate or oversized cursor".into(),
                        });
                    }
                    cursor = Some(next.to_string());
                }
                _ => {
                    self.annotations = new_headers;
                    if self.registered_tools.is_empty() {
                        self.registered_tools = collected
                            .iter()
                            .map(|d| (d.name.clone(), d.clone()))
                            .collect();
                    }
                    if let Some(ms) = ttl_ms.filter(|&n| n > 0) {
                        self.catalog = Some(CachedCatalog {
                            tools: collected.clone(),
                            expires_at: Instant::now() + Duration::from_millis(ms),
                        });
                    }
                    return Ok(collected);
                }
            }
        }
        Err(McpError::CatalogBudgetExceeded {
            reason: "MCP HTTP catalog page limit".into(),
        })
    }

    pub async fn call_tool_until(
        &mut self,
        name: &str,
        arguments: Value,
        deadline: Instant,
    ) -> Result<McpCallResult, McpError> {
        let since = Instant::now();
        // Revalidate expired catalogs without silently changing the model-visible registered schema.
        let tools = self.list_tools().await?;
        let originally_registered = self.registered_tools.get(name).ok_or_else(|| {
            McpError::Protocol("MCP HTTP tool was not registered at host initialization".into())
        })?;
        if tools.iter().find(|d| d.name == name) != Some(originally_registered) {
            return Err(McpError::Protocol(
                "MCP HTTP tool schema changed or tool was removed; remount server first".into(),
            ));
        }
        let bindings = self.annotations.get(name).ok_or_else(|| {
            McpError::Protocol("MCP HTTP tool is not in the validated catalog".into())
        })?;
        let headers = extract_argument_headers(bindings, &arguments)?;
        let result = self
            .request(
                "tools/call",
                json!({"name":name,"arguments":arguments}),
                deadline,
                &headers,
            )
            .await
            .map_err(|e| match e {
                McpError::Timeout(_) => McpError::StartedDeadline(since.elapsed()),
                other => other,
            })?;
        Ok(McpCallResult::from_value(&result))
    }
}

/// Encode a UTF-8 value using the sentinel whenever it is not a safe, unambiguous ASCII header.
fn encode_header_value(value: &str) -> String {
    let needs_encoding = value.bytes().any(|b| !(0x20..=0x7e).contains(&b))
        || value.starts_with(' ')
        || value.ends_with(' ')
        || (value.starts_with("=?base64?") && value.ends_with("?="));
    let safe = !needs_encoding;
    if safe {
        return value.to_string();
    }
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::from("=?base64?");
    for chunk in value.as_bytes().chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        out.push(B64[(a >> 2) as usize] as char);
        out.push(B64[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    out.push_str("?=");
    out
}

/// Explicitly reject annotations hiding under composition, references, arrays, or arbitrary schema
/// keywords. Annotations are allowed ONLY on statically reachable primitive `properties` fields.
fn compile_headers(schema: &Value) -> Result<Vec<HeaderBinding>, &'static str> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    fn walk(
        node: &Value,
        path: &mut Vec<String>,
        out: &mut Vec<HeaderBinding>,
        seen: &mut HashSet<String>,
        reachable: bool,
    ) -> Result<(), &'static str> {
        if let Some(children) = node.as_array() {
            // Arrays (including composition branches and examples) are never statically
            // reachable argument fields, but must still be inspected for hidden annotations.
            for child in children {
                walk(child, path, out, seen, false)?;
            }
            return Ok(());
        }
        let Some(object) = node.as_object() else {
            return Ok(());
        };
        for (k, v) in object {
            if k == "x-mcp-header" {
                if !reachable || path.is_empty() {
                    return Err("unreachable annotation");
                }
                let name = v.as_str().ok_or("non-string annotation")?;
                if out.len() >= MAX_ANNOTATED_HEADERS {
                    return Err("too many annotated headers");
                }
                if name.is_empty() || !name.bytes().all(is_http_token) {
                    return Err("invalid annotation header name");
                }
                if !seen.insert(name.to_ascii_lowercase()) {
                    return Err("duplicate annotation header");
                }
                let kind = match object.get("type").and_then(Value::as_str) {
                    Some("string") => "string",
                    Some("integer") => "integer",
                    Some("boolean") => "boolean",
                    _ => return Err("annotated parameter is not a primitive"),
                };
                out.push(HeaderBinding {
                    name: name.to_string(),
                    path: path.clone(),
                    kind,
                });
            } else if k == "properties" && reachable {
                if let Some(properties) = v.as_object() {
                    for (p, child) in properties {
                        path.push(p.clone());
                        walk(child, path, out, seen, true)?;
                        path.pop();
                    }
                }
            } else {
                // No other keyword (including descriptions/examples) may smuggle annotations.
                walk(v, path, out, seen, false)?;
            }
        }
        Ok(())
    }
    walk(schema, &mut Vec::new(), &mut out, &mut seen, true)?;
    Ok(out)
}

fn is_http_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn extract_argument_headers(
    bindings: &[HeaderBinding],
    arguments: &Value,
) -> Result<Vec<(String, String)>, McpError> {
    let mut headers = Vec::new();
    for binding in bindings {
        let mut value = arguments;
        for segment in &binding.path {
            value = match value.get(segment.as_str()) {
                Some(v) => v,
                None => &Value::Null,
            };
        }
        let text = match (binding.kind, value) {
            (_, Value::Null) => continue,
            ("string", Value::String(s)) => s.clone(),
            ("boolean", Value::Bool(b)) => b.to_string(),
            ("integer", Value::Number(n)) => {
                const MAX: i64 = 9_007_199_254_740_991;
                let i = n.as_i64().ok_or_else(|| {
                    McpError::Protocol("annotated integer parameter is not integral".into())
                })?;
                if !(-MAX..=MAX).contains(&i) {
                    return Err(McpError::Protocol(
                        "annotated integer outside safe range".into(),
                    ));
                }
                i.to_string()
            }
            _ => {
                return Err(McpError::Protocol(
                    "annotated parameter type mismatch".into(),
                ))
            }
        };
        // Count bytes before serialization so untrusted model arguments cannot turn a small JSON
        // body into huge HTTP headers. Base64 can expand input by roughly 4/3.
        if text.len() > MAX_ANNOTATED_VALUE_BYTES {
            return Err(McpError::Protocol(
                "annotated header value exceeds 4 KiB".into(),
            ));
        }
        let name = format!("Mcp-Param-{}", binding.name);
        let encoded = encode_header_value(&text);
        let projected = headers
            .iter()
            .map(|(k, v): &(String, String)| k.len() + v.len())
            .sum::<usize>()
            .saturating_add(name.len())
            .saturating_add(encoded.len());
        if projected > MAX_TOTAL_ANNOTATED_HEADER_BYTES {
            return Err(McpError::Protocol(
                "annotated headers exceed 16 KiB total".into(),
            ));
        }
        headers.push((name, encoded));
    }
    Ok(headers)
}

/// Parse complete bounded SSE responses, permitting request-related notifications but never
/// independent server requests or a second final response. A missing final is a protocol error.
fn parse_sse_final(input: &[u8], expected_id: i64) -> Result<Value, McpError> {
    let text = std::str::from_utf8(input).map_err(|_| McpError::Protocol("non-UTF8 SSE".into()))?;
    let mut data = String::new();
    let mut final_response = None;
    for line in text.lines().chain(std::iter::once("")) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if data.is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(data.trim_end_matches('\n'))
                .map_err(|_| McpError::Protocol("invalid MCP SSE JSON frame".into()))?;
            data.clear();
            if parsed.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                return Err(McpError::Protocol(
                    "MCP SSE notification/response has invalid jsonrpc version".into(),
                ));
            }
            if parsed.get("id").and_then(Value::as_i64) == Some(expected_id) {
                if final_response.replace(parsed).is_some() {
                    return Err(McpError::Protocol(
                        "duplicate MCP SSE final response".into(),
                    ));
                }
            } else if parsed
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|m| m.starts_with("notifications/"))
                && parsed.get("id").is_none()
            {
                // Notifications on this response stream are scoped to its request.
            } else {
                return Err(McpError::Protocol(
                    "MCP SSE unexpected request or mismatched id".into(),
                ));
            }
        } else if let Some(payload) = line.strip_prefix("data:") {
            data.push_str(payload.strip_prefix(' ').unwrap_or(payload));
            data.push('\n');
        } else if line.starts_with(':') || line.starts_with("event:") || line.starts_with("id:") {
            // Only data carries JSON-RPC content.
        } else {
            return Err(McpError::Protocol("invalid MCP SSE frame".into()));
        }
    }
    final_response
        .ok_or_else(|| McpError::Protocol("MCP SSE stream ended without final result".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn header_values_base64_sentinel_and_unicode_are_safe() {
        assert_eq!(encode_header_value("us-west1"), "us-west1");
        assert_eq!(
            encode_header_value("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
        assert_eq!(encode_header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(
            encode_header_value("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
    }
    #[test]
    fn schema_headers_nested_case_collision_rejected() {
        let schema = json!({"type":"object","properties":{
            "region":{"type":"string","x-mcp-header":"Region"},
            "nested":{"type":"object","properties":{"a":{"type":"integer","x-mcp-header":"region"}}}
        }});
        assert_eq!(compile_headers(&schema), Err("duplicate annotation header"));
        for hidden in [
            json!({"oneOf":[{"x-mcp-header":"Danger"}]}),
            json!({"anyOf":[{"properties":{"x":{"type":"string","x-mcp-header":"Danger"}}}]}),
            json!({"examples":[[{"x-mcp-header":"Danger"}]]}),
        ] {
            assert_eq!(compile_headers(&hidden), Err("unreachable annotation"));
        }
    }
    #[test]
    fn nested_headers_encoded_and_absent_or_null_omitted() {
        let schema = json!({"type":"object","properties":{"filters":{"type":"object",
            "properties":{"query":{"type":"string","x-mcp-header":"Search"},"limit":{"type":"integer","x-mcp-header":"Limit"}}}}});
        let bindings = compile_headers(&schema).unwrap();
        let headers =
            extract_argument_headers(&bindings, &json!({"filters":{"query":"你好","limit":2}}))
                .unwrap();
        assert!(headers.contains(&("Mcp-Param-Search".into(), "=?base64?5L2g5aW9?=".into())));
        assert!(headers.contains(&("Mcp-Param-Limit".into(), "2".into())));
        assert!(
            extract_argument_headers(&bindings, &json!({"filters":{"query":null}}))
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn sse_final_checks_id_and_does_not_drop_request() {
        let s=b": ping\n\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"tools\":[]}}\n\n";
        assert!(parse_sse_final(s, 7).is_ok());
        assert!(parse_sse_final(s, 8).is_err());
        assert!(parse_sse_final(
            b"data: {\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"sampling/createMessage\"}\n\n",
            7
        )
        .is_err());
    }
    #[test]
    fn unsafe_remote_http_and_invalid_token_rejected() {
        assert!(
            McpHttpClient::new("http://example.com/mcp", None, Duration::from_secs(2)).is_err()
        );
        assert!(McpHttpClient::new(
            "https://example.com/mcp",
            Some("x\r\nY".into()),
            Duration::from_secs(2)
        )
        .is_err());
    }
    #[test]
    fn rejects_unbounded_annotated_headers_before_transport() {
        let schema =
            json!({"type":"object","properties":{"x":{"type":"string","x-mcp-header":"Search"}}});
        let bindings = compile_headers(&schema).unwrap();
        assert!(extract_argument_headers(&bindings, &json!({"x":"x".repeat(4097)})).is_err());
        assert!(extract_argument_headers(&bindings, &json!({"x":"x".repeat(4096)})).is_ok());
        let mut properties = serde_json::Map::new();
        for i in 0..33 {
            properties.insert(
                format!("k{i}"),
                json!({"type":"string","x-mcp-header":format!("h{i}")}),
            );
        }
        assert_eq!(
            compile_headers(&json!({"type":"object","properties":properties})),
            Err("too many annotated headers")
        );
    }

    #[test]
    fn rejects_malformed_jsonrpc_notifications_and_repeated_results() {
        assert!(parse_sse_final(b"data: {\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n", 1).is_err());
        assert!(parse_sse_final(b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n", 1).is_err());
    }
}
