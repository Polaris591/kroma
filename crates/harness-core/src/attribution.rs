//! Gate 4 — predictive post-mutation validation and candidate failure attribution.
//!
//! Two things live here:
//!
//! 1. [`PredictiveValidator`] runs a deterministic, in-process structural check of a file right
//!    after `edit`/`write` mutated it. It never spawns a process and never rolls anything back: a
//!    warning is advisory evidence attached to the tool result, so a false positive can never
//!    destroy a model or operator edit (`false_positive_rollback_rate == 0.0`).
//! 2. [`MutationLedger`] + [`AttributionEngine`] record what was changed and, when a verification
//!    command fails, rank the recorded mutations against the compiler/test diagnostics parsed out of
//!    the failure output. The output is always framed as a **candidate** attribution — the blueprint
//!    forbids claiming root-cause certainty from a single failing run.

use crate::prompt::{escape_fence_attribute, sanitize_untrusted_doc};
use crate::revision::RevisionId;
use crate::workspace::WorkspaceCap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

/// Hard cap on the file size the structural checker will tokenize.
const MAX_VALIDATED_BYTES: u64 = 2 * 1024 * 1024;
/// Upper bound on reported structural issues per file, so a hostile file cannot flood the context.
const MAX_ISSUES_PER_FILE: usize = 12;
/// Upper bound on the rendered candidate block appended to a failure prompt.
const ATTRIBUTION_BLOCK_MAX_BYTES: usize = 4096;
/// Upper bound on retained mutation records; the oldest are dropped first.
pub const MAX_LEDGER_RECORDS: usize = 128;
/// Line adjacency that still counts as "the diagnostic points at this edit".
const ATTRIBUTION_ADJACENT_LINES: usize = 5;

/// The report never claims certainty; this literal is carried in the rendered block and in
/// `AttributionReport::framing` so a downstream consumer cannot mistake it for a root cause.
pub const CANDIDATE_ATTRIBUTION_FRAMING: &str = "candidate_attribution";

// ---------------------------------------------------------------------------------------------
// Predictive structural validation
// ---------------------------------------------------------------------------------------------

/// One structural problem found in a mutated file, with exact 1-based line/column coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictiveIssue {
    pub line: usize,
    pub column: usize,
    pub kind: String,
    pub message: String,
}

/// Compact telemetry mirror of a [`PredictiveReport`], small enough to persist in the WAL transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PredictiveSummary {
    pub status: String,
    pub issue_count: usize,
    pub duration_ms: u64,
    pub summary: String,
}

/// Result of one post-mutation structural check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictiveReport {
    pub path: String,
    pub language: String,
    /// `ok` | `warning` | `skipped`.
    pub status: String,
    pub issues: Vec<PredictiveIssue>,
    pub duration_ms: u64,
    pub skipped_reason: Option<String>,
}

impl PredictiveReport {
    fn new(path: &str, language: &str, status: &str, issues: Vec<PredictiveIssue>, duration_ms: u64) -> Self {
        Self {
            path: path.to_string(),
            language: language.to_string(),
            status: status.to_string(),
            issues,
            duration_ms,
            skipped_reason: None,
        }
    }

    pub fn skipped(path: &str, language: &str, reason: &str, duration_ms: u64) -> Self {
        Self {
            path: path.to_string(),
            language: language.to_string(),
            status: "skipped".to_string(),
            issues: Vec::new(),
            duration_ms,
            skipped_reason: Some(reason.to_string()),
        }
    }

    pub fn issue_count(&self) -> usize {
        self.issues.len()
    }

    pub fn summary(&self) -> PredictiveSummary {
        let summary = if self.issues.is_empty() {
            match &self.skipped_reason {
                Some(reason) => format!("{} (not checked: {})", self.path, reason),
                None => format!("{} clean", self.path),
            }
        } else {
            format!(
                "{}: {} structural issue(s); first at line {} col {} ({})",
                self.path,
                self.issues.len(),
                self.issues[0].line,
                self.issues[0].column,
                self.issues[0].message
            )
        };
        PredictiveSummary {
            status: self.status.clone(),
            issue_count: self.issues.len(),
            duration_ms: self.duration_ms,
            summary,
        }
    }

    /// Render the advisory block appended to an `edit`/`write` result, or `None` when clean.
    ///
    /// The block is explicitly marked advisory: the mutation has already been applied and is never
    /// rolled back because of it.
    pub fn render_block(&self) -> Option<String> {
        if self.issues.is_empty() {
            return None;
        }
        let mut out = String::new();
        out.push_str(&format!(
            "<predictive_validation status=\"warning\" path=\"{}\" language=\"{}\" issues=\"{}\">\n",
            escape_fence_attribute(&self.path),
            escape_fence_attribute(&self.language),
            self.issues.len()
        ));
        out.push_str(
            "advisory only — the change WAS applied and was NOT rolled back. Confirm with a real \
             build/test before relying on it.\n",
        );
        for issue in &self.issues {
            out.push_str(&format!(
                "- line {} col {}: {} ({})\n",
                issue.line,
                issue.column,
                sanitize_untrusted_doc(&issue.message),
                escape_fence_attribute(&issue.kind)
            ));
        }
        out.push_str("</predictive_validation>");
        Some(out)
    }
}

/// Post-mutation structural checker (Gate 4). `enabled == false` short-circuits to a `skipped`
/// report so the whole feature can be turned off with `--no-predictive-check`.
#[derive(Debug, Clone, Copy)]
pub struct PredictiveValidator {
    enabled: bool,
}

impl Default for PredictiveValidator {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl PredictiveValidator {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Read `rel_path` back from the workspace (symlink-safe) and validate it.
    pub fn validate_file(&self, workspace: &WorkspaceCap, rel_path: &str) -> PredictiveReport {
        let language = language_name(rel_path);
        if !self.enabled {
            return PredictiveReport::skipped(rel_path, language, "predictive check disabled", 0);
        }
        if language == "unknown" {
            return PredictiveReport::skipped(rel_path, language, "unsupported file type", 0);
        }
        match read_workspace_text(workspace, rel_path) {
            Ok(content) => self.validate_content(rel_path, &content),
            Err(reason) => PredictiveReport::skipped(rel_path, language, &reason, 0),
        }
    }

    /// Validate already-known content (no filesystem access), so the checker is trivially testable.
    pub fn validate_content(&self, rel_path: &str, content: &str) -> PredictiveReport {
        let language = language_name(rel_path);
        if !self.enabled {
            return PredictiveReport::skipped(rel_path, language, "predictive check disabled", 0);
        }
        if language == "unknown" {
            return PredictiveReport::skipped(rel_path, language, "unsupported file type", 0);
        }
        let started = std::time::Instant::now();
        let issues = structural_issues(language, content);
        let duration_ms = started.elapsed().as_millis() as u64;
        let status = if issues.is_empty() { "ok" } else { "warning" };
        PredictiveReport::new(rel_path, language, status, issues, duration_ms)
    }
}

/// Read a workspace file back as UTF-8, refusing symlinks and bounding the read size.
///
/// Returns `Err(reason)` (never panics) so callers can surface a `skipped` report verbatim.
pub fn read_workspace_text(workspace: &WorkspaceCap, rel_path: &str) -> Result<String, String> {
    let (_, mut file) = workspace
        .open_existing_no_symlink(rel_path)
        .map_err(|err| format!("unreadable: {}", err))?;
    let mut bytes = Vec::new();
    let mut limited = (&mut file).take(MAX_VALIDATED_BYTES + 1);
    limited
        .read_to_end(&mut bytes)
        .map_err(|err| format!("read failed: {}", err))?;
    if bytes.len() as u64 > MAX_VALIDATED_BYTES {
        return Err("file exceeds the predictive validation size cap".to_string());
    }
    String::from_utf8(bytes).map_err(|_| "not valid UTF-8".to_string())
}

/// Map a path to the checker family used for it.
pub fn language_name(path: &str) -> &'static str {
    let ext = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "json" => "json",
        "toml" => "toml",
        "rs" => "rust",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" | "tsx" => "typescript",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "java" => "java",
        _ => "unknown",
    }
}

fn structural_issues(language: &str, content: &str) -> Vec<PredictiveIssue> {
    match language {
        "json" => json_issues(content),
        "toml" => toml_issues(content),
        other => {
            let Some(lang) = CodeLang::from_name(other) else {
                return Vec::new();
            };
            scan_code(lang, content)
        }
    }
}

fn json_issues(content: &str) -> Vec<PredictiveIssue> {
    let mut issues = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if let Some(marker) = line_conflict_marker(line) {
            issues.push(PredictiveIssue {
                line: idx + 1,
                column: 1,
                kind: "conflict_marker".to_string(),
                message: format!("unresolved version-control conflict marker '{}'", marker),
            });
        }
    }
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(_) => issues,
        Err(err) => {
            issues.push(PredictiveIssue {
                line: err.line(),
                column: err.column(),
                kind: "json_syntax".to_string(),
                message: err.to_string(),
            });
            issues.truncate(MAX_ISSUES_PER_FILE);
            issues
        }
    }
}

/// Deliberately conservative TOML structure check: section headers must close their brackets and
/// single-line strings must be balanced. Anything richer would risk the false positives the
/// blueprint forbids, so dotted keys and inline tables are left to a real parser.
fn toml_issues(content: &str) -> Vec<PredictiveIssue> {
    let mut issues = Vec::new();
    let mut in_multiline: Option<char> = None;
    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        if let Some(quote) = in_multiline {
            if raw_line.contains(&format!("{0}{0}{0}", quote)) {
                in_multiline = None;
            }
            continue;
        }
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(marker) = line_conflict_marker(raw_line) {
            issues.push(PredictiveIssue {
                line: line_no,
                column: 1,
                kind: "conflict_marker".to_string(),
                message: format!("unresolved version-control conflict marker '{}'", marker),
            });
            continue;
        }
        // Section header: `[table]` / `[[array of tables]]`.
        if trimmed.starts_with('[') {
            let (open, close) = if trimmed.starts_with("[[") {
                ("[[", "]]")
            } else {
                ("[", "]")
            };
            let body = trimmed.split('#').next().unwrap_or(trimmed).trim();
            if !body.starts_with(open) || !body.ends_with(close) || body.len() <= open.len() + close.len() - 1 {
                issues.push(PredictiveIssue {
                    line: line_no,
                    column: 1,
                    kind: "toml_structure".to_string(),
                    message: format!("malformed TOML section header: {}", trimmed),
                });
                continue;
            }
            continue;
        }
        // Multi-line string openers.
        if trimmed.contains("\"\"\"") {
            if trimmed.matches("\"\"\"").count() % 2 == 1 {
                in_multiline = Some('"');
            }
            continue;
        }
        if trimmed.contains("'''") {
            if trimmed.matches("'''").count() % 2 == 1 {
                in_multiline = Some('\'');
            }
            continue;
        }
        // Strip a trailing comment that is not inside a string, then require balanced quotes.
        let (code, _) = split_toml_comment(trimmed);
        if code.matches('"').count() % 2 == 1 {
            issues.push(PredictiveIssue {
                line: line_no,
                column: 1,
                kind: "unclosed_string".to_string(),
                message: "unterminated basic string (\"…) on this line".to_string(),
            });
        } else if code.matches('\'').count() % 2 == 1 {
            issues.push(PredictiveIssue {
                line: line_no,
                column: 1,
                kind: "unclosed_string".to_string(),
                message: "unterminated literal string ('…) on this line".to_string(),
            });
        }
        if issues.len() >= MAX_ISSUES_PER_FILE {
            break;
        }
    }
    issues
}

/// Split `line` at the first `#` that is not inside a single/double quoted span.
fn split_toml_comment(line: &str) -> (String, Option<String>) {
    let mut out = String::new();
    let mut comment = None;
    let mut quote: Option<char> = None;
    for ch in line.chars() {
        match quote {
            Some(q) => {
                out.push(ch);
                if ch == q {
                    quote = None;
                }
            }
            None => {
                if ch == '#' {
                    comment = Some(line[out.len()..].to_string());
                    break;
                }
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                }
                out.push(ch);
            }
        }
    }
    (out, comment)
}

fn line_conflict_marker(line: &str) -> Option<&'static str> {
    let chars: Vec<char> = line.chars().take(8).collect();
    if chars.len() < 7 {
        return None;
    }
    let c = chars[0];
    if c != '<' && c != '=' && c != '>' {
        return None;
    }
    if !chars[..7].iter().all(|ch| *ch == c) {
        return None;
    }
    match chars.get(7) {
        None | Some(' ') | Some('\t') | Some('\r') | Some('\n') => Some(match c {
            '<' => "<<<<<<<",
            '=' => "=======",
            '>' => ">>>>>>>",
            _ => return None,
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Lexical-aware delimiter / string / comment balance scanner
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeLang {
    Rust,
    JavaScript,
    TypeScript,
    Python,
    Go,
    C,
    Cpp,
    Java,
}

impl CodeLang {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "rust" => CodeLang::Rust,
            "javascript" => CodeLang::JavaScript,
            "typescript" => CodeLang::TypeScript,
            "python" => CodeLang::Python,
            "go" => CodeLang::Go,
            "c" => CodeLang::C,
            "cpp" => CodeLang::Cpp,
            "java" => CodeLang::Java,
            _ => return None,
        })
    }

    fn has_slash_line_comment(self) -> bool {
        !matches!(self, CodeLang::Python)
    }

    fn has_hash_line_comment(self) -> bool {
        matches!(self, CodeLang::Python)
    }

    fn has_block_comment(self) -> bool {
        !matches!(self, CodeLang::Python)
    }

    /// Only Rust nests `/* /* */ */`; in C/JS/Go/Java the first `*/` closes the comment.
    fn nests_block_comment(self) -> bool {
        matches!(self, CodeLang::Rust)
    }
}

#[derive(Debug, Clone, Copy)]
enum ScanState {
    Normal,
    LineComment,
    BlockComment { depth: usize, open_line: usize, open_col: usize },
    /// `raw_hashes = Some(h)` means the terminator is `"` followed by `h` `#` characters.
    Str {
        quote: char,
        triple: bool,
        raw_hashes: Option<usize>,
        escape: bool,
        open_line: usize,
        open_col: usize,
        multiline: bool,
    },
}

fn scan_code(lang: CodeLang, content: &str) -> Vec<PredictiveIssue> {
    let chars: Vec<char> = content.chars().collect();
    let n = chars.len();
    let mut issues: Vec<PredictiveIssue> = Vec::new();
    let mut stack: Vec<(char, usize, usize)> = Vec::new();
    let mut state = ScanState::Normal;
    let mut i = 0usize;
    let mut line = 1usize;
    let mut col = 1usize;

    macro_rules! advance {
        ($count:expr) => {{
            for _ in 0..$count {
                if i >= n {
                    break;
                }
                if chars[i] == '\n' {
                    line += 1;
                    col = 1;
                } else {
                    col += 1;
                }
                i += 1;
            }
        }};
    }

    while i < n && issues.len() < MAX_ISSUES_PER_FILE {
        let c = chars[i];
        match state {
            ScanState::LineComment => {
                if c == '\n' {
                    state = ScanState::Normal;
                }
                advance!(1);
            }
            ScanState::BlockComment { depth, open_line, open_col } => {
                if lang.nests_block_comment() && c == '/' && chars.get(i + 1) == Some(&'*') {
                    state = ScanState::BlockComment { depth: depth + 1, open_line, open_col };
                    advance!(2);
                    continue;
                }
                if c == '*' && chars.get(i + 1) == Some(&'/') {
                    if depth <= 1 {
                        state = ScanState::Normal;
                    } else {
                        state = ScanState::BlockComment { depth: depth - 1, open_line, open_col };
                    }
                    advance!(2);
                    continue;
                }
                advance!(1);
                if i >= n && depth >= 1 {
                    issues.push(PredictiveIssue {
                        line: open_line,
                        column: open_col,
                        kind: "unclosed_block_comment".to_string(),
                        message: "unterminated block comment /*".to_string(),
                    });
                }
            }
            ScanState::Str { quote, triple, raw_hashes, escape, open_line, open_col, multiline } => {
                if escape && c == '\\' {
                    advance!(2);
                    continue;
                }
                let closes = if triple {
                    c == quote && chars.get(i + 1) == Some(&quote) && chars.get(i + 2) == Some(&quote)
                } else if let Some(h) = raw_hashes {
                    c == quote
                        && (0..h).all(|k| chars.get(i + 1 + k) == Some(&'#'))
                } else {
                    c == quote
                };
                if closes {
                    let consumed = if triple { 3 } else { 1 + raw_hashes.unwrap_or(0) };
                    state = ScanState::Normal;
                    advance!(consumed);
                    continue;
                }
                if c == '\n' && !multiline {
                    issues.push(PredictiveIssue {
                        line: open_line,
                        column: open_col,
                        kind: "unclosed_string".to_string(),
                        message: format!("unterminated string literal starting with {}", quote),
                    });
                    state = ScanState::Normal;
                    advance!(1);
                    continue;
                }
                advance!(1);
                if i >= n {
                    issues.push(PredictiveIssue {
                        line: open_line,
                        column: open_col,
                        kind: "unclosed_string".to_string(),
                        message: format!("unterminated string literal starting with {}", quote),
                    });
                }
            }
            ScanState::Normal => {
                // Conflict markers are only meaningful at column 1 outside strings/comments.
                if col == 1 {
                    if let Some(marker) = line_conflict_marker(&chars[i..].iter().take(8).collect::<String>()) {
                        issues.push(PredictiveIssue {
                            line,
                            column: 1,
                            kind: "conflict_marker".to_string(),
                            message: format!("unresolved version-control conflict marker '{}'", marker),
                        });
                        while i < n && chars[i] != '\n' {
                            advance!(1);
                        }
                        continue;
                    }
                }
                // Line comments.
                if lang.has_slash_line_comment() && c == '/' && chars.get(i + 1) == Some(&'/') {
                    state = ScanState::LineComment;
                    advance!(2);
                    continue;
                }
                if lang.has_hash_line_comment() && c == '#' {
                    state = ScanState::LineComment;
                    advance!(1);
                    continue;
                }
                // Block comments.
                if lang.has_block_comment() && c == '/' && chars.get(i + 1) == Some(&'*') {
                    state = ScanState::BlockComment { depth: 1, open_line: line, open_col: col };
                    advance!(2);
                    continue;
                }
                // String openers, language by language.
                if let Some(consumed) = open_string(lang, &chars, i, &mut state, line, col) {
                    advance!(consumed);
                    continue;
                }
                if let Some(consumed) = open_char_literal(lang, &chars, i) {
                    advance!(consumed);
                    continue;
                }
                match c {
                    '(' | '[' | '{' => {
                        stack.push((c, line, col));
                    }
                    ')' | ']' | '}' => {
                        let expected = match c {
                            ')' => '(',
                            ']' => '[',
                            _ => '{',
                        };
                        if stack.last().map(|(d, _, _)| *d) == Some(expected) {
                            stack.pop();
                        } else if let Some(pos) = stack.iter().rposition(|(d, _, _)| *d == expected) {
                            for (unclosed, ul, uc) in stack.drain(pos..).rev() {
                                if unclosed != expected {
                                    issues.push(PredictiveIssue {
                                        line: ul,
                                        column: uc,
                                        kind: "unclosed_delimiter".to_string(),
                                        message: format!(
                                            "unclosed '{}' — closed by mismatched '{}'",
                                            unclosed, c
                                        ),
                                    });
                                }
                            }
                        } else {
                            issues.push(PredictiveIssue {
                                line,
                                column: col,
                                kind: "mismatched_delimiter".to_string(),
                                message: format!("unmatched closing '{}'", c),
                            });
                        }
                    }
                    _ => {}
                }
                advance!(1);
            }
        }
    }

    if issues.len() < MAX_ISSUES_PER_FILE {
        for (delim, l, cc) in stack.iter().rev().take(MAX_ISSUES_PER_FILE - issues.len()) {
            issues.push(PredictiveIssue {
                line: *l,
                column: *cc,
                kind: "unclosed_delimiter".to_string(),
                message: format!("unclosed delimiter '{}'", delim),
            });
        }
    }
    issues.truncate(MAX_ISSUES_PER_FILE);
    issues
}

/// Detect a string opener at `chars[i]`, install the matching scan state, and return how many
/// characters the opener consumes. Returns `None` when `chars[i]` does not start a string.
fn open_string(
    lang: CodeLang,
    chars: &[char],
    i: usize,
    state: &mut ScanState,
    line: usize,
    col: usize,
) -> Option<usize> {
    let c = chars[i];
    let next = chars.get(i + 1).copied();
    let mut set = |quote: char, triple: bool, raw_hashes: Option<usize>, escape: bool, multiline: bool, consumed: usize| {
        *state = ScanState::Str { quote, triple, raw_hashes, escape, open_line: line, open_col: col, multiline };
        consumed
    };

    match lang {
        CodeLang::Rust => {
            if c == '"' {
                // A Rust string literal may legally span physical lines (unlike C/JS), so a raw
                // newline inside one is data, not an unterminated literal.
                return Some(set('"', false, None, true, true, 1));
            }
            if c == 'r' || c == 'b' {
                let (prefix_len, byte) = if c == 'b' && next == Some('r') {
                    (2usize, true)
                } else {
                    (1usize, c == 'b')
                };
                let after_prefix = i + prefix_len;
                if byte && chars.get(after_prefix) == Some(&'"') {
                    return Some(set('"', false, None, true, true, prefix_len + 1));
                }
                if chars.get(after_prefix) == Some(&'"') && !byte {
                    // `r"…"` — a Rust raw string may span lines.
                    return Some(set('"', false, Some(0), false, true, prefix_len + 1));
                }
                if chars.get(after_prefix) == Some(&'#') {
                    let mut hashes = 0usize;
                    while chars.get(after_prefix + hashes) == Some(&'#') {
                        hashes += 1;
                    }
                    if chars.get(after_prefix + hashes) == Some(&'"') {
                        return Some(set('"', false, Some(hashes), false, true, prefix_len + hashes + 1));
                    }
                }
            }
            None
        }
        CodeLang::JavaScript | CodeLang::TypeScript => {
            if c == '"' || c == '\'' {
                return Some(set(c, false, None, true, false, 1));
            }
            if c == '`' {
                return Some(set('`', false, None, true, true, 1));
            }
            None
        }
        CodeLang::Python => {
            if c == '"' || c == '\'' {
                let triple = next == Some(c) && chars.get(i + 2) == Some(&c);
                let consumed = if triple { 3 } else { 1 };
                return Some(set(c, triple, None, true, triple, consumed));
            }
            // Prefixed string literal (`f"…"`, `rb'…'`, `u"…"`).
            if c.is_ascii_alphabetic() {
                let mut prefix_len = 0usize;
                while let Some(pc) = chars.get(i + prefix_len) {
                    if pc.is_ascii_alphabetic() && matches!(pc.to_ascii_lowercase(), 'r' | 'b' | 'f' | 'u') {
                        prefix_len += 1;
                    } else {
                        break;
                    }
                    if prefix_len > 2 {
                        break;
                    }
                }
                if prefix_len > 0 {
                    if let Some(q) = chars.get(i + prefix_len).copied() {
                        if q == '"' || q == '\'' {
                            let triple = chars.get(i + prefix_len + 1) == Some(&q)
                                && chars.get(i + prefix_len + 2) == Some(&q);
                            let consumed = prefix_len + if triple { 3 } else { 1 };
                            return Some(set(q, triple, None, true, triple, consumed));
                        }
                    }
                }
            }
            None
        }
        CodeLang::Go => {
            if c == '"' {
                return Some(set('"', false, None, true, false, 1));
            }
            if c == '`' {
                return Some(set('`', false, None, false, true, 1));
            }
            None
        }
        CodeLang::C | CodeLang::Cpp | CodeLang::Java => {
            if c == '"' {
                return Some(set('"', false, None, true, false, 1));
            }
            None
        }
    }
}

/// Consume a Rust/Go/C/C++/Java char or rune literal at `chars[i]`, or `None` when the quote is
/// something else (a Rust lifetime, a C++ digit separator, …). Char literals never span newlines.
fn open_char_literal(lang: CodeLang, chars: &[char], i: usize) -> Option<usize> {
    if chars[i] != '\'' {
        return None;
    }
    // Rust `b'x'` byte literal.
    if lang == CodeLang::Rust && i > 0 && chars[i - 1] == 'b' {
        // Handled by the caller's normal advance; treat as a plain char literal here.
    }
    let mut j = i + 1;
    if chars.get(j) == Some(&'\\') {
        j += 1;
        match chars.get(j) {
            Some('u') | Some('U') | Some('x') if chars.get(j + 1) == Some(&'{') => {
                let mut k = j + 2;
                while k < chars.len() && chars[k] != '}' && chars[k] != '\n' {
                    k += 1;
                }
                if chars.get(k) == Some(&'}') && chars.get(k + 1) == Some(&'\'') {
                    return Some(k + 2 - i);
                }
                return None;
            }
            Some('x') => {
                // `\xNN` (C/Java/C++): two hex digits then `'`.
                if chars.get(j + 1).is_some_and(|c| c.is_ascii_hexdigit())
                    && chars.get(j + 2).is_some_and(|c| c.is_ascii_hexdigit())
                    && chars.get(j + 3) == Some(&'\'')
                {
                    return Some(j + 4 - i);
                }
                return None;
            }
            Some(_) => {
                if chars.get(j + 1) == Some(&'\'') {
                    return Some(j + 2 - i);
                }
                return None;
            }
            None => return None,
        }
    }
    match chars.get(j) {
        Some(c) if *c != '\'' && *c != '\n' => {
            if chars.get(j + 1) == Some(&'\'') {
                Some(j + 2 - i)
            } else {
                None
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Mutation ledger
// ---------------------------------------------------------------------------------------------

/// A file/line span touched by one mutating tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutatedFileSpan {
    pub relative_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub added_symbols: Vec<String>,
    pub summary: String,
}

/// Per-call telemetry attached to a mutating tool result so the agent loop can build a
/// [`MutationRecord`] without re-reading the file or re-parsing the tool arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MutationTelemetry {
    pub spans: Vec<MutatedFileSpan>,
    pub predictive: Option<PredictiveSummary>,
}

/// One recorded mutating operation, linked to the Gate 2 evidence graph and the Gate 1 revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationRecord {
    /// Deterministic identifier: `mut-<seq:04>`.
    pub mutation_id: String,
    pub turn: u32,
    pub tool_name: String,
    pub call_id: String,
    /// Gate 2 pointer (`ev-<canonical_index:04>-<call_id>`) for `recall_evidence`.
    #[serde(default)]
    pub source_entry_id: Option<String>,
    /// Gate 1 revision captured before this mutation (the state a `--restore-rev` would return to).
    #[serde(default)]
    pub revision_before: Option<RevisionId>,
    /// Gate 1 revision captured after the tool batch that carried this mutation.
    #[serde(default)]
    pub revision_after: Option<RevisionId>,
    #[serde(default)]
    pub touched_ranges: Vec<MutatedFileSpan>,
}

impl MutationRecord {
    fn seq(&self) -> u64 {
        self.mutation_id
            .strip_prefix("mut-")
            .and_then(|rest| rest.parse::<u64>().ok())
            .unwrap_or(0)
    }

    pub fn touched_paths(&self) -> Vec<&str> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for span in &self.touched_ranges {
            if seen.insert(span.relative_path.as_str()) {
                out.push(span.relative_path.as_str());
            }
        }
        out
    }
}

/// Session-scoped, branch-aware ledger of every mutating operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationLedger {
    records: Vec<MutationRecord>,
}

impl MutationLedger {
    pub fn new() -> Self {
        Self { records: Vec::new() }
    }

    pub fn records(&self) -> &[MutationRecord] {
        &self.records
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn latest_turn(&self) -> u32 {
        self.records.iter().map(|r| r.turn).max().unwrap_or(0)
    }

    fn next_seq(&self) -> u64 {
        self.records.iter().map(|r| r.seq()).max().unwrap_or(0) + 1
    }

    /// Append a mutation, assigning the deterministic `mut-<seq:04>` id and trimming the oldest
    /// records once [`MAX_LEDGER_RECORDS`] is exceeded.
    #[allow(clippy::too_many_arguments)]
    pub fn record_mutation(
        &mut self,
        turn: u32,
        tool_name: impl Into<String>,
        call_id: impl Into<String>,
        source_entry_id: Option<String>,
        revision_before: Option<RevisionId>,
        touched_ranges: Vec<MutatedFileSpan>,
    ) -> String {
        let mutation_id = format!("mut-{:04}", self.next_seq());
        self.records.push(MutationRecord {
            mutation_id: mutation_id.clone(),
            turn,
            tool_name: tool_name.into(),
            call_id: call_id.into(),
            source_entry_id,
            revision_before,
            revision_after: None,
            touched_ranges,
        });
        if self.records.len() > MAX_LEDGER_RECORDS {
            let overflow = self.records.len() - MAX_LEDGER_RECORDS;
            self.records.drain(0..overflow);
        }
        mutation_id
    }

    /// Bind the post-batch revision to every record from `turn` that has not been bound yet.
    pub fn backfill_revision_after(&mut self, turn: u32, revision_after: Option<RevisionId>) {
        for record in self.records.iter_mut() {
            if record.turn == turn && record.revision_after.is_none() {
                record.revision_after = revision_after.clone();
            }
        }
    }

    /// Drop every record from a turn later than `turn`, so a `--fork <TURN>` rewind cannot attribute
    /// a failure to a mutation that only exists on the abandoned branch.
    pub fn prune_after_turn(&mut self, turn: u32) {
        self.records.retain(|record| record.turn <= turn);
    }

    /// Replace the ledger contents with a restored branch's records (WAL checkpoint restore).
    pub fn restore(&mut self, records: Vec<MutationRecord>) {
        self.records = records;
        if self.records.len() > MAX_LEDGER_RECORDS {
            let overflow = self.records.len() - MAX_LEDGER_RECORDS;
            self.records.drain(0..overflow);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Diagnostic anchors
// ---------------------------------------------------------------------------------------------

/// One structured location extracted from compiler/test/gate output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticAnchor {
    pub file_path: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub symbol: Option<String>,
    pub error_code: Option<String>,
    pub excerpt: String,
}

/// Extract every recognisable `file:line[:col]` anchor from raw failure output.
///
/// Covers Rust (`--> path:line:col`), Python (`File "path", line N`), JS/TS/Node
/// (`path:line:col`), Go (`path:line:col:`), and generic `path:line` stack traces.
pub fn parse_diagnostic_anchors(output: &str) -> Vec<DiagnosticAnchor> {
    let lines: Vec<&str> = output.lines().collect();
    let mut anchors: Vec<DiagnosticAnchor> = Vec::new();
    let mut seen: BTreeSet<(String, Option<usize>, Option<usize>)> = BTreeSet::new();

    for (idx, raw) in lines.iter().enumerate() {
        let line = raw.trim_end();
        let trimmed = line.trim();

        // Rust/LLVM style arrow: `--> src/a.rs:12:5`
        if let Some(pos) = trimmed.find("-->") {
            let target = trimmed[pos + 3..].trim();
            if let Some((path, ln, col)) = parse_path_line_col(target) {
                let (error_code, symbol, excerpt) = rust_error_context(&lines, idx);
                push_anchor(&mut anchors, &mut seen, path, Some(ln), col, symbol, error_code, excerpt);
                continue;
            }
        }

        // Python: `  File "src/a.py", line 12, in foo`
        if let Some(rest) = trimmed.strip_prefix("File ") {
            if let Some((path, after)) = split_quoted(rest) {
                let ln = after
                    .find("line ")
                    .and_then(|p| after[p + 5..].split(|c: char| !c.is_ascii_digit()).next())
                    .and_then(|digits| digits.parse::<usize>().ok());
                let symbol = after
                    .find(", in ")
                    .map(|p| after[p + 5..].trim().to_string())
                    .filter(|s| !s.is_empty() && s != "<module>");
                let error_code = python_error_code(&lines, idx);
                push_anchor(
                    &mut anchors,
                    &mut seen,
                    path,
                    ln,
                    None,
                    symbol,
                    error_code,
                    trimmed.to_string(),
                );
                continue;
            }
        }

        // `path:line:col` / `path:line:` / `path:line` anywhere in the line, preferring the first
        // colon-run that is followed by digits.
        if let Some((path, ln, col)) = find_inline_anchor(trimmed) {
            let symbol = extract_symbol(trimmed);
            push_anchor(
                &mut anchors,
                &mut seen,
                path,
                Some(ln),
                col,
                symbol,
                None,
                trimmed.to_string(),
            );
        }
    }

    anchors.truncate(32);
    anchors
}

fn push_anchor(
    anchors: &mut Vec<DiagnosticAnchor>,
    seen: &mut BTreeSet<(String, Option<usize>, Option<usize>)>,
    path: String,
    line: Option<usize>,
    column: Option<usize>,
    symbol: Option<String>,
    error_code: Option<String>,
    excerpt: String,
) {
    if path.is_empty() {
        return;
    }
    let key = (path.clone(), line, column);
    if !seen.insert(key) {
        return;
    }
    anchors.push(DiagnosticAnchor {
        file_path: path,
        line,
        column,
        symbol,
        error_code,
        excerpt,
    });
}

/// `path:line[:col]` with no surrounding noise.
fn parse_path_line_col(text: &str) -> Option<(String, usize, Option<usize>)> {
    let text = text.trim().trim_end_matches([')', ',', ':']);
    if text.is_empty() || text.contains(' ') {
        return None;
    }
    let mut parts = text.rsplitn(3, ':');
    let last = parts.next()?;
    let second = parts.next();
    let first = parts.next();
    let last_line = last.trim().parse::<usize>().ok();
    let second_line = second.and_then(|s| s.trim().parse::<usize>().ok());
    match (last_line, second_line) {
        // `path:line:col`
        (Some(col), Some(line)) => {
            let path = first?;
            if path.trim().is_empty() {
                return None;
            }
            Some((path.trim().to_string(), line, Some(col)))
        }
        // `path:line`
        (Some(line), None) => {
            let path = second?;
            if path.trim().is_empty() {
                return None;
            }
            Some((path.trim().to_string(), line, None))
        }
        _ => None,
    }
}

/// Find the first `path:line[:col]` token inside an arbitrary log line.
fn find_inline_anchor(text: &str) -> Option<(String, usize, Option<usize>)> {
    let bytes: Vec<char> = text.chars().collect();
    let mut start = 0usize;
    while start < bytes.len() {
        if bytes[start] == ':' {
            // Candidate `:digits` follows.
            let mut j = start + 1;
            let mut digits = String::new();
            while j < bytes.len() && bytes[j].is_ascii_digit() && digits.len() < 9 {
                digits.push(bytes[j]);
                j += 1;
            }
            if !digits.is_empty() {
                let ln: usize = digits.parse().ok()?;
                let mut col = None;
                if bytes.get(j) == Some(&':') {
                    let mut k = j + 1;
                    let mut col_digits = String::new();
                    while k < bytes.len() && bytes[k].is_ascii_digit() && col_digits.len() < 9 {
                        col_digits.push(bytes[k]);
                        k += 1;
                    }
                    if !col_digits.is_empty() {
                        col = col_digits.parse().ok();
                    }
                }
                if let Some(path) = path_before(text, start) {
                    return Some((path, ln, col));
                }
            }
        }
        start += 1;
    }
    None
}

/// Walk backwards from a `:` to recover the file path token preceding it.
fn path_before(text: &str, colon_char_index: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = colon_char_index;
    while i > 0 {
        let c = chars[i - 1];
        if c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '\\' | '~' | '+' | '@') {
            i -= 1;
        } else {
            break;
        }
    }
    let path: String = chars[i..colon_char_index].iter().collect();
    let path = path.trim().to_string();
    // Require a plausible file token: a path separator or a dot-extension, plus no spaces.
    if path.len() < 3 || path.contains(' ') {
        return None;
    }
    if !path.contains('.') && !path.contains('/') {
        return None;
    }
    if path.starts_with('.') && !path.starts_with("./") && !path.starts_with("../") {
        return None;
    }
    Some(path)
}

/// Nearest `error[EXXXX]` / `error:` / `warning[...]` header above an arrow line, plus its symbol.
fn rust_error_context(lines: &[&str], arrow_idx: usize) -> (Option<String>, Option<String>, String) {
    let mut error_code = None;
    let mut symbol = None;
    let start = arrow_idx.saturating_sub(4);
    let mut excerpt = String::new();
    for candidate in lines.iter().take(arrow_idx).skip(start) {
        let trimmed = candidate.trim();
        if trimmed.starts_with("error") || trimmed.starts_with("warning") {
            if let Some(open) = trimmed.find('[') {
                if let Some(close) = trimmed[open + 1..].find(']') {
                    error_code = Some(trimmed[open + 1..open + 1 + close].to_string());
                }
            } else {
                error_code = Some("error".to_string());
            }
            excerpt = trimmed.to_string();
            symbol = extract_symbol(trimmed);
            break;
        }
    }
    if excerpt.is_empty() {
        excerpt = lines.get(arrow_idx).unwrap_or(&"").trim().to_string();
    }
    if symbol.is_none() {
        symbol = extract_symbol(&excerpt);
    }
    (error_code, symbol, excerpt)
}

fn python_error_code(lines: &[&str], idx: usize) -> Option<String> {
    let end = (idx + 12).min(lines.len());
    for candidate in lines.iter().take(end).skip(idx) {
        let trimmed = candidate.trim();
        if trimmed.starts_with("SyntaxError") || trimmed.starts_with("NameError") {
            return Some(trimmed.split(':').next().unwrap_or(trimmed).to_string());
        }
    }
    None
}

/// Extract the first quoted identifier-ish token, e.g. ``cannot find value `foo` `` → `foo`.
fn extract_symbol(text: &str) -> Option<String> {
    for quote in ['\'', '`', '"'] {
        let mut search = text;
        while let Some(open) = search.find(quote) {
            let rest = &search[open + 1..];
            let Some(close) = rest.find(quote) else { break };
            let candidate = rest[..close].trim();
            if is_plausible_symbol(candidate) {
                return Some(candidate.to_string());
            }
            search = &rest[close + 1..];
        }
    }
    None
}

fn is_plausible_symbol(candidate: &str) -> bool {
    if candidate.is_empty() || candidate.len() > 120 || candidate.contains(' ') {
        return false;
    }
    let mut chars = candidate.chars();
    let Some(first) = chars.next() else { return false };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '.') {
        return false;
    }
    candidate
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '<' | '>' | '.' | '#' | '(' | ')' | '[' | ']' | '!' | '&' | '\''))
}

/// `"path"` → (`path`, remainder-after-quote)
fn split_quoted(text: &str) -> Option<(String, &str)> {
    let open = text.find('"').or_else(|| text.find('\''))?;
    let quote = text[open..].chars().next()?;
    let rest = &text[open + 1..];
    let close = rest.find(quote)?;
    Some((rest[..close].to_string(), &rest[close + 1..]))
}

// ---------------------------------------------------------------------------------------------
// Candidate attribution
// ---------------------------------------------------------------------------------------------

/// One ranked hypothesis linking a recorded mutation to a failure diagnostic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributionCandidate {
    pub mutation_id: String,
    pub turn: u32,
    pub tool_name: String,
    pub relative_path: Option<String>,
    pub line_range: Option<(usize, usize)>,
    pub source_entry_id: Option<String>,
    pub revision_before: Option<RevisionId>,
    pub revision_after: Option<RevisionId>,
    pub confidence: f64,
    pub rationale: String,
}

/// Ranked, explicitly non-causal report over a failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributionReport {
    /// Always [`CANDIDATE_ATTRIBUTION_FRAMING`]; never a root-cause claim.
    pub framing: String,
    pub source: String,
    pub anchors: Vec<DiagnosticAnchor>,
    pub candidates: Vec<AttributionCandidate>,
}

impl AttributionReport {
    pub fn top(&self) -> Option<&AttributionCandidate> {
        self.candidates.first()
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// Render the model-facing block appended to a failure prompt, or `None` when there is nothing
    /// to say (no diagnostics and no mutations).
    pub fn render_for_model(&self) -> Option<String> {
        if self.candidates.is_empty() {
            return None;
        }
        let mut body = String::new();
        body.push_str(
            "Ranked hypotheses for what introduced the failure. These are CANDIDATES, not a confirmed \
             root cause: reproduce the failure before reverting anything.\n",
        );
        for (rank, candidate) in self.candidates.iter().enumerate() {
            let path = candidate.relative_path.as_deref().unwrap_or("<unattributed>");
            let lines = match candidate.line_range {
                Some((start, end)) if end >= start && start > 0 => format!("{}:{}-{}", path, start, end),
                _ => path.to_string(),
            };
            body.push_str(&format!(
                "{}. {} (turn {}, tool `{}`, {}) confidence={:.2}\n",
                rank + 1,
                candidate.mutation_id,
                candidate.turn,
                candidate.tool_name,
                lines,
                candidate.confidence
            ));
            body.push_str(&format!("   why: {}\n", candidate.rationale));
            let mut pointers = Vec::new();
            if let Some(sid) = &candidate.source_entry_id {
                pointers.push(format!("raw output: recall_evidence(source_entry_id={})", sid));
            }
            if let Some(rev) = &candidate.revision_before {
                pointers.push(format!(
                    "pre-change revision {} (inspect, or --restore-rev {} to revert)",
                    rev, rev
                ));
            }
            if let Some(rev) = &candidate.revision_after {
                pointers.push(format!("post-change revision {}", rev));
            }
            if !pointers.is_empty() {
                body.push_str(&format!("   evidence: {}\n", pointers.join("; ")));
            }
        }
        if !self.anchors.is_empty() {
            let rendered: Vec<String> = self
                .anchors
                .iter()
                .take(6)
                .map(|anchor| {
                    let location = match (anchor.line, anchor.column) {
                        (Some(line), Some(col)) => format!("{}:{}:{}", anchor.file_path, line, col),
                        (Some(line), None) => format!("{}:{}", anchor.file_path, line),
                        _ => anchor.file_path.clone(),
                    };
                    match &anchor.error_code {
                        Some(code) => format!("{} ({})", location, code),
                        None => location,
                    }
                })
                .collect();
            body.push_str(&format!("diagnostics: {}\n", rendered.join(", ")));
        }
        Some(crate::prompt::fence_untrusted(
            "candidate_failure_attribution",
            &format!(" framing=\"{}\" source=\"{}\"", CANDIDATE_ATTRIBUTION_FRAMING, escape_fence_attribute(&self.source)),
            &body,
            ATTRIBUTION_BLOCK_MAX_BYTES,
        ))
    }
}

/// The failure-attribution engine. Stateless; every input is explicit so results are reproducible.
pub struct AttributionEngine;

impl AttributionEngine {
    /// Score `ledger`'s mutations against the diagnostics parsed out of `failure_output`.
    pub fn attribute_failure(
        ledger: &MutationLedger,
        failure_output: &str,
        source: &str,
        top_k: usize,
    ) -> AttributionReport {
        let anchors = parse_diagnostic_anchors(failure_output);
        let latest_turn = ledger.latest_turn();
        let mut candidates: Vec<AttributionCandidate> = Vec::new();

        for record in ledger.records() {
            let mut best: Option<(f64, String)> = None;
            for anchor in &anchors {
                let (score, rationale) = score_record_against_anchor(record, anchor, latest_turn);
                match &best {
                    Some((best_score, _)) if *best_score >= score => {}
                    _ => best = Some((score, rationale)),
                }
            }
            let (confidence, rationale) = match best {
                Some(found) => found,
                None => {
                    // No diagnostics at all: fall back to pure recency so a resumable ledger still
                    // points at the newest change, at the bottom of the confidence band.
                    let recency = recency_factor(record.turn, latest_turn);
                    (
                        round3(0.20 + 0.25 * recency),
                        format!(
                            "no parseable diagnostic anchor; recent mutation in turn {} (recency only)",
                            record.turn
                        ),
                    )
                }
            };
            if confidence < 0.20 {
                continue;
            }
            let first_span = record.touched_ranges.first();
            candidates.push(AttributionCandidate {
                mutation_id: record.mutation_id.clone(),
                turn: record.turn,
                tool_name: record.tool_name.clone(),
                relative_path: first_span.map(|span| span.relative_path.clone()),
                line_range: first_span.and_then(|span| {
                    if span.start_line > 0 && span.end_line >= span.start_line {
                        Some((span.start_line, span.end_line))
                    } else {
                        None
                    }
                }),
                source_entry_id: record.source_entry_id.clone(),
                revision_before: record.revision_before.clone(),
                revision_after: record.revision_after.clone(),
                confidence,
                rationale,
            });
        }

        // Descending confidence, then most recent mutation first, so ties are deterministic.
        candidates.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.mutation_id.cmp(&a.mutation_id))
        });
        candidates.truncate(top_k.max(1));

        AttributionReport {
            framing: CANDIDATE_ATTRIBUTION_FRAMING.to_string(),
            source: source.to_string(),
            anchors,
            candidates,
        }
    }
}

fn recency_factor(turn: u32, latest_turn: u32) -> f64 {
    let distance = latest_turn.saturating_sub(turn) as f64;
    1.0 / (1.0 + 0.35 * distance)
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn paths_match(anchor_path: &str, span_path: &str) -> bool {
    let normalize = |p: &str| p.trim_start_matches("./").replace('\\', "/");
    let a = normalize(anchor_path);
    let b = normalize(span_path);
    if a == b {
        return true;
    }
    a.ends_with(&format!("/{}", b)) || b.ends_with(&format!("/{}", a))
}

fn score_record_against_anchor(
    record: &MutationRecord,
    anchor: &DiagnosticAnchor,
    latest_turn: u32,
) -> (f64, String) {
    let recency = recency_factor(record.turn, latest_turn);
    let mut best: Option<(f64, String)> = None;
    let mut consider = |score: f64, rationale: String| {
        let score = round3(score);
        match &best {
            Some((current, _)) if *current >= score => {}
            _ => best = Some((score, rationale)),
        }
    };

    let mut path_matched = false;
    for span in &record.touched_ranges {
        if !paths_match(&anchor.file_path, &span.relative_path) {
            continue;
        }
        path_matched = true;
        let line = anchor.line;
        if let Some(line) = line {
            if span.start_line > 0 && span.end_line >= span.start_line {
                if line >= span.start_line && line <= span.end_line {
                    consider(
                        0.98,
                        format!(
                            "diagnostic anchor {}:{} falls inside the edited span {}-{}",
                            span.relative_path, line, span.start_line, span.end_line
                        ),
                    );
                    continue;
                }
                let distance = if line < span.start_line {
                    span.start_line - line
                } else {
                    line - span.end_line
                };
                if distance <= ATTRIBUTION_ADJACENT_LINES {
                    consider(
                        0.85 + 0.13 * (1.0 - distance as f64 / (ATTRIBUTION_ADJACENT_LINES as f64 + 1.0)),
                        format!(
                            "diagnostic anchor {}:{} is {} line(s) from the edited span {}-{}",
                            span.relative_path, line, distance, span.start_line, span.end_line
                        ),
                    );
                    continue;
                }
            }
        }
        if let Some(symbol) = &anchor.symbol {
            if symbol_matches_span(symbol, span) {
                consider(
                    0.82,
                    format!(
                        "diagnostic symbol `{}` matches a symbol added by this edit to {}",
                        symbol, span.relative_path
                    ),
                );
                continue;
            }
        }
        consider(
            0.50 + 0.20 * recency,
            format!(
                "{} was modified in turn {} and appears in the failure output",
                span.relative_path, record.turn
            ),
        );
    }

    if !path_matched {
        let paths = record.touched_paths();
        let detail = if paths.is_empty() {
            format!("`{}` ran in turn {} (individual files not attributed)", record.tool_name, record.turn)
        } else {
            format!("turn {} modified {} with no matching diagnostic path", record.turn, paths.join(", "))
        };
        consider(0.20 + 0.25 * recency, detail);
    }

    best.unwrap_or((0.20, "unrelated recent mutation".to_string()))
}

fn symbol_matches_span(symbol: &str, span: &MutatedFileSpan) -> bool {
    let needle = symbol.trim();
    if needle.is_empty() {
        return false;
    }
    let last = needle.rsplit("::").next().unwrap_or(needle);
    let last = last.rsplit('.').next().unwrap_or(last);
    span.added_symbols
        .iter()
        .any(|candidate| candidate == needle || candidate == last)
        || span.summary.contains(needle)
}

// ---------------------------------------------------------------------------------------------
// Symbol extraction helpers (shared by the tool runtime)
// ---------------------------------------------------------------------------------------------

const SYMBOL_KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "pub", "use", "mod", "impl", "struct", "enum", "trait", "const", "static",
    "type", "where", "self", "Self", "return", "match", "if", "else", "for", "while", "loop",
    "break", "continue", "true", "false", "None", "Some", "Ok", "Err", "as", "in", "ref", "move",
    "async", "await", "dyn", "unsafe", "extern", "crate", "super", "function", "var", "class",
    "def", "import", "from", "new", "this", "void", "int", "char", "float", "double", "package",
    "func", "interface", "and", "or", "not", "is", "None", "print", "len", "String", "Vec",
];

/// Identifiers that appear in `new_text` but not in `old_text`, i.e. the names an `edit` introduced.
///
/// This is a lexical approximation used only as an attribution hint; it never gates anything.
pub fn added_identifiers(old_text: &str, new_text: &str, max: usize) -> Vec<String> {
    let before: BTreeSet<String> = identifiers(old_text).into_iter().collect();
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for ident in identifiers(new_text) {
        if before.contains(&ident) || !seen.insert(ident.clone()) {
            continue;
        }
        out.push(ident);
        if out.len() >= max {
            break;
        }
    }
    out.sort();
    out
}

fn identifiers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else {
            flush_identifier(&mut current, &mut out);
        }
    }
    flush_identifier(&mut current, &mut out);
    out
}

fn flush_identifier(current: &mut String, out: &mut Vec<String>) {
    if current.len() >= 3
        && !SYMBOL_KEYWORDS.contains(&current.as_str())
        && current.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        out.push(current.clone());
    }
    current.clear();
}

/// Names declared by a whole-file `write`: `fn foo`, `struct Bar`, `class Baz`, `def qux`, …
pub fn top_level_symbol_names(content: &str, max: usize) -> Vec<String> {
    const KINDS: &[&str] = &[
        "fn", "struct", "enum", "trait", "impl", "const", "static", "type", "mod", "class",
        "interface", "function", "def", "func", "let", "var", "public", "private", "protected",
    ];
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        let trimmed = trimmed
            .strip_prefix("pub ")
            .or_else(|| trimmed.strip_prefix("export "))
            .or_else(|| trimmed.strip_prefix("async "))
            .unwrap_or(trimmed);
        let Some((head, rest)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        if !KINDS.contains(&head) {
            continue;
        }
        let name: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.len() >= 3 && seen.insert(name.clone()) {
            out.push(name);
            if out.len() >= max {
                break;
            }
        }
    }
    out.sort();
    out
}

/// Build the summary line stored alongside a mutation span.
pub fn mutation_summary(tool_name: &str, relative_path: &str, start_line: usize, end_line: usize) -> String {
    if start_line > 0 && end_line >= start_line {
        format!("{} {} lines {}-{}", tool_name, relative_path, start_line, end_line)
    } else {
        format!("{} {}", tool_name, relative_path)
    }
}

/// Convenience: build the `added_symbols` hint for one mutating tool call from its raw arguments.
pub fn added_symbols_for_call(tool_name: &str, raw_arguments: &str, new_content: Option<&str>) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw_arguments) else {
        return Vec::new();
    };
    match tool_name {
        "edit" => {
            let mut out: BTreeMap<String, ()> = BTreeMap::new();
            if let Some(edits) = value.get("edits").and_then(|v| v.as_array()) {
                for item in edits {
                    let old = item.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
                    let new = item.get("new_text").and_then(|v| v.as_str()).unwrap_or("");
                    for ident in added_identifiers(old, new, 12) {
                        out.insert(ident, ());
                    }
                }
            }
            let mut names: Vec<String> = out.into_keys().collect();
            names.sort();
            names.truncate(16);
            names
        }
        "write" => match new_content {
            Some(content) => top_level_symbol_names(content, 16),
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_accepts_braces_inside_strings_and_comments() {
        let source = r##"
fn main() {
    let s = "} { ( ) [ ]";
    let r = r#"raw "{" }"#;
    let c = '}';
    // } } }
    /* } { */
    let t = 'a'; // lifetime-free
}
"##;
        let issues = scan_code(CodeLang::Rust, source);
        assert!(issues.is_empty(), "clean Rust must not warn: {:?}", issues);
    }

    #[test]
    fn scanner_accepts_rust_lifetimes_and_raw_strings() {
        let source = "struct S<'a> { r: &'a str }\nfn f<'a>(x: &'a str) -> &'a str { x }\n";
        let issues = scan_code(CodeLang::Rust, source);
        assert!(issues.is_empty(), "lifetimes are not char literals: {:?}", issues);
    }

    #[test]
    fn scanner_reports_unclosed_brace_at_its_opening_position() {
        let issues = scan_code(CodeLang::Rust, "fn main() {\n    let x = 1;\n");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, "unclosed_delimiter");
        assert_eq!((issues[0].line, issues[0].column), (1, 11));
    }

    #[test]
    fn scanner_reports_conflict_markers_only_outside_strings() {
        // A raw string (and a JS template literal) may legitimately span lines, so a marker inside
        // one is data, not a conflict.
        let inside_rust = "fn f() {\n    let s = r#\"\n=======\n\"#;\n}\n";
        assert!(
            scan_code(CodeLang::Rust, inside_rust).is_empty(),
            "{:?}",
            scan_code(CodeLang::Rust, inside_rust)
        );
        let inside_js = "const s = `\n<<<<<<<\n`;\n";
        assert!(scan_code(CodeLang::JavaScript, inside_js).is_empty());
        let outside = "fn f() {}\n<<<<<<< HEAD\nlet x = 1;\n=======\nlet x = 2;\n>>>>>>> other\n";
        let issues = scan_code(CodeLang::Rust, outside);
        assert_eq!(issues.len(), 3);
        assert!(issues.iter().all(|i| i.kind == "conflict_marker"));
    }

    #[test]
    fn json_and_toml_checks() {
        assert!(structural_issues("json", "{\"a\": 1}").is_empty());
        let json = structural_issues("json", "{\"a\": 1,}");
        assert_eq!(json.len(), 1);
        assert_eq!(json[0].kind, "json_syntax");
        assert!(structural_issues("toml", "[a]\nk = \"v\"\n").is_empty());
        let toml = structural_issues("toml", "[a\nk = 1\n");
        assert_eq!(toml.len(), 1);
        assert_eq!(toml[0].kind, "toml_structure");
    }

    #[test]
    fn diagnostic_parsing_covers_rust_python_and_generic_stacks() {
        let rust = "error[E0425]: cannot find value `foo` in this scope\n  --> src/a.rs:12:5\n";
        let anchors = parse_diagnostic_anchors(rust);
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].file_path, "src/a.rs");
        assert_eq!(anchors[0].line, Some(12));
        assert_eq!(anchors[0].column, Some(5));
        assert_eq!(anchors[0].error_code.as_deref(), Some("E0425"));
        assert_eq!(anchors[0].symbol.as_deref(), Some("foo"));

        let py = "Traceback (most recent call last):\n  File \"/tmp/ws/app.py\", line 7, in <module>\n";
        let anchors = parse_diagnostic_anchors(py);
        assert_eq!(anchors[0].file_path, "/tmp/ws/app.py");
        assert_eq!(anchors[0].line, Some(7));

        let node = "TypeError: x is not a function\n    at run (/tmp/ws/src/a.ts:4:11)\n";
        let anchors = parse_diagnostic_anchors(node);
        assert_eq!(anchors[0].file_path, "/tmp/ws/src/a.ts");
        assert_eq!(anchors[0].line, Some(4));
        assert_eq!(anchors[0].column, Some(11));
    }

    #[test]
    fn attribution_ranks_the_edited_span_highest() {
        let mut ledger = MutationLedger::new();
        ledger.record_mutation(
            1,
            "edit",
            "call_a",
            Some("ev-0001-call_a".to_string()),
            None,
            vec![MutatedFileSpan {
                relative_path: "src/a.rs".to_string(),
                start_line: 1,
                end_line: 3,
                added_symbols: vec![],
                summary: "edit src/a.rs lines 1-3".to_string(),
            }],
        );
        ledger.record_mutation(
            2,
            "edit",
            "call_b",
            Some("ev-0002-call_b".to_string()),
            None,
            vec![MutatedFileSpan {
                relative_path: "src/b.rs".to_string(),
                start_line: 10,
                end_line: 14,
                added_symbols: vec!["compute".to_string()],
                summary: "edit src/b.rs lines 10-14".to_string(),
            }],
        );
        let report = AttributionEngine::attribute_failure(
            &ledger,
            "error[E0308]: mismatched types\n  --> src/b.rs:12:9\n",
            "completion_gate",
            3,
        );
        assert_eq!(report.framing, CANDIDATE_ATTRIBUTION_FRAMING);
        let top = report.top().expect("a candidate");
        assert_eq!(top.mutation_id, "mut-0002");
        assert!(top.confidence >= 0.85, "{}", top.confidence);
        assert_eq!(top.relative_path.as_deref(), Some("src/b.rs"));
        assert!(report.render_for_model().unwrap().contains("mut-0002"));
    }

    #[test]
    fn fork_rewind_prunes_future_mutations() {
        let mut ledger = MutationLedger::new();
        ledger.record_mutation(1, "edit", "c1", None, None, Vec::new());
        ledger.record_mutation(2, "edit", "c2", None, None, Vec::new());
        ledger.record_mutation(3, "edit", "c3", None, None, Vec::new());
        ledger.prune_after_turn(1);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.records()[0].call_id, "c1");
    }
}
