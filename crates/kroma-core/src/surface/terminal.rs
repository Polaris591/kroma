//! terminal surface of the multi-surface runtime (split from `surface.rs`).

/// Escape untrusted text before rendering it to a terminal surface (F-06).
///
/// Preserves `\n` and `\t` while escaping `\r`, ESC (`\x1b`), C0/C1 control characters, and Unicode
/// bidirectional overrides so untrusted repository content or model output cannot spoof terminal UI.
pub fn sanitize_terminal_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let cp = ch as u32;
        let is_bidi = (0x202A..=0x202E).contains(&cp) || (0x2066..=0x2069).contains(&cp);
        match ch {
            '\n' => out.push('\n'),
            '\t' => out.push('\t'),
            '\r' => out.push_str("\\r"),
            '\x1b' => out.push_str("\\x1b"),
            _ if cp < 0x20 || cp == 0x7f || (0x80..=0x9f).contains(&cp) || is_bidi => {
                out.push_str(&format!("\\x{:02x}", cp));
            }
            _ => out.push(ch),
        }
    }
    out
}

/// `v1.5.0`: width for the Design D spectrum hairline, clamped to a readable range.
pub(crate) fn terminal_hairline_width() -> usize {
    crate::theme::terminal_width().saturating_sub(2).max(20)
}

/// P5 (`v1.5.0`): escape untrusted text for a single-line diagnostic (stderr).
///
/// Unlike [`sanitize_terminal_text`], this also escapes `\n` and `\t`: a diagnostic label is
/// embedded in one line, so an untrusted key or path containing a newline or tab could otherwise
/// forge or misalign the message. ESC, CR, C0/C1 and Unicode bidi overrides are escaped as well.
pub fn sanitize_diagnostic_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let cp = ch as u32;
        let is_bidi = (0x202A..=0x202E).contains(&cp) || (0x2066..=0x2069).contains(&cp);
        match ch {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\x1b' => out.push_str("\\x1b"),
            _ if cp < 0x20 || cp == 0x7f || (0x80..=0x9f).contains(&cp) || is_bidi => {
                out.push_str(&format!("\\x{:02x}", cp));
            }
            _ => out.push(ch),
        }
    }
    out
}
