use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub const MAX_TEXT_CHARS: usize = 4000;
pub const KINDS: &[&str] = &["decision", "pitfall", "preference", "reference", "review"];
/// Kinds the model may write with `memory_remember`.
pub const TOOL_KINDS: &[&str] = &["preference", "decision", "review"];
pub const MIN_QUOTE_CHARS: usize = 8;
pub const MAX_QUOTE_CHARS: usize = 500;
pub const MAX_EVIDENCE_CHARS: usize = 512;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Source {
    pub origin: String,
    pub session_id: Option<String>,
    pub call_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewMemory {
    pub kind: String,
    pub text: String,
    pub paths: Vec<String>,
    pub evidence_refs: Vec<String>,
    pub source: Source,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub state: String,
    pub version: u32,
    pub paths: Vec<String>,
    pub evidence_refs: Vec<String>,
    pub source: Source,
    pub created_at: i64,
    pub updated_at: i64,
    /// Verbatim user quote that keeps a `pending` entry promotable.
    #[serde(default)]
    pub source_quote: Option<String>,
    /// Absolute Unix seconds after which the entry is no longer active.
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// Memory replaced when this entry is promoted.
    #[serde(default)]
    pub supersedes: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub id: String,
    pub kind: String,
    pub summary: String,
    pub version: u32,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub use_enabled: bool,
    pub capture_enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stats {
    pub accepted: u64,
    pub pending: u64,
    pub candidates: u64,
    pub forgotten: u64,
    pub versions: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportReport {
    pub created: usize,
    pub skipped: usize,
    pub already_imported: bool,
}

/// Collapse whitespace and case-fold so a quote survives re-wrapping and
/// capitalization changes but still requires a contiguous verbatim fragment.
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// True when the normalized quote is a contiguous substring of any candidate.
pub fn quote_matches(quote: &str, candidates: &[String]) -> bool {
    let needle = normalize(quote);
    if needle.is_empty() {
        return false;
    }
    candidates
        .iter()
        .any(|candidate| normalize(candidate).contains(&needle))
}

pub fn validate_text(text: &str) -> Result<()> {
    if text.trim().is_empty() || text.chars().count() > MAX_TEXT_CHARS {
        bail!("Memory text must contain 1..={MAX_TEXT_CHARS} characters");
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        bail!("Memory text contains unsupported control characters");
    }
    crate::privacy::check_text(text)
}

/// Quotes are verbatim user words: bounded, single-purpose and secret-scanned.
pub fn validate_quote(quote: &str) -> Result<()> {
    let quote = quote.trim();
    let chars = quote.chars().count();
    if !(MIN_QUOTE_CHARS..=MAX_QUOTE_CHARS).contains(&chars) {
        bail!("Quote must contain {MIN_QUOTE_CHARS}..={MAX_QUOTE_CHARS} characters");
    }
    if quote
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        bail!("Quote contains unsupported control characters");
    }
    crate::privacy::check_text(quote)
}

pub fn validate_evidence_refs(refs: &[String]) -> Result<()> {
    if refs.len() > 8 {
        bail!("At most 8 unverified evidence references are allowed");
    }
    for item in refs {
        if item.trim().is_empty()
            || item.chars().count() > MAX_EVIDENCE_CHARS
            || item.chars().any(char::is_control)
        {
            bail!("Evidence references must be non-empty, bounded and control-free");
        }
        crate::privacy::check_text(item)?;
    }
    Ok(())
}

pub fn validate_paths(paths: &[String]) -> Result<()> {
    if paths.len() > 8 {
        bail!("At most 8 relative path hints are allowed");
    }
    for path in paths {
        if path.is_empty()
            || path.chars().count() > 256
            || path.starts_with('/')
            || path.contains(['\\', ':'])
            || path.split('/').any(|part| part == ".." || part.is_empty())
        {
            bail!("Path hints must be bounded relative paths without parent traversal");
        }
    }
    Ok(())
}

pub fn validate_input(input: &NewMemory) -> Result<()> {
    if !KINDS.contains(&input.kind.as_str()) {
        bail!("Unknown memory kind");
    }
    validate_text(&input.text)?;
    validate_paths(&input.paths)?;
    if input.evidence_refs.len() > 8 {
        bail!("At most 8 unverified evidence references are allowed");
    }
    for item in input.evidence_refs.iter().chain(
        input
            .source
            .session_id
            .iter()
            .chain(input.source.call_id.iter()),
    ) {
        if item.chars().count() > 512 || item.chars().any(char::is_control) {
            bail!("Source metadata is too long or contains control characters");
        }
        crate::privacy::check_text(item)?;
    }
    if !["manual", "model_proposal", "legacy_import"].contains(&input.source.origin.as_str()) {
        bail!("Unknown source origin");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_require_contiguous_normalized_matches() {
        let users = vec!["Please use tabs for indentation in this repo".to_string()];
        assert!(quote_matches("use tabs for indentation", &users));
        assert!(quote_matches("  USE   Tabs\nfor indentation ", &users));
        assert!(!quote_matches(
            "tabs for indentation in this repo ok",
            &users
        ));
        assert!(!quote_matches("spaces", &users));
        assert!(!quote_matches("   ", &[]));
    }

    #[test]
    fn quotes_and_evidence_are_bounded() {
        assert!(validate_quote("short").is_err());
        assert!(validate_quote("        ").is_err());
        assert!(validate_quote("use tabs for indentation").is_ok());
        assert!(validate_quote(&"a".repeat(MAX_QUOTE_CHARS + 1)).is_err());
        assert!(validate_evidence_refs(&[String::new()]).is_err());
        assert!(validate_evidence_refs(&["src/lib.rs:12".to_string()]).is_ok());
    }
}
