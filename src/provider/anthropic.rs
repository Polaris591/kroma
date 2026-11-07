//! Native Anthropic Messages wire format. The runtime keeps its own neutral
//! transcripts; signed thinking blocks are opaque continuation items.
use super::{ApecTextAttempt, ModelInfo, Usage};
use crate::protocol::{FunctionCall, Message, ToolCall};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

pub(super) fn payload(
    model: &str,
    messages: &[Message],
    tools: &[Value],
    max_tokens: usize,
    effort: &str,
    info: &ModelInfo,
    origin: &str,
) -> Result<Value> {
    payload_with_cache(
        model, messages, tools, max_tokens, effort, info, origin, None,
    )
}

// Keep the existing protocol payload inputs unchanged; the optional cache
// only affects image materialization, not wire format or request settings.
#[allow(clippy::too_many_arguments)]
pub(super) fn payload_with_cache(
    model: &str,
    messages: &[Message],
    tools: &[Value],
    max_tokens: usize,
    effort: &str,
    info: &ModelInfo,
    origin: &str,
    image_cache: Option<&crate::model::TaskImageCache>,
) -> Result<Value> {
    payload_with_cache_at(
        model,
        messages,
        tools,
        max_tokens,
        effort,
        info,
        origin,
        image_cache,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn payload_with_cache_at(
    model: &str,
    messages: &[Message],
    tools: &[Value],
    max_tokens: usize,
    effort: &str,
    info: &ModelInfo,
    origin: &str,
    image_cache: Option<&crate::model::TaskImageCache>,
    active_turn_override: Option<usize>,
) -> Result<Value> {
    let mut system = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    let active_turn = active_turn_override.unwrap_or_else(|| {
        messages
            .iter()
            .rposition(|msg| msg.role == "user")
            .unwrap_or(0)
    });
    for (index, msg) in messages.iter().enumerate() {
        match msg.role.as_str() {
            "system" | "developer" => {
                if let Some(text) = &msg.content {
                    system.push(text.clone());
                }
            }
            "user" => items.push(
                json!({"role":"user","content":crate::attachments::content_with_cache(
                    msg.content.as_deref().unwrap_or(""),
                    msg.provider_items.as_deref().unwrap_or(&[]),"anthropic",image_cache)?}),
            ),
            "assistant" => {
                let replay = super::replay_opaque_reasoning(msg, index, active_turn, origin);
                let native = msg
                    .provider_items
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .find(|item| item["type"] == "kroma_anthropic_content_blocks");
                let blocks = if let Some(native) = native {
                    // Signed content and tool blocks must retain their exact
                    // original order, including interleaved thinking blocks.
                    let mut original = native["blocks"]
                        .as_array()
                        .context("invalid saved Anthropic content blocks")?
                        .clone();
                    if !replay {
                        original.retain(|b| {
                            !matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking"))
                        });
                    }
                    original
                } else {
                    // Legacy sessions had separate opaque thinking and the
                    // normalized text/tool projections. Preserve compatibility.
                    let mut blocks = msg
                        .provider_items
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|v| {
                            replay
                                && matches!(
                                    v["type"].as_str(),
                                    Some("thinking" | "redacted_thinking")
                                )
                        })
                        .collect::<Vec<_>>();
                    if let Some(text) = msg.content.as_deref().filter(|s| !s.is_empty()) {
                        blocks.push(json!({"type":"text","text":text}));
                    }
                    for call in msg.tool_calls.as_deref().unwrap_or(&[]) {
                        let input: Value = serde_json::from_str(&call.function.arguments)
                            .context("invalid persisted tool arguments for Anthropic")?;
                        blocks.push(json!({"type":"tool_use","id":call.id,"name":call.function.name,"input":input}));
                    }
                    blocks
                };
                if !blocks.is_empty() {
                    items.push(json!({"role":"assistant","content":blocks}));
                }
            }
            "tool" => {
                let mut block = json!({"type":"tool_result",
                    "tool_use_id":msg.tool_call_id.as_deref().context("missing tool result call ID")?,
                    "content":msg.content.as_deref().unwrap_or("")});
                if msg.tool_failed() {
                    block["is_error"] = json!(true);
                }
                if let Some(last) = items.last_mut() {
                    if last["role"] == "user" && last["content"].is_array() {
                        last["content"].as_array_mut().unwrap().push(block);
                        continue;
                    }
                }
                items.push(json!({"role":"user","content":[block]}));
            }
            other => bail!("unsupported transcript role for Anthropic: {other}"),
        }
    }
    let tools = tools
        .iter()
        .map(|tool| {
            let function = tool.get("function").context("missing tool function")?;
            Ok(json!({
                "name":function["name"],"description":function["description"],
                "input_schema":function["parameters"]
            }))
        })
        .collect::<Result<Vec<Value>>>()?;
    let mut request = json!({
        "model":model,"max_tokens":max_tokens,"stream":true,
        "system":system.join("\n\n"),"messages":items,"tools":tools,
        // Automatic prompt caching moves the breakpoint with the growing
        // conversation; no hand-maintained cache markers or settings.
        "cache_control":{"type":"ephemeral"}
    });
    if !tools.is_empty() {
        // No tool_choice override: the model chooses whether to call tools.
    } else {
        request.as_object_mut().unwrap().remove("tools");
    }
    apply_reasoning(&mut request, effort, info, max_tokens)?;
    Ok(request)
}

pub(super) fn apply_reasoning(
    request: &mut Value,
    effort: &str,
    info: &ModelInfo,
    max_tokens: usize,
) -> Result<()> {
    let adaptive = info.supports("/thinking/types/adaptive") == Some(true);
    let enabled = info.supports("/thinking/types/enabled") == Some(true);
    match effort {
        "auto" => {
            if adaptive {
                request["thinking"] = json!({"type":"adaptive"});
            }
        }
        "none" | "off" => {
            match (
                info.supports("/thinking/types/disabled"),
                info.supports("/thinking/types/between_tools"),
            ) {
                (Some(false), Some(true)) => request["thinking"] = json!({"type":"between_tools"}),
                (Some(false), _) => bail!("upstream model does not allow disabling thinking"),
                _ => request["thinking"] = json!({"type":"disabled"}),
            }
        }
        "minimal" => bail!("Anthropic does not accept minimal effort; choose low or auto"),
        level => {
            if info.supports(&format!("/effort/{level}")) == Some(false) {
                bail!("upstream Anthropic model does not support {level} effort");
            }
            request["output_config"] = json!({"effort":level});
            if adaptive {
                request["thinking"] = json!({"type":"adaptive"});
            } else if enabled {
                if max_tokens <= 1024 {
                    bail!("extended thinking needs max_tokens greater than 1024");
                }
                request["thinking"] = json!({"type":"enabled","budget_tokens":(max_tokens/2).clamp(1024,8192).min(max_tokens-1)});
            }
            // Unknown capabilities: let the provider validate explicit effort,
            // rather than guessing model family or inventing support.
        }
    }
    Ok(())
}

#[derive(Default)]
enum Block {
    #[default]
    Empty,
    Text {
        text: String,
        citations: Vec<Value>,
    },
    ServerTool {
        original: Value,
        input: String,
    },
    NativeResult(Value),
    Tool {
        id: String,
        name: String,
        input: String,
        initial: Value,
    },
    Thinking {
        text: String,
        signature: String,
    },
    Redacted(Value),
}

// Anthropic reports *uncached* input separately from cache writes and reads.
// The common Usage.input field must include all three, while Usage.cached
// remains just the read portion (a subset of Usage.input). Missing optional
// cache counters mean no reported cache traffic; keep the read count unknown
// instead of presenting it as a measured zero.
#[derive(Default)]
struct InputBreakdown {
    uncached: Option<u64>,
    created: Option<u64>,
    read: Option<u64>,
}

impl InputBreakdown {
    fn update(&mut self, usage: &Value) {
        if let Some(tokens) = usage["input_tokens"].as_u64() {
            self.uncached = Some(tokens);
        }
        if let Some(tokens) = usage["cache_creation_input_tokens"].as_u64() {
            self.created = Some(tokens);
        }
        if let Some(tokens) = usage["cache_read_input_tokens"].as_u64() {
            self.read = Some(tokens);
        }
    }

    fn total(&self) -> Option<u64> {
        self.uncached?
            .checked_add(self.created.unwrap_or(0))?
            .checked_add(self.read.unwrap_or(0))
    }
}

#[derive(Default)]
pub(super) struct Stream {
    blocks: BTreeMap<usize, Block>,
    finish_reason: Option<String>,
    input: InputBreakdown,
    output: Option<u64>,
    pub received_delta: bool,
    pub done: bool,
    web_searches: Option<u64>,
    native_calls: BTreeMap<String, NativeSearchState>,
}

#[derive(Default)]
struct NativeSearchState {
    started: bool,
    finished: bool,
}

impl Stream {
    #[cfg(test)]
    pub fn apply_with_reasoning(
        &mut self,
        data: &str,
        on_text: &mut (impl FnMut(&str) + Send),
        on_reasoning: &mut (impl FnMut(&str) + Send),
    ) -> Result<()> {
        self.apply_with_activity(data, on_text, on_reasoning, &mut |_| {})
    }

    pub fn apply_with_activity(
        &mut self,
        data: &str,
        on_text: &mut (impl FnMut(&str) + Send),
        on_reasoning: &mut (impl FnMut(&str) + Send),
        on_activity: &mut (impl FnMut(crate::model::NativeActivity) + Send),
    ) -> Result<()> {
        if data == "[DONE]" {
            return Ok(());
        }
        let v: Value = serde_json::from_str(data).context("invalid Anthropic stream event")?;
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                let usage = &v["message"]["usage"];
                self.input.update(usage);
                self.output = usage["output_tokens"].as_u64();
                if let Some(count) = usage.pointer("/server_tool_use/web_search_requests").and_then(Value::as_u64) {
                    self.web_searches = Some(count);
                }
            }
            "content_block_start" => {
                let index = v["index"]
                    .as_u64()
                    .context("Anthropic block index missing")? as usize;
                let block = &v["content_block"];
                let new = match block["type"].as_str().unwrap_or("") {
                    "text" => {
                        let text = block["text"].as_str().unwrap_or("").to_owned();
                        if !text.is_empty() {
                            on_text(&text);
                            self.received_delta = true;
                        }
                        Block::Text {
                            text,
                            citations: block["citations"].as_array().cloned().unwrap_or_default(),
                        }
                    }
                    "server_tool_use" => {
                        if block["name"] == "web_search" {
                            if let Some(id) = block["id"].as_str().filter(|id| !id.is_empty()) {
                                self.native_calls.entry(id.to_owned()).or_default();
                            }
                        }
                        Block::ServerTool { original: block.clone(), input: String::new() }
                    }
                    "web_search_tool_result" => {
                        if let Some(id) = block["tool_use_id"].as_str() {
                            if let Some(state) = self.native_calls.get_mut(id) {
                                if !state.finished {
                                    let failed = block["content"]["type"] == "web_search_tool_result_error";
                                    let summary = if failed {
                                        format!("Search error: {}", block["content"]["error_code"].as_str().unwrap_or("unknown"))
                                    } else {
                                        let sources = block["content"].as_array().into_iter().flatten()
                                            .filter_map(|item| {
                                                let url = item["url"].as_str()?;
                                                let title = item["title"].as_str().unwrap_or("");
                                                Some(format!("- {url} ({title})"))
                                            })
                                            .take(8).collect::<Vec<_>>();
                                        if sources.is_empty() { "Search returned no sources".into() }
                                        else { format!("Sources:\n{}", sources.join("\n")) }
                                    };
                                    on_activity(crate::model::NativeActivity {
                                        name: "web_search".into(), id: id.to_owned(),
                                        arguments: "{}".into(), result: Some(summary),
                                        done: true, failed,
                                    });
                                    state.finished = true;
                                }
                            }
                        }
                        Block::NativeResult(block.clone())
                    }
                    "tool_use" => Block::Tool {
                        id: block["id"]
                            .as_str()
                            .context("tool_use missing id")?
                            .to_owned(),
                        name: block["name"]
                            .as_str()
                            .context("tool_use missing name")?
                            .to_owned(),
                        input: String::new(),
                        initial: block["input"].clone(),
                    },
                    "thinking" => {
                        let text = block["thinking"].as_str().unwrap_or("").to_owned();
                        if !text.is_empty() {
                            self.received_delta = true;
                            on_reasoning(&text);
                        }
                        Block::Thinking {
                            text,
                            signature: block["signature"].as_str().unwrap_or("").to_owned(),
                        }
                    }
                    "redacted_thinking" => Block::Redacted(block.clone()),
                    _ => Block::Empty,
                };
                self.blocks.insert(index, new);
            }
            "content_block_delta" => {
                let index = v["index"]
                    .as_u64()
                    .context("Anthropic delta index missing")? as usize;
                let delta = &v["delta"];
                let part = self
                    .blocks
                    .get_mut(&index)
                    .context("Anthropic delta without block start")?;
                match (part, delta["type"].as_str().unwrap_or("")) {
                    (Block::Text { text, .. }, "text_delta") => {
                        let s = delta["text"].as_str().unwrap_or("");
                        text.push_str(s);
                        if !s.is_empty() {
                            self.received_delta = true;
                            on_text(s);
                        }
                    }
                    (Block::Text { citations, .. }, "citations_delta") => {
                        if delta["citation"].is_object() {
                            citations.push(delta["citation"].clone());
                        }
                    }
                    (Block::ServerTool { input, .. }, "input_json_delta") => {
                        input.push_str(delta["partial_json"].as_str().unwrap_or(""));
                        self.received_delta = true;
                    }
                    (Block::Tool { input, .. }, "input_json_delta") => {
                        input.push_str(delta["partial_json"].as_str().unwrap_or(""));
                        self.received_delta = true;
                    }
                    (Block::Thinking { text, .. }, "thinking_delta") => {
                        let chunk = delta["thinking"].as_str().unwrap_or("");
                        text.push_str(chunk);
                        if !chunk.is_empty() {
                            self.received_delta = true;
                            on_reasoning(chunk);
                        }
                    }
                    (Block::Thinking { signature, .. }, "signature_delta") => {
                        signature.push_str(delta["signature"].as_str().unwrap_or(""));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = v["index"].as_u64().context("Anthropic block index missing")? as usize;
                if let Some(Block::ServerTool { original, input }) = self.blocks.get_mut(&index) {
                    if !input.is_empty() {
                        original["input"] = serde_json::from_str(input)
                            .context("incomplete Anthropic server tool input")?;
                    }
                    if original["name"] == "web_search" {
                        if let Some(id) = original["id"].as_str() {
                            if let Some(state) = self.native_calls.get_mut(id) {
                                if !state.started {
                                    state.started = true;
                                    on_activity(crate::model::NativeActivity {
                                        name: "web_search".into(), id: id.to_owned(),
                                        arguments: original["input"].to_string(),
                                        result: None, done: false, failed: false,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            "message_delta" => {
                if let Some(count) = v["usage"].pointer("/server_tool_use/web_search_requests").and_then(Value::as_u64) {
                    self.web_searches = Some(count);
                }
                self.finish_reason = v["delta"]["stop_reason"].as_str().map(str::to_owned);
                self.input.update(&v["usage"]);
                if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                    self.output = Some(n);
                }
            }
            "message_stop" => self.done = true,
            "ping" => {}
            "error" => bail!("Anthropic stream error: {}", v["error"]),
            _ => {}
        }
        Ok(())
    }

    /// Close live search entries on stream failure, pause, or completion.
    pub(super) fn drain_finished_native(&mut self) -> Vec<crate::model::NativeActivity> {
        let paused = self.finish_reason.as_deref() == Some("pause_turn");
        let mut finished = Vec::new();
        for (id, state) in &mut self.native_calls {
            if state.finished { continue; }
            state.finished = true;
            finished.push(crate::model::NativeActivity {
                name: "web_search".into(), id: id.clone(), arguments: "{}".into(),
                result: Some(if paused { "Search paused; resuming".into() }
                    else { "Search was interrupted before receiving results".into() }),
                done: true, failed: !paused,
            });
        }
        finished
    }

    pub(super) fn reported_usage(&self) -> Option<Usage> {
        let input = self.input.total();
        let output = self.output;
        if input.is_none() && output.is_none() {
            return None;
        }
        Some(Usage {
            input,
            output,
            total: input.and_then(|i| output.and_then(|o| i.checked_add(o))),
            cached: self.input.read,
            reasoning: None,
            web_searches: self.web_searches,
        })
    }

    pub fn finish(self) -> Result<(Message, Option<Usage>)> {
        let paused = self.finish_reason.as_deref() == Some("pause_turn");
        match self.finish_reason.as_deref() {
            Some("end_turn" | "tool_use" | "stop_sequence" | "pause_turn") => {}
            Some("max_tokens") => bail!("Anthropic response truncated at max_tokens"),
            Some(reason) => bail!("Anthropic stopped unexpectedly: {reason}"),
            None => bail!("Anthropic stream ended without stop_reason"),
        }
        if !self.done {
            bail!("Anthropic stream ended without message_stop");
        }
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls = Vec::new();
        let mut opaque = Vec::new();
        let mut native_blocks = Vec::new();
        let mut seen = HashSet::new();
        for block in self.blocks.into_values() {
            match block {
                Block::Text { text, citations } => {
                    content.push_str(&text);
                    let mut block = json!({"type":"text","text":text});
                    if !citations.is_empty() { block["citations"] = json!(citations); }
                    native_blocks.push(block);
                }
                Block::ServerTool { original, .. } => {
                    native_blocks.push(original);
                }
                Block::NativeResult(value) => {
                    native_blocks.push(value);
                }
                Block::Tool {
                    id,
                    name,
                    input,
                    initial,
                } => {
                    if !seen.insert(id.clone()) {
                        bail!("duplicate Anthropic tool ID");
                    }
                    let arguments = if input.is_empty() {
                        initial.to_string()
                    } else {
                        let value: Value = serde_json::from_str(&input)
                            .context("incomplete Anthropic tool input")?;
                        value.to_string()
                    };
                    crate::provider::validate_tool_call(&id, &name, &arguments)?;
                    native_blocks.push(json!({"type":"tool_use","id":id,"name":name,
                        "input":serde_json::from_str::<Value>(&arguments)?}));
                    calls.push(ToolCall {
                        id,
                        kind: "function".into(),
                        function: FunctionCall { name, arguments },
                    });
                }
                Block::Thinking { text, signature } => {
                    if signature.is_empty() {
                        bail!("Anthropic thinking signature missing");
                    }
                    reasoning.push_str(&text);
                    let block = json!({"type":"thinking","thinking":text,"signature":signature});
                    native_blocks.push(block.clone());
                    opaque.push(block);
                }
                Block::Redacted(value) => {
                    native_blocks.push(value.clone());
                    opaque.push(value);
                }
                Block::Empty => {}
            }
        }
        let has_server_search = native_blocks.iter().any(|block| {
            matches!(block["type"].as_str(), Some("server_tool_use" | "web_search_tool_result"))
                || block["citations"].as_array().is_some_and(|items| !items.is_empty())
        });
        if !opaque.is_empty() || has_server_search || paused {
            opaque.push(json!({"type":"kroma_anthropic_content_blocks",
                "blocks":native_blocks}));
        }
        if paused {
            opaque.push(json!({"type":"kroma_anthropic_pause_turn"}));
        }
        if content.trim().is_empty() && calls.is_empty() && !paused {
            bail!("Anthropic returned no text or tool calls");
        }
        let input = self.input.total();
        let total = input.and_then(|a| self.output.and_then(|b| a.checked_add(b)));
        Ok((
            Message {
                role: "assistant".into(),
                content: Some(content),
                reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                tool_calls: (!calls.is_empty()).then_some(calls),
                tool_call_id: None,
                provider_items: (!opaque.is_empty()).then_some(opaque),
            },
            Some(Usage {
                input,
                output: self.output,
                total,
                cached: self.input.read,
                reasoning: None,
                web_searches: self.web_searches,
            }),
        ))
    }
}

pub(super) fn parse_text(body: &str) -> Result<ApecTextAttempt> {
    let v: Value = serde_json::from_str(body).context("invalid Anthropic text response")?;
    let usage = &v["usage"];
    let mut breakdown = InputBreakdown::default();
    breakdown.update(usage);
    let input = breakdown.total();
    let output = usage["output_tokens"].as_u64();
    let stats = Some(Usage {
        input,
        output,
        total: input.and_then(|i| output.and_then(|o| i.checked_add(o))),
        cached: breakdown.read,
        reasoning: None,
        web_searches: None,
    });
    if v["stop_reason"] == "max_tokens" {
        return Ok(ApecTextAttempt::Truncated(stats));
    }
    if v["stop_reason"] != "end_turn" {
        bail!("unexpected Anthropic summary stop_reason");
    }
    let text = v["content"]
        .as_array()
        .context("Anthropic summary content missing")?
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    if text.trim().is_empty() {
        bail!("Anthropic summary was empty");
    }
    Ok(ApecTextAttempt::Complete(text.trim().to_owned(), stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_anthropic_request_enables_prompt_caching() {
        let history = [
            Message::new("system", "You are a coding assistant."),
            Message::new("user", "Inspect the repository."),
        ];
        let request = payload(
            "claude-test",
            &history,
            &[],
            1024,
            "auto",
            &ModelInfo::default(),
            "origin",
        )
        .unwrap();
        assert_eq!(
            request["cache_control"]["type"], "ephemeral",
            "Anthropic prompt caching requires an explicit breakpoint even with automatic caching"
        );
    }

    #[test]
    fn stream_usage_includes_cache_reads_and_writes_without_double_counting_deltas() {
        let mut stream = Stream::default();
        let mut feed = |event: Value| {
            stream
                .apply_with_reasoning(&event.to_string(), &mut |_| {}, &mut |_| {})
                .unwrap();
        };
        feed(json!({"type":"message_start","message":{"usage":{
            "input_tokens":12,"cache_creation_input_tokens":20,
            "cache_read_input_tokens":30,"output_tokens":1
        }}}));
        feed(json!({"type":"content_block_start","index":0,
            "content_block":{"type":"text","text":"result"}}));
        // A cumulative delta must replace a reported count, not add to it;
        // missing fields must preserve the earlier counter values.
        feed(
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
            "usage":{"output_tokens":5,"cache_read_input_tokens":31}}),
        );
        feed(json!({"type":"message_stop"}));
        let (_, usage) = stream.finish().unwrap();
        let usage = usage.unwrap();
        assert_eq!(usage.input, Some(63));
        assert_eq!(usage.cached, Some(31));
        assert_eq!(usage.output, Some(5));
        assert_eq!(usage.total, Some(68));
    }

    #[test]
    fn nonstream_apec_usage_includes_all_input_categories() {
        let body = json!({"stop_reason":"end_turn",
            "content":[{"type":"text","text":"summary"}],
            "usage":{"input_tokens":12,"cache_creation_input_tokens":20,
                "cache_read_input_tokens":30,"output_tokens":5}});
        let ApecTextAttempt::Complete(text, Some(usage)) = parse_text(&body.to_string()).unwrap()
        else {
            panic!("expected complete text with Anthropic usage");
        };
        assert_eq!(text, "summary");
        assert_eq!(usage.input, Some(62));
        assert_eq!(usage.cached, Some(30));
        assert_eq!(usage.total, Some(67));
        let legacy = json!({"stop_reason":"max_tokens", "usage":{
            "input_tokens":11,"output_tokens":3
        }});
        let ApecTextAttempt::Truncated(Some(usage)) = parse_text(&legacy.to_string()).unwrap()
        else {
            panic!("expected truncated response with preserved counters");
        };
        assert_eq!(usage.input, Some(11));
        assert_eq!(
            usage.cached, None,
            "unreported cache reads are not measured zero"
        );
        let invalid = json!({"stop_reason":"max_tokens", "usage":{
            "cache_read_input_tokens":99,"output_tokens":3
        }});
        let ApecTextAttempt::Truncated(Some(usage)) = parse_text(&invalid.to_string()).unwrap()
        else {
            panic!("expected truncated response with unknown input");
        };
        assert_eq!(usage.input, None, "missing uncached input remains unknown");
    }

    #[test]
    fn overflowed_anthropic_input_sum_does_not_wrap() {
        let mut breakdown = InputBreakdown::default();
        breakdown.update(&json!({"input_tokens":u64::MAX,
            "cache_read_input_tokens":1,"cache_creation_input_tokens":0}));
        assert_eq!(breakdown.total(), None);
        assert_eq!(breakdown.read, Some(1));
    }

    #[test]
    fn signed_thinking_tool_round_trip() {
        let mut s = Stream::default();
        let mut out = String::new();
        let live_reasoning = std::sync::Mutex::new(String::new());
        let mut push = |value: Value| {
            s.apply_with_reasoning(&value.to_string(), &mut |t| out.push_str(t), &mut |t| {
                live_reasoning.lock().unwrap().push_str(t)
            })
            .unwrap()
        };
        push(
            json!({"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}),
        );
        push(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        );
        push(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hidden"}}),
        );
        assert_eq!(
            *live_reasoning.lock().unwrap(),
            "hidden",
            "thinking delta must be visible before message_stop"
        );
        push(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signature"}}),
        );
        push(
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool1","name":"shell","input":{}}}),
        );
        push(
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"pwd\"}"}}),
        );
        push(
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":8}}),
        );
        push(json!({"type":"message_stop"}));
        let (message, _) = s.finish().unwrap();
        assert_eq!(out, "");
        assert_eq!(message.reasoning_content.as_deref(), Some("hidden"));
        let request = payload(
            "claude",
            &[
                Message::new("user", "hi"),
                message,
                Message::tool("tool1".into(), "done".into()),
            ],
            &[],
            4096,
            "auto",
            &ModelInfo::default(),
            "test-origin",
        )
        .unwrap();
        assert_eq!(
            request["messages"][1]["content"][0]["signature"],
            "signature"
        );
        assert_eq!(request["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(request["messages"][2]["content"][0]["tool_use_id"], "tool1");
    }

    #[test]
    fn anthropic_rejects_invalid_tool_calls_before_execution() {
        for (id, name, input) in [
            ("", "shell", "{}"),
            ("tool1", "", "{}"),
            ("tool1", "shell", "[]"),
        ] {
            let mut stream = Stream::default();
            let events = [
                json!({"type":"message_start","message":{"usage":{"input_tokens":8}}}),
                json!({"type":"content_block_start","index":0,
                    "content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
                json!({"type":"content_block_delta","index":0,
                    "delta":{"type":"input_json_delta","partial_json":input}}),
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},
                    "usage":{"output_tokens":9}}),
                json!({"type":"message_stop"}),
            ];
            for event in events {
                stream
                    .apply_with_reasoning(&event.to_string(), &mut |_| {}, &mut |_| {})
                    .unwrap();
            }
            assert!(
                stream.finish().is_err(),
                "id={id:?} name={name:?} input={input}"
            );
        }
    }

    #[test]
    fn signed_thinking_and_tools_preserve_original_interleaved_block_order() {
        let mut stream = Stream::default();
        let events = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":8}}}),
            json!({"type":"content_block_start","index":0,
                "content_block":{"type":"thinking","thinking":"first","signature":"sig1"}}),
            json!({"type":"content_block_start","index":1,
                "content_block":{"type":"tool_use","id":"a","name":"shell",
                    "input":{"command":"pwd"}}}),
            json!({"type":"content_block_start","index":2,
                "content_block":{"type":"thinking","thinking":"second","signature":"sig2"}}),
            json!({"type":"content_block_start","index":3,
                "content_block":{"type":"tool_use","id":"b","name":"shell",
                    "input":{"command":"ls"}}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},
                "usage":{"output_tokens":9}}),
            json!({"type":"message_stop"}),
        ];
        for event in events {
            stream
                .apply_with_reasoning(&event.to_string(), &mut |_| {}, &mut |_| {})
                .unwrap();
        }
        let (message, _) = stream.finish().unwrap();
        let request = payload(
            "claude",
            &[
                Message::new("user", "check"),
                message,
                Message::tool("a".into(), "ok".into()),
                Message::tool("b".into(), "ok".into()),
            ],
            &[],
            4096,
            "auto",
            &ModelInfo::default(),
            "origin",
        )
        .unwrap();
        let blocks = request["messages"][1]["content"].as_array().unwrap();
        let kinds = blocks
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["thinking", "tool_use", "thinking", "tool_use"]);
        assert_eq!(blocks[0]["signature"], "sig1");
        assert_eq!(blocks[2]["signature"], "sig2");
    }

    #[test]
    fn disable_thinking_respects_capabilities() {
        let mut request = json!({"model":"mock"});
        let only_between_tools = ModelInfo::parse(&json!({"capabilities":{"thinking":{"types":{
            "disabled":{"supported":false}, "between_tools":{"supported":true}
        }}}}));
        apply_reasoning(&mut request, "off", &only_between_tools, 4096).unwrap();
        assert_eq!(request["thinking"]["type"], "between_tools");
        let cannot_disable = ModelInfo::parse(&json!({"capabilities":{"thinking":{"types":{
            "disabled":{"supported":false}, "between_tools":{"supported":false}
        }}}}));
        assert!(apply_reasoning(&mut json!({}), "off", &cannot_disable, 4096).is_err());
    }
}
