use harness_core::tool::policy::{ApprovalDecision, ApprovalHandler, ApprovalRequest};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// F-06: set by the CLI's Ctrl-C handler so an in-flight approval prompt stops waiting for input
/// and denies instead of blocking the runtime until the user happens to press a key.
pub static APPROVAL_CANCELLED: AtomicBool = AtomicBool::new(false);

/// How long the approval read blocks before re-checking the cancellation flag.
const APPROVAL_POLL_INTERVAL_MS: libc::c_int = 100;

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
        let interactive_tty = unsafe {
            libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDERR_FILENO) == 1
        };
        Self { interactive_tty }
    }

    pub fn with_tty_override(interactive_tty: bool) -> Self {
        Self { interactive_tty }
    }

    pub fn format_prompt_banner(req: &ApprovalRequest) -> String {
        let mut out = String::new();
        if req.is_sensitive_path {
            out.push_str("=== [HIGH-RISK SENSITIVE PATH APPROVAL REQUIRED] ===\n");
        } else {
            out.push_str("=== [HARNESS TOOL APPROVAL REQUIRED] ===\n");
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
        if APPROVAL_CANCELLED.load(Ordering::SeqCst) {
            return ApprovalDecision::Deny;
        }

        let banner = Self::format_prompt_banner(req);
        let mut stderr = io::stderr();
        if stderr.write_all(banner.as_bytes()).is_err() || stderr.flush().is_err() {
            return ApprovalDecision::Deny;
        }

        // F-06: wait for input in bounded poll slices so a Ctrl-C (which sets `APPROVAL_CANCELLED`
        // from the async signal task) aborts the prompt instead of leaving the runtime wedged on a
        // synchronous read that will never return.
        loop {
            if APPROVAL_CANCELLED.load(Ordering::SeqCst) {
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
                return ApprovalDecision::Deny;
            }
            if rc == 0 {
                continue;
            }
            break;
        }

        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return ApprovalDecision::Deny;
        }

        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => ApprovalDecision::AllowOnce,
            "a" | "all" => ApprovalDecision::AllowCategoryForRun,
            _ => ApprovalDecision::Deny,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_terminal_text;

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
}
