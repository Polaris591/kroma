//! Provider-neutral conversion of durable attachment references.
//! Binary bytes are loaded only at request time, never saved inline in WAL.
use crate::model::TaskImageCache;
use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashSet;

pub(crate) const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_ACTIVE_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const MAX_LOG_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const INLINE_LOG_BYTES: u64 = 4096;
pub(crate) const IMAGE_TOKEN_ALLOWANCE: usize = 4096;

pub(crate) fn is_image(part: &Value) -> bool {
    part["type"] == "kroma_attachment" && part["kind"] == "image"
}

pub(crate) fn inline_image(part: &Value) -> bool {
    is_image(part) && part["mode"] != "reference"
}

// Attachment activation belongs to a specific occurrence in the transcript,
// not to its file path. Recalling the same file cannot reactivate old turns.
struct ActiveScope {
    current: Option<usize>,
    previous: Option<usize>,
    previous_images: HashSet<usize>,
}

impl ActiveScope {
    fn for_messages(messages: &[crate::protocol::Message], vision: Option<bool>) -> Self {
        let mut users = messages
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, message)| message.role == "user");
        let current = users.next().map(|(index, _)| index);
        let previous = users.next().map(|(index, _)| index);
        let mut previous_images = HashSet::new();
        if vision != Some(false) {
            let used = current
                .and_then(|index| messages[index].provider_items.as_deref())
                .into_iter()
                .flatten()
                .filter(|part| inline_image(part))
                .map(image_bytes)
                .fold(0u64, u64::saturating_add);
            let mut remaining = MAX_ACTIVE_IMAGE_BYTES.saturating_sub(used);
            if let Some(parts) =
                previous.and_then(|index| messages[index].provider_items.as_deref())
            {
                for (index, part) in parts.iter().enumerate() {
                    if !inline_image(part) {
                        continue;
                    }
                    let bytes = image_bytes(part);
                    if bytes <= remaining {
                        previous_images.insert(index);
                        remaining -= bytes;
                    }
                }
            }
        }
        Self {
            current,
            previous,
            previous_images,
        }
    }

    fn includes(&self, message: usize, part: usize, item: &Value, vision: Option<bool>) -> bool {
        if is_image(item) && vision == Some(false) {
            return false;
        }
        self.current == Some(message)
            || (self.previous == Some(message)
                && is_image(item)
                && self.previous_images.contains(&part))
    }
}

fn image_bytes(part: &Value) -> u64 {
    part["bytes"].as_u64().unwrap_or(MAX_IMAGE_BYTES as u64)
}

/// Image allowance for the same message instances project_history will send.
/// No path-based lookup: repeated references in older turns stay inactive.
pub(crate) fn active_image_tokens(
    messages: &[crate::protocol::Message],
    vision: Option<bool>,
) -> usize {
    let scope = ActiveScope::for_messages(messages, vision);
    messages
        .iter()
        .enumerate()
        .flat_map(|(message, item)| {
            item.provider_items
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .enumerate()
                .map(move |(part, value)| (message, part, value))
        })
        .filter(|(message, part, value)| {
            inline_image(value) && scope.includes(*message, *part, value, vision)
        })
        .count()
        .saturating_mul(IMAGE_TOKEN_ALLOWANCE)
}

/// A request projection, never an edit to durable history. Only the newest
/// user turn and eligible images from the immediately previous user turn
/// remain inline. The same path in any older occurrence stays a reference.
pub(crate) fn project_history(view: &mut [crate::protocol::Message], vision: Option<bool>) {
    let scope = ActiveScope::for_messages(view, vision);
    for (message_index, message) in view.iter_mut().enumerate() {
        if message.role != "user" {
            continue;
        }
        for (part_index, part) in message
            .provider_items
            .as_mut()
            .into_iter()
            .flatten()
            .enumerate()
        {
            if part["type"] != "kroma_attachment" {
                continue;
            }
            if !scope.includes(message_index, part_index, part, vision) {
                part["mode"] = json!("reference");
            }
        }
    }
}

/// One internal scalar identifies one attachment in a user message. The
/// persisted record contains both this marker and the immutable file ref;
/// the human-readable label is presentation, not the lookup key.
pub(crate) fn marker(part: &Value) -> Option<char> {
    let raw = part["marker"].as_str()?;
    let mut chars = raw.chars();
    let marker = chars.next()?;
    if chars.next().is_some() || !(0xE000..=0xF8FF).contains(&(marker as u32)) {
        return None;
    }
    Some(marker)
}

fn inline_log(part: &Value) -> bool {
    part["type"] == "kroma_attachment"
        && part["kind"] == "log"
        && part["mode"] != "reference"
        && part["bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes <= INLINE_LOG_BYTES)
}

fn inline_log_text(part: &Value, index: usize, total: usize) -> Result<String> {
    let path = part["path"].as_str().context("attachment missing path")?;
    let name = part["name"].as_str().unwrap_or("attachment");
    let raw = std::fs::read(path).with_context(|| format!("attachment missing: {name}"))?;
    let log = String::from_utf8(raw).context("attached text is not UTF-8")?;
    Ok(format!(
        "[Pasted text {name}, attachment #{}/{}]\n{log}",
        index + 1,
        total
    ))
}

/// Additional escaped request bytes from logs which the provider will expand.
/// The planner and final validator share this projection estimate, including
/// repeated inline markers and historical marker-less attachments. Older user
/// turns are references and are not charged as inline logs.
pub(crate) fn inline_log_extra_bytes(messages: &[crate::protocol::Message]) -> Result<usize> {
    let Some(message) = messages.iter().rev().find(|message| message.role == "user") else {
        return Ok(0);
    };
    let attached = message
        .provider_items
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|part| part["type"] == "kroma_attachment")
        .collect::<Vec<_>>();
    let mut occurrences = vec![0usize; attached.len()];
    for character in message.content.as_deref().unwrap_or("").chars() {
        if let Some(index) = attached
            .iter()
            .position(|part| marker(part) == Some(character))
        {
            occurrences[index] = occurrences[index].saturating_add(1);
        }
    }
    let mut extra = 0usize;
    for (index, part) in attached.iter().enumerate() {
        if marker(part).is_none() {
            occurrences[index] = occurrences[index].saturating_add(1);
        }
        if occurrences[index] == 0 || !inline_log(part) {
            continue;
        }
        // Responses has the longest text-block envelope of the adapters.
        // Count escaped JSON, not filesystem bytes: newlines and quotes grow.
        let text = inline_log_text(part, index, attached.len())?;
        let bytes = serde_json::to_vec(&json!({"type":"input_text","text":text}))?.len() + 1;
        extra = extra.saturating_add(bytes.saturating_mul(occurrences[index]));
    }
    Ok(extra)
}

fn encode_image(path: &str, name: &str) -> Result<String> {
    let raw = std::fs::read(path).with_context(|| format!("attachment missing: {name}"))?;
    if raw.len() > MAX_IMAGE_BYTES {
        anyhow::bail!("image exceeds 8 MiB");
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(raw))
}

impl TaskImageCache {
    fn image_base64(&self, path: &str, name: &str) -> Result<String> {
        if let Some(encoded) = self.encoded.lock().expect("image cache poisoned").get(path) {
            return Ok(encoded.clone());
        }
        // Persisted attachment paths are immutable copies under ~/.kroma.
        // One encoding can be reused by every tool round of this task.
        let encoded = encode_image(path, name)?;
        self.encoded
            .lock()
            .expect("image cache poisoned")
            .insert(path.to_owned(), encoded.clone());
        Ok(encoded)
    }
}

#[cfg(test)]
pub(crate) fn content(text: &str, parts: &[Value], protocol: &str) -> Result<Value> {
    content_with_cache(text, parts, protocol, None)
}

pub(crate) fn content_with_cache(
    text: &str,
    parts: &[Value],
    protocol: &str,
    cache: Option<&TaskImageCache>,
) -> Result<Value> {
    if !parts.iter().any(|p| p["type"] == "kroma_attachment") {
        return Ok(json!(text));
    }
    let mut body = Vec::new();
    let text_block = |text: String| -> Value {
        match protocol {
            "responses" => json!({"type":"input_text","text":text}),
            _ => json!({"type":"text","text":text}),
        }
    };
    let attached = parts
        .iter()
        .filter(|part| part["type"] == "kroma_attachment")
        .collect::<Vec<_>>();
    // Refuse oversized multimodal payloads before base64 encoding and
    // wasting latency/bandwidth. This is independent of the context-window
    // heuristic; image-token accounting is provider-specific.
    let image_bytes = attached
        .iter()
        .filter(|p| inline_image(p))
        .map(|p| p["bytes"].as_u64().unwrap_or(0))
        .sum::<u64>();
    if image_bytes > MAX_ACTIVE_IMAGE_BYTES {
        anyhow::bail!("active images exceed 16 MiB combined; send fewer images");
    }
    let total = attached.len();
    let add_attachment = |index: usize, part: &Value, body: &mut Vec<Value>| -> Result<()> {
        let path = part["path"].as_str().context("attachment missing path")?;
        let name = part["name"].as_str().unwrap_or("attachment");
        let number = index + 1;
        if inline_image(part) {
            body.push(text_block(format!(
                "[Image {name}, attachment #{number}/{total}]"
            )));
            let mime = part["mime"].as_str().unwrap_or("image/png");
            let b64 = match cache {
                Some(cache) => cache.image_base64(path, name)?,
                None => encode_image(path, name)?,
            };
            match protocol {
                "anthropic" => body.push(json!({"type":"image","source":{
                    "type":"base64","media_type":mime,"data":b64}})),
                "responses" => body.push(json!({"type":"input_image",
                    "image_url":format!("data:{mime};base64,{b64}")})),
                _ => body.push(json!({"type":"image_url","image_url":{
                    "url":format!("data:{mime};base64,{b64}")}})),
            }
        } else if is_image(part) {
            body.push(text_block(format!(
                "[Image {name}, attachment #{number}/{total}: omitted from model input. Original image available at {path}; previous analysis may be used, but visual details cannot be inferred without reloading.]"
            )));
        } else if inline_log(part) {
            // Short pasted text should not cost an avoidable Shell round trip.
            body.push(text_block(inline_log_text(part, index, total)?));
        } else {
            let kind = if part["kind"] == "log" { "log" } else { "file" };
            body.push(text_block(format!(
                "[Attachment #{number}/{total}: {kind} {name}. Read its complete content from {path} using shell if needed.]"
            )));
        }
        Ok(())
    };
    let mut pending = String::new();
    let mut encountered = vec![false; total];
    for character in text.chars() {
        let found = attached
            .iter()
            .position(|item| marker(item) == Some(character));
        if let Some(index) = found {
            if !pending.is_empty() {
                body.push(text_block(std::mem::take(&mut pending)));
            }
            encountered[index] = true;
            add_attachment(index, attached[index], &mut body)?;
        } else if (0xE000..=0xF8FF).contains(&(character as u32)) {
            pending.push_str("【missing attachment】");
        } else {
            pending.push(character);
        }
    }
    if !pending.is_empty() {
        body.push(text_block(pending));
    }
    // Session histories saved before inline chips had no marker. Keep those
    // records readable and in their original attachment-list order.
    for (index, part) in attached.iter().enumerate() {
        if marker(part).is_none() && !encountered[index] {
            add_attachment(index, part, &mut body)?;
        }
    }
    Ok(Value::Array(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Message;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn image_encoding_is_identical_per_protocol_and_scoped_to_one_task() -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let dir =
            std::env::temp_dir().join(format!("kroma-image-cache-{}-{now}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let path = dir.join("test.png");
        let parts = vec![json!({
            "type":"kroma_attachment", "kind":"image", "name":"test.png",
            "path":path, "mime":"image/png", "bytes":5, "marker":"\u{e000}"
        })];
        for protocol in ["chat", "responses", "anthropic"] {
            fs::write(&path, b"first")?;
            let task = TaskImageCache::default();
            let baseline = content("Inspect \u{e000}", &parts, protocol)?;
            assert_eq!(
                content_with_cache("Inspect \u{e000}", &parts, protocol, Some(&task))?,
                baseline,
                "first request must be byte-for-byte identical"
            );
            assert_eq!(task.encoded.lock().unwrap().len(), 1);
            fs::write(&path, b"later")?;
            assert_eq!(
                content_with_cache("Inspect \u{e000}", &parts, protocol, Some(&task))?,
                baseline,
                "tool rounds reuse the original immutable image encoding"
            );
            let next_task = TaskImageCache::default();
            assert_ne!(
                content_with_cache("Inspect \u{e000}", &parts, protocol, Some(&next_task))?,
                baseline,
                "a new task must not inherit previous image bytes"
            );
            // WAL payload is still a disk reference, never encoded data.
            let serialized = serde_json::to_string(&parts)?;
            assert!(
                !serialized.contains(&base64::engine::general_purpose::STANDARD.encode(b"first"))
            );
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn mixed_image_log_image_order_survives_all_protocols_and_session_roundtrip() -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let dir =
            std::env::temp_dir().join(format!("kroma-order-test-{}-{now}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let image1 = dir.join("before.png");
        let image2 = dir.join("after.png");
        let log = dir.join("errors.log");
        fs::write(&image1, b"first-image")?;
        fs::write(&image2, b"second-image")?;
        fs::write(&log, b"line 1\nline 2\n")?;
        let items = vec![
            json!({"type":"kroma_attachment","kind":"image","name":"before.png","path":image1,"mime":"image/png"}),
            json!({"type":"kroma_attachment","kind":"log","name":"errors.log","path":log}),
            json!({"type":"kroma_attachment","kind":"image","name":"after.png","path":image2,"mime":"image/png"}),
        ];
        let mut saved = Message::new("user", "Review these in order");
        saved.provider_items = Some(items.clone());
        let restored: Message = serde_json::from_str(&serde_json::to_string(&saved)?)?;
        assert_eq!(restored.provider_items.as_deref(), Some(items.as_slice()));
        for protocol in ["chat", "responses", "anthropic"] {
            let content = content(
                saved.content.as_deref().unwrap(),
                restored.provider_items.as_deref().unwrap(),
                protocol,
            )?;
            let blocks = content
                .as_array()
                .context("expected ordered content array")?;
            assert_eq!(blocks.len(), 6, "{protocol}");
            let text_field = if protocol == "responses" {
                "input_text"
            } else {
                "text"
            };
            for index in [0, 1, 3, 4] {
                assert_eq!(
                    blocks[index]["type"], text_field,
                    "{protocol} block {index}"
                );
            }
            assert_eq!(blocks[0]["text"], "Review these in order");
            assert!(blocks[1]["text"]
                .as_str()
                .unwrap()
                .contains("Image before.png, attachment #1/3"));
            assert!(blocks[3]["text"]
                .as_str()
                .unwrap()
                .contains("#2/3: log errors.log"));
            assert!(blocks[3]["text"]
                .as_str()
                .unwrap()
                .contains(log.to_str().unwrap()));
            assert!(blocks[4]["text"]
                .as_str()
                .unwrap()
                .contains("Image after.png, attachment #3/3"));
            let image_field = match protocol {
                "responses" => "input_image",
                "anthropic" => "image",
                _ => "image_url",
            };
            assert_eq!(blocks[2]["type"], image_field);
            assert_eq!(blocks[5]["type"], image_field);
            let payload = blocks[2].to_string();
            let second = blocks[5].to_string();
            assert!(
                payload.contains(&base64::engine::general_purpose::STANDARD.encode(b"first-image"))
            );
            assert!(
                second.contains(&base64::engine::general_purpose::STANDARD.encode(b"second-image"))
            );
        }
        assert_eq!(content("plain", &[], "chat")?, json!("plain"));
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn inline_attachment_text_order_delete_and_legacy_history() -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let folder = std::env::temp_dir().join(format!("kroma-inline-{now}"));
        fs::create_dir_all(&folder)?;
        let img = folder.join("ui.png");
        let log = folder.join("short.log");
        fs::write(&img, b"fake image")?;
        fs::write(&log, "分析这段短日志")?;
        let first = char::from_u32(0xE000).unwrap();
        let second = char::from_u32(0xE001).unwrap();
        let attachments = vec![
            json!({"type":"kroma_attachment","kind":"image","name":"ui.png","path":img,
              "mime":"image/png","marker":first.to_string(),"bytes":10}),
            json!({"type":"kroma_attachment","kind":"log","name":"Pasted text","path":log,
              "marker":second.to_string(),"bytes":fs::metadata(&log)?.len()}),
        ];
        for protocol in ["chat", "responses", "anthropic"] {
            let request = content(
                &format!("你看看{first}怎么样，分析一下{second}"),
                &attachments,
                protocol,
            )?;
            let b = request.as_array().unwrap();
            assert_eq!(b.len(), 5);
            assert_eq!(b[0]["text"], "你看看");
            assert!(b[1]["text"].as_str().unwrap().contains("Image ui.png"));
            assert_eq!(
                b[2]["type"],
                match protocol {
                    "responses" => "input_image",
                    "anthropic" => "image",
                    _ => "image_url",
                }
            );
            assert_eq!(b[3]["text"], "怎么样，分析一下");
            assert!(b[4]["text"].as_str().unwrap().contains("分析这段短日志"));
        }
        // Old sessions without inline markers remain supported.
        let legacy = content(
            "old message",
            &attachments
                .iter()
                .map(|part| {
                    let mut p = part.clone();
                    p.as_object_mut().unwrap().remove("marker");
                    p
                })
                .collect::<Vec<_>>(),
            "chat",
        )?;
        assert!(legacy.as_array().unwrap().len() > 3);
        fs::remove_dir_all(folder)?;
        Ok(())
    }

    #[test]
    fn text_only_and_old_history_never_load_visual_bytes() -> Result<()> {
        let path = "/nonexistent/historical-screenshot.png";
        let marker = char::from_u32(0xE000).unwrap();
        let source = json!({"type":"kroma_attachment","kind":"image","name":"old.png",
            "path":path,"mime":"image/png","marker":marker.to_string(),"bytes":900000});
        let mut message = Message::new("user", format!("see {marker}"));
        message.provider_items = Some(vec![source]);
        let original = serde_json::to_vec(&message)?;
        let mut historical = vec![
            message.clone(),
            Message::new("user", "follow-up"),
            Message::new("user", "another follow-up"),
        ];
        project_history(&mut historical, Some(true));
        let reference = content(
            historical[0].content.as_deref().unwrap(),
            historical[0].provider_items.as_deref().unwrap(),
            "chat",
        )?;
        assert!(!reference.to_string().contains("image_url"));
        assert!(reference.to_string().contains("omitted from model input"));
        assert_eq!(
            serde_json::to_vec(&message)?,
            original,
            "authoritative history is unchanged"
        );
        let mut switched = vec![message];
        project_history(&mut switched, Some(false));
        let text_only = content(
            switched[0].content.as_deref().unwrap(),
            switched[0].provider_items.as_deref().unwrap(),
            "responses",
        )?;
        assert!(!text_only.to_string().contains("input_image"));
        assert!(text_only.to_string().contains("old.png"));
        Ok(())
    }

    #[test]
    fn repeating_the_same_image_does_not_reactivate_older_user_turns() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "kroma-repeated-image-{}-{}.png",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::write(&path, b"image-data")?;
        let image = json!({"type":"kroma_attachment","kind":"image",
            "name":"picture.png","path":path,"bytes":10});
        let mut history = Vec::new();
        for _ in 0..5 {
            let mut message = Message::new("user", "Look at the same picture");
            message.provider_items = Some(vec![image.clone()]);
            history.push(message);
            history.push(Message::new("assistant", "Acknowledged"));
        }
        assert_eq!(
            active_image_tokens(&history, Some(true)),
            2 * IMAGE_TOKEN_ALLOWANCE
        );
        project_history(&mut history, Some(true));
        let inline = history
            .iter()
            .flat_map(|m| m.provider_items.as_deref().unwrap_or(&[]))
            .filter(|part| inline_image(part))
            .count();
        assert_eq!(
            inline, 2,
            "only the current and immediately previous user turns may carry image bytes"
        );
        for (protocol, kind) in [
            ("chat", "image_url"),
            ("responses", "input_image"),
            ("anthropic", "image"),
        ] {
            let wire_images = history
                .iter()
                .filter(|m| m.role == "user")
                .map(|message| {
                    content(
                        message.content.as_deref().unwrap(),
                        message.provider_items.as_deref().unwrap(),
                        protocol,
                    )
                    .unwrap()
                })
                .flat_map(|body| body.as_array().unwrap().clone())
                .filter(|block| block["type"] == kind)
                .count();
            assert_eq!(wire_images, 2, "incorrect {protocol} image payload");
        }
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn thirty_historical_images_are_lightweight_refs() -> Result<()> {
        let file =
            std::env::temp_dir().join(format!("kroma-media-load-{}.png", std::process::id()));
        fs::write(&file, vec![42; 256 * 1024])?;
        let path = file.to_string_lossy().into_owned();
        let marker = char::from_u32(0xE000).unwrap();
        let source = || {
            let mut message = Message::new("user", format!("image {marker}"));
            message.provider_items = Some(vec![json!({"type":"kroma_attachment","kind":"image",
                "name":"photo.png","path":path,"mime":"image/png",
                "marker":marker.to_string(),"bytes":256*1024})]);
            message
        };
        let history = (0..30).map(|_| source()).collect::<Vec<_>>();
        let baseline = history
            .iter()
            .map(|m| {
                content(
                    m.content.as_deref().unwrap(),
                    m.provider_items.as_deref().unwrap(),
                    "chat",
                )
                .unwrap()
                .to_string()
                .len()
            })
            .sum::<usize>();
        let mut view = history;
        view.push(Message::new("user", "follow-up without picture"));
        view.push(Message::new("user", "second follow-up without picture"));
        let started = std::time::Instant::now();
        project_history(&mut view, Some(true));
        let projected = view
            .iter()
            .map(|m| {
                content(
                    m.content.as_deref().unwrap(),
                    m.provider_items.as_deref().unwrap_or(&[]),
                    "chat",
                )
                .unwrap()
                .to_string()
                .len()
            })
            .sum::<usize>();
        eprintln!(
            "30-image projection: initial_wire={} bytes, history_wire={} bytes, build={:?}",
            baseline,
            projected,
            started.elapsed()
        );
        assert!(
            baseline > projected * 20,
            "old images should not be uploaded again"
        );
        assert!(projected < 30_000);
        fs::remove_file(file)?;
        Ok(())
    }
}
