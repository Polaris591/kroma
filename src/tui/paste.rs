//! On Windows, Crossterm's native console input source reports terminal
//! paste as individual key events, including Enter for pasted newlines.
//! Only treat an Enter as pasted when the clipboard corroborates that exact
//! newline after a recent sequence of characters.

use std::time::{Duration, Instant};

const KEY_GAP: Duration = Duration::from_millis(500);
const MAX_PREFIX_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub(super) struct NativePasteKeys {
    prefix: String,
    last_char: Option<Instant>,
}

impl NativePasteKeys {
    pub(super) fn reset(&mut self) {
        self.prefix.clear();
        self.last_char = None;
    }

    pub(super) fn recent(&self, now: Instant) -> bool {
        !self.prefix.is_empty()
            && self
                .last_char
                .is_some_and(|last| now.saturating_duration_since(last) <= KEY_GAP)
    }

    pub(super) fn record_char(&mut self, ch: char, now: Instant) {
        if !self.recent(now) || self.prefix.len() + ch.len_utf8() > MAX_PREFIX_BYTES {
            self.reset();
        }
        self.prefix.push(ch);
        self.last_char = Some(now);
    }

    pub(super) fn pasted_newline(&mut self, clipboard: &str, now: Instant) -> bool {
        if !self.recent(now) {
            self.reset();
            return false;
        }
        let text = clipboard.replace("\r\n", "\n").replace('\r', "\n");
        if !text.contains('\n') {
            self.reset();
            return false;
        }
        // The draft can contain text typed before the paste began. Match a
        // complete first clipboard line at the end of that input as well.
        let expected = if text.starts_with(&self.prefix) {
            Some(self.prefix.len())
        } else {
            let first = text.split('\n').next().unwrap_or_default();
            (!first.is_empty() && self.prefix.ends_with(first)).then_some(first.len())
        };
        let confirmed = expected.is_some_and(|len| text.as_bytes().get(len) == Some(&b'\n'));
        if confirmed {
            let matched_len = expected.unwrap_or_default();
            if matched_len != self.prefix.len() {
                self.prefix = text[..matched_len].to_owned();
            }
            self.prefix.push('\n');
            self.last_char = Some(now);
        } else {
            self.reset();
        }
        confirmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_key_stream_converts_multiline_paste_without_submitting() {
        let now = Instant::now();
        let mut keys = NativePasteKeys::default();
        for ch in "first".chars() {
            keys.record_char(ch, now);
        }
        assert!(keys.pasted_newline("first\r\nsecond", now));
        for ch in "second".chars() {
            keys.record_char(ch, now);
        }
        assert!(!keys.pasted_newline("first\r\nsecond", now));
    }

    #[test]
    fn handles_blank_lines_and_a_prefilled_draft() {
        let now = Instant::now();
        let mut keys = NativePasteKeys::default();
        for ch in "draft:first".chars() {
            keys.record_char(ch, now);
        }
        assert!(keys.pasted_newline("first\n\nthird", now));
        assert!(keys.pasted_newline("first\n\nthird", now));
        for ch in "third".chars() {
            keys.record_char(ch, now);
        }
        assert!(!keys.pasted_newline("first\n\nthird", now));
    }

    #[test]
    fn stale_or_unrelated_clipboard_never_intercepts_enter() {
        let now = Instant::now();
        let mut keys = NativePasteKeys::default();
        for ch in "hello".chars() {
            keys.record_char(ch, now);
        }
        assert!(!keys.pasted_newline("unrelated\ntext", now));
        for ch in "hello".chars() {
            keys.record_char(ch, now);
        }
        assert!(!keys.pasted_newline("hello\nthere", now + Duration::from_secs(1)));
    }
}
