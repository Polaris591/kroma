use harness_core::budget::BudgetConfig;
use harness_core::messages::FinishReason;
use harness_core::provider::sse::SseStreamAssembler;
use harness_core::provider::{validate_assistant_turn_batch, ProviderError};

const SAMPLE_SSE_STREAM: &str = "\
: keepalive comment\r\n\
\r\n\
data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Let me read \",\"signature\":\"sig_part1_\"}}]}\r\n\
\r\n\
data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"both files.\",\"signature\":\"sig_part2\",\"content\":\"Reading files...\"}}]}\n\
\n\
data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_alpha\",\"type\":\"function\",\"function\":{\"name\":\"re\",\"arguments\":\"{\\\"pa\"}}]}}]}\r\n\
\r\n\
data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"ad\",\"arguments\":\"th\\\":\\\"src/a.rs\\\"}\"}},{\"index\":1,\"id\":\"call_beta\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"src/b.rs\\\"}\"}}]}}]}\n\
\n\
data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\
\n\
data: {\"id\":\"chatcmpl-1\",\"choices\":[],\"usage\":{\"prompt_tokens\":1200,\"completion_tokens\":85,\"prompt_tokens_details\":{\"cached_tokens\":1024},\"completion_tokens_details\":{\"reasoning_tokens\":30}}}\n\
\n\
data: [DONE]\n\n";

#[test]
fn test_sse_arbitrary_chunking_100_iterations_and_1_byte_splits() {
    let raw_bytes = SAMPLE_SSE_STREAM.as_bytes();

    // 1. Extreme 1-byte-at-a-time chunking
    let mut one_byte_asm = SseStreamAssembler::new();
    for b in raw_bytes {
        one_byte_asm.feed_bytes( std::slice::from_ref(b) ).unwrap();
    }
    let baseline = one_byte_asm.finalize().unwrap();

    assert_eq!(baseline.content.as_deref(), Some("Reading files..."));
    assert_eq!(baseline.finish_reason, FinishReason::ToolCalls);
    assert_eq!(baseline.reasoning_blocks.len(), 1);
    assert_eq!(
        baseline.reasoning_blocks[0].text.as_deref(),
        Some("Let me read both files.")
    );
    assert_eq!(
        baseline.reasoning_blocks[0].signature.as_deref(),
        Some("sig_part1_sig_part2")
    );
    assert_eq!(baseline.tool_calls.len(), 2);
    assert_eq!(baseline.tool_calls[0].call_id.as_str(), "call_alpha");
    assert_eq!(baseline.tool_calls[0].name, "read");
    assert_eq!(baseline.tool_calls[0].raw_arguments, "{\"path\":\"src/a.rs\"}");
    assert_eq!(baseline.tool_calls[1].call_id.as_str(), "call_beta");
    assert_eq!(baseline.tool_calls[1].name, "read");
    assert_eq!(baseline.tool_calls[1].raw_arguments, "{\"path\":\"src/b.rs\"}");

    let usage = baseline.usage.expect("trailing choices=[] usage parsed");
    assert_eq!(usage.input_tokens, 1200);
    assert_eq!(usage.cached_input_tokens, Some(1024));
    assert_eq!(usage.output_tokens, 85);
    assert_eq!(usage.reasoning_tokens, Some(30));

    // 2. 100 deterministic pseudo-random chunk segmentations
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    for _iter in 0..100 {
        let mut asm = SseStreamAssembler::new();
        let mut pos = 0;
        while pos < raw_bytes.len() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let step = ((seed >> 32) as usize % 29) + 1;
            let end = (pos + step).min(raw_bytes.len());
            asm.feed_bytes(&raw_bytes[pos..end]).unwrap();
            pos = end;
        }
        let turn = asm.finalize().unwrap();
        assert_eq!(turn, baseline);
    }
}

#[test]
fn test_sse_missing_done_or_truncated_length_rejects_all_tools() {
    let budget = BudgetConfig::default();

    // Missing [DONE] (stream disconnect mid-flight)
    let disconnected = "\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"rm -rf /\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
    let mut asm = SseStreamAssembler::new();
    asm.feed_bytes(disconnected.as_bytes()).unwrap();
    let res = asm.finalize();
    assert!(matches!(res, Err(ProviderError::IncompleteStream(_))));

    // finish_reason=length even when tool arguments happen to be valid JSON
    let length_truncated = "\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\",\\\"content\\\":\\\"ok\\\"}\"}}]},\"finish_reason\":\"length\"}]}\n\n\
data: [DONE]\n\n";
    let mut asm2 = SseStreamAssembler::new();
    asm2.feed_bytes(length_truncated.as_bytes()).unwrap();
    let turn = asm2.finalize().unwrap();
    assert_eq!(turn.finish_reason, FinishReason::Length);

    let batch_res = validate_assistant_turn_batch(&turn, &budget, false);
    assert!(matches!(batch_res, Err(ProviderError::TruncatedToolCall(_))));
}

#[test]
fn test_pre_dispatch_batch_rejects_empty_or_duplicate_call_id() {
    let budget = BudgetConfig::default();

    let dup_sse = "\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"dup_id\",\"function\":{\"name\":\"ls\",\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"dup_id\",\"function\":{\"name\":\"ls\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";
    let mut asm = SseStreamAssembler::new();
    asm.feed_bytes(dup_sse.as_bytes()).unwrap();
    let turn = asm.finalize().unwrap();
    let res = validate_assistant_turn_batch(&turn, &budget, false);
    assert!(matches!(res, Err(ProviderError::InvalidToolBatch(_))));

    let empty_id_sse = "\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"   \",\"function\":{\"name\":\"ls\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";
    let mut asm2 = SseStreamAssembler::new();
    asm2.feed_bytes(empty_id_sse.as_bytes()).unwrap();
    let turn2 = asm2.finalize().unwrap();
    let res2 = validate_assistant_turn_batch(&turn2, &budget, false);
    assert!(matches!(res2, Err(ProviderError::InvalidToolBatch(_))));
}

#[test]
fn test_sse_multi_block_reasoning_and_indexless_tool_calls_and_unterminated_done() {
    // 1. Multiple reasoning blocks preserve distinct signatures
    //    and tool_calls missing `index` but carrying new `id` allocate distinct slots.
    let sse = "\
data: {\"choices\":[{\"index\":0,\"delta\":{\"thinking\":\"First thought\",\"signature\":\"sig_one\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"thinking\":\"Second thought\",\"signature\":\"sig_two\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"call_a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}},{\"id\":\"call_b\",\"function\":{\"name\":\"edit\",\"arguments\":\"{\\\"path\\\":\\\"b.rs\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";
    let mut asm = SseStreamAssembler::new();
    asm.feed_bytes(sse.as_bytes()).unwrap();
    let turn = asm.finalize().unwrap();

    assert_eq!(turn.reasoning_blocks.len(), 2);
    assert_eq!(turn.reasoning_blocks[0].text.as_deref(), Some("First thought"));
    assert_eq!(turn.reasoning_blocks[0].signature.as_deref(), Some("sig_one"));
    assert_eq!(turn.reasoning_blocks[1].text.as_deref(), Some("Second thought"));
    assert_eq!(turn.reasoning_blocks[1].signature.as_deref(), Some("sig_two"));

    assert_eq!(turn.tool_calls.len(), 2);
    assert_eq!(turn.tool_calls[0].call_id.as_str(), "call_a");
    assert_eq!(turn.tool_calls[0].name, "read");
    assert_eq!(turn.tool_calls[1].call_id.as_str(), "call_b");
    assert_eq!(turn.tool_calls[1].name, "edit");

    // 2. Unterminated `data: [DONE]` without trailing newline is rejected by finalize()
    let unterminated = "\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]";
    let mut asm_unterm = SseStreamAssembler::new();
    asm_unterm.feed_bytes(unterminated.as_bytes()).unwrap();
    assert!(matches!(
        asm_unterm.finalize(),
        Err(ProviderError::IncompleteStream(_))
    ));
}
