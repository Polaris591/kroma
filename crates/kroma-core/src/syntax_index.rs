//! KR-I06: selective syntax parsing for retrieval candidates.
//!
//! The task's rule is that the Tree-sitter *name* is not the acceptance criterion: the candidate has
//! to beat the existing rule-based ranking on localisation at no worse total cost. This module is the
//! candidate -- a dependency-free, line-oriented parser that extracts declarations and their nesting
//! depth -- plus the comparison harness that decides whether it may be promoted. It is not wired into
//! the graph builder; promotion is a separate, evidence-gated step.

use serde::{Deserialize, Serialize};

/// One declaration found in a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    pub name: String,
    pub kind: DeclKind,
    pub line: usize,
    pub depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeclKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Const,
}

/// Parse declarations out of Rust source without a grammar dependency.
///
/// Deliberately shallow: it tracks brace depth and matches declaration keywords at line starts. A
/// string literal or a comment containing `fn ` will not be misread because both are skipped.
pub fn parse_declarations(source: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_block_comment = false;
    for (idx, raw) in source.lines().enumerate() {
        let mut line = raw.trim_start();
        if in_block_comment {
            if let Some(end) = line.find("*/") {
                line = &line[end + 2..];
                in_block_comment = false;
            } else {
                continue;
            }
        }
        if let Some(start) = line.find("/*") {
            if !line[start..].contains("*/") {
                in_block_comment = true;
                line = &line[..start];
            }
        }
        if line.starts_with("//") || line.is_empty() {
            continue;
        }
        // Strip trailing line comments so `fn x() {} // fn y` is not double-counted.
        let code = match line.find("//") {
            Some(pos) => &line[..pos],
            None => line,
        };
        for (kw, kind) in [
            ("fn ", DeclKind::Function),
            ("struct ", DeclKind::Struct),
            ("enum ", DeclKind::Enum),
            ("trait ", DeclKind::Trait),
            ("impl ", DeclKind::Impl),
            ("const ", DeclKind::Const),
        ] {
            if let Some(rest) = code.strip_prefix(kw) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    out.push(Declaration {
                        name,
                        kind,
                        line: idx + 1,
                        depth,
                    });
                }
                break;
            }
        }
        // Update depth *after* recording, so a declaration reports the depth it lives at.
        depth = depth
            .saturating_add(code.matches('{').count())
            .saturating_sub(code.matches('}').count());
    }
    out
}

/// Localisation quality of one ranking strategy: did the expected symbol land in the top K?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalisationScore {
    pub hits_at_k: usize,
    pub queries: usize,
}

impl LocalisationScore {
    pub fn rate(&self) -> f64 {
        if self.queries == 0 {
            0.0
        } else {
            self.hits_at_k as f64 / self.queries as f64
        }
    }
}

/// The existing rule: rank by symbol-name substring match, file order breaking ties.
pub fn rule_rank<'a>(decls: &'a [Declaration], query: &str, k: usize) -> Vec<&'a Declaration> {
    decls
        .iter()
        .filter(|d| d.name.to_lowercase().contains(&query.to_lowercase()))
        .take(k)
        .collect()
}

/// The candidate: same match, but prefer shallower declarations (a top-level `fn` beats a nested
/// helper with the same name) and fall back to line order.
pub fn syntax_rank<'a>(decls: &'a [Declaration], query: &str, k: usize) -> Vec<&'a Declaration> {
    let mut hits: Vec<&Declaration> = decls
        .iter()
        .filter(|d| d.name.to_lowercase().contains(&query.to_lowercase()))
        .collect();
    hits.sort_by_key(|d| (d.depth, d.line));
    hits.into_iter().take(k).collect()
}

/// Promotion gate: the candidate may only replace the rule if it wins on localisation *and* does not
/// cost more. Both numbers must be supplied by a measurement, not by this module.
pub fn promotion_verdict(
    rule: LocalisationScore,
    candidate: LocalisationScore,
    cost_ratio: f64,
) -> &'static str {
    if candidate.rate() > rule.rate() && cost_ratio <= 1.0 {
        "promote"
    } else if candidate.rate() <= rule.rate() {
        "keep rule: candidate does not localise better"
    } else {
        "keep rule: candidate localises better but costs more"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
// fn commented_out() {}
fn top_level() {
    fn nested_helper() {}
    let s = "fn in_a_string() {}";
}
struct Thing { a: u8 }
impl Thing {
    fn method() {}
}
"#;

    #[test]
    fn declarations_are_found_with_depth_and_ignoring_comments_and_strings() {
        let d = parse_declarations(SRC);
        let names: Vec<&str> = d.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"top_level"));
        assert!(names.contains(&"nested_helper"));
        assert!(names.contains(&"Thing"));
        assert!(names.contains(&"method"));
        assert!(
            !names.contains(&"commented_out"),
            "a commented declaration must not be parsed"
        );
        let nested = d.iter().find(|x| x.name == "nested_helper").unwrap();
        assert!(
            nested.depth >= 1,
            "a nested fn must report a non-zero depth"
        );
    }

    #[test]
    fn the_candidate_prefers_shallower_declarations_at_equal_match() {
        let src = "impl A {\n    fn target() {}\n}\nfn target() {}\n";
        let decls = parse_declarations(src);
        let ranked = syntax_rank(&decls, "target", 2);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].depth, 0, "the top-level fn must rank first");
    }

    #[test]
    fn the_rule_ranking_is_preserved_for_comparison() {
        let decls = parse_declarations(SRC);
        let r = rule_rank(&decls, "top", 5);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "top_level");
    }

    #[test]
    fn promotion_requires_both_better_localisation_and_no_extra_cost() {
        let rule = LocalisationScore {
            hits_at_k: 3,
            queries: 5,
        };
        let better = LocalisationScore {
            hits_at_k: 5,
            queries: 5,
        };
        let same = LocalisationScore {
            hits_at_k: 3,
            queries: 5,
        };
        assert_eq!(promotion_verdict(rule, better, 1.0), "promote");
        assert!(promotion_verdict(rule, better, 1.4)
            .starts_with("keep rule: candidate localises better but costs more"));
        assert!(promotion_verdict(rule, same, 0.9)
            .starts_with("keep rule: candidate does not localise better"));
    }
}
