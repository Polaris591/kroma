use crate::messages::{ImageAttachment, ToolResultMeta};
use crate::prompt::{hex_encode, sha256_hex16};
use crate::tool::output_buffer::floor_char_boundary;
use crate::tools::ToolExecutionOutput;
use crate::types::ErrorCode;
use crate::workspace::WorkspaceCap;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read};
use std::time::Instant;

pub const MAX_READ_SCAN_BYTES: usize = 8 * 1024 * 1024; // 8 MiB
pub const MAX_SHA256_FILE_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB
pub const MAX_INLINE_OUTPUT_BYTES: usize = 64 * 1024; // 64 KiB
pub const MAX_READ_LINES: usize = 2000;
pub const MAX_SINGLE_LINE_BYTES: usize = 2048; // 2 KiB
/// Gate 7 (`v1.2.0`): largest image returned as a multimodal attachment (raw bytes, before base64).
///
/// Capped at 1 MiB so a single attachment can never, on its own, push the projected request past
/// `BudgetConfig::context_high_watermark_bytes` (1.5 MiB default): base64 inflates by 4/3, so the
/// encoded payload stays below 1.4 MiB and the remaining budget still fits the prefix and history.
pub const MAX_IMAGE_ATTACHMENT_BYTES: u64 = 1024 * 1024; // 1 MiB

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

pub fn execute_read(workspace: &WorkspaceCap, raw_args: &str) -> ToolExecutionOutput {
    let started = Instant::now();
    let args: ReadArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `read`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    let offset = args.offset.unwrap_or(1);
    if offset == 0 {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            "`offset` is 1-indexed and must be >= 1".to_string(),
            ToolResultMeta {
                relative_path: Some(args.path),
                ..Default::default()
            },
        );
    }

    let limit = args.limit.unwrap_or(MAX_READ_LINES);
    if limit == 0 {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            "`limit` must be >= 1".to_string(),
            ToolResultMeta {
                relative_path: Some(args.path),
                ..Default::default()
            },
        );
    }
    let effective_limit = limit.min(MAX_READ_LINES);

    let (norm_rel, file) = match workspace.open_existing_no_symlink(&args.path) {
        Ok(p) => p,
        Err(e) => {
            return ToolExecutionOutput::error(
                e.error_code(),
                e.to_string(),
                ToolResultMeta {
                    relative_path: Some(args.path),
                    ..Default::default()
                },
            );
        }
    };
    let rel_str = norm_rel.to_string_lossy().replace('\\', "/");

    let meta = match file.metadata() {
        Ok(m) => m,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::IoError,
                format!("failed to stat '{}': {}", rel_str, e),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    ..Default::default()
                },
            );
        }
    };

    if !meta.file_type().is_file() {
        return ToolExecutionOutput::error(
            ErrorCode::InvalidArguments,
            format!("'{}' is not a regular file", rel_str),
            ToolResultMeta {
                relative_path: Some(rel_str),
                ..Default::default()
            },
        );
    }

    let file_size = meta.len();

    // Gate 7 (`v1.2.0`): native multimodal image input.
    //
    // A file whose extension is an image type AND whose magic bytes agree is returned as an
    // `ImageAttachment` (base64 + exact pixel dimensions) instead of being rejected as binary, so a
    // vision-capable model can inspect screenshots, UI mockups and error captures directly. A file
    // that only *claims* to be an image falls through to the text path below and is rejected there,
    // so arbitrary binary data can never smuggle itself in as an attachment.
    if let Some(expected_mime) = image_mime_for_path(&rel_str) {
        // A file whose extension claims an image type but exceeds the attachment budget must fail
        // fast with the real reason: falling through would scan up to `MAX_READ_SCAN_BYTES` (8 MiB)
        // of binary and report a misleading `BinaryOrInvalidUtf8`.
        if file_size > MAX_IMAGE_ATTACHMENT_BYTES {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!(
                    "image file '{}' is {} bytes, exceeding the {} byte multimodal attachment limit",
                    rel_str, file_size, MAX_IMAGE_ATTACHMENT_BYTES
                ),
                ToolResultMeta {
                    relative_path: Some(rel_str),
                    duration_ms: started.elapsed().as_millis() as u64,
                    ..Default::default()
                },
            );
        }
        // Use a dedicated handle: the sniff below must not advance the primary handle, otherwise a
        // non-image file would be read from a stale offset further down.
        if file_size > 0 {
            if let Ok((_, mut image_file)) = workspace.open_existing_no_symlink(&args.path) {
                let mut bytes = Vec::with_capacity(file_size as usize);
                let mut limited = (&mut image_file).take(MAX_IMAGE_ATTACHMENT_BYTES + 1);
                if limited.read_to_end(&mut bytes).is_ok()
                    && bytes.len() as u64 <= MAX_IMAGE_ATTACHMENT_BYTES
                {
                    // The magic bytes must agree with the extension: a `.png` holding JPEG bytes
                    // is rejected rather than mislabelled on the wire.
                    if let Some(detected) =
                        sniff_image(&bytes).filter(|d| d.mime == expected_mime)
                    {
                        let (width, height) = detected.dimensions;
                        let attachment = ImageAttachment {
                            mime_type: detected.mime.to_string(),
                            base64_data: std::sync::Arc::from(base64_encode(&bytes)),
                            width: Some(width),
                            height: Some(height),
                            byte_len: bytes.len(),
                        };
                        let summary = format!(
                            "Read image file {} [{}, {}x{}, {} bytes]\n",
                            rel_str,
                            attachment.mime_type,
                            width,
                            height,
                            bytes.len()
                        );
                        return ToolExecutionOutput::success_with_images(
                            summary,
                            ToolResultMeta {
                                relative_path: Some(rel_str),
                                content_sha256: Some(sha256_hex16(&bytes)),
                                lines_range: Some((1, 1)),
                                duration_ms: started.elapsed().as_millis() as u64,
                                ..Default::default()
                            },
                            vec![attachment],
                        );
                    }
                }
            }
        }
    }
    let compute_hash = file_size <= MAX_SHA256_FILE_BYTES;
    let mut hasher = compute_hash.then(Sha256::new);

    // Bounded streaming read up to MAX_READ_SCAN_BYTES (8 MiB)
    let mut reader = BufReader::with_capacity(16 * 1024, file);
    let mut raw_bytes = Vec::with_capacity((file_size as usize).min(64 * 1024));
    let mut chunk = [0u8; 8192];
    let mut scanned_bytes: usize = 0;
    let mut hit_scan_cap = false;

    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let slice = &chunk[..n];
                // Check for NUL byte (binary file)
                if slice.contains(&0u8) {
                    return ToolExecutionOutput::error(
                        ErrorCode::BinaryOrInvalidUtf8,
                        format!("file '{}' contains NUL bytes and appears to be binary", rel_str),
                        ToolResultMeta {
                            relative_path: Some(rel_str),
                            duration_ms: started.elapsed().as_millis() as u64,
                            ..Default::default()
                        },
                    );
                }
                if let Some(ref mut h) = hasher {
                    h.update(slice);
                }
                let remaining_scan = MAX_READ_SCAN_BYTES.saturating_sub(scanned_bytes);
                let take_n = n.min(remaining_scan);
                raw_bytes.extend_from_slice(&slice[..take_n]);
                scanned_bytes += take_n;
                if scanned_bytes >= MAX_READ_SCAN_BYTES {
                    if take_n < n || file_size > MAX_READ_SCAN_BYTES as u64 {
                        hit_scan_cap = true;
                        hasher = None;
                        break;
                    }
                    let mut probe = [0u8; 1];
                    match reader.read(&mut probe) {
                        Ok(0) => break,
                        Ok(_) => {
                            hit_scan_cap = true;
                            hasher = None;
                            break;
                        }
                        Err(e) => {
                            return ToolExecutionOutput::error(
                                ErrorCode::IoError,
                                format!("error reading '{}': {}", rel_str, e),
                                ToolResultMeta {
                                    relative_path: Some(rel_str),
                                    ..Default::default()
                                },
                            );
                        }
                    }
                }
            }
            Err(e) => {
                return ToolExecutionOutput::error(
                    ErrorCode::IoError,
                    format!("error reading '{}': {}", rel_str, e),
                    ToolResultMeta {
                        relative_path: Some(rel_str),
                        ..Default::default()
                    },
                );
            }
        }
    }

    // Validate UTF-8 (if cut at 8 MiB scan boundary, check valid prefix)
    let text = match std::str::from_utf8(&raw_bytes) {
        Ok(s) => s,
        Err(e) => {
            if hit_scan_cap && e.error_len().is_none() {
                unsafe { std::str::from_utf8_unchecked(&raw_bytes[..e.valid_up_to()]) }
            } else {
                return ToolExecutionOutput::error(
                    ErrorCode::BinaryOrInvalidUtf8,
                    format!("file '{}' is not valid UTF-8", rel_str),
                    ToolResultMeta {
                        relative_path: Some(rel_str),
                        duration_ms: started.elapsed().as_millis() as u64,
                        ..Default::default()
                    },
                );
            }
        }
    };

    let sha256_display = if let Some(h) = hasher {
        let digest = h.finalize();
        let hex = hex_encode(&digest);
        hex[..16].to_string()
    } else {
        "skipped_large_file".to_string()
    };

    // Strip optional UTF-8 BOM for line display while keeping SHA256 over raw file bytes
    let body_text = text.strip_prefix('\u{FEFF}').unwrap_or(text);

    let start_line = offset;
    let mut total_lines = 0usize;
    let mut formatted_body = String::new();
    let mut end_line = start_line.saturating_sub(1);
    let mut truncated_by_cap = false;

    if !body_text.is_empty() {
        let trimmed_body = body_text.strip_suffix('\n').unwrap_or(body_text);
        for raw_line in trimmed_body.split('\n') {
            total_lines += 1;
            let line_num = total_lines;
            if line_num < start_line || truncated_by_cap {
                continue;
            }
            if line_num - start_line >= effective_limit {
                truncated_by_cap = true;
                continue;
            }
            let line_content = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            let formatted_line = if line_content.len() > MAX_SINGLE_LINE_BYTES {
                let cut = floor_char_boundary(line_content, MAX_SINGLE_LINE_BYTES);
                format!("{:>5}\t{}[line truncated]\n", line_num, &line_content[..cut])
            } else {
                format!("{:>5}\t{}\n", line_num, line_content)
            };

            if formatted_body.len() + formatted_line.len() > MAX_INLINE_OUTPUT_BYTES
                && !formatted_body.is_empty()
            {
                truncated_by_cap = true;
                continue;
            }
            formatted_body.push_str(&formatted_line);
            end_line = line_num;
        }
    }

    let more_lines_remain = end_line < total_lines;
    let truncated = truncated_by_cap || more_lines_remain || hit_scan_cap;

    let (display_start, display_end) = if total_lines == 0 {
        (0, 0)
    } else if start_line > total_lines {
        (total_lines, total_lines)
    } else {
        (start_line, end_line)
    };

    let mut output = format!(
        "[File: {} | Lines: {}-{} of {} | SHA256: {}]\n",
        rel_str, display_start, display_end, total_lines, sha256_display
    );
    output.push_str(&formatted_body);

    if start_line > total_lines && total_lines > 0 {
        output.push_str(&format!(
            "[Requested offset={} exceeds file length ({} lines).]\n",
            start_line, total_lines
        ));
    } else if more_lines_remain {
        output.push_str(&format!(
            "[Truncated at line {}. Call read with offset={} to continue.]\n",
            display_end,
            display_end + 1
        ));
    } else if hit_scan_cap {
        output.push_str(&format!(
            "[Scan limit ({} MiB) reached at line {}. Use grep to locate lines beyond the scan cap.]\n",
            MAX_READ_SCAN_BYTES / (1024 * 1024),
            display_end
        ));
    }

    let content_sha256 = (sha256_display != "skipped_large_file").then_some(sha256_display);

    ToolExecutionOutput::success(
        output,
        ToolResultMeta {
            relative_path: Some(rel_str),
            content_sha256,
            lines_range: Some((display_start, display_end)),
            duration_ms: started.elapsed().as_millis() as u64,
            truncated,
            ..Default::default()
        },
    )
}

// ---------------------------------------------------------------------------------------------
// Gate 7 (`v1.2.0`): pure-Rust image sniffing, dimension parsing and base64 encoding
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectedImage {
    pub mime: &'static str,
    pub dimensions: (u32, u32),
}

/// Map a workspace-relative path to the image MIME type its extension claims.
pub fn image_mime_for_path(rel: &str) -> Option<&'static str> {
    let ext = rel
        .rsplit('/')
        .next()
        .unwrap_or(rel)
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Identify an image by magic bytes and parse its exact pixel dimensions from the container header.
///
/// Returns `None` when the bytes are not a recognised image: callers then treat the file as binary.
pub fn sniff_image(bytes: &[u8]) -> Option<DetectedImage> {
    if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR" {
        // IHDR: width and height are big-endian u32 at offsets 16 and 20.
        let width = be32(&bytes[16..20])?;
        let height = be32(&bytes[20..24])?;
        if width == 0 || height == 0 {
            return None;
        }
        return Some(DetectedImage {
            mime: "image/png",
            dimensions: (width, height),
        });
    }

    if bytes.len() >= 4 && bytes.starts_with(b"\xFF\xD8\xFF") {
        let mut i = 2usize;
        while i + 3 < bytes.len() {
            if bytes[i] != 0xFF {
                // A JPEG payload is a sequence of `0xFF`-prefixed marker segments. A non-marker
                // byte means the header is corrupt, and any `FF C0..` found later in the raw scan
                // data would be an accidental match rather than a real frame header.
                return None;
            }
            let marker = bytes[i + 1];
            // Padding fill bytes before a marker.
            if marker == 0xFF {
                i += 1;
                continue;
            }
            // EOI (D9) and SOS (DA) end the marker-segment list: entropy-coded scan data follows
            // SOS, so a SOF appearing after either marker is a false positive.
            if matches!(marker, 0xD9 | 0xDA) {
                return None;
            }
            // Standalone markers carry no length field.
            if matches!(marker, 0x01 | 0xD0..=0xD8) {
                i += 2;
                continue;
            }
            let seg_len = be16(&bytes[i + 2..i + 4])? as usize;
            if seg_len < 2 {
                return None;
            }
            // SOF0..SOF15 except DHT (C4), JPG (C8) and DAC (CC).
            let is_sof = (0xC0..=0xCF).contains(&marker)
                && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
            if is_sof {
                // A SOF segment is 2 (length) + 1 (precision) + 2 (height) + 2 (width) bytes and
                // must fit entirely inside the buffer before the dimensions are read.
                if seg_len < 7 || i + 2 + seg_len > bytes.len() || i + 9 > bytes.len() {
                    return None;
                }
                let height = be16(&bytes[i + 5..i + 7])? as u32;
                let width = be16(&bytes[i + 7..i + 9])? as u32;
                if width == 0 || height == 0 {
                    return None;
                }
                return Some(DetectedImage {
                    mime: "image/jpeg",
                    dimensions: (width, height),
                });
            }
            i += 2 + seg_len;
        }
        return None;
    }

    if bytes.len() >= 10 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        let width = le16(&bytes[6..8])? as u32;
        let height = le16(&bytes[8..10])? as u32;
        if width == 0 || height == 0 {
            return None;
        }
        return Some(DetectedImage {
            mime: "image/gif",
            dimensions: (width, height),
        });
    }

    if bytes.len() >= 30 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        match &bytes[12..16] {
            // Extended format: canvas size minus one, 24-bit little-endian.
            b"VP8X" => {
                let width = le24(&bytes[24..27])? + 1;
                let height = le24(&bytes[27..30])? + 1;
                return Some(DetectedImage {
                    mime: "image/webp",
                    dimensions: (width, height),
                });
            }
            // Lossy: the frame header starts with the 0x9D 0x01 0x2A start code.
            b"VP8 " => {
                if bytes.len() < 30 || &bytes[23..26] != b"\x9D\x01\x2A" {
                    return None;
                }
                let width = (le16(&bytes[26..28])? & 0x3FFF) as u32;
                let height = (le16(&bytes[28..30])? & 0x3FFF) as u32;
                if width == 0 || height == 0 {
                    return None;
                }
                return Some(DetectedImage {
                    mime: "image/webp",
                    dimensions: (width, height),
                });
            }
            // Lossless: 14-bit width-1 then 14-bit height-1, packed little-endian from offset 21.
            b"VP8L" => {
                if bytes.len() < 25 || bytes[20] != 0x2F {
                    return None;
                }
                let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
                let width = (bits & 0x3FFF) + 1;
                let height = ((bits >> 14) & 0x3FFF) + 1;
                if width == 0 || height == 0 {
                    return None;
                }
                return Some(DetectedImage {
                    mime: "image/webp",
                    dimensions: (width, height),
                });
            }
            _ => return None,
        }
    }

    None
}

fn be16(b: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes([*b.first()?, *b.get(1)?]))
}

fn be32(b: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.first()?,
        *b.get(1)?,
        *b.get(2)?,
        *b.get(3)?,
    ]))
}

fn le16(b: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes([*b.first()?, *b.get(1)?]))
}

fn le24(b: &[u8]) -> Option<u32> {
    Some(
        (*b.first()? as u32) | ((*b.get(1)? as u32) << 8) | ((*b.get(2)? as u32) << 16),
    )
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (RFC 4648, padded). Hand-rolled so the harness keeps zero extra dependencies.
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(BASE64_ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64_ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64_ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
