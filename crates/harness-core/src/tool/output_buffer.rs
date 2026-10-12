use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Per-stream UTF-8 incomplete tail buffer (3 bytes) + 2 KiB line buffer (§5.4 V6-06).
/// Prevents multi-byte UTF-8 characters split across read chunks from being corrupted
/// and prevents concurrent stdout/stderr streams from interleaving mid-line.
pub struct StreamLineCollector {
    incomplete_utf8: [u8; 3],
    incomplete_len: usize,
    line_buf: String,
    line_cap: usize,
}

impl Default for StreamLineCollector {
    fn default() -> Self {
        Self::new(2048)
    }
}

impl StreamLineCollector {
    pub fn new(line_cap: usize) -> Self {
        Self {
            incomplete_utf8: [0u8; 3],
            incomplete_len: 0,
            line_buf: String::with_capacity(line_cap),
            line_cap: line_cap.max(64),
        }
    }

    /// Feed a raw byte chunk and invoke `on_segment` for each completed line or 2 KiB segment.
    pub fn push_chunk<F>(&mut self, chunk: &[u8], mut on_segment: F)
    where
        F: FnMut(&str),
    {
        if chunk.is_empty() {
            return;
        }

        let mut combined: Vec<u8> = Vec::with_capacity(self.incomplete_len + chunk.len());
        if self.incomplete_len > 0 {
            combined.extend_from_slice(&self.incomplete_utf8[..self.incomplete_len]);
            self.incomplete_len = 0;
        }
        combined.extend_from_slice(chunk);

        let mut slice: &[u8] = &combined;
        while !slice.is_empty() {
            match std::str::from_utf8(slice) {
                Ok(valid) => {
                    self.append_decoded_str(valid, &mut on_segment);
                    break;
                }
                Err(e) => {
                    let valid_up_to = e.valid_up_to();
                    if valid_up_to > 0 {
                        let valid_str =
                            unsafe { std::str::from_utf8_unchecked(&slice[..valid_up_to]) };
                        self.append_decoded_str(valid_str, &mut on_segment);
                    }
                    slice = &slice[valid_up_to..];
                    match e.error_len() {
                        None => {
                            // Incomplete multi-byte UTF-8 sequence at end of chunk (1..=3 bytes)
                            let rem = slice.len().min(3);
                            self.incomplete_utf8[..rem].copy_from_slice(&slice[..rem]);
                            self.incomplete_len = rem;
                            break;
                        }
                        Some(err_len) => {
                            // Genuine invalid UTF-8 byte sequence: replace with U+FFFD
                            self.append_decoded_str("\u{FFFD}", &mut on_segment);
                            slice = &slice[err_len..];
                        }
                    }
                }
            }
        }
    }

    fn append_decoded_str<F>(&mut self, s: &str, on_segment: &mut F)
    where
        F: FnMut(&str),
    {
        for ch in s.chars() {
            self.line_buf.push(ch);
            if ch == '\n' || self.line_buf.len() >= self.line_cap {
                on_segment(&self.line_buf);
                self.line_buf.clear();
            }
        }
    }

    pub fn finish<F>(&mut self, mut on_segment: F)
    where
        F: FnMut(&str),
    {
        if self.incomplete_len > 0 {
            let lossy = String::from_utf8_lossy(&self.incomplete_utf8[..self.incomplete_len]);
            self.line_buf.push_str(&lossy);
            self.incomplete_len = 0;
        }
        if !self.line_buf.is_empty() {
            on_segment(&self.line_buf);
            self.line_buf.clear();
        }
    }
}

/// 16 KiB Head + 48 KiB Rolling Tail bounded streaming accumulator with 4 MiB disk spill (§5.4).
pub struct HeadTailBuffer {
    head_cap: usize,
    tail_cap: usize,
    max_inline_lines: usize,
    spill_cap: usize,
    head_buf: String,
    head_sealed: bool,
    tail_chunks: VecDeque<String>,
    tail_bytes: usize,
    total_observed_bytes: usize,
    total_observed_lines: usize,
    spill_path: Option<PathBuf>,
    spill_file: Option<File>,
    spill_written_bytes: usize,
    spilled: bool,
}

impl HeadTailBuffer {
    pub fn new(
        head_cap: usize,
        tail_cap: usize,
        max_inline_lines: usize,
        spill_cap: usize,
        spill_path: Option<PathBuf>,
    ) -> Self {
        Self {
            head_cap,
            tail_cap,
            max_inline_lines,
            spill_cap,
            head_buf: String::with_capacity(head_cap.min(16 * 1024)),
            head_sealed: head_cap == 0,
            tail_chunks: VecDeque::new(),
            tail_bytes: 0,
            total_observed_bytes: 0,
            total_observed_lines: 0,
            spill_path,
            spill_file: None,
            spill_written_bytes: 0,
            spilled: false,
        }
    }

    pub fn push_str(&mut self, segment: &str) {
        if segment.is_empty() {
            return;
        }
        self.total_observed_bytes += segment.len();
        self.total_observed_lines += segment.bytes().filter(|&b| b == b'\n').count();

        let inline_cap = self.head_cap + self.tail_cap;
        let would_exceed = self.total_observed_bytes > inline_cap
            || self.total_observed_lines > self.max_inline_lines
            || (self.head_sealed && self.tail_bytes + segment.len() > self.tail_cap);

        if would_exceed && !self.spilled {
            self.spilled = true;
            self.init_spill_file();
        }

        if self.spilled {
            self.write_to_spill(segment);
        }

        // Fill head_buf up to head_cap on UTF-8 char boundary until sealed
        if !self.head_sealed && self.head_buf.len() < self.head_cap {
            let remaining_head = self.head_cap - self.head_buf.len();
            if segment.len() <= remaining_head {
                self.head_buf.push_str(segment);
                if self.head_buf.len() == self.head_cap {
                    self.head_sealed = true;
                }
                return;
            } else {
                let split_idx = floor_char_boundary(segment, remaining_head);
                self.head_buf.push_str(&segment[..split_idx]);
                self.head_sealed = true;
                let rest = &segment[split_idx..];
                if !self.spilled && self.tail_bytes + rest.len() > self.tail_cap {
                    self.spilled = true;
                    self.init_spill_file();
                    self.write_to_spill(rest);
                }
                self.push_to_tail(rest);
                return;
            }
        }

        self.head_sealed = true;
        self.push_to_tail(segment);
    }

    fn push_to_tail(&mut self, s: &str) {
        if s.is_empty() || self.tail_cap == 0 {
            return;
        }
        if s.len() >= self.tail_cap {
            self.tail_chunks.clear();
            let start = ceil_char_boundary(s, s.len() - self.tail_cap);
            let sub = &s[start..];
            self.tail_bytes = sub.len();
            self.tail_chunks.push_back(sub.to_string());
            return;
        }

        self.tail_chunks.push_back(s.to_string());
        self.tail_bytes += s.len();

        while self.tail_bytes > self.tail_cap {
            let excess = self.tail_bytes - self.tail_cap;
            if let Some(front) = self.tail_chunks.pop_front() {
                if front.len() <= excess {
                    self.tail_bytes -= front.len();
                } else {
                    // Trim front chunk at newline or UTF-8 char boundary
                    let cut = ceil_char_boundary(&front, excess);
                    let aligned_cut = front[cut..]
                        .find('\n')
                        .map(|pos| cut + pos + 1)
                        .unwrap_or(cut);
                    if aligned_cut >= front.len() {
                        self.tail_bytes -= front.len();
                    } else {
                        let kept = front[aligned_cut..].to_string();
                        self.tail_bytes -= front.len() - kept.len();
                        self.tail_chunks.push_front(kept);
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn init_spill_file(&mut self) {
        let Some(ref path) = self.spill_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        opts.mode(0o600);
        if let Ok(mut f) = opts.open(path) {
            #[cfg(unix)]
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
            // Write already-accumulated head + tail to spill file aligned on UTF-8 char boundaries
            let to_write = floor_char_boundary(&self.head_buf, self.spill_cap);
            let _ = f.write_all(&self.head_buf.as_bytes()[..to_write]);
            self.spill_written_bytes += to_write;

            for chunk in &self.tail_chunks {
                if self.spill_written_bytes >= self.spill_cap {
                    break;
                }
                let rem = self.spill_cap - self.spill_written_bytes;
                let n = floor_char_boundary(chunk, rem);
                if n == 0 {
                    self.spill_written_bytes = self.spill_cap;
                    break;
                }
                let _ = f.write_all(&chunk.as_bytes()[..n]);
                self.spill_written_bytes += n;
            }
            self.spill_file = Some(f);
        }
    }

    fn write_to_spill(&mut self, s: &str) {
        if self.spill_written_bytes >= self.spill_cap {
            return;
        }
        if let Some(ref mut f) = self.spill_file {
            let rem = self.spill_cap - self.spill_written_bytes;
            let n = floor_char_boundary(s, rem);
            if n == 0 {
                self.spill_written_bytes = self.spill_cap;
                return;
            }
            let _ = f.write_all(&s.as_bytes()[..n]);
            self.spill_written_bytes += n;
        }
    }

    pub fn total_observed_bytes(&self) -> usize {
        self.total_observed_bytes
    }

    pub fn is_truncated(&self) -> bool {
        self.spilled
    }

    pub fn spill_path(&self) -> Option<&Path> {
        if self.spilled {
            self.spill_path.as_deref()
        } else {
            None
        }
    }

    pub fn finalize(mut self) -> (String, bool) {
        if let Some(ref mut f) = self.spill_file {
            let _ = f.flush();
        }
        let mut tail_str = String::with_capacity(self.tail_bytes);
        for chunk in self.tail_chunks {
            tail_str.push_str(&chunk);
        }

        if !self.spilled {
            let mut full = self.head_buf;
            full.push_str(&tail_str);
            return (full, false);
        }

        let retained_bytes = self.head_buf.len() + tail_str.len();
        let omitted_bytes = self.total_observed_bytes.saturating_sub(retained_bytes);
        let retained_lines = self.head_buf.bytes().filter(|&b| b == b'\n').count()
            + tail_str.bytes().filter(|&b| b == b'\n').count();
        let omitted_lines = self
            .total_observed_lines
            .saturating_sub(retained_lines)
            .max(1);
        let omitted_kib = (omitted_bytes as f64 / 1024.0).ceil() as usize;

        let formatted = if self.spill_file.is_some() {
            let spill_display = self
                .spill_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<unknown spill path>".to_string());
            format!(
                "{}\n\n[... Omitted {} lines ({} KiB). Full log up to {} bytes saved at {} ...]\n\n{}",
                self.head_buf.trim_end_matches('\n'),
                omitted_lines,
                omitted_kib.max(1),
                self.spill_cap,
                spill_display,
                tail_str.trim_start_matches('\n')
            )
        } else {
            format!(
                "{}\n\n[... Omitted {} lines ({} KiB). Only the head and rolling tail were retained; no spill file was available.]\n\n{}",
                self.head_buf.trim_end_matches('\n'),
                omitted_lines,
                omitted_kib.max(1),
                tail_str.trim_start_matches('\n')
            )
        };
        (formatted, true)
    }
}

pub fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}
