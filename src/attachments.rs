//! Provider-neutral conversion of durable attachment references.
//! Binary bytes are loaded only at request time, never saved inline in WAL.
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

pub(crate) fn active_image(part: &Value, active: &HashSet<String>, vision: Option<bool>) -> bool {
    vision != Some(false)
        && inline_image(part)
        && part["path"]
            .as_str()
            .is_some_and(|path| active.contains(path))
}

/// Reserve the same per-image allowance used by the final request check
/// *before* TRC/APEC decides whether the request fits. Only images that will
/// be sent in this task count; older references and text-only models do not.
pub(crate) fn active_image_tokens(
    messages: &[crate::protocol::Message],
    active: &HashSet<String>,
    vision: Option<bool>,
) -> usize {
    if vision == Some(false) {
        return 0;
    }
    messages
        .iter()
        .flat_map(|message| message.provider_items.as_deref().unwrap_or(&[]))
        .filter(|part| active_image(part, active, vision))
        .count()
        .saturating_mul(IMAGE_TOKEN_ALLOWANCE)
}

/// A request projection, never an edit to durable history. The current
/// task can continue seeing its image through tool rounds, but a later task
/// receives a lightweight image reference instead of the old binary.
pub(crate) fn project_history(
    view: &mut [crate::protocol::Message],
    active: &HashSet<String>,
    vision: Option<bool>,
) {
    for message in view {
        if message.role != "user" {
            continue;
        }
        for part in message.provider_items.as_mut().into_iter().flatten() {
            if part["type"] != "kroma_attachment" {
                continue;
            }
            let is_active = part["path"]
                .as_str()
                .is_some_and(|path| active.contains(path));
            if !is_active || (is_image(part) && vision == Some(false)) {
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

pub(crate) fn content(text: &str, parts: &[Value], protocol: &str) -> Result<Value> {
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
            let raw = std::fs::read(path).with_context(|| format!("attachment missing: {name}"))?;
            if raw.len() > MAX_IMAGE_BYTES {
                anyhow::bail!("image exceeds 8 MiB");
            }
            let mime = part["mime"].as_str().unwrap_or("image/png");
            let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
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
        } else if part["kind"] == "log"
            && part["mode"] != "reference"
            && part["bytes"]
                .as_u64()
                .is_some_and(|size| size <= INLINE_LOG_BYTES)
        {
            // Short pasted text should not cost an avoidable Shell round trip.
            let raw = std::fs::read(path).with_context(|| format!("attachment missing: {name}"))?;
            let log = String::from_utf8(raw).context("attached text is not UTF-8")?;
            body.push(text_block(format!(
                "[Pasted text {name}, attachment #{number}/{total}]\n{log}"
            )));
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
        collections::HashSet,
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

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
        let mut historical = vec![message.clone()];
        project_history(&mut historical, &HashSet::new(), Some(true));
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
        project_history(
            &mut switched,
            &HashSet::from([path.to_owned()]),
            Some(false),
        );
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
        let started = std::time::Instant::now();
        project_history(&mut view, &HashSet::new(), Some(true));
        let projected = view
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
