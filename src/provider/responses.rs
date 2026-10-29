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
    payload_with_cache(model, messages, tools, max_output, effort, origin, None)
}

pub(super) fn payload_with_cache(
    model: &str,
    messages: &[Message],
    tools: &[Value],
    max_output: Option<usize>,
    effort: &str,
    origin: &str,
    image_cache: Option<&crate::model::TaskImageCache>,
) -> Result<Value> {
    let mut input = Vec::new();
    let active_turn = messages
        .iter()
        .rposition(|msg| msg.role == "user")
        .unwrap_or(0);
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
    let definitions = tools.iter().map(|tool| {
        let f = &tool["function"];
        Ok(json!({"type":"function","name":f["name"],"description":f["description"],"parameters":f["parameters"]}))
    }).collect::<Result<Vec<Value>>>()?;
    let mut request = json!({"model":model,"input":input,"tools":definitions,
        "stream":true,"store":false,"include":["reasoning.encrypted_content"]});
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

#[derive(Default)]
pub(super) struct Stream {
    items: BTreeMap<usize, Value>,
    text: String,
    reasoning_summary: String,
    usage: Option<Usage>,
    status: Option<String>,
    pub received_delta: bool,
    pub done: bool,
}

impl Stream {
    #[cfg(test)]
    pub fn apply(&mut self, data: &str, on_text: &mut (impl FnMut(&str) + Send)) -> Result<()> {
        self.apply_with_reasoning(data, on_text, &mut |_| {})
    }

    pub fn apply_with_reasoning(
        &mut self,
        data: &str,
        on_text: &mut (impl FnMut(&str) + Send),
        on_reasoning: &mut (impl FnMut(&str) + Send),
    ) -> Result<()> {
        if data == "[DONE]" {
            return Ok(());
        }
        let v: Value = serde_json::from_str(data).context("invalid Responses SSE event")?;
        match v["type"].as_str().unwrap_or("") {
            "response.output_item.added" => {
                if let Some(index) = v["output_index"].as_u64() {
                    self.items.insert(index as usize, v["item"].clone());
                }
            }
            "response.output_item.done" => {
                if let Some(index) = v["output_index"].as_u64() {
                    self.items.insert(index as usize, v["item"].clone());
                }
            }
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
                self.done = true;
            }
            "error" => bail!("Responses stream error: {v}"),
            _ => {}
        }
        Ok(())
    }
    pub fn finish(self) -> Result<(Message, Option<Usage>)> {
        if !self.done || self.status.as_deref() != Some("completed") {
            bail!("Responses stream was not completed: {:?}", self.status);
        }
        let mut calls = Vec::new();
        let mut opaque = Vec::new();
        let mut content = self.text;
        let mut seen = HashSet::new();
        for item in self.items.into_values() {
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
                    let _: Value = serde_json::from_str(&arguments)
                        .context("incomplete Responses arguments")?;
                    if !seen.insert(id.clone()) {
                        bail!("duplicate Responses call ID");
                    }
                    calls.push(ToolCall {
                        id,
                        kind: "function".into(),
                        function: FunctionCall { name, arguments },
                    });
                }
                "reasoning" => opaque.push(item),
                "message" if content.is_empty() => {
                    for part in item["content"].as_array().into_iter().flatten() {
                        if part["type"] == "output_text" {
                            content.push_str(part["text"].as_str().unwrap_or(""));
                        }
                    }
                }
                _ => {}
            }
        }
        if calls.is_empty() && content.trim().is_empty() {
            bail!("Responses returned neither text nor tools");
        }
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
