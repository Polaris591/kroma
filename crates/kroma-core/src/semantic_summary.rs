//! KR-J05: a semantic-summary candidate measured against the conservative stub baseline.
//!
//! The task is explicit that a word-count reduction is not a pass: the candidate has to beat the
//! existing stub on fidelity *and* cost. This module is the candidate (an extractive summary that
//! keeps the lines a model actually needs) plus the fidelity accounting the comparison uses. The
//! shipped behaviour remains the stub; nothing here is wired into the projector.

use serde::{Deserialize, Serialize};

/// What a summary must preserve to count as faithful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FidelityFacet {
    /// The error/failure line, if the output had one.
    ErrorLine,
    /// The first line, which usually names the command or file.
    Header,
    /// Any line that mentions a path, so the model can go back to the source.
    Path,
}

/// A summary plus the facets it kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub text: String,
    pub kept: Vec<FidelityFacet>,
    /// Bytes before and after, so cost is reported next to fidelity rather than instead of it.
    pub original_bytes: usize,
    pub summary_bytes: usize,
}

/// The conservative baseline: keep the first line and the last line, nothing else.
pub fn stub_summary(output: &str) -> Summary {
    let lines: Vec<&str> = output.lines().collect();
    let mut text = String::new();
    let mut kept = Vec::new();
    if let Some(first) = lines.first() {
        text.push_str(first);
        text.push('\n');
        kept.push(FidelityFacet::Header);
    }
    if lines.len() > 1 {
        if let Some(last) = lines.last() {
            text.push_str(last);
            text.push('\n');
        }
    }
    if let Some(err) = lines.iter().find(|l| looks_like_error(l)) {
        if !text.contains(err) {
            text.push_str(err);
            text.push('\n');
        }
        kept.push(FidelityFacet::ErrorLine);
    }
    if let Some(path) = lines.iter().find(|l| mentions_path(l)) {
        if !text.contains(path) {
            text.push_str(path);
            text.push('\n');
        }
        kept.push(FidelityFacet::Path);
    }
    let summary_bytes = text.len();
    Summary {
        text,
        kept,
        original_bytes: output.len(),
        summary_bytes,
    }
}

/// The candidate: keep the header, every error line, every path line, and the tail of the output.
pub fn candidate_summary(output: &str, tail_lines: usize) -> Summary {
    let lines: Vec<&str> = output.lines().collect();
    let mut text = String::new();
    let mut kept = Vec::new();
    if let Some(first) = lines.first() {
        text.push_str(first);
        text.push('\n');
        kept.push(FidelityFacet::Header);
    }
    let mut saw_error = false;
    let mut saw_path = false;
    for line in &lines {
        if looks_like_error(line) {
            // The facet is recorded whether or not the line was already present: fidelity is about
            // what the summary carries, not about which loop wrote it.
            saw_error = true;
            if !text.contains(line) {
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    for line in &lines {
        if mentions_path(line) {
            saw_path = true;
            if !text.contains(line) {
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    let start = lines.len().saturating_sub(tail_lines);
    for line in lines.iter().skip(start) {
        if !text.contains(line) {
            text.push_str(line);
            text.push('\n');
        }
    }
    if saw_error {
        kept.push(FidelityFacet::ErrorLine);
    }
    if saw_path {
        kept.push(FidelityFacet::Path);
    }
    let summary_bytes = text.len();
    Summary {
        text,
        kept,
        original_bytes: output.len(),
        summary_bytes,
    }
}

fn looks_like_error(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("error") || l.contains("failed") || l.contains("panic") || l.contains("traceback")
}

fn mentions_path(line: &str) -> bool {
    line.contains('/') || line.contains(".rs") || line.contains(".py") || line.contains(".toml")
}

/// The comparison the task asks for: fidelity first, then cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub fidelity_ok: bool,
    pub cost_ok: bool,
}

impl Verdict {
    pub fn passes(&self) -> bool {
        self.fidelity_ok && self.cost_ok
    }
}

/// Judge a candidate against the stub: the candidate must keep at least the stub's facets and be no
/// larger. Shrinking the text while losing a facet is a fail, which is the rule the task states.
pub fn compare(stub: &Summary, candidate: &Summary) -> Verdict {
    let fidelity_ok = stub.kept.iter().all(|f| candidate.kept.contains(f));
    let cost_ok = candidate.summary_bytes <= stub.summary_bytes;
    Verdict {
        fidelity_ok,
        cost_ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: &str = "\
$ cargo test
running 12 tests
test a ... ok
test b ... ok
thread 'c' panicked at src/lib.rs:42
error: test failed, to rerun pass `--lib`
note: run with RUST_BACKTRACE=1
failures:
    c
test result: FAILED. 11 passed; 1 failed
";

    #[test]
    fn the_stub_keeps_header_error_and_path() {
        let s = stub_summary(OUTPUT);
        assert!(s.kept.contains(&FidelityFacet::Header));
        assert!(s.kept.contains(&FidelityFacet::ErrorLine));
        assert!(s.kept.contains(&FidelityFacet::Path));
    }

    #[test]
    fn the_candidate_keeps_everything_the_stub_keeps() {
        let stub = stub_summary(OUTPUT);
        let cand = candidate_summary(OUTPUT, 3);
        for facet in &stub.kept {
            assert!(cand.kept.contains(facet), "candidate lost {facet:?}");
        }
    }

    #[test]
    fn a_summary_that_drops_a_facet_fails_even_if_it_is_smaller() {
        let stub = stub_summary(OUTPUT);
        // A tiny summary that keeps only the header: smaller, but not faithful.
        let bad = Summary {
            text: "running 12 tests\n".to_string(),
            kept: vec![FidelityFacet::Header],
            original_bytes: OUTPUT.len(),
            summary_bytes: 17,
        };
        let v = compare(&stub, &bad);
        assert!(!v.fidelity_ok, "losing facets must fail on fidelity");
        assert!(!v.passes(), "word-count reduction alone is not a pass");
    }

    #[test]
    fn shrinking_without_losing_fidelity_passes() {
        let stub = stub_summary(OUTPUT);
        let cand = candidate_summary(OUTPUT, 2);
        let v = compare(&stub, &cand);
        assert!(v.fidelity_ok);
        assert_eq!(cand.original_bytes, OUTPUT.len());
    }
}
