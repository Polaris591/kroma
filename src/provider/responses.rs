//! Stateless OpenAI Responses protocol adapter.
use super::{ApecTextAttempt, Usage};
use crate::protocol::{FunctionCall, Message, ToolCall};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

pub(super) fn payload(
    model: &str,
    messages: &[Message],
    tools: &[Value],
    max_output: Option<usize>,
    effort: &str,
    origin: &str,
) -> Result<Value> {
    payload_with_cache_at(
        model,
        messages,
        tools,
        max_output,
        effort,
        origin,
        None,
        None,
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn payload_with_cache(
    model: &str,
    messages: &[Message],
    tools: &[Value],
    max_output: Option<usize>,
    effort: &str,
    origin: &str,
    image_cache: Option<&crate::model::TaskImageCache>,
    native_tools: &[Value],
) -> Result<Value> {
    payload_with_cache_at(
        model,
        messages,
        tools,
        max_output,
        effort,
        origin,
        image_cache,
        None,
        native_tools,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn payload_with_cache_at(
    model: &str,
    messages: &[Message],
    tools: &[Value],
    max_output: Option<usize>,
    effort: &str,
    origin: &str,
    image_cache: Option<&crate::model::TaskImageCache>,
    active_turn_override: Option<usize>,
    // Server-side tools the endpoint executes itself (for example
    // `web_search`). They are appended verbatim and never become function
    // definitions the local ToolRegistry could execute.
    native_tools: &[Value],
) -> Result<Value> {
    let mut input = Vec::new();
    let active_turn = active_turn_override.unwrap_or_else(|| {
        messages
            .iter()
            .rposition(|msg| msg.role == "user")
            .unwrap_or(0)
    });
    for (index, msg) in messages.iter().enumerate() {
        match msg.role.as_str() {
            "system" | "developer" | "user" => {
                let content = if msg.role == "user" {
                    crate::attachments::content_with_cache(
                        msg.content.as_deref().unwrap_or(""),
                        msg.provider_items.as_deref().unwrap_or(&[]),
                        "responses",
                        image_cache,
                    )?
                } else {
                    json!(msg.content.as_deref().unwrap_or(""))
                };
                input.push(json!({"role":msg.role,"content":content}))
            }
            "assistant" => {
                for item in msg.provider_items.as_deref().unwrap_or(&[]) {
                    if item["type"] == "reasoning"
                        && super::replay_opaque_reasoning(msg, index, active_turn, origin)
                    {
                        input.push(item.clone());
                    }
                }
                if let Some(text) = msg.content.as_deref().filter(|s| !s.is_empty()) {
                    input.push(json!({"role":"assistant","content":text}));
                }
                for call in msg.tool_calls.as_deref().unwrap_or(&[]) {
                    input.push(json!({"type":"function_call","call_id":call.id,
                        "name":call.function.name,"arguments":call.function.arguments}));
                }
            }
            "tool" => input.push(json!({"type":"function_call_output",
                "call_id":msg.tool_call_id.as_deref().context("missing tool_call_id")?,
                "output":msg.content.as_deref().unwrap_or("")})),
            other => bail!("unsupported role for Responses: {other}"),
        }
    }
    let mut definitions = tools.iter().map(|tool| {
        let f = &tool["function"];
        Ok(json!({"type":"function","name":f["name"],"description":f["description"],"parameters":f["parameters"]}))
    }).collect::<Result<Vec<Value>>>()?;
    definitions.extend(native_tools.iter().cloned());
    let mut request = json!({"model":model,"input":input,"tools":definitions,
        "stream":true,"store":false,"include":["reasoning.encrypted_content"]});
    if request["tools"].as_array().is_some_and(Vec::is_empty) {
        // Several Responses-compatible endpoints reject an empty tool list; a
        // tool-less request must omit the field.
        request.as_object_mut().unwrap().remove("tools");
    }
    if let Some(n) = max_output {
        request["max_output_tokens"] = json!(n);
    }
    if effort != "auto" && effort != "off" {
        request["reasoning"] = json!({"effort":effort});
    } else if effort == "off" {
        request["reasoning"] = json!({"effort":"none"});
    }
    if !matches!(effort, "off" | "none") {
        request["reasoning"]["summary"] = json!("auto");
    }
    Ok(request)
}

/// A server-side tool call the endpoint executed itself. The local
/// ToolRegistry never sees it; it exists for display and for the persisted
/// source list.
#[derive(Default)]
struct NativeCall {
    id: String,
    arguments: String,
    summary: String,
    reported: bool,
    finished: bool,
    failed: bool,
}

#[derive(Default)]
pub(super) struct Stream {
    items: BTreeMap<usize, Value>,
    text: String,
    indexed_text: BTreeMap<usize, String>,
    unindexed_text: String,
    reasoning_summary: String,
    usage: Option<Usage>,
    status: Option<String>,
    native: BTreeMap<usize, NativeCall>,
    /// Real sources the endpoint cited, kept in the assistant turn.
    citations: Vec<Value>,
    citation_keys: std::collections::BTreeSet<String>,
    pub received_delta: bool,
    pub done: bool,
}

impl Stream {
    #[cfg(test)]
    pub fn apply(&mut self, data: &str, on_text: &mut (impl FnMut(&str) + Send)) -> Result<()> {
        self.apply_with_reasoning(data, on_text, &mut |_| {}, &mut |_| {})
    }

    pub fn apply_with_reasoning(
        &mut self,
        data: &str,
        on_text: &mut (impl FnMut(&str) + Send),
        on_reasoning: &mut (impl FnMut(&str) + Send),
        on_activity: &mut (impl FnMut(crate::model::NativeActivity) + Send),
    ) -> Result<()> {
        if data == "[DONE]" {
            return Ok(());
        }
        let v: Value = serde_json::from_str(data).context("invalid Responses SSE event")?;
        match v["type"].as_str().unwrap_or("") {
            "response.output_item.added" => {
                if let Some(index) = v["output_index"].as_u64() {
                    self.items.insert(index as usize, v["item"].clone());
                    self.observe_native(index as usize, &v["item"], on_activity);
                }
            }
            "response.output_item.done" => {
                if let Some(index) = v["output_index"].as_u64() {
                    self.items.insert(index as usize, v["item"].clone());
                    self.observe_native(index as usize, &v["item"], on_activity);
                }
            }
            // Sources are cited while the answer streams; the final message
            // item repeats them. Store both, ignore unrelated annotations.
            "response.output_text.annotation.added" => self.record_citation(&v["annotation"]),
            "response.function_call_arguments.delta" => {
                let index = v["output_index"]
                    .as_u64()
                    .context("missing function output_index")? as usize;
                let item = self
                    .items
                    .get_mut(&index)
                    .context("function arguments before item")?;
                let old = item["arguments"].as_str().unwrap_or("").to_string();
                item["arguments"] = json!(old + v["delta"].as_str().unwrap_or(""));
                self.received_delta = true;
            }
            "response.output_text.delta" => {
                let s = v["delta"].as_str().unwrap_or("");
                if !s.is_empty() {
                    self.received_delta = true;
                    self.text.push_str(s);
                    if let Some(index) = v["output_index"].as_u64() {
                        self.indexed_text
                            .entry(index as usize)
                            .or_default()
                            .push_str(s);
                    } else {
                        self.unindexed_text.push_str(s);
                    }
                    on_text(s);
                }
            }
            "response.refusal.delta" => {
                let s = v["delta"].as_str().unwrap_or("");
                if !s.is_empty() {
                    self.received_delta = true;
                    self.text.push_str(s);
                    if let Some(index) = v["output_index"].as_u64() {
                        self.indexed_text
                            .entry(index as usize)
                            .or_default()
                            .push_str(s);
                    } else {
                        self.unindexed_text.push_str(s);
                    }
                    on_text(s);
                }
            }
            // Responses exposes a *summary*, never the model's hidden raw
            // reasoning. Encrypted reasoning items must not be rendered.
            "response.reasoning_summary_text.delta" => {
                let s = v["delta"].as_str().unwrap_or("");
                if !s.is_empty() {
                    self.received_delta = true;
                    self.reasoning_summary.push_str(s);
                    on_reasoning(s);
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                self.status = v["response"]["status"].as_str().map(str::to_owned);
                self.usage = Usage::from_value(&v["response"]["usage"]);
                // Server-side search counts are a provider measurement, never
                // derived from the number of observed calls.
                if let Some(count) = v["response"]
                    .pointer("/tool_usage/web_search/num_requests")
                    .and_then(Value::as_u64)
                {
                    self.usage.get_or_insert_with(Usage::default).web_searches = Some(count);
                }
                if let Some(output) = v["response"]["output"].as_array() {
                    for item in output {
                        if item["type"] != "message" {
                            continue;
                        }
                        for part in item["content"].as_array().into_iter().flatten() {
                            for annotation in part["annotations"].as_array().into_iter().flatten() {
                                self.record_citation(annotation);
                            }
                        }
                    }
                }
                // The final Responses output, when supplied, is authoritative.
                // A stream may carry only deltas for one message and a full
                // output item for a second; do not silently drop that second
                // message just because the first already emitted text.
                if let Some(output) = v["response"]["output"].as_array() {
                    for (index, item) in output.iter().enumerate() {
                        self.items.insert(index, item.clone());
                    }
                }
                self.done = true;
            }
            "error" => bail!("Responses stream error: {v}"),
            _ => {}
        }
        Ok(())
    }
    /// Track one server-side tool item. The query or URL is only known once
    /// the endpoint finishes the call, so progress is reported from there;
    /// the result and sources follow when the stream ends.
    fn observe_native(
        &mut self,
        index: usize,
        item: &Value,
        on_activity: &mut (impl FnMut(crate::model::NativeActivity) + Send),
    ) {
        if item["type"] != "web_search_call" {
            return;
        }
        let Some(id) = item["id"].as_str().filter(|id| !id.is_empty()) else {
            return;
        };
        let call = self.native.entry(index).or_insert_with(|| NativeCall {
            id: id.to_owned(),
            arguments: "{}".into(),
            ..NativeCall::default()
        });
        if item["action"].is_object() {
            call.arguments = native_arguments(item);
            call.summary = native_summary(item);
        }
        call.failed |= item["status"] == "failed";
        if call.reported || !item["action"].is_object() {
            return;
        }
        call.reported = true;
        on_activity(crate::model::NativeActivity {
            name: "web_search".into(),
            id: call.id.clone(),
            arguments: call.arguments.clone(),
            result: None,
            done: false,
            failed: false,
        });
    }

    /// Keep the first occurrence of each cited source, with its position.
    fn record_citation(&mut self, annotation: &Value) {
        if annotation["type"] != "url_citation" {
            return;
        }
        let Some(url) = annotation["url"].as_str().filter(|url| !url.is_empty()) else {
            return;
        };
        let title = annotation["title"].as_str().unwrap_or("");
        if !self.citation_keys.insert(format!("{url}|{title}")) {
            return;
        }
        let mut item = json!({"type":"url_citation","url":url,"title":title});
        for (key, field) in [("start_index", "start_index"), ("end_index", "end_index")] {
            if let Some(value) = annotation[key].as_u64() {
                item[field] = json!(value);
            }
        }
        self.citations.push(item);
    }

    /// Finish every server-side call whose progress was already shown. Called
    /// once the stream ends, including after a failure, so no display record
    /// stays open and no source list is invented.
    pub(super) fn drain_finished_native(&mut self) -> Vec<crate::model::NativeActivity> {
        let citations = self.citations.clone();
        let mut finished = Vec::new();
        for call in self.native.values_mut() {
            if !call.reported || call.finished {
                continue;
            }
            call.finished = true;
            let mut lines = Vec::new();
            if !call.summary.is_empty() {
                lines.push(call.summary.clone());
            }
            if !citations.is_empty() {
                lines.push("Sources:".into());
                for citation in &citations {
                    let url = citation["url"].as_str().unwrap_or("");
                    let title = citation["title"].as_str().unwrap_or("");
                    lines.push(match title.is_empty() {
                        true => format!("- {url}"),
                        false => format!("- {url} ({title})"),
                    });
                }
            }
            finished.push(crate::model::NativeActivity {
                name: "web_search".into(),
                id: call.id.clone(),
                arguments: call.arguments.clone(),
                result: Some(lines.join(
                    "
",
                )),
                done: true,
                failed: call.failed,
            });
        }
        finished
    }

    pub(super) fn reported_usage(&self) -> Option<Usage> {
        self.usage.clone()
    }

    pub fn finish(self) -> Result<(Message, Option<Usage>)> {
        if !self.done || self.status.as_deref() != Some("completed") {
            bail!("Responses stream was not completed: {:?}", self.status);
        }
        let mut calls = Vec::new();
        let mut opaque = Vec::new();
        let mut content = String::new();
        let mut seen = HashSet::new();
        let mut indexes = std::collections::BTreeSet::new();
        indexes.extend(self.items.keys().copied());
        indexes.extend(self.indexed_text.keys().copied());
        for index in indexes {
            let Some(item) = self.items.get(&index) else {
                if let Some(partial) = self.indexed_text.get(&index) {
                    content.push_str(partial);
                }
                continue;
            };
            match item["type"].as_str().unwrap_or("") {
                "function_call" => {
                    let id = item["call_id"]
                        .as_str()
                        .context("Responses missing call_id")?
                        .to_owned();
                    let name = item["name"]
                        .as_str()
                        .context("Responses missing name")?
                        .to_owned();
                    let arguments = item["arguments"]
                        .as_str()
                        .context("Responses missing arguments")?
                        .to_owned();
                    crate::provider::validate_tool_call(&id, &name, &arguments)?;
                    if !seen.insert(id.clone()) {
                        bail!("duplicate Responses call ID");
                    }
                    calls.push(ToolCall {
                        id,
                        kind: "function".into(),
                        function: FunctionCall { name, arguments },
                    });
                }
                "reasoning" => opaque.push(item.clone()),
                "message" => {
                    let before = content.len();
                    for part in item["content"].as_array().into_iter().flatten() {
                        if part["type"] == "output_text" {
                            content.push_str(part["text"].as_str().unwrap_or(""));
                        } else if part["type"] == "refusal" {
                            // Refusal is a terminal assistant answer, not an
                            // empty response that the Agent should retry.
                            content.push_str(part["refusal"].as_str().unwrap_or(""));
                        }
                    }
                    if before == content.len() {
                        if let Some(partial) = self.indexed_text.get(&index) {
                            content.push_str(partial);
                        }
                    }
                }
                _ => {}
            }
        }
        // Older gateway streams omit output_index even when the completed
        // item contains the same text. Prefer the authoritative final item
        // and avoid returning duplicated answer text.
        if !self.unindexed_text.is_empty() && !content.contains(&self.unindexed_text) {
            content.push_str(&self.unindexed_text);
        }
        if content.is_empty() {
            content = self.text;
        }
        if calls.is_empty() && content.trim().is_empty() {
            bail!("Responses returned neither text nor tools");
        }
        // Cited sources stay with the turn they belong to. They are never
        // replayed upstream: the request builder only forwards reasoning
        // items from an assistant message.
        opaque.extend(self.citations.iter().cloned());
        Ok((
            Message {
                role: "assistant".into(),
                content: Some(content),
                reasoning_content: (!self.reasoning_summary.is_empty())
                    .then_some(self.reasoning_summary),
                tool_calls: (!calls.is_empty()).then_some(calls),
                tool_call_id: None,
                provider_items: (!opaque.is_empty()).then_some(opaque),
            },
            self.usage,
        ))
    }
}

fn native_query(item: &Value) -> String {
    let action = &item["action"];
    let queries = action["queries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if queries.is_empty() {
        action["query"].as_str().unwrap_or("").to_owned()
    } else {
        queries.join(", ")
    }
}

/// Compact JSON arguments for the existing tool activity display.
fn native_arguments(item: &Value) -> String {
    match item["action"]["type"].as_str() {
        Some("open_page") => {
            json!({"url": item["action"]["url"].as_str().unwrap_or("")}).to_string()
        }
        Some("search") => json!({"query": native_query(item)}).to_string(),
        _ => "{}".into(),
    }
}

fn native_summary(item: &Value) -> String {
    match item["action"]["type"].as_str() {
        Some("open_page") => {
            format!("opened {}", item["action"]["url"].as_str().unwrap_or(""))
        }
        _ => {
            let query = native_query(item);
            if query.is_empty() {
                "searched".into()
            } else {
                format!("searched {query}")
            }
        }
    }
}

pub(super) fn parse_text(body: &str) -> Result<ApecTextAttempt> {
    let v: Value = serde_json::from_str(body).context("invalid Responses text response")?;
    let usage = Usage::from_value(&v["usage"]);
    if v["status"] == "incomplete" {
        return Ok(ApecTextAttempt::Truncated(usage));
    }
    if v["status"] != "completed" {
        bail!("Responses summary was not completed");
    }
    let mut text = String::new();
    for item in v["output"].as_array().context("Responses output missing")? {
        if item["type"] == "message" {
            for part in item["content"].as_array().into_iter().flatten() {
                if part["type"] == "output_text" {
                    text.push_str(part["text"].as_str().unwrap_or(""));
                }
            }
        }
    }
    if text.trim().is_empty() {
        bail!("Responses returned empty summary");
    }
    Ok(ApecTextAttempt::Complete(text.trim().to_owned(), usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_message_items_keep_text_even_when_only_first_streamed() {
        let mut stream = Stream::default();
        let mut streamed = String::new();
        stream
            .apply(
                &json!({"type":"response.output_text.delta","output_index":0,
            "delta":"First "})
                .to_string(),
                &mut |s| streamed.push_str(s),
            )
            .unwrap();
        stream
            .apply(
                &json!({"type":"response.completed","response":{
                    "status":"completed", "output":[
                        {"type":"message","content":[{"type":"output_text","text":"First "}]},
                        {"type":"reasoning","id":"opaque","encrypted_content":"hidden"},
                        {"type":"message","content":[{"type":"output_text","text":"second"}]}
                    ],"usage":{"input_tokens":23,"output_tokens":4}
                }})
                .to_string(),
                &mut |s| streamed.push_str(s),
            )
            .unwrap();
        let (message, usage) = stream.finish().unwrap();
        assert_eq!(streamed, "First ");
        assert_eq!(message.content.as_deref(), Some("First second"));
        assert_eq!(usage.unwrap().input, Some(23));
        assert_eq!(message.provider_items.as_ref().unwrap()[0]["id"], "opaque");
    }

    #[test]
    fn mixed_refusal_and_text_are_not_dropped_or_retried() {
        let mut stream = Stream::default();
        stream
            .apply(
                &json!({"type":"response.output_item.done","output_index":0,
                "item":{"type":"message","content":[
                    {"type":"output_text","text":"Unable to proceed: "},
                    {"type":"refusal","refusal":"request declined"}
                ]}})
                .to_string(),
                &mut |_| {},
            )
            .unwrap();
        stream
            .apply(
                &json!({"type":"response.completed","response":{
            "status":"completed"}})
                .to_string(),
                &mut |_| {},
            )
            .unwrap();
        let (message, _) = stream.finish().unwrap();
        assert_eq!(
            message.content.as_deref(),
            Some("Unable to proceed: request declined")
        );
    }

    #[test]
    fn unindexed_deltas_and_final_message_never_duplicate_answer() {
        let mut stream = Stream::default();
        stream
            .apply(
                &json!({"type":"response.output_text.delta","delta":"Hello"}).to_string(),
                &mut |_| {},
            )
            .unwrap();
        stream
            .apply(
                &json!({"type":"response.completed","response":{
                    "status":"completed","output":[{"type":"message","content":[
                        {"type":"output_text","text":"Hello"}]}]
                }})
                .to_string(),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(stream.finish().unwrap().0.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn responses_reject_non_object_tool_arguments() {
        let mut stream = Stream::default();
        let mut push = |value: Value| {
            stream.apply(&value.to_string(), &mut |_| {}).unwrap();
        };
        push(json!({"type":"response.output_item.added","output_index":0,
            "item":{"type":"function_call","call_id":"call_1","name":"shell","arguments":""}}));
        push(json!({"type":"response.output_item.done","output_index":0,
            "item":{"type":"function_call","call_id":"call_1","name":"shell","arguments":"[]"}}));
        push(json!({"type":"response.completed","response":{"status":"completed"}}));
        let error = stream
            .finish()
            .expect_err("array arguments must be refused");
        assert!(
            format!("{error:#}").contains("must be a JSON object"),
            "{error:#}"
        );
    }

    #[test]
    fn responses_reject_empty_call_id_or_name() {
        for (id, name) in [("", "shell"), ("call_1", "")] {
            let mut stream = Stream::default();
            let mut push = |value: Value| {
                stream.apply(&value.to_string(), &mut |_| {}).unwrap();
            };
            push(json!({"type":"response.output_item.added","output_index":0,
                "item":{"type":"function_call","call_id":id,"name":name,"arguments":""}}));
            push(json!({"type":"response.output_item.done","output_index":0,
                "item":{"type":"function_call","call_id":id,"name":name,"arguments":"{}"}}));
            push(json!({"type":"response.completed","response":{"status":"completed"}}));
            assert!(stream.finish().is_err(), "id={id:?} name={name:?}");
        }
    }

    #[test]
    fn round_trip_tool_and_encrypted_reasoning() {
        let mut s = Stream::default();
        let mut output = String::new();
        let mut push = |value: Value| {
            s.apply(&value.to_string(), &mut |t| output.push_str(t))
                .unwrap()
        };
        push(json!({"type":"response.output_item.added","output_index":0,
            "item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}}));
        push(json!({"type":"response.output_item.done","output_index":0,
            "item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}}));
        push(json!({"type":"response.output_item.added","output_index":1,
            "item":{"type":"function_call","call_id":"call_1","name":"shell","arguments":""}}));
        push(
            json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"command\":\"pwd\"}"}),
        );
        push(json!({"type":"response.output_item.done","output_index":1,
            "item":{"type":"function_call","call_id":"call_1","name":"shell","arguments":"{\"command\":\"pwd\"}"}}));
        push(
            json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":15,"output_tokens":3}}}),
        );
        let (message, usage) = s.finish().unwrap();
        assert!(output.is_empty());
        assert_eq!(usage.unwrap().input, Some(15));
        let history = vec![
            Message::new("user", "hello"),
            message,
            Message::tool("call_1".into(), "ok".into()),
        ];
        let request = payload("test", &history, &[], Some(512), "high", "test-origin").unwrap();
        assert_eq!(request["input"][1]["type"], "reasoning");
        assert_eq!(request["input"][2]["type"], "function_call");
        assert_eq!(request["input"][3]["type"], "function_call_output");
        assert_eq!(request["reasoning"]["effort"], "high");
        assert_eq!(request["store"], false);
    }

    #[test]
    fn only_visible_reasoning_summaries_stream_not_encrypted_items() {
        let mut stream = Stream::default();
        let mut answer = String::new();
        let summary = std::sync::Mutex::new(String::new());
        let mut push = |value: Value| {
            stream
                .apply_with_reasoning(
                    &value.to_string(),
                    &mut |text| answer.push_str(text),
                    &mut |text| summary.lock().unwrap().push_str(text),
                    &mut |_| {},
                )
                .unwrap();
        };
        push(json!({"type":"response.output_item.added","output_index":0,
            "item":{"type":"reasoning","id":"rs_1","encrypted_content":"secret"}}));
        assert!(summary.lock().unwrap().is_empty());
        push(json!({"type":"response.reasoning_summary_text.delta","delta":"Summary "}));
        assert_eq!(
            *summary.lock().unwrap(),
            "Summary ",
            "summary must arrive before response.completed"
        );
        push(json!({"type":"response.reasoning_summary_text.delta","delta":"only"}));
        push(json!({"type":"response.output_text.delta","delta":"Final"}));
        push(json!({"type":"response.completed","response":{"status":"completed"}}));
        let (message, _) = stream.finish().unwrap();
        assert_eq!(answer, "Final");
        assert_eq!(*summary.lock().unwrap(), "Summary only");
        assert_eq!(message.reasoning_content.as_deref(), Some("Summary only"));
        assert_eq!(
            message.provider_items.as_ref().unwrap()[0]["encrypted_content"],
            "secret"
        );
    }

    #[test]
    fn server_side_search_tool_is_forwarded_verbatim() {
        let request = payload_with_cache_at(
            "mock",
            &[Message::new("user", "search")],
            &[],
            Some(512),
            "high",
            "test-origin",
            None,
            None,
            &[json!({"type":"web_search"})],
        )
        .unwrap();
        assert_eq!(request["tools"][0], json!({"type":"web_search"}));
        assert_eq!(request["store"], false);
        assert_eq!(request["stream"], true);
        assert_eq!(request["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn server_side_search_citations_and_function_calls_stay_separate() {
        let mut stream = Stream::default();
        let mut text = String::new();
        let mut activities = Vec::new();
        let events = vec![
            json!({"type":"response.output_item.added","output_index":0,
                "item":{"id":"ws_1","type":"web_search_call","status":"in_progress"}}),
            json!({"type":"response.web_search_call.searching","item_id":"ws_1","output_index":0}),
            json!({"type":"response.output_item.done","output_index":0,
                "item":{"id":"ws_1","type":"web_search_call","status":"completed",
                    "action":{"type":"search","queries":["rust stable version"],
                        "query":"rust stable version"}}}),
            json!({"type":"response.output_item.added","output_index":1,
                "item":{"type":"function_call","call_id":"call_1","name":"shell","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","output_index":1,
                "delta":"{\"command\":\"pwd\"}"}),
            json!({"type":"response.output_text.delta","output_index":2,"delta":"Rust 1.99"}),
            json!({"type":"response.output_text.annotation.added","output_index":2,
                "annotation":{"type":"url_citation","start_index":0,"end_index":4,
                    "title":"Rust releases","url":"https://blog.rust-lang.org/releases/"}}),
            json!({"type":"response.completed","response":{"status":"completed",
            "tool_usage":{"web_search":{"num_requests":1}},
            "usage":{"input_tokens":120,"output_tokens":8},
            "output":[
                {"type":"web_search_call","id":"ws_1","status":"completed",
                 "action":{"type":"search","queries":["rust stable version"]}},
                {"type":"function_call","call_id":"call_1","name":"shell",
                 "arguments":"{\"command\":\"pwd\"}"},
                {"type":"message","content":[{"type":"output_text","text":"Rust 1.99",
                    "annotations":[{"type":"url_citation","start_index":0,"end_index":4,
                        "title":"Rust releases","url":"https://blog.rust-lang.org/releases/"}]}]}
            ]}}),
        ];
        for event in events {
            stream
                .apply_with_reasoning(
                    &event.to_string(),
                    &mut |delta| text.push_str(delta),
                    &mut |_| {},
                    &mut |activity| activities.push(activity),
                )
                .unwrap();
        }
        activities.extend(stream.drain_finished_native());
        let (message, usage) = stream.finish().unwrap();
        assert_eq!(text, "Rust 1.99");
        assert_eq!(message.content.as_deref(), Some("Rust 1.99"));
        let calls = message.tool_calls.unwrap_or_default();
        assert_eq!(calls.len(), 1, "a server-side call is never a local tool");
        assert_eq!(calls[0].function.name, "shell");
        let usage = usage.unwrap();
        assert_eq!(usage.web_searches, Some(1));
        assert_eq!(usage.input, Some(120));
        let items = message.provider_items.unwrap_or_default();
        let citation = items
            .iter()
            .find(|item| item["type"] == "url_citation")
            .expect("the cited source must stay with the turn");
        assert_eq!(citation["url"], "https://blog.rust-lang.org/releases/");
        assert_eq!(citation["title"], "Rust releases");
        assert_eq!(citation["start_index"], 0);
        assert_eq!(citation["end_index"], 4);

        assert_eq!(activities.len(), 2, "{activities:?}");
        assert!(!activities[0].done);
        assert_eq!(activities[0].name, "web_search");
        assert_eq!(
            activities[0].arguments,
            "{\"query\":\"rust stable version\"}"
        );
        assert!(activities[1].done);
        let result = activities[1].result.as_deref().unwrap_or("");
        assert!(result.contains("searched rust stable version"), "{result}");
        assert!(
            result.contains("https://blog.rust-lang.org/releases/"),
            "the source must reach the existing tool display: {result}"
        );
    }

    #[test]
    fn unknown_server_tool_items_never_break_or_become_tools() {
        let mut stream = Stream::default();
        let mut text = String::new();
        let events = vec![
            json!({"type":"response.output_item.added","output_index":0,
                "item":{"id":"ig_1","type":"image_generation_call","status":"in_progress"}}),
            json!({"type":"response.output_item.done","output_index":1,
                "item":{"id":"x_1","type":"mystery_gateway_tool"}}),
            json!({"type":"response.output_text.delta","output_index":2,"delta":"answer"}),
            json!({"type":"response.completed","response":{"status":"completed",
            "usage":{"input_tokens":5,"output_tokens":2},
            "output":[
                {"id":"ig_1","type":"image_generation_call","status":"completed"},
                {"id":"x_1","type":"mystery_gateway_tool"},
                {"type":"message","content":[{"type":"output_text","text":"answer"}]}
            ]}}),
        ];
        for event in events {
            stream
                .apply_with_reasoning(
                    &event.to_string(),
                    &mut |delta| text.push_str(delta),
                    &mut |_| {},
                    &mut |_| {},
                )
                .unwrap();
        }
        let (message, usage) = stream.finish().unwrap();
        assert_eq!(message.content.as_deref(), Some("answer"));
        assert!(message.tool_calls.is_none());
        assert!(message.provider_items.is_none());
        assert_eq!(usage.and_then(|u| u.web_searches), None);
    }

    #[test]
    fn cited_sources_persist_with_the_turn_but_are_never_replayed() {
        let mut answer = Message::new("assistant", "answer");
        answer.provider_items = Some(vec![
            json!({"type":"url_citation","url":"https://example.com/a","title":"A"}),
        ]);
        let history = vec![Message::new("user", "question"), answer];
        let request = payload("mock", &history, &[], None, "high", "test-origin").unwrap();
        assert_eq!(request["input"][1]["role"], "assistant");
        assert!(
            request["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "url_citation"),
            "stored citations must not be replayed upstream: {}",
            request["input"]
        );
        let restored: Message =
            serde_json::from_str(&serde_json::to_string(&history[1]).unwrap()).unwrap();
        assert_eq!(
            restored.provider_items.as_ref().unwrap()[0]["url"],
            "https://example.com/a"
        );
    }

    #[test]
    fn max_effort_is_not_downgraded() {
        let request = payload(
            "mock",
            &[Message::new("user", "test")],
            &[],
            None,
            "max",
            "test-origin",
        )
        .unwrap();
        assert_eq!(request["reasoning"]["effort"], "max");
        let auto = payload(
            "mock",
            &[Message::new("user", "test")],
            &[],
            None,
            "auto",
            "test-origin",
        )
        .unwrap();
        assert_eq!(auto["reasoning"]["summary"], "auto");
        assert!(auto["reasoning"].get("effort").is_none());
        assert_eq!(request["reasoning"]["summary"], "auto");
        let off = payload(
            "mock",
            &[Message::new("user", "test")],
            &[],
            None,
            "off",
            "test-origin",
        )
        .unwrap();
        assert_eq!(off["reasoning"]["effort"], "none");
        assert!(off["reasoning"].get("summary").is_none());
    }
}
