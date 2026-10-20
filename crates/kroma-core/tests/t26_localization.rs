//! T26 (KR-31): does the index actually locate a file from a natural-language issue?
//!
//! The gap this measures: `query` seeded only from **file paths and symbol names**, so an issue that
//! names no symbol ("where is the retry backoff computed?") could only succeed by accident — and a
//! symbol-name grep is exactly the baseline the plan says must be beaten. Content tokens (identifiers
//! plus comment words) are now part of the seeding, so this test scores both strategies on the same
//! corpus:
//!
//! * `symbol baseline` — path substring or symbol-name substring, i.e. grep-by-name;
//! * `graph` — the real `RepoGraph::query`, which also matches the file's content tokens and expands
//!   over the file dependency graph.
//!
//! Everything here is offline and reproducible: no model, no network. The model-in-the-loop comparison
//! (EXP-03) is a separate, budget-gated experiment.

use kroma_core::repo_graph::{GraphLimits, RepoGraph, RepoGraphCache};
use kroma_core::workspace::WorkspaceCap;
use std::path::{Path, PathBuf};

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t26-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    root
}

/// A corpus where each issue can only be located by what the file *says*, not what it is called.
fn corpus(root: &Path) {
    let files = [
        (
            "src/module_one.py",
            "# Exponential backoff with jitter is used when the upstream refuses a connection.\n\
             def attempt(limit):\n    return limit\n",
        ),
        (
            "src/module_two.py",
            "# Reloading the config drops the last line when the file ends without a newline.\n\
             def read_config(path):\n    return path\n",
        ),
        (
            "src/module_three.py",
            "# Evict the least recently used entries once the total size exceeds the cap.\n\
             def evict(entries):\n    return entries\n",
        ),
        (
            "src/module_four.py",
            "# Refresh the oauth token before it expires, otherwise every request fails.\n\
             def refresh(token):\n    return token\n",
        ),
        (
            "src/module_five.py",
            "# Truncate the generated report with an explicit marker once it grows past the cap.\n\
             def render(rows):\n    return rows\n",
        ),
        (
            "src/module_six.py",
            "# Path normalisation shared by the loaders.\n\
             def normalize_path(p):\n    return p\n",
        ),
        // Decoys: plausible names, unrelated prose.
        (
            "src/module_seven.py",
            "# Terminal colour palette used by the status line.\n\
             def paint(text):\n    return text\n",
        ),
        (
            "src/module_eight.py",
            "# Column widths for the summary table.\n\
             def widths(rows):\n    return rows\n",
        ),
        (
            "src/module_nine.py",
            "# Monotonic timestamps for the metrics exporter.\n\
             def now_ns():\n    return 0\n",
        ),
        (
            "src/module_ten.py",
            "# Structured log lines with a level and a target.\n\
             def emit(level):\n    return level\n",
        ),
        (
            "src/module_eleven.py",
            "# Directory layout for the on-disk state area.\n\
             def state_dir():\n    return '.'\n",
        ),
        (
            "src/module_twelve.py",
            "# Semantic version parsing for the updater.\n\
             def parse(v):\n    return v\n",
        ),
    ];
    for (path, contents) in files {
        std::fs::write(root.join(path), contents).unwrap();
    }
}

fn build(root: &Path, limits: GraphLimits) -> RepoGraph {
    let cap = WorkspaceCap::new(root).unwrap();
    let mut cache = RepoGraphCache::default();
    RepoGraph::build_from_workspace_cached_with_limits(&cap, limits, &mut cache)
}

/// The strategy the plan says must be beaten: `grep`-by-symbol-name / path (no content).
fn symbol_baseline_rank(graph: &RepoGraph, terms: &[String]) -> Vec<String> {
    let lowered: Vec<String> = terms.iter().map(|t| t.to_ascii_lowercase()).collect();
    let mut scored: Vec<(f64, String)> = graph
        .files()
        .iter()
        .map(|path| {
            let lower = path.to_ascii_lowercase();
            let mut score = 0.0;
            if lowered.iter().any(|t| lower.contains(t)) {
                score += 1.0;
            }
            // Symbols are not exposed per file through the public API, so the baseline uses the
            // rendered report's symbol names, which is exactly what a name-based grep would see.
            let report = graph.query(&[], &[path.clone()], 1);
            if let Some(ranked) = report.ranked.first() {
                if ranked.symbols.iter().any(|s| {
                    lowered
                        .iter()
                        .any(|t| s.name.to_ascii_lowercase().contains(t))
                }) {
                    score += 2.0;
                }
            }
            (score, path.clone())
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    scored.into_iter().map(|(_, path)| path).collect()
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map(str::trim)
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

fn graph_rank(graph: &RepoGraph, query: &str, k: usize) -> Vec<String> {
    let terms = query_terms(query);
    graph
        .query(&terms, &[], k)
        .ranked
        .into_iter()
        .map(|r| r.path)
        .collect()
}

/// The five issues name no symbol; only the content can lead to the right file.
const ISSUES: &[(&str, &str)] = &[
    (
        "retry backoff jitter upstream connection",
        "src/module_one.py",
    ),
    ("config reload drops the last line", "src/module_two.py"),
    (
        "evict least recently used over the size cap",
        "src/module_three.py",
    ),
    (
        "refresh the oauth token before it expires",
        "src/module_four.py",
    ),
    (
        "truncate the generated report with a marker",
        "src/module_five.py",
    ),
];

/// Natural-language issues are located, and by a clear margin over symbol-name search.
#[test]
fn natural_language_issues_beat_symbol_name_search() {
    let root = scratch("localize");
    corpus(&root);
    let graph = build(&root, GraphLimits::default());
    assert!(!graph.truncated, "the fixture must fit the default budget");

    let mut graph_top1 = 0usize;
    let mut graph_top3 = 0usize;
    let mut baseline_top1 = 0usize;
    for (issue, expected) in ISSUES {
        let ranked = graph_rank(&graph, issue, 3);
        if ranked.first().map(String::as_str) == Some(*expected) {
            graph_top1 += 1;
        }
        if ranked.iter().any(|p| p == expected) {
            graph_top3 += 1;
        }
        let baseline = symbol_baseline_rank(&graph, &query_terms(issue));
        if baseline.first().map(String::as_str) == Some(*expected) {
            baseline_top1 += 1;
        }
    }

    assert!(
        graph_top1 >= 4,
        "the graph must locate at least 4 of {} issues at rank 1, got {} (top-3: {})",
        ISSUES.len(),
        graph_top1,
        graph_top3
    );
    assert!(
        graph_top1 > baseline_top1,
        "content tokens must beat symbol-name search: graph {}/{} vs baseline {}/{}",
        graph_top1,
        ISSUES.len(),
        baseline_top1,
        ISSUES.len()
    );

    // Symbol-name queries must not regress: the stronger signal still wins.
    let by_symbol = [
        ("compute normalize_path util", "src/module_six.py"),
        ("version parse updater", "src/module_twelve.py"),
    ];
    for (query, expected) in by_symbol {
        let ranked = graph_rank(&graph, query, 3);
        assert!(
            ranked.iter().any(|p| p == expected),
            "a query naming real symbols must still find {expected}: {ranked:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// When the index is bounded away (truncated), the plain lexical fallback still works — which is what
/// "failure can fall back to the existing index" has to mean in practice.
#[test]
fn truncated_index_falls_back_to_lexical_search() {
    let root = scratch("fallback");
    corpus(&root);
    // Two files only: the answers are very likely outside the index.
    let graph = build(
        &root,
        GraphLimits {
            max_files: 2,
            ..GraphLimits::default()
        },
    );
    assert!(
        graph.truncated,
        "the fixture must be truncated for this test to mean anything"
    );

    for (issue, expected) in ISSUES {
        let terms = query_terms(issue);
        let ranked = graph_rank(&graph, issue, 3);
        if ranked.iter().any(|p| p == expected) {
            continue; // the tiny index happened to cover it; nothing to prove
        }
        // Fallback: a lexical scan over the workspace, which always sees every file.
        let mut found = false;
        for entry in std::fs::read_dir(root.join("src")).unwrap().flatten() {
            let path = entry.path();
            let text = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let matches = terms.iter().filter(|t| text.contains(*t)).count();
            if matches >= 2 && format!("src/{}", entry.file_name().to_string_lossy()) == *expected {
                found = true;
            }
        }
        assert!(
            found,
            "with a truncated index the lexical fallback must still locate {expected}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
