use kroma_core::events::EventPayload;
use kroma_core::tool::policy::{ApprovalDecision, ApprovalHandler, ApprovalRequest};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

/// F-06: set by the CLI's Ctrl-C handler so an in-flight approval prompt stops waiting for input
/// and denies instead of blocking the runtime until the user happens to press a key.
pub static APPROVAL_CANCELLED: AtomicBool = AtomicBool::new(false);

/// How long the approval read blocks before re-checking the cancellation flag.
#[cfg(unix)]
const APPROVAL_POLL_INTERVAL_MS: libc::c_int = 100;
#[cfg(windows)]
const APPROVAL_POLL_INTERVAL_MS: u32 = 100;

/// Escape untrusted text before writing it to a terminal (F-06).
///
/// Approval banners embed repository-controlled content (file paths, diffs) and model/tool output
/// (shell commands). Without escaping, an embedded `\r`, ESC/ANSI sequence, C0/C1 control byte or
/// Unicode bidirectional override can visually rewrite the banner so the operator approves
/// something other than what is actually shown. Newlines and tabs are preserved (they carry
/// legitimate structure); everything else dangerous is rendered as a visible escape.
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

pub struct TtyApprovalUi {
    interactive_tty: bool,
    next_seq: AtomicUsize,
    recorded_events: Mutex<Vec<EventPayload>>,
}

impl Default for TtyApprovalUi {
    fn default() -> Self {
        Self::new()
    }
}

impl TtyApprovalUi {
    pub fn new() -> Self {
        // F-06: both stdin (to read the decision) and stderr (to render the banner) must be a TTY,
        // otherwise the interactive prompt cannot be trusted or displayed correctly.
        #[cfg(unix)]
        let interactive_tty = unsafe {
            libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDERR_FILENO) == 1
        };
        #[cfg(windows)]
        let interactive_tty = {
            use std::io::IsTerminal;
            io::stdin().is_terminal() && io::stderr().is_terminal()
        };
        Self {
            interactive_tty,
            next_seq: AtomicUsize::new(1),
            recorded_events: Mutex::new(Vec::new()),
        }
    }

    pub fn with_tty_override(interactive_tty: bool) -> Self {
        Self {
            interactive_tty,
            next_seq: AtomicUsize::new(1),
            recorded_events: Mutex::new(Vec::new()),
        }
    }

    fn record_requested(&self, tool_name: &str) -> String {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let req_id = format!("tty-apr-{:04}", seq);
        self.recorded_events
            .lock()
            .unwrap()
            .push(EventPayload::ApprovalRequested {
                request_id: req_id.clone(),
                tool_name: tool_name.to_string(),
                surface: "headless".to_string(),
            });
        req_id
    }

    fn record_resolved(&self, req_id: String, decision: &str) {
        self.recorded_events
            .lock()
            .unwrap()
            .push(EventPayload::ApprovalResolved {
                request_id: req_id,
                decision: decision.to_string(),
            });
    }

    pub fn format_prompt_banner(req: &ApprovalRequest) -> String {
        let mut out = String::new();
        if req.is_sensitive_path {
            out.push_str("=== [HIGH-RISK SENSITIVE PATH APPROVAL REQUIRED] ===\n");
        } else {
            out.push_str("=== [KROMA TOOL APPROVAL REQUIRED] ===\n");
        }
        out.push_str(&format!(
            "Tool:      {}\nWorkspace: {}\n",
            sanitize_terminal_text(&req.tool_name),
            sanitize_terminal_text(&req.workspace_root.display().to_string())
        ));
        if let Some(ref p) = req.target_path {
            out.push_str(&format!(
                "Target:    {}\n",
                sanitize_terminal_text(&p.display().to_string())
            ));
        }
        if let Some(ref diff) = req.diff_preview {
            out.push_str("--- Edit / Write Preview (escaped) ---\n");
            let escaped = sanitize_terminal_text(diff);
            out.push_str(&escaped);
            if !escaped.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("--- End Preview ---\n");
        }
        if let Some(ref cmd) = req.command_preview {
            out.push_str("--- Shell Command Preview (escaped) ---\n");
            let escaped = sanitize_terminal_text(cmd);
            out.push_str(&escaped);
            if !escaped.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("--- End Command ---\n");
        }
        out.push_str("Choose: [y] Allow once | [a] Allow this tool category for current run | [n] Deny: ");
        out
    }
}

impl ApprovalHandler for TtyApprovalUi {
    fn is_interactive(&self) -> bool {
        self.interactive_tty
    }

    fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        // Non-TTY fail-safe (§2.1): immediately deny without blocking or spawning.
        if !self.interactive_tty {
            return ApprovalDecision::Deny;
        }
        let req_id = self.record_requested(&req.tool_name);
        if APPROVAL_CANCELLED.load(Ordering::SeqCst) {
            self.record_resolved(req_id, "deny_cancelled");
            return ApprovalDecision::Deny;
        }

        let banner = Self::format_prompt_banner(req);
        let mut stderr = io::stderr();
        if stderr.write_all(banner.as_bytes()).is_err() || stderr.flush().is_err() {
            self.record_resolved(req_id, "deny_io_error");
            return ApprovalDecision::Deny;
        }

        // F-06: wait for input in bounded poll slices and read byte-by-byte inside the poll loop so
        // a Ctrl-C (which sets `APPROVAL_CANCELLED` from the async signal task) aborts the prompt
        // within 100ms even if stdin received a partial line without a trailing newline.
        let mut raw_bytes: Vec<u8> = Vec::with_capacity(64);
        #[cfg(unix)]
        loop {
            if APPROVAL_CANCELLED.load(Ordering::SeqCst) {
                self.record_resolved(req_id, "deny_cancelled");
                return ApprovalDecision::Deny;
            }
            let mut pfd = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, APPROVAL_POLL_INTERVAL_MS) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                self.record_resolved(req_id, "deny_io_error");
                return ApprovalDecision::Deny;
            }
            if rc == 0 {
                continue;
            }
            let mut b = [0u8; 1];
            let n = unsafe { libc::read(libc::STDIN_FILENO, b.as_mut_ptr() as *mut libc::c_void, 1) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                self.record_resolved(req_id, "deny_io_error");
                return ApprovalDecision::Deny;
            }
            if n == 0 || b[0] == b'\n' {
                break;
            }
            if raw_bytes.len() < 256 {
                raw_bytes.push(b[0]);
            }
        }
        #[cfg(windows)]
        loop {
            use windows_sys::Win32::Foundation::{
                INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
            };
            use windows_sys::Win32::Storage::FileSystem::ReadFile;
            use windows_sys::Win32::System::Console::{
                GetConsoleMode, GetNumberOfConsoleInputEvents, GetStdHandle, ReadConsoleInputW,
                 INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
            };
            use windows_sys::Win32::System::Threading::WaitForSingleObject;

            if APPROVAL_CANCELLED.load(Ordering::SeqCst) {
                self.record_resolved(req_id, "deny_cancelled");
                return ApprovalDecision::Deny;
            }
            let h_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            if h_in.is_null() || h_in == INVALID_HANDLE_VALUE {
                self.record_resolved(req_id, "deny_io_error");
                return ApprovalDecision::Deny;
            }
            let wait_rc = unsafe { WaitForSingleObject(h_in, APPROVAL_POLL_INTERVAL_MS) };
            if wait_rc == WAIT_TIMEOUT {
                continue;
            }
            if wait_rc != WAIT_OBJECT_0 {
                self.record_resolved(req_id, "deny_io_error");
                return ApprovalDecision::Deny;
            }

            let mut console_mode: u32 = 0;
            let is_console = unsafe { GetConsoleMode(h_in, &mut console_mode) != 0 };
            if is_console {
                let mut unread: u32 = 0;
                if unsafe { GetNumberOfConsoleInputEvents(h_in, &mut unread) } == 0 {
                    self.record_resolved(req_id, "deny_io_error");
                    return ApprovalDecision::Deny;
                }
                if unread == 0 {
                    continue;
                }
                let mut rec: INPUT_RECORD = unsafe { std::mem::zeroed() };
                let mut events_read: u32 = 0;
                if unsafe { ReadConsoleInputW(h_in, &mut rec, 1, &mut events_read) } == 0
                    || events_read == 0
                {
                    continue;
                }
                if rec.EventType != (KEY_EVENT as u16) {
                    continue;
                }
                let key = unsafe { rec.Event.KeyEvent };
                if key.bKeyDown == 0 {
                    continue;
                }
                let ch_u16 = unsafe { key.uChar.UnicodeChar };
                if ch_u16 == 0 {
                    continue;
                }
                // Ctrl+C in raw console input
                if ch_u16 == 3 {
                    self.record_resolved(req_id, "deny_cancelled");
                    return ApprovalDecision::Deny;
                }
                if ch_u16 == (b'\r' as u16) || ch_u16 == (b'\n' as u16) {
                    let _ = stderr.write_all(b"\n");
                    let _ = stderr.flush();
                    break;
                }
                // Backspace
                if ch_u16 == 8 {
                    raw_bytes.pop();
                    continue;
                }
                if let Some(ch) = char::from_u32(ch_u16 as u32) {
                    let mut utf8_buf = [0u8; 4];
                    let encoded = ch.encode_utf8(&mut utf8_buf);
                    let _ = stderr.write_all(encoded.as_bytes());
                    let _ = stderr.flush();
                    for &b in encoded.as_bytes() {
                        if raw_bytes.len() < 256 {
                            raw_bytes.push(b);
                        }
                    }
                }
            } else {
                let mut b = [0u8; 1];
                let mut n_read: u32 = 0;
                let ok =
                    unsafe { ReadFile(h_in, b.as_mut_ptr(), 1, &mut n_read, std::ptr::null_mut()) };
                if ok == 0 {
                    self.record_resolved(req_id, "deny_io_error");
                    return ApprovalDecision::Deny;
                }
                if n_read == 0 || b[0] == b'\n' {
                    break;
                }
                if raw_bytes.len() < 256 {
                    raw_bytes.push(b[0]);
                }
            }
        }

        let line = String::from_utf8_lossy(&raw_bytes);

        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => {
                self.record_resolved(req_id, "approve_once");
                ApprovalDecision::AllowOnce
            }
            "a" | "all" => {
                self.record_resolved(req_id, "approve_session");
                ApprovalDecision::AllowCategoryForRun
            }
            _ => {
                self.record_resolved(req_id, "deny");
                ApprovalDecision::Deny
            }
        }
    }

    fn drain_events(&self) -> Vec<EventPayload> {
        std::mem::take(&mut *self.recorded_events.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_terminal_text, TtyApprovalUi, APPROVAL_CANCELLED};
    use kroma_core::tool::policy::{ApprovalDecision, ApprovalHandler, ApprovalRequest};
    use kroma_core::types::EffectClass;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    #[test]
    fn escapes_ansi_and_carriage_return_but_keeps_newline_and_tab() {
        let hostile = "rm -rf /\x1b[2K\rmalicious\n\ttabbed";
        let escaped = sanitize_terminal_text(hostile);
        assert!(!escaped.contains('\x1b'), "ESC must not survive escaping");
        assert!(!escaped.contains('\r'), "CR must not survive escaping");
        assert!(escaped.contains("\\x1b"));
        assert!(escaped.contains("\\r"));
        assert!(escaped.contains('\n'));
        assert!(escaped.contains('\t'));
    }

    #[test]
    fn escapes_bidi_override() {
        let escaped = sanitize_terminal_text("safe\u{202E}gnp.exe");
        assert!(!escaped.contains('\u{202E}'));
        assert!(escaped.contains("\\x202e"));
    }

    /// F-06: an already-cancelled approval (e.g. Ctrl-C) must deny immediately instead of blocking
    /// on a synchronous stdin read. `with_tty_override(true)` forces the interactive path so the
    /// cancellation short-circuit is actually exercised.
    #[test]
    fn cancelled_approval_denies_promptly_without_blocking() {
        let ui = TtyApprovalUi::with_tty_override(true);
        let req = ApprovalRequest {
            tool_name: "bash".into(),
            effect_class: EffectClass::SideEffect,
            is_sensitive_path: false,
            target_path: None,
            diff_preview: None,
            command_preview: Some("rm -rf /tmp/x".into()),
            workspace_root: PathBuf::from("/tmp/workspace"),
        };

        APPROVAL_CANCELLED.store(true, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let decision = ui.request_approval(&req);
        let elapsed = started.elapsed();
        APPROVAL_CANCELLED.store(false, Ordering::SeqCst);

        assert_eq!(
            decision,
            ApprovalDecision::Deny,
            "a cancelled approval must deny"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "a cancelled approval must not block (took {:?})",
            elapsed
        );
        let events = ui.drain_events();
        assert_eq!(events.len(), 2, "expected ApprovalRequested + ApprovalResolved: {:?}", events);
    }
}
