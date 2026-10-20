//! Request-only repeated-read elision. Disk execution, policy hooks and canonical WAL stay intact.
//! A reference is emitted only while an identical full result is present in this SAME request.
use crate::messages::TranscriptItem;
use crate::prompt::sha256_hex64;
use crate::tools::read::{ReadArgs, MAX_READ_LINES};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReadDedupStats {
    pub elided_results: usize,
    pub saved_utf8_bytes: usize,
}

#[derive(Hash, PartialEq, Eq)]
struct ReadIdentity {
    path: String,
    offset: usize,
    limit: usize,
    output_sha256: String,
}

/// O(history bytes), no filesystem access and no canonical/projector mutation.
/// Hashing indexes candidates; exact byte comparison is authoritative (not the hash).
pub fn elide_repeated_reads(messages: &mut [TranscriptItem]) -> ReadDedupStats {
    let mut calls = HashMap::new();
    let mut sources: HashMap<ReadIdentity, (String, Arc<str>)> = HashMap::new();
    let mut stats = ReadDedupStats::default();
    for message in messages {
        match message {
            TranscriptItem::Assistant(turn) => {
                for call in &turn.tool_calls {
                    if call.name == "read" {
                        if let Ok(args) = serde_json::from_str::<ReadArgs>(&call.raw_arguments) {
                            calls.insert(call.call_id.as_str().to_owned(), args);
                        }
                    }
                }
            }
            TranscriptItem::ToolResult(result) if result.tool_name == "read" => {
                let Some(args) = calls.remove(result.call_id.as_str()) else {
                    continue;
                };
                // Failed/truncated/image/archived output must never advertise reusable full text.
                if !result.outcome.is_success()
                    || result.meta.truncated
                    || !result.meta.image_attachments.is_empty()
                    || result.meta.compaction.is_some()
                {
                    continue;
                }
                let (Some(path), Some(_fingerprint), Some(_range)) = (
                    result.meta.relative_path.as_ref(),
                    result.meta.content_sha256.as_ref(),
                    result.meta.lines_range,
                ) else {
                    continue;
                };
                let key = ReadIdentity {
                    path: path.clone(),
                    offset: args.offset.unwrap_or(1),
                    limit: args.limit.unwrap_or(MAX_READ_LINES).min(MAX_READ_LINES),
                    output_sha256: sha256_hex64(result.content.as_bytes()),
                };
                if !args.force.unwrap_or(false) {
                    if let Some((source_call, source_text)) = sources.get(&key) {
                        if source_text.as_ref() == result.content.as_ref() {
                            let source =
                                serde_json::to_string(source_call).expect("call ID serializes");
                            let note = format!("[Repeated read: identical text and range already present in read tool_call_id={source}. Reuse that full result above; read(force=true) returns the body again.]\n");
                            if note.len() < result.content.len() {
                                stats.elided_results += 1;
                                stats.saved_utf8_bytes += result.content.len() - note.len();
                                result.content = Arc::from(note);
                                continue;
                            }
                        }
                    }
                }
                // Keep the FIRST full result as the anchor; never form reference chains.
                sources.entry(key).or_insert_with(|| {
                    (
                        result.call_id.as_str().to_owned(),
                        Arc::clone(&result.content),
                    )
                });
            }
            _ => {}
        }
    }
    stats
}
