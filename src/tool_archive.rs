//! First-ingestion projection for large tool results returned by MCP servers
//! and plugins.
//!
//! A tool result is authoritative evidence, so this module never trims it. It
//! writes the exact bytes to one bounded file beside the session archive and
//! hands the model a preview that names the file, keeps every block name and
//! size, and quotes diagnostic lines from the omitted middle. The same shell
//! diagnostic helpers are reused, and no new storage format is introduced.
//!
//! Two rules are non-negotiable:
//! * a failed archive keeps the original result intact (`Inline`), because the
//!   caller must not lose the only copy of the output;
//! * nothing here can cause a side effect to run again. The remote call already
//!   finished; archival only decides how its result is shown to the model.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// A result at or below this size and line count is sent unchanged. Matches the
/// Shell capture threshold so one policy explains every large tool output.
pub(crate) const INLINE_RESULT_BYTES: usize = 64 * 1024;
pub(crate) const INLINE_RESULT_LINES: usize = 2_000;
/// Characters of each text block kept in the preview (head and tail each).
const EDGE_CHARS: usize = 1_200;
/// Character budget for a structured value copied into the preview.
const STRUCTURED_PREVIEW_CHARS: usize = 2_000;
const MAX_BLOCKS: usize = 12;
/// Bounded diagnostic lines quoted from the omitted middle.
const MAX_DIAGNOSTIC_LINES: usize = 16;

static ARCHIVE_ID: AtomicU64 = AtomicU64::new(0);

/// What the caller should hand to the model.
pub(crate) enum OutputProjection {
    /// Small enough to send unchanged.
    Inline,
    /// Authoritative bytes archived; this text replaces the model-facing result.
    Archived(String),
    /// Above the threshold but not archived. The caller keeps the original
    /// output and states the failure instead of pretending a preview is whole.
    ArchiveFailed(String),
}

/// Project one tool result at first ingestion.
///
/// `archive` is the session archive path; the file is written next to it and is
/// named `<session-id>.tool-output-…`, so `kroma sessions prune` deletes it with
/// the conversation it belongs to. The model-facing transcript keeps only the
/// preview and the path, so the archive must outlive the session only as long as
/// the session itself does. `label` only prefixes the file name and never
/// carries content.
pub(crate) fn project_large_output(
    archive: &std::path::Path,
    label: &str,
    content: &str,
    is_error: bool,
) -> OutputProjection {
    if content.len() <= INLINE_RESULT_BYTES && content.lines().count() <= INLINE_RESULT_LINES {
        return OutputProjection::Inline;
    }
    match archive_bytes(archive, label, content) {
        Ok(path) => {
            let preview = render_preview(&path, content, is_error);
            crate::runtime_log!(
                "[tool-output] archived {} bytes at {}; sending a bounded preview to the model",
                content.len(),
                path.display()
            );
            OutputProjection::Archived(preview)
        }
        Err(error) => {
            let message = format!(
                "Kroma could not archive this large tool result: {error:#}. The complete result is sent unchanged below; do not rerun the call to obtain it."
            );
            crate::runtime_log!("[tool-output] {message}");
            OutputProjection::ArchiveFailed(message)
        }
    }
}

/// Keep the full post-hook result in the WAL; only request projections use the
/// bounded preview. A failed archive must not change the external outcome.
pub(crate) fn archive_message(
    message: &mut crate::protocol::Message,
    archive: &std::path::Path,
    label: &str,
) {
    let Some(content) = message.content.as_deref() else {
        return;
    };
    let item = match project_large_output(archive, label, content, message.tool_failed()) {
        OutputProjection::Archived(preview) => {
            json!({"type": "kroma_tool_projection", "preview": preview})
        }
        OutputProjection::ArchiveFailed(warning) => {
            json!({"type": "kroma_tool_warning", "message": warning})
        }
        OutputProjection::Inline => return,
    };
    message
        .provider_items
        .get_or_insert_with(Vec::new)
        .push(item);
}

pub(crate) fn model_content(message: &crate::protocol::Message) -> &str {
    message
        .provider_items
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|part| part["type"] == "kroma_tool_projection")
        .and_then(|part| part["preview"].as_str())
        .unwrap_or_else(|| message.content.as_deref().unwrap_or(""))
}

pub(crate) fn project_message(message: &mut crate::protocol::Message) {
    if message.role == "tool" {
        let content = model_content(message);
        if Some(content) != message.content.as_deref() {
            message.content = Some(content.to_owned());
        }
    }
}

/// Fork/rewind must give each branch its own immutable evidence link. Also
/// handles the preview-only messages saved by older builds.
pub(crate) fn clone_references(
    source: &std::path::Path,
    destination: &std::path::Path,
    messages: &mut [crate::protocol::Message],
) -> Result<Vec<(String, String)>> {
    let prefix = format!(
        "{}.tool-output-",
        source.file_stem().unwrap_or_default().to_string_lossy()
    );
    let target = format!(
        "{}.tool-output-",
        destination
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
    );
    let mut paths = Vec::new();
    for message in messages.iter_mut().filter(|message| message.role == "tool") {
        let mut copy_preview = |text: &str| -> Result<String> {
            let Ok(mut value) = serde_json::from_str::<Value>(text) else {
                return Ok(text.to_owned());
            };
            if value.get("kroma_tool_output_archive").is_none() {
                return Ok(text.to_owned());
            }
            let Some(original) = value["archive_file"].as_str() else {
                return Ok(text.to_owned());
            };
            let old = std::path::Path::new(original);
            let Some(suffix) = old
                .parent()
                .filter(|parent| Some(*parent) == source.parent())
                .and_then(|_| old.file_name())
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix(&prefix))
            else {
                return Ok(text.to_owned());
            };
            let new = destination
                .parent()
                .context("branch has no parent")?
                .join(format!("{target}{suffix}"));
            if !new.exists() {
                fs::hard_link(old, &new)
                    .or_else(|_| fs::copy(old, &new).map(|_| ()))
                    .with_context(|| {
                        format!("unable to preserve tool evidence {}", old.display())
                    })?;
            }
            let pair = (original.to_owned(), new.to_string_lossy().into_owned());
            value["archive_file"] = json!(pair.1);
            paths.push(pair);
            Ok(value.to_string())
        };
        if let Some(parts) = &mut message.provider_items {
            for part in parts
                .iter_mut()
                .filter(|part| part["type"] == "kroma_tool_projection")
            {
                if let Some(text) = part["preview"].as_str() {
                    part["preview"] = json!(copy_preview(text)?);
                }
            }
        }
        if let Some(content) = &message.content {
            // A current message keeps raw evidence; only old preview envelopes
            // have this marker at the top level.
            message.content = Some(copy_preview(content)?);
        }
    }
    Ok(paths)
}

fn archive_path(archive: &std::path::Path, label: &str) -> Result<std::path::PathBuf> {
    let dir = archive
        .parent()
        .context("session archive has no parent directory")?;
    let owner = archive
        .file_stem()
        .context("session archive has no file stem")?
        .to_string_lossy();
    let label: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(32)
        .collect();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(dir.join(format!(
        "{owner}.tool-output-{}-{stamp}-{}-{}.txt",
        if label.is_empty() { "tool" } else { &label },
        std::process::id(),
        ARCHIVE_ID.fetch_add(1, Ordering::Relaxed)
    )))
}

fn archive_bytes(
    archive: &std::path::Path,
    label: &str,
    content: &str,
) -> Result<std::path::PathBuf> {
    let path = archive_path(archive, label)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .context("unable to create tool-output archive")?;
    use std::io::Write;
    file.write_all(content.as_bytes())
        .context("unable to write tool-output archive")?;
    // Commit evidence before a durable WAL entry can refer to it.
    file.sync_all()
        .context("unable to sync tool-output archive")?;
    #[cfg(unix)]
    fs::File::open(path.parent().context("archive has no parent")?)?.sync_all()?;
    Ok(path)
}

/// Head and tail of one text block, with an explicit middle notice carrying the
/// size of what was not shown.
fn preview_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= EDGE_CHARS * 2 {
        return text.to_owned();
    }
    let head: String = chars[..EDGE_CHARS].iter().collect();
    let tail: String = chars[chars.len() - EDGE_CHARS..].iter().collect();
    format!(
        "{head}\n[... {} characters omitted from this preview; PREVIEW ONLY, the archive holds the complete text ...]\n{tail}",
        chars.len() - EDGE_CHARS * 2
    )
}

fn render_preview(path: &std::path::Path, content: &str, is_error: bool) -> String {
    let mut blocks: Vec<Value> = Vec::new();
    let parsed: Option<Value> = serde_json::from_str(content).ok();
    let mut format = "text";
    let mut structured: Option<Value> = None;
    let mut omitted: Vec<Value> = Vec::new();
    // Diagnostics are read from the decoded text, not the escaped JSON line,
    // so a failure in the omitted middle is still quotable.
    let mut texts: Vec<String> = Vec::new();
    if let Some(Value::Object(object)) = &parsed {
        format = "json";
        for (key, value) in object {
            match key.as_str() {
                // Preserved verbatim below: neither is dropped or treated as a
                // duplicate of the other.
                "content" | "structuredContent" | "isError" => continue,
                _ => {}
            }
            if value.is_null() {
                continue;
            }
            if blocks.len() >= MAX_BLOCKS {
                omitted.push(json!({"key": key, "bytes": value.to_string().len()}));
                continue;
            }
            blocks.push(json!({
                "key": key,
                "bytes": value.to_string().len(),
                "value": if value.to_string().len() <= 200 {
                    value.clone()
                } else {
                    Value::Null
                },
            }));
        }
        if let Some(items) = object.get("content").and_then(Value::as_array) {
            for (index, item) in items.iter().enumerate() {
                if blocks.len() >= MAX_BLOCKS {
                    omitted.push(json!({
                        "content_index": index,
                        "type": item["type"].as_str().unwrap_or("unknown"),
                        "bytes": item.to_string().len(),
                    }));
                    continue;
                }
                let kind = item["type"].as_str().unwrap_or("unknown");
                match kind {
                    "text" => {
                        let text = item["text"].as_str().unwrap_or("");
                        texts.push(text.to_owned());
                        blocks.push(json!({
                            "content_index": index,
                            "type": "text",
                            "bytes": text.len(),
                            "text": preview_text(text),
                        }));
                    }
                    // Never invent equivalence: an image, embedded resource or
                    // unknown block is named and sized, not deleted.
                    other => blocks.push(json!({
                        "content_index": index,
                        "type": other,
                        "bytes": item.to_string().len(),
                        "note": "block type preserved as a name; content is in the archive",
                    })),
                }
            }
        }
        if let Some(value) = object.get("structuredContent") {
            let encoded = value.to_string();
            if encoded.len() <= STRUCTURED_PREVIEW_CHARS {
                structured = Some(value.clone());
            } else {
                // Named, sized, and explicitly not shown: never a silent drop.
                structured = Some(json!({
                    "kroma_structured_content_omitted": true,
                    "bytes": encoded.len(),
                    "note": "structuredContent is in the archive; it is not reproduced here"
                }));
            }
        }
    }
    let mut diagnostics: Vec<String> = Vec::new();
    if texts.is_empty() {
        texts.push(content.to_owned());
    }
    for text in &texts {
        if diagnostics.len() >= MAX_DIAGNOSTIC_LINES {
            break;
        }
        for line in crate::shell::diagnostic_lines(text, MAX_DIAGNOSTIC_LINES - diagnostics.len()) {
            if !diagnostics.contains(&line) {
                diagnostics.push(line);
            }
        }
    }
    let mut preview = json!({
        "kroma_tool_output_archive": "PREVIEW ONLY: the full result is in archive_file. This preview is not the complete evidence; read the archive in slices before concluding, and do not rerun the call to obtain it.",
        "archive_file": path.to_string_lossy(),
        "archive_bytes": content.len(),
        "archive_lines": content.lines().count(),
        "original_format": format,
        "is_error": is_error,
    });
    if let Value::Object(object) = &mut preview {
        object.insert("blocks".into(), Value::Array(blocks));
        if parsed.is_none() {
            // Not JSON: the archive is authoritative and the preview still
            // shows both ends of the same bytes.
            object.insert("text_preview".into(), json!(preview_text(content)));
        }
        if let Some(structured) = structured {
            object.insert("structured_content".into(), structured);
        }
        if !omitted.is_empty() {
            object.insert("blocks_not_shown".into(), Value::Array(omitted));
        }
        if !diagnostics.is_empty() {
            object.insert("diagnostic_lines".into(), json!(diagnostics));
        }
    }
    preview.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn session_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kroma-tool-archive-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join("session.json")
    }

    fn archived(projection: OutputProjection) -> String {
        match projection {
            OutputProjection::Archived(preview) => preview,
            _ => panic!("expected an archived preview"),
        }
    }

    #[test]
    fn small_results_are_sent_unchanged() {
        let archive = session_dir("small");
        assert!(matches!(
            project_large_output(&archive, "mcp", "{\"ok\":true}", false),
            OutputProjection::Inline
        ));
        fs::remove_dir_all(archive.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_large_result_is_archived_once_and_retrievable_byte_for_byte() {
        let archive = session_dir("retrieval");
        let body = json!({
            "content": [{"type": "text", "text": format!("start\n{}\nend", "x".repeat(200_000))}],
            "structuredContent": {"rows": 3},
            "isError": false
        })
        .to_string();
        let preview = archived(project_large_output(&archive, "mcp", &body, false));
        let value: Value = serde_json::from_str(&preview).unwrap();
        let path = PathBuf::from(value["archive_file"].as_str().unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            body,
            "the archive is the authoritative result, not a reconstruction"
        );
        assert!(value["archive_file"]
            .as_str()
            .unwrap()
            .contains(".tool-output-mcp"));
        assert_eq!(value["original_format"], "json");
        assert_eq!(value["is_error"], false);
        assert!(value["archive_bytes"].as_u64().unwrap() > 200_000);
        assert!(preview.len() < INLINE_RESULT_BYTES);
        fs::remove_dir_all(archive.parent().unwrap()).unwrap();
    }

    #[test]
    fn middle_evidence_and_block_names_survive_the_preview() {
        let archive = session_dir("middle");
        let mut lines = vec!["begin".to_owned()];
        lines.extend((0..4_000).map(|n| format!("filler {n}")));
        lines.push("error: the middle failure is here".into());
        lines.extend((0..4_000).map(|n| format!("tail {n}")));
        let body = json!({
            "content": [
                {"type": "text", "text": lines.join("\n")},
                {"type": "image", "data": "AAAA", "mimeType": "image/png"},
                {"type": "resource", "resource": {"uri": "file:///x"}}
            ],
            "isError": true
        })
        .to_string();
        let preview = archived(project_large_output(&archive, "mcp", &body, true));
        let value: Value = serde_json::from_str(&preview).unwrap();
        assert_eq!(value["is_error"], true);
        assert!(
            value["diagnostic_lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line
                    .as_str()
                    .unwrap_or("")
                    .contains("the middle failure is here")),
            "an error in the omitted middle must still be quoted"
        );
        let types: Vec<&str> = value["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["type"].as_str())
            .collect();
        assert!(types.contains(&"text"));
        assert!(
            types.contains(&"image") && types.contains(&"resource"),
            "non-text blocks are named, never silently dropped"
        );
        let text = value["blocks"][0]["text"].as_str().unwrap();
        assert!(text.contains("characters omitted from this preview"));
        assert!(text.contains("begin") && text.contains("tail 3999"));
        fs::remove_dir_all(archive.parent().unwrap()).unwrap();
    }

    #[test]
    fn large_structured_content_is_named_and_sized_not_dropped() {
        let archive = session_dir("structured");
        let body = json!({
            "content": [{"type": "text", "text": "ok"}],
            "structuredContent": {"blob": "y".repeat(80_000)},
        })
        .to_string();
        let preview = archived(project_large_output(&archive, "plugin", &body, false));
        let value: Value = serde_json::from_str(&preview).unwrap();
        assert_eq!(
            value["structured_content"]["kroma_structured_content_omitted"],
            true
        );
        assert!(
            value["structured_content"]["bytes"].as_u64().unwrap() > 50_000,
            "the preview states the real size of the omitted structured payload"
        );
        // A small structured payload is copied, because it is not a duplicate.
        let small = json!({
            "content": [{"type": "text", "text": "z".repeat(70_000)}],
            "structuredContent": {"rows": 2},
        })
        .to_string();
        let value: Value = serde_json::from_str(&archived(project_large_output(
            &archive, "plugin", &small, false,
        )))
        .unwrap();
        assert_eq!(value["structured_content"]["rows"], 2);
        fs::remove_dir_all(archive.parent().unwrap()).unwrap();
    }

    #[test]
    fn repeated_rounds_archive_separately_and_never_overwrite() {
        let archive = session_dir("repeat");
        let body = format!("repeated evidence {}", "r".repeat(70_000));
        let first: Value = serde_json::from_str(&archived(project_large_output(
            &archive, "plugin", &body, false,
        )))
        .unwrap();
        let second: Value = serde_json::from_str(&archived(project_large_output(
            &archive, "plugin", &body, false,
        )))
        .unwrap();
        assert_ne!(first["archive_file"], second["archive_file"]);
        let first_path = PathBuf::from(first["archive_file"].as_str().unwrap());
        let second_path = PathBuf::from(second["archive_file"].as_str().unwrap());
        assert_eq!(fs::read_to_string(&first_path).unwrap(), body);
        assert_eq!(fs::read_to_string(&second_path).unwrap(), body);
        fs::remove_dir_all(archive.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_failed_archive_keeps_the_original_and_says_so() {
        // A path whose parent cannot be a directory: the archive must fail
        // without consuming or shortening the authoritative result.
        let bogus = std::env::temp_dir()
            .join(format!("kroma-tool-archive-missing-{}", std::process::id()))
            .join("nested")
            .join("session.json");
        let body = format!("unique evidence {}", "u".repeat(70_000));
        match project_large_output(&bogus, "mcp", &body, true) {
            OutputProjection::ArchiveFailed(message) => {
                assert!(message.contains("could not archive"));
                assert!(message.contains("do not rerun"));
            }
            _ => panic!("a missing archive directory must be reported, not hidden"),
        }
    }

    #[test]
    fn plain_text_results_are_bounded_too() {
        let archive = session_dir("text");
        let body = (0..30_000)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = archived(project_large_output(&archive, "plugin", &body, false));
        let value: Value = serde_json::from_str(&preview).unwrap();
        assert_eq!(value["original_format"], "text");
        assert_eq!(value["archive_lines"], 30_000);
        assert!(value["text_preview"]
            .as_str()
            .unwrap()
            .contains("characters omitted from this preview"));
        assert!(preview.len() < INLINE_RESULT_BYTES);
        assert!(!value["archive_file"].as_str().unwrap().is_empty());
        fs::remove_dir_all(archive.parent().unwrap()).unwrap();
    }
}
