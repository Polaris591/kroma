//! `v1.5.0`: the Kroma terminal presentation layer.
//!
//! This module implements **Design D — "Kroma Zen"**, the default borderless, ultra-minimal
//! terminal style:
//!
//! * a single ultra-minimal header line (`◆ kroma <version> ...`) with no boxes or mascot,
//! * a borderless cyan `› ` prompt so copying code out of the terminal never copies box borders,
//! * four-colour spectrum status dots (`●`) for live streaming events
//!   (purple `thinking`, cyan tools, emerald `edit`/`gate`, amber/coral alerts),
//! * a one-line truecolour spectrum hairline and a compact telemetry footer.
//!
//! Everything degrades gracefully: when `NO_COLOR` is set, `TERM=dumb`, or the output is not a
//! TTY, [`ThemeRenderer`] emits plain ASCII instead of ANSI/Unicode so piped output stays clean.
//! The presentation layer never interprets untrusted text — callers hand it already-decoded
//! values and it sanitises them through [`crate::surface::sanitize_terminal_text`].

use std::fmt::Write as _;
use std::io::IsTerminal;

use crate::surface::sanitize_terminal_text;

/// Environment variable that selects the initial theme (`zen` or `classic`).
pub const KROMA_UI_THEME_ENV: &str = "KROMA_UI_THEME";

/// Environment variable that disables colour even on a TTY.
pub const KROMA_NO_COLOR_ENV: &str = "KROMA_NO_COLOR";

/// The Kroma terminal theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// Design D — borderless, ultra-minimal ("Kroma Zen"). The default.
    #[default]
    Zen,
    /// The pre-`v1.5.0` boxed layout, retained for operators who prefer it.
    Classic,
}

impl Theme {
    /// Parse a theme name (`zen` | `classic`, plus a couple of friendly aliases).
    pub fn parse(raw: &str) -> Result<Theme, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "zen" | "d" | "borderless" => Ok(Theme::Zen),
            "classic" | "legacy" => Ok(Theme::Classic),
            other => Err(format!(
                "unknown theme '{}'; expected 'zen' or 'classic'",
                other
            )),
        }
    }

    /// Canonical lower-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Zen => "zen",
            Theme::Classic => "classic",
        }
    }

    /// The initial theme for a surface: `KROMA_UI_THEME` when set and valid, else [`Theme::Zen`].
    pub fn from_env() -> Theme {
        std::env::var(KROMA_UI_THEME_ENV)
            .ok()
            .and_then(|raw| Theme::parse(&raw).ok())
            .unwrap_or_default()
    }
}

/// 24-bit foreground SGR parameters (without the leading `ESC[` or trailing `m`).
mod sgr {
    pub const BOLD: &str = "1";
    pub const DIM: &str = "2";
    pub const CYAN: &str = "38;2;56;189;248";
    pub const INDIGO: &str = "38;2;99;102;241";
    pub const PURPLE: &str = "38;2;168;85;247";
    pub const EMERALD: &str = "38;2;52;211;153";
    pub const AMBER: &str = "38;2;251;191;36";
    pub const CORAL: &str = "38;2;248;113;113";
    pub const SLATE: &str = "38;2;148;163;184";
    pub const INK: &str = "38;2;241;245;249";
}

/// The Kroma spectrum: cyan → indigo → purple → pink → emerald.
const SPECTRUM: [(u8, u8, u8); 5] = [
    (56, 189, 248),
    (99, 102, 241),
    (168, 85, 247),
    (244, 114, 182),
    (52, 211, 153),
];

/// Interpolate the Kroma spectrum at `t` in `0.0..=1.0`.
fn spectrum_at(t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let segments = (SPECTRUM.len() - 1) as f32;
    let scaled = t * segments;
    let idx = (scaled.floor() as usize).min(SPECTRUM.len() - 2);
    let local = scaled - idx as f32;
    let (r0, g0, b0) = SPECTRUM[idx];
    let (r1, g1, b1) = SPECTRUM[idx + 1];
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * local).round() as u8;
    (lerp(r0, r1), lerp(g0, g1), lerp(b0, b1))
}

/// The kind of live event a status dot represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusDot {
    /// Reasoning stream / summary — purple.
    Thinking,
    /// Read-only tool activity (read / grep / find / ls / code_graph) — cyan.
    Tool,
    /// Successful mutation or verification (edit / write / gate) — emerald.
    Success,
    /// Warning, policy block or tournament notice — amber.
    Alert,
    /// Hard failure — coral.
    Error,
}

impl StatusDot {
    fn color(self) -> &'static str {
        match self {
            StatusDot::Thinking => sgr::PURPLE,
            StatusDot::Tool => sgr::CYAN,
            StatusDot::Success => sgr::EMERALD,
            StatusDot::Alert => sgr::AMBER,
            StatusDot::Error => sgr::CORAL,
        }
    }

    /// A stable, machine-readable label for the dot kind.
    pub fn as_str(self) -> &'static str {
        match self {
            StatusDot::Thinking => "thinking",
            StatusDot::Tool => "tool",
            StatusDot::Success => "success",
            StatusDot::Alert => "alert",
            StatusDot::Error => "error",
        }
    }
}

/// A single line of a borderless inline diff preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub line_number: Option<u32>,
    pub text: String,
}

/// The role of a [`DiffLine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Added,
    Removed,
}

/// Format a token count as a compact human string (`1234` → `1.2k`).
pub fn human_tokens(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
}

/// Shorten a revision id to its display form (`0b92f1ab...` → `0b92f1ab`).
///
/// P2 (`v1.5.0`): the input is sanitised before slicing so a caller that forwards an untrusted
/// revision string (for example from a model or an RPC peer) can never inject control characters
/// into the header or telemetry footer.
pub fn short_rev(rev: &str) -> String {
    let sanitized = sanitize_terminal_text(rev);
    let trimmed = sanitized.trim().trim_start_matches('#');
    if trimmed.is_empty() || trimmed == "<none>" {
        return "<none>".to_string();
    }
    trimmed.chars().take(8).collect()
}

/// Render a revision as `#<short-id>` or `none` when no revision exists yet.
pub fn rev_label(rev: &str) -> String {
    let short = short_rev(rev);
    if short == "<none>" {
        "none".to_string()
    } else {
        format!("#{}", short)
    }
}

/// Pure colour-enablement decision, kept free of environment access so it is unit-testable.
///
/// Colour is disabled when `NO_COLOR` is present and non-empty, when `KROMA_NO_COLOR` is present
/// and non-empty, when `TERM=dumb`, or when the output stream is not a terminal.
pub fn color_enabled_with(no_color: Option<&str>, term: Option<&str>, is_tty: bool) -> bool {
    let disabled = no_color.map(|v| !v.is_empty()).unwrap_or(false)
        || term
            .map(|t| t.eq_ignore_ascii_case("dumb"))
            .unwrap_or(false);
    !disabled && is_tty
}

/// Whether ANSI colour should be emitted on stdout right now.
pub fn color_enabled() -> bool {
    let no_color = effective_no_color(
        std::env::var("NO_COLOR").ok(),
        std::env::var(KROMA_NO_COLOR_ENV).ok(),
    );
    let term = std::env::var("TERM").ok();
    color_enabled_with(
        no_color.as_deref(),
        term.as_deref(),
        std::io::stdout().is_terminal(),
    )
}

/// Resolve the effective `NO_COLOR` value from the standard `NO_COLOR` variable and Kroma's
/// `KROMA_NO_COLOR`.
///
/// P2 (`v1.5.0`): an empty value counts as "unset" per the no-color.org convention, so an empty
/// `NO_COLOR=""` must fall through to `KROMA_NO_COLOR=1` instead of shadowing it.
fn effective_no_color(primary: Option<String>, kroma: Option<String>) -> Option<String> {
    primary
        .filter(|v| !v.is_empty())
        .or_else(|| kroma.filter(|v| !v.is_empty()))
}

/// Terminal width from `COLUMNS`, clamped to a sane range; defaults to 80.
pub fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|w| w.clamp(20, 240))
        .unwrap_or(80)
}

/// Renders Design D ("Kroma Zen") chrome for the terminal surfaces.
#[derive(Debug, Clone, Copy)]
pub struct ThemeRenderer {
    theme: Theme,
    color: bool,
}

impl ThemeRenderer {
    /// Build a renderer with an explicit theme and colour decision.
    pub fn new(theme: Theme, color: bool) -> Self {
        Self { theme, color }
    }

    /// Build a renderer that auto-detects colour support for stdout.
    pub fn detect(theme: Theme) -> Self {
        Self::new(theme, color_enabled())
    }

    /// Build a plain (colour-free) renderer, used for tests, logs and non-TTY output.
    pub fn plain(theme: Theme) -> Self {
        Self::new(theme, false)
    }

    pub fn theme(&self) -> Theme {
        self.theme
    }

    pub fn color_enabled(&self) -> bool {
        self.color
    }

    /// Wrap `text` in an SGR foreground sequence when colour is enabled.
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{}m{}\x1b[0m", code, text)
        } else {
            text.to_string()
        }
    }

    fn paint_rgb(&self, rgb: (u8, u8, u8), text: &str) -> String {
        self.paint(&format!("38;2;{};{};{}", rgb.0, rgb.1, rgb.2), text)
    }

    /// The ultra-minimal header line, with no boxes and no mascot.
    pub fn header(
        &self,
        model: &str,
        thinking: &str,
        protocol: &str,
        cache_hit_pct: f64,
        rev: &str,
        workspace: &str,
    ) -> String {
        let model = sanitize_terminal_text(model);
        let thinking = sanitize_terminal_text(thinking);
        let protocol = sanitize_terminal_text(protocol);
        let workspace = sanitize_terminal_text(workspace);
        let sep = if self.color {
            self.paint(sgr::DIM, "·")
        } else {
            "·".to_string()
        };
        let diamond = if self.color {
            self.paint(sgr::CYAN, "◆")
        } else {
            "◆".to_string()
        };
        let name = if self.color {
            self.paint(sgr::BOLD, "kroma")
        } else {
            "kroma".to_string()
        };
        let version = self.paint(sgr::DIM, crate::version());
        let model_seg = if self.color {
            format!(
                "{}:{}",
                self.paint(sgr::INK, &model),
                self.paint(sgr::PURPLE, &thinking)
            )
        } else {
            format!("{}:{}", model, thinking)
        };
        let proto_seg = self.paint(sgr::CYAN, &protocol);
        let cache_seg = self.paint(sgr::EMERALD, &format!("cache {:.1}%", cache_hit_pct));
        let rev_seg = self.paint(sgr::AMBER, &format!("rev {}", rev_label(rev)));
        let ws_seg = self.paint(sgr::SLATE, &workspace);

        format!(
            "{diamond} {name} {version}   {model_seg}  {sep}  {proto_seg}  {sep}  {cache_seg}  {sep}  {rev_seg}  {sep}  {ws_seg}"
        )
    }

    /// The borderless prompt prefix (`› ` in cyan).
    pub fn prompt_prefix(&self) -> String {
        if self.color {
            format!("{} ", self.paint(sgr::CYAN, "›"))
        } else {
            "› ".to_string()
        }
    }

    /// One live-event line: a coloured `●`, the label, and a dim detail.
    pub fn status_line(&self, dot: StatusDot, label: &str, detail: &str) -> String {
        let bullet = self.paint(dot.color(), "●");
        let label = self.paint(dot.color(), &sanitize_terminal_text(label));
        let detail = self.paint(sgr::SLATE, &sanitize_terminal_text(detail));
        if detail.is_empty() {
            format!("{} {}", bullet, label)
        } else {
            format!("{} {}  {}", bullet, label, detail)
        }
    }

    /// A borderless inline diff preview: one-character prismatic left bar, coloured `+`/`-`.
    pub fn diff_preview(&self, lines: &[DiffLine]) -> String {
        let bar = if self.color {
            self.paint(sgr::INDIGO, "▎")
        } else {
            "|".to_string()
        };
        let mut out = String::new();
        for line in lines {
            let (marker, sgr_code) = match line.kind {
                DiffKind::Context => (' ', sgr::DIM),
                DiffKind::Added => ('+', sgr::EMERALD),
                DiffKind::Removed => ('-', sgr::CORAL),
            };
            let number = line
                .line_number
                .map(|n| format!("{:>5}", n))
                .unwrap_or_else(|| "     ".to_string());
            let text = sanitize_terminal_text(&line.text);
            let body = self.paint(sgr_code, &format!("{}{} {}", marker, number, text));
            let _ = writeln!(out, "{} {}", bar, body);
        }
        out
    }

    /// Parse a unified-diff text block and render it as a borderless inline preview.
    ///
    /// Lines beginning with `+`/`-` (excluding the `+++`/`---` file headers) become added/removed
    /// entries; everything else is context. Leading `@@` hunk headers are dropped.
    pub fn preview_from_unified_diff(&self, unified: &str) -> String {
        let mut lines = Vec::new();
        for raw in unified.lines() {
            if raw.starts_with("@@") || raw.starts_with("+++") || raw.starts_with("---") {
                continue;
            }
            let (kind, text) = if let Some(rest) = raw.strip_prefix('+') {
                (DiffKind::Added, rest.to_string())
            } else if let Some(rest) = raw.strip_prefix('-') {
                (DiffKind::Removed, rest.to_string())
            } else if let Some(rest) = raw.strip_prefix(' ') {
                (DiffKind::Context, rest.to_string())
            } else {
                (DiffKind::Context, raw.to_string())
            };
            lines.push(DiffLine {
                kind,
                line_number: None,
                text,
            });
        }
        self.diff_preview(&lines)
    }

    /// A gate / verification outcome line.
    pub fn gate_line(&self, command: &str, passed: bool, elapsed_secs: f64) -> String {
        let command = sanitize_terminal_text(command);
        let dot = if passed {
            StatusDot::Success
        } else {
            StatusDot::Error
        };
        let verdict = if passed {
            self.paint(sgr::EMERALD, &format!("PASSED ({:.2}s)", elapsed_secs))
        } else {
            self.paint(sgr::CORAL, &format!("FAILED ({:.2}s)", elapsed_secs))
        };
        format!(
            "{} {}  {} → {}",
            self.paint(dot.color(), "●"),
            self.paint(dot.color(), "gate"),
            self.paint(sgr::SLATE, &command),
            verdict
        )
    }

    /// A compact telemetry footer for the end of a turn.
    pub fn telemetry_footer(
        &self,
        self_tokens: u64,
        child_tokens: u64,
        cache_hit_pct: f64,
        rev: &str,
        warnings: usize,
    ) -> String {
        let sep = self.paint(sgr::DIM, "·");
        let self_seg = format!(
            "self {}",
            self.paint(sgr::INK, &format!("{} tok", human_tokens(self_tokens)))
        );
        let child_seg = format!(
            "child {}",
            self.paint(sgr::INK, &format!("{} tok", human_tokens(child_tokens)))
        );
        let cache_seg = self.paint(sgr::EMERALD, &format!("cache {:.1}%", cache_hit_pct));
        let rev_seg = self.paint(sgr::AMBER, &format!("rev {}", rev_label(rev)));
        let warn_seg = if warnings == 0 {
            self.paint(sgr::EMERALD, "0 warnings")
        } else {
            self.paint(sgr::AMBER, &format!("{} warnings", warnings))
        };
        format!(
            "  {} + {}  {}  {}  {}  {}",
            self_seg, child_seg, sep, cache_seg, sep, rev_seg
        ) + &format!("  {}  {}", sep, warn_seg)
    }

    /// The one-line truecolour spectrum hairline; ASCII `-` when colour is disabled.
    pub fn hairline(&self, width: usize) -> String {
        let width = width.max(1);
        if !self.color {
            return "-".repeat(width);
        }
        let mut out = String::with_capacity(width * 20);
        for i in 0..width {
            let t = if width <= 1 {
                0.0
            } else {
                i as f32 / (width - 1) as f32
            };
            out.push_str(&self.paint_rgb(spectrum_at(t), "━"));
        }
        out
    }

    /// The full startup banner (header line, session facts, and a one-line hint).
    #[allow(clippy::too_many_arguments)]
    pub fn banner(
        &self,
        model: &str,
        thinking: &str,
        protocol: &str,
        cache_hit_pct: f64,
        rev: &str,
        workspace: &str,
        session_id: &str,
        policy: &str,
        turns: u64,
    ) -> String {
        let header = self.header(model, thinking, protocol, cache_hit_pct, rev, workspace);
        let facts = format!(
            "model: {} | thinking: {} | protocol: {} | session: {} | policy: {}",
            sanitize_terminal_text(model),
            sanitize_terminal_text(thinking),
            sanitize_terminal_text(protocol),
            sanitize_terminal_text(session_id),
            sanitize_terminal_text(policy),
        );
        let hint = format!(
            "Kroma v{} Interactive REPL · turns: {} · Type a task prompt, or /help for all commands. /theme zen|classic",
            crate::version(),
            turns
        );
        format!(
            "{header}\n  {facts}\n  {hint}\n",
            header = header,
            facts = self.paint(sgr::SLATE, &facts),
            hint = self.paint(sgr::DIM, &hint),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_parses_known_names_and_rejects_unknown() {
        assert_eq!(Theme::parse("zen").unwrap(), Theme::Zen);
        assert_eq!(Theme::parse(" ZEN ").unwrap(), Theme::Zen);
        assert_eq!(Theme::parse("classic").unwrap(), Theme::Classic);
        assert!(Theme::parse("neon").is_err());
        assert_eq!(Theme::default(), Theme::Zen);
    }

    #[test]
    fn color_decision_respects_no_color_dumb_term_and_tty() {
        assert!(color_enabled_with(None, Some("xterm-256color"), true));
        assert!(!color_enabled_with(Some("1"), Some("xterm-256color"), true));
        assert!(!color_enabled_with(None, Some("dumb"), true));
        assert!(!color_enabled_with(None, Some("xterm-256color"), false));
        // An empty NO_COLOR is treated as "not set" per the no-color.org convention.
        assert!(color_enabled_with(Some(""), Some("xterm-256color"), true));
    }

    #[test]
    fn effective_no_color_treats_empty_values_as_unset() {
        // An empty `NO_COLOR` must not shadow a set `KROMA_NO_COLOR` (P2 regression).
        assert_eq!(
            effective_no_color(Some(String::new()), Some("1".to_string())),
            Some("1".to_string())
        );
        assert_eq!(
            effective_no_color(Some("1".to_string()), Some(String::new())),
            Some("1".to_string())
        );
        assert_eq!(effective_no_color(Some(String::new()), None), None);
        assert_eq!(effective_no_color(None, Some(String::new())), None);
        assert_eq!(effective_no_color(None, None), None);
    }

    #[test]
    fn short_rev_sanitizes_control_characters() {
        // P2 regression: an untrusted revision string must not inject control chars into chrome.
        let rendered = short_rev("#\u{1b}[31mred");
        assert!(!rendered.contains('\u{1b}'), "leaked ESC: {rendered:?}");
        assert!(rendered.contains("\\x1b"), "got: {rendered:?}");
        assert_eq!(short_rev("r-0b92f1ab"), "r-0b92f1");
        assert_eq!(short_rev("#0b92f1ab"), "0b92f1ab");
        assert_eq!(short_rev("  "), "<none>");
    }

    #[test]
    fn plain_renderer_emits_no_escape_sequences() {
        let r = ThemeRenderer::plain(Theme::Zen);
        let header = r.header("m", "high", "responses", 99.6, "0b92f1ab", "/ws");
        let line = r.status_line(StatusDot::Thinking, "thinking", "checking");
        let footer = r.telemetry_footer(3100, 0, 99.6, "0b92f1ab", 0);
        let hair = r.hairline(8);
        for text in [&header, &line, &footer, &hair] {
            assert!(
                !text.contains('\x1b'),
                "plain output leaked ESC: {:?}",
                text
            );
        }
        assert!(hair.chars().all(|c| c == '-'));
        assert!(header.starts_with("◆ kroma "));
    }

    #[test]
    fn coloured_renderer_emits_truecolor_sequences() {
        let r = ThemeRenderer::new(Theme::Zen, true);
        let header = r.header(
            "deepseek",
            "high",
            "responses",
            99.6,
            "0b92f1ab",
            "/opt/kroma",
        );
        assert!(
            header.contains("\x1b[38;2;56;189;248m"),
            "header: {header:?}"
        );
        assert!(
            header.contains('◆') && header.contains("kroma"),
            "header: {header:?}"
        );
        let hair = r.hairline(5);
        assert!(hair.contains("\x1b[38;2;"), "hairline: {hair:?}");
        assert_eq!(hair.matches('━').count(), 5);
        assert_eq!(r.prompt_prefix(), "\x1b[38;2;56;189;248m›\x1b[0m ");
    }

    #[test]
    fn status_dots_use_expected_spectrum_colors() {
        let r = ThemeRenderer::new(Theme::Zen, true);
        assert!(r
            .status_line(StatusDot::Thinking, "thinking", "x")
            .contains("\x1b[38;2;168;85;247m"));
        assert!(r
            .status_line(StatusDot::Tool, "read", "x")
            .contains("\x1b[38;2;56;189;248m"));
        assert!(r
            .status_line(StatusDot::Success, "edit", "x")
            .contains("\x1b[38;2;52;211;153m"));
        assert!(r
            .status_line(StatusDot::Alert, "policy", "x")
            .contains("\x1b[38;2;251;191;36m"));
        assert!(r
            .status_line(StatusDot::Error, "gate", "x")
            .contains("\x1b[38;2;248;113;113m"));
    }

    #[test]
    fn diff_preview_marks_added_removed_and_is_borderless() {
        let r = ThemeRenderer::new(Theme::Zen, true);
        let lines = vec![
            DiffLine {
                kind: DiffKind::Context,
                line_number: Some(92),
                text: "let x = 1;".into(),
            },
            DiffLine {
                kind: DiffKind::Removed,
                line_number: Some(93),
                text: "old();".into(),
            },
            DiffLine {
                kind: DiffKind::Added,
                line_number: Some(93),
                text: "new();".into(),
            },
        ];
        let out = r.diff_preview(&lines);
        assert!(out.contains("+   93 new();"), "out: {out:?}");
        assert!(out.contains("-   93 old();"), "out: {out:?}");
        assert!(out.contains("    92 let x = 1;"), "out: {out:?}");
        // Borderless: no box-drawing corners, only the thin left bar.
        assert!(!out.contains('╭') && !out.contains('│') && !out.contains('╰'));
        assert!(out.contains('▎'));
        let plain = ThemeRenderer::plain(Theme::Zen).diff_preview(&lines);
        assert!(!plain.contains('\x1b'));
        assert!(plain.contains('|'));
    }

    #[test]
    fn unified_diff_preview_classifies_lines() {
        let r = ThemeRenderer::plain(Theme::Zen);
        let diff =
            "@@ -1,2 +1,2 @@\n--- a/f.rs\n+++ b/f.rs\n let x = 1;\n-let y = 2;\n+let y = 3;\n";
        let out = r.preview_from_unified_diff(diff);
        assert!(out.contains("let y = 3;"), "out: {out:?}");
        assert!(out.contains("let y = 2;"), "out: {out:?}");
        assert!(out.contains("let x = 1;"), "out: {out:?}");
        assert!(out.contains("+ ") && out.contains("- "), "out: {out:?}");
        assert!(!out.contains("@@") && !out.contains("+++"));
    }

    #[test]
    fn spectrum_hairline_is_monotonic_and_bounded() {
        assert_eq!(spectrum_at(0.0), SPECTRUM[0]);
        assert_eq!(spectrum_at(1.0), SPECTRUM[4]);
        let mid = spectrum_at(0.5);
        assert!(mid.0 > 0 && mid.1 > 0 && mid.2 > 0);
    }

    #[test]
    fn helper_formatters_are_stable() {
        assert_eq!(human_tokens(999), "999");
        assert_eq!(human_tokens(3100), "3.1k");
        assert_eq!(short_rev("#0b92f1ab99"), "0b92f1ab");
        assert_eq!(short_rev("<none>"), "<none>");
        assert_eq!(short_rev(""), "<none>");
    }

    #[test]
    fn gate_line_reports_pass_and_fail() {
        let r = ThemeRenderer::plain(Theme::Zen);
        assert!(r
            .gate_line("cargo test", true, 0.84)
            .contains("PASSED (0.84s)"));
        assert!(r
            .gate_line("cargo test", false, 1.5)
            .contains("FAILED (1.50s)"));
    }

    #[test]
    fn untrusted_text_is_sanitised_before_colouring() {
        let r = ThemeRenderer::new(Theme::Zen, true);
        let header = r.header("evil\x1b[31m", "high", "responses", 1.0, "r", "/ws");
        // The raw ESC from the model id must not survive as a live escape; it is rendered as text.
        assert!(header.contains("\\x1b[31m"));
    }
}
