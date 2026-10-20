use kroma_core::messages::{
    AssistantTurn, FinishReason, ImageAttachment, ReasoningBlock, ToolCall, ToolOutcome,
    ToolResultMessage, ToolResultMeta, TranscriptItem, UserMessage,
};
use kroma_core::provider::ProviderRequest;
use kroma_core::request_observation::RequestObserver;
use kroma_core::tool::definition::ToolDefinition;
use kroma_core::types::ToolCallId;
use std::sync::Arc;

fn user(text: &str) -> TranscriptItem {
    TranscriptItem::User(UserMessage {
        content: text.into(),
    })
}

fn fixture() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "SYSTEM_SECRET".into(),
        tools: vec![
            ToolDefinition {
                name: "z".into(),
                description: "last".into(),
                parameters: serde_json::json!({}),
            },
            ToolDefinition {
                name: "a".into(),
                description: "first".into(),
                parameters: serde_json::json!({}),
            },
        ],
        messages: vec![
            user("你好"),
            TranscriptItem::Assistant(AssistantTurn {
                content: Some("ANSWER_SECRET".into()),
                reasoning_blocks: vec![ReasoningBlock {
                    text: Some("THOUGHT_SECRET".into()),
                    signature: Some("SIGNATURE_SECRET".into()),
                    opaque_payload: Some("OPAQUE_SECRET".into()),
                }],
                tool_calls: vec![ToolCall {
                    call_id: ToolCallId::new("call-1"),
                    name: "read".into(),
                    raw_arguments: "ARGUMENT_SECRET".into(),
                }],
                finish_reason: FinishReason::ToolCalls,
                usage: None,
            }),
            TranscriptItem::ToolResult(ToolResultMessage {
                call_id: ToolCallId::new("call-1"),
                tool_name: "read".into(),
                content: Arc::from("RESULT_SECRET"),
                outcome: ToolOutcome::Success,
                meta: ToolResultMeta {
                    image_attachments: vec![ImageAttachment {
                        mime_type: "image/png".into(),
                        base64_data: Arc::from("IMAGE_SECRET"),
                        width: None,
                        height: None,
                        byte_len: 9,
                    }],
                    ..Default::default()
                },
            }),
        ],
        ..Default::default()
    }
}

#[test]
fn partitions_utf8_payload_without_persisting_content() {
    let mut observer = RequestObserver::default();
    let request = fixture();
    let original = serde_json::to_string(&request.messages).unwrap();
    let result = observer.observe(&request);
    assert_eq!(result.user_utf8_bytes, 6);
    assert_eq!(
        result.assistant_reasoning_utf8_bytes,
        "THOUGHT_SECRET".len()
    );
    assert_eq!(
        result.reasoning_signature_utf8_bytes,
        "SIGNATURE_SECRET".len()
    );
    assert_eq!(result.reasoning_opaque_utf8_bytes, "OPAQUE_SECRET".len());
    assert_eq!(result.tool_argument_utf8_bytes, "ARGUMENT_SECRET".len());
    assert_eq!(result.tool_result_utf8_bytes, "RESULT_SECRET".len());
    assert_eq!(result.image_base64_bytes, "IMAGE_SECRET".len());
    assert_eq!(result.tool_call_count, 1);
    assert_eq!(result.tool_result_count, 1);
    let json = serde_json::to_value(&result).unwrap();
    let total: u64 = [
        "system_utf8_bytes",
        "logical_tool_schema_json_bytes",
        "user_utf8_bytes",
        "assistant_text_utf8_bytes",
        "assistant_reasoning_utf8_bytes",
        "reasoning_signature_utf8_bytes",
        "reasoning_opaque_utf8_bytes",
        "tool_argument_utf8_bytes",
        "tool_result_utf8_bytes",
        "image_base64_bytes",
    ]
    .iter()
    .map(|key| json[key].as_u64().unwrap())
    .sum();
    assert_eq!(total, result.payload_bytes as u64);
    assert!(!json.to_string().contains("SECRET"));
    assert_eq!(result.system_unchanged, None);
    assert_eq!(result.matching_history_prefix_messages, None);
    assert_eq!(original, serde_json::to_string(&request.messages).unwrap());
}

#[test]
fn append_only_prefix_and_tool_reordering_are_stable() {
    let mut observer = RequestObserver::default();
    let mut request = fixture();
    observer.observe(&request);
    request.tools.reverse();
    request.messages.push(user("new input"));
    let result = observer.observe(&request);
    assert_eq!(result.system_unchanged, Some(true));
    assert_eq!(result.tools_unchanged, Some(true));
    assert_eq!(result.matching_history_prefix_messages, Some(3));
}

#[test]
fn detects_in_place_rewrite_tool_change_and_system_change() {
    let mut observer = RequestObserver::default();
    let mut request = fixture();
    observer.observe(&request);
    request.system_prompt.push('!');
    request.tools[0].description.push('!');
    request.messages[0] = user("rewritten");
    let result = observer.observe(&request);
    assert_eq!(result.system_unchanged, Some(false));
    assert_eq!(result.tools_unchanged, Some(false));
    assert_eq!(result.matching_history_prefix_messages, Some(0));
}

#[test]
fn host_metadata_and_usage_do_not_invalidate_model_visible_prefix() {
    let mut observer = RequestObserver::default();
    let mut request = fixture();
    observer.observe(&request);
    if let TranscriptItem::Assistant(turn) = &mut request.messages[1] {
        turn.usage = Some(Default::default());
    }
    if let TranscriptItem::ToolResult(result) = &mut request.messages[2] {
        result.meta.duration_ms = 999;
        result.meta.relative_path = Some("HOST_SECRET".into());
    }
    let result = observer.observe(&request);
    assert_eq!(result.matching_history_prefix_messages, Some(3));
}

#[test]
fn image_and_reasoning_changes_invalidate_the_correct_prefix() {
    let mut observer = RequestObserver::default();
    let mut request = fixture();
    observer.observe(&request);
    if let TranscriptItem::ToolResult(result) = &mut request.messages[2] {
        result.meta.image_attachments[0].base64_data = Arc::from("CHANGED");
    }
    assert_eq!(
        observer.observe(&request).matching_history_prefix_messages,
        Some(2)
    );
    if let TranscriptItem::Assistant(turn) = &mut request.messages[1] {
        turn.reasoning_blocks[0].text = Some("CHANGED".into());
    }
    assert_eq!(
        observer.observe(&request).matching_history_prefix_messages,
        Some(1)
    );
}

#[test]
fn empty_history_and_new_observer_do_not_claim_provider_cache_hits() {
    let request = ProviderRequest::default();
    let mut observer = RequestObserver::default();
    assert_eq!(
        observer.observe(&request).matching_history_prefix_messages,
        None
    );
    assert_eq!(
        observer.observe(&request).matching_history_prefix_messages,
        Some(0)
    );
    assert_eq!(
        RequestObserver::default()
            .observe(&request)
            .system_unchanged,
        None
    );
}

#[test]
fn candidate_prompt_is_opt_in_and_preserves_default_and_verification() {
    use kroma_core::prompt::{PromptBuilder, DEFAULT_SYSTEM_PROMPT};
    assert_eq!(
        PromptBuilder::default().system_prompt(),
        DEFAULT_SYSTEM_PROMPT
    );
    let candidate = PromptBuilder::efficient_candidate();
    assert_ne!(candidate.system_prompt(), DEFAULT_SYSTEM_PROMPT);
    assert!(candidate.system_prompt().contains("not as a prerequisite"));
    assert!(candidate.system_prompt().contains("Run relevant tests"));
    assert!(candidate.system_prompt().contains("never weaken tests"));
    assert!(candidate.system_prompt().contains("required fingerprints"));
}
