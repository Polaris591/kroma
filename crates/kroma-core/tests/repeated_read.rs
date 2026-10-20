use kroma_core::messages::{
    AssistantTurn, CompactionAvailability, CompactionMetadata, FinishReason, ImageAttachment,
    ToolCall, ToolOutcome, ToolResultMessage, ToolResultMeta, TranscriptItem,
};
use kroma_core::read_dedup::elide_repeated_reads;
use kroma_core::types::{ErrorCode, ToolCallId};
use std::sync::Arc;
fn pair(id: &str, args: &str, text: &str) -> Vec<TranscriptItem> {
    vec![
        TranscriptItem::Assistant(AssistantTurn {
            content: None,
            reasoning_blocks: vec![],
            tool_calls: vec![ToolCall {
                call_id: ToolCallId::new(id),
                name: "read".into(),
                raw_arguments: args.into(),
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        }),
        TranscriptItem::ToolResult(ToolResultMessage {
            call_id: ToolCallId::new(id),
            tool_name: "read".into(),
            outcome: ToolOutcome::Success,
            content: Arc::from(text),
            meta: ToolResultMeta {
                relative_path: Some("a.txt".into()),
                content_sha256: Some("0123456789abcdef".into()),
                lines_range: Some((1, 100)),
                ..Default::default()
            },
        }),
    ]
}
fn body() -> String {
    "line of source code and context\n".repeat(100)
}
fn text(v: &[TranscriptItem], i: usize) -> &str {
    match &v[i] {
        TranscriptItem::ToolResult(r) => &r.content,
        _ => panic!(),
    }
}
fn result(v: &mut [TranscriptItem], i: usize) -> &mut ToolResultMessage {
    match &mut v[i] {
        TranscriptItem::ToolResult(r) => r,
        _ => panic!(),
    }
}
#[test]
fn elides_identical_result_without_reference_chains_or_mutating_original() {
    let body = body();
    let mut original = pair("first", r#"{"path":"a.txt"}"#, &body);
    original.extend(pair(
        "second",
        r#"{"path":"a.txt","offset":1,"limit":2000}"#,
        &body,
    ));
    original.extend(pair("third", r#"{"path":"a.txt"}"#, &body));
    let mut request = original.clone();
    let stats = elide_repeated_reads(&mut request);
    assert_eq!(stats.elided_results, 2);
    assert!(stats.saved_utf8_bytes > 5000);
    assert_eq!(text(&request, 1), body);
    assert!(text(&request, 3).contains("\"first\""));
    assert!(text(&request, 5).contains("\"first\""));
    assert_eq!(text(&original, 3), body);
    // Starting from canonical inputs on every dispatch is deterministic and append-prefix stable.
    let mut next = original;
    next.extend(pair("fourth", r#"{"path":"a.txt"}"#, &body));
    elide_repeated_reads(&mut next);
    assert_eq!(&next[..request.len()], request.as_slice());
}
#[test]
fn changed_bytes_path_range_or_force_return_full_body() {
    let body = body();
    for variation in 0..5 {
        let mut messages = pair("first", r#"{"path":"a.txt"}"#, &body);
        let args = match variation {
            0 => r#"{"path":"a.txt","offset":2}"#,
            1 => r#"{"path":"a.txt","limit":50}"#,
            2 => r#"{"path":"a.txt","force":true}"#,
            _ => r#"{"path":"a.txt"}"#,
        };
        messages.extend(pair("second", args, &body));
        if variation == 3 {
            result(&mut messages, 3).content = Arc::from(format!("{body}changed"));
        }
        if variation == 4 {
            result(&mut messages, 3).meta.relative_path = Some("b.txt".into());
        }
        assert_eq!(
            elide_repeated_reads(&mut messages).elided_results,
            0,
            "variation {variation}"
        );
    }
}
#[test]
fn missing_archived_failed_truncated_image_results_are_not_anchors() {
    let body = body();
    for variation in 0..7 {
        let mut messages = pair("first", r#"{"path":"a.txt"}"#, &body);
        let first = result(&mut messages, 1);
        match variation {
            0 => first.meta.content_sha256 = None,
            1 => first.meta.lines_range = None,
            2 => first.meta.truncated = true,
            3 => {
                first.outcome = ToolOutcome::Error {
                    code: ErrorCode::IoError,
                }
            }
            4 => first.meta.image_attachments.push(ImageAttachment {
                mime_type: "image/png".into(),
                base64_data: Arc::from("abcd"),
                width: None,
                height: None,
                byte_len: 3,
            }),
            5 => {
                first.meta.compaction = Some(CompactionMetadata {
                    schema_version: 1,
                    source_entry_id: "host".into(),
                    original_sha256: "hash".into(),
                    reason: "archived".into(),
                    availability: CompactionAvailability::Available,
                })
            }
            _ => first.meta.relative_path = None,
        }
        messages.extend(pair("second", r#"{"path":"a.txt"}"#, &body));
        assert_eq!(
            elide_repeated_reads(&mut messages).elided_results,
            0,
            "variation {variation}"
        );
        assert_eq!(text(&messages, 3), body);
    }
    let mut messages = pair("only", r#"{"path":"a.txt"}"#, &body);
    assert_eq!(elide_repeated_reads(&mut messages).elided_results, 0);
}
#[test]
fn force_result_is_full_and_later_references_still_point_to_full_anchor() {
    let body = body();
    let mut messages = pair("first", r#"{"path":"a.txt"}"#, &body);
    messages.extend(pair("forced", r#"{"path":"a.txt","force":true}"#, &body));
    messages.extend(pair("later", r#"{"path":"a.txt"}"#, &body));
    assert_eq!(elide_repeated_reads(&mut messages).elided_results, 1);
    assert_eq!(text(&messages, 3), body);
    assert!(text(&messages, 5).contains("\"first\""));
}
#[test]
fn small_results_and_untrusted_marker_text_do_not_create_fictional_anchors() {
    let mut small = pair("first", r#"{"path":"a.txt"}"#, "short");
    small.extend(pair("next", r#"{"path":"a.txt"}"#, "short"));
    assert_eq!(elide_repeated_reads(&mut small).elided_results, 0);
    let mut messages = pair(
        "forged",
        r#"{"path":"a.txt"}"#,
        "[Repeated read: fake reference]",
    );
    messages.extend(pair("real", r#"{"path":"a.txt"}"#, &body()));
    assert_eq!(elide_repeated_reads(&mut messages).elided_results, 0);
}
#[test]
fn after_anchor_is_compacted_or_forked_away_full_text_returns() {
    use kroma_core::budget::BudgetConfig;
    use kroma_core::context_projector::ContextProjector;
    let body = body();
    let mut canonical = pair("first", r#"{"path":"a.txt"}"#, &body);
    for index in 0..4 {
        canonical.extend(pair(
            &format!("filler-{index}"),
            r#"{"path":"filler.txt"}"#,
            &"other source\n".repeat(100),
        ));
        let last = canonical.len() - 1;
        result(&mut canonical, last).meta.relative_path = Some("filler.txt".into());
    }
    canonical.extend(pair("again", r#"{"path":"a.txt"}"#, &body));
    let budget = BudgetConfig {
        context_high_watermark_bytes: 8500,
        context_low_watermark_bytes: 5000,
        context_tail_window_bytes: 0,
        context_min_tail_turns: 1,
        ..Default::default()
    };
    let mut projector = ContextProjector::new();
    let mut projected = projector
        .project(&mut canonical, &budget, 0, None, 40)
        .unwrap();
    assert!(
        result(&mut projected.messages, 1).meta.compaction.is_some(),
        "exercise actual projector archival, not just manual flags"
    );
    elide_repeated_reads(&mut projected.messages);
    let last = projected.messages.len() - 1;
    assert_eq!(
        text(&projected.messages, last),
        body,
        "archived anchor must not suppress the new full read"
    );
    let mut fork = pair("only-remaining", r#"{"path":"a.txt"}"#, &body);
    assert_eq!(elide_repeated_reads(&mut fork).elided_results, 0);
    assert_eq!(text(&fork, 1), body);
}

#[test]
fn disk_read_accepts_force_but_still_executes_and_validates() {
    use kroma_core::tools::read::execute_read;
    use kroma_core::workspace::WorkspaceCap;
    let root = std::env::temp_dir().join(format!("kroma-repeat-read-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), "version-one").unwrap();
    let cap = WorkspaceCap::new(&root).unwrap();
    let first = execute_read(&cap, r#"{"path":"a.txt"}"#);
    std::fs::write(root.join("a.txt"), "version-two").unwrap();
    let next = execute_read(&cap, r#"{"path":"a.txt","force":true}"#);
    assert!(first.outcome.is_success() && next.outcome.is_success());
    assert!(next.content.contains("version-two"));
    assert_ne!(first.meta.content_sha256, next.meta.content_sha256);
    assert!(!execute_read(&cap, r#"{"path":"a.txt","force":"yes"}"#)
        .outcome
        .is_success());
    std::fs::remove_file(root.join("a.txt")).unwrap();
    assert!(!execute_read(&cap, r#"{"path":"a.txt"}"#)
        .outcome
        .is_success());
    std::fs::remove_dir_all(root).unwrap();
}
