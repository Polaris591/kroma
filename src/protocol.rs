//! Provider-neutral conversation records, also used for durable transcripts.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Option<String>,
    // Thinking models can require this field in a subsequent tool round.
    // Keep it separate from text: never display it as the assistant answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Opaque provider continuation items (signed Claude thinking blocks or
    /// Responses reasoning objects). Persist them with the original turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_items: Option<Vec<Value>>,
}

impl Message {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            provider_items: None,
        }
    }

    pub fn tool(id: String, result: String) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(result),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(id),
            provider_items: None,
        }
    }

    /// Preserve the tool adapter's authoritative error bit through WAL,
    /// conversation branches, and provider switching. Legacy tool messages
    /// without metadata fall back to their structured result where possible.
    pub fn tool_with_status(id: String, result: String, failed: bool) -> Self {
        let mut message = Self::tool(id, result);
        message.provider_items = Some(vec![serde_json::json!({
            "type": "kroma_tool_status", "is_error": failed
        })]);
        message
    }

    pub fn tool_failed(&self) -> bool {
        if let Some(value) = self
            .provider_items
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .find(|item| item["type"] == "kroma_tool_status")
            .and_then(|item| item["is_error"].as_bool())
        {
            return value;
        }
        let Ok(value) = serde_json::from_str::<Value>(self.content.as_deref().unwrap_or("")) else {
            return false;
        };
        value.get("error").is_some()
            || value["isError"] == true
            || value
                .get("exit_code")
                .is_some_and(|code| code.as_i64() != Some(0))
    }
}

#[cfg(test)]
mod tool_status_tests {
    use super::*;

    #[test]
    fn status_is_durable_and_legacy_results_remain_readable() {
        let failed = Message::tool_with_status("t".into(), "plain-text failure".into(), true);
        let restored: Message =
            serde_json::from_str(&serde_json::to_string(&failed).unwrap()).unwrap();
        assert!(restored.tool_failed());
        assert!(
            !Message::tool_with_status("t".into(), "plain-text success".into(), false)
                .tool_failed()
        );
        assert!(Message::tool("t".into(), r#"{"exit_code":1}"#.into()).tool_failed());
        assert!(Message::tool("t".into(), r#"{"exit_code":null}"#.into()).tool_failed());
        assert!(!Message::tool("t".into(), r#"{"exit_code":0}"#.into()).tool_failed());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}
