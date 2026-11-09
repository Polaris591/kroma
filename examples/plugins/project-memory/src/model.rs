use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub const MAX_TEXT_CHARS: usize = 4000;
pub const KINDS: &[&str] = &["decision", "pitfall", "preference", "reference", "review"];
/// Kinds the model may write with `memory_remember`.
pub const TOOL_KINDS: &[&str] = &["preference", "decision", "review"];
/// A quoted rule is the user's own words: one character is enough because the
/// quote is stored verbatim instead of being summarized by the model.
pub const MIN_QUOTE_CHARS: usize = 1;
pub const MAX_QUOTE_CHARS: usize = MAX_TEXT_CHARS;
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
    /// Verbatim user quote this rule came from, when one was verified.
    #[serde(default)]
    pub source_quote: Option<String>,
    /// Obsolete: kept for schema compatibility and always cleared on write.
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// Active rule this row replaced.
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

/// Collapse whitespace and case-fold. Used for text lookup in the admin
/// commands (`/memory forget TEXT`), never for verifying a user quote.
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Locate `quote` in `message` and return the exact slice the user wrote.
///
/// Matching is case-sensitive and requires a genuinely contiguous fragment:
/// every non-whitespace character must appear in the same order, and no extra
/// word may be inserted. Only whitespace is flexible, so a quote copied across
/// a line break or re-indented still matches; the returned slice is the
/// original text, which is what gets stored as the rule.
pub fn find_quote<'a>(quote: &str, message: &'a str) -> Option<&'a str> {
    let quote = quote.trim();
    if quote.is_empty() {
        return None;
    }
    if let Some(start) = message.find(quote) {
        return Some(&message[start..start + quote.len()]);
    }
    if !quote.chars().any(char::is_whitespace) {
        return None;
    }
    let head = quote.split_whitespace().next()?;
    let needle: Vec<char> = quote.chars().collect();
    let hay: Vec<(usize, char)> = message.char_indices().collect();
    let mut search = 0usize;
    while search < message.len() {
        let offset = message[search..].find(head)?;
        let start_byte = search + offset;
        let start = hay.partition_point(|(byte, _)| *byte < start_byte);
        if let Some(end) = match_from(&needle, &hay, message.len(), start) {
            return Some(&message[start_byte..end]);
        }
        search = start_byte + head.len();
    }
    None
}

/// Match `needle` from `hay[start]`, treating each whitespace run in the needle
/// as one or more whitespace characters in the message. Returns the byte offset
/// one past the match.
fn match_from(
    needle: &[char],
    hay: &[(usize, char)],
    end_of_message: usize,
    start: usize,
) -> Option<usize> {
    let mut at = start;
    let mut index = 0;
    while index < needle.len() {
        if needle[index].is_whitespace() {
            while index < needle.len() && needle[index].is_whitespace() {
                index += 1;
            }
            let before = at;
            while at < hay.len() && hay[at].1.is_whitespace() {
                at += 1;
            }
            if at == before {
                return None;
            }
        } else {
            if hay.get(at).map(|(_, ch)| *ch) != Some(needle[index]) {
                return None;
            }
            at += 1;
            index += 1;
        }
    }
    Some(hay.get(at).map(|(byte, _)| *byte).unwrap_or(end_of_message))
}

/// True when the quote is a contiguous verbatim fragment of any candidate.
pub fn quote_matches(quote: &str, candidates: &[String]) -> bool {
    candidates
        .iter()
        .any(|candidate| find_quote(quote, candidate).is_some())
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
    Ok(())
}

/// Quotes are verbatim user words: bounded and single-purpose.
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
    Ok(())
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
    fn a_quote_must_be_a_contiguous_verbatim_slice_of_the_user_message() {
        let users = vec![
            "Please use tabs for indentation in this repo".to_string(),
            "First line\n  then   use   pnpm  for packages".to_string(),
        ];
        assert_eq!(
            find_quote("use tabs for indentation", &users[0]),
            Some("use tabs for indentation")
        );
        // Case is the user's, so a re-capitalized restatement is not a quote.
        assert!(find_quote("USE TABS FOR INDENTATION", &users[0]).is_none());
        // Words may not be reordered, dropped or added around a real fragment.
        assert!(find_quote("tabs for indentation in this repo ok", &users[0]).is_none());
        assert!(find_quote("use tabs indentation", &users[0]).is_none());
        // Only whitespace is flexible: a re-wrapped quote still matches, and the
        // original slice is what a caller stores.
        assert_eq!(
            find_quote("First line then use pnpm", &users[1]),
            Some("First line\n  then   use   pnpm")
        );
        assert_eq!(
            find_quote("line then use pnpm for packages", &users[1]),
            Some("line\n  then   use   pnpm  for packages")
        );
        assert!(quote_matches("then use pnpm for packages", &users));
        assert!(!quote_matches("First line then use cargo", &users));
        assert!(find_quote("   ", &users[0]).is_none());
        assert!(!quote_matches("", &users));
    }

    #[test]
    fn a_quote_matches_cjk_without_a_false_fragment() {
        let users = ["这个项目不要使用审批流程".to_string()];
        assert_eq!(
            find_quote("不要使用审批流程", &users[0]),
            Some("不要使用审批流程")
        );
        assert!(find_quote("使用审批流程", &users[0]).is_some());
        assert!(find_quote("不要审批流程", &users[0]).is_none());
    }

    #[test]
    fn quotes_and_evidence_are_bounded() {
        assert!(validate_quote("").is_err());
        assert!(validate_quote("        ").is_err());
        assert!(validate_quote("a").is_ok());
        assert!(validate_quote("use tabs for indentation").is_ok());
        assert!(validate_quote(&"a".repeat(MAX_QUOTE_CHARS)).is_ok());
        assert!(validate_quote(&"a".repeat(MAX_QUOTE_CHARS + 1)).is_err());
        assert!(validate_evidence_refs(&[String::new()]).is_err());
        assert!(validate_evidence_refs(&["src/lib.rs:12".to_string()]).is_ok());
    }
}
