//! T16 (KR-25): a hot cache must not change what a graph build covers.
//!
//! The defect this pins: the cache-hit branch added a file's cached byte length to the running total
//! *without checking the cap*, so the same workspace produced a larger index — and a different
//! declared coverage — just because the cache happened to be warm. The third test covers the other
//! half of the task: the corpus has to carry natural language, otherwise a question can only match if
//! an identifier happens to be spelled like the question.

use kroma_core::repo_graph::{
    execute_code_graph, extract_doc_tokens, extract_reference_tokens, GraphLimits, RepoGraph,
    RepoGraphCache,
};
use kroma_core::workspace::WorkspaceCap;
use std::path::PathBuf;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t16-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// 16 KiB of parseable Rust whose symbol names are unique per file.
fn source(i: usize, pad: usize) -> String {
    let mut text = format!("// module {i} handles the retry path\n");
    text.push_str(&format!("pub fn file_{i}_entry() -> usize {{ {} }}\n", i));
    while text.len() < pad {
        text.push_str(&format!("// filler for module {i} keeps the file large\n"));
    }
    text
}

fn write_files(root: &std::path::Path, count: usize, bytes: usize) {
    for i in 0..count {
        std::fs::write(root.join(format!("m{i}.rs")), source(i, bytes)).unwrap();
    }
}

/// Cold and warm builds stop at the same file and the same byte total.
#[test]
fn cached_and_uncached_graph_obey_same_total_cap() {
    let root = scratch("cap");
    // Eight files of ~16 KiB, with a 64 KiB total budget: exactly four fit.
    write_files(&root, 8, 16 * 1024);
    let cap = WorkspaceCap::new(&root).unwrap();
    let limits = GraphLimits {
        max_files: 64,
        max_total_bytes: 64 * 1024,
    };

    // Prime the cache with a build that has no byte budget, so every file is cached and the capped
    // builds below are *pure cache hits*. That is the configuration in which the old code indexed the
    // whole workspace: a hit skipped the budget check entirely.
    let mut cache = RepoGraphCache::default();
    let primed = RepoGraph::build_from_workspace_cached_with_limits(
        &cap,
        GraphLimits {
            max_files: 64,
            max_total_bytes: usize::MAX,
        },
        &mut cache,
    );
    assert_eq!(
        primed.file_hashes.len(),
        8,
        "the priming build must see every file"
    );

    let cold = RepoGraph::build_from_workspace_cached_with_limits(
        &cap,
        limits,
        &mut RepoGraphCache::default(),
    );
    assert_eq!(cold.cache_hits, 0, "a fresh cache cannot hit");
    assert!(
        cold.truncated,
        "the byte budget must be reported as truncated"
    );
    let cold_files = cold.file_hashes.clone();
    assert!(
        !cold_files.is_empty() && cold_files.len() < 8,
        "expected a partial index, got {} files",
        cold_files.len()
    );

    let warm = RepoGraph::build_from_workspace_cached_with_limits(&cap, limits, &mut cache);
    assert_eq!(
        warm.cache_hits,
        cold_files.len(),
        "every indexed file must be served from the primed cache"
    );
    assert!(
        warm.truncated,
        "the warm build must report the same truncation"
    );
    assert_eq!(
        warm.file_hashes, cold_files,
        "a warm cache changed which files the graph covers"
    );

    // And the cap really was enforced: the indexed bytes do not exceed the budget.
    let indexed: usize = cold_files
        .iter()
        .map(|(path, _)| std::fs::metadata(root.join(path)).unwrap().len() as usize)
        .sum();
    assert!(
        indexed <= 64 * 1024,
        "the cold build indexed {indexed} bytes against a 64 KiB cap"
    );
    let warm_indexed: usize = warm
        .file_hashes
        .iter()
        .map(|(path, _)| std::fs::metadata(root.join(path)).unwrap().len() as usize)
        .sum();
    assert!(
        warm_indexed <= 64 * 1024,
        "the warm build indexed {warm_indexed} bytes against a 64 KiB cap"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Changing an early file while the tail is cached still respects the cap, and the changed file is
/// re-parsed rather than served stale.
#[test]
fn changed_early_file_plus_cached_tail_is_bounded() {
    let root = scratch("changed");
    write_files(&root, 8, 16 * 1024);
    let cap = WorkspaceCap::new(&root).unwrap();
    let limits = GraphLimits {
        max_files: 64,
        max_total_bytes: 64 * 1024,
    };

    let mut cache = RepoGraphCache::default();
    let first = RepoGraph::build_from_workspace_cached_with_limits(&cap, limits, &mut cache);
    let first_files = first.file_hashes.clone();
    assert!(first_files.len() >= 2, "need at least two indexed files");

    // Rewrite the earliest indexed file with different content.
    let (changed_path, old_hash) = first_files[0].clone();
    std::fs::write(
        root.join(&changed_path),
        source(99, 16 * 1024).replace("retry", "recovery"),
    )
    .unwrap();

    let second = RepoGraph::build_from_workspace_cached_with_limits(&cap, limits, &mut cache);
    assert!(second.truncated, "the cap still applies");
    let second_indexed: usize = second
        .file_hashes
        .iter()
        .map(|(path, _)| std::fs::metadata(root.join(path)).unwrap().len() as usize)
        .sum();
    assert!(
        second_indexed <= 64 * 1024,
        "a changed file plus a cached tail exceeded the cap: {second_indexed} bytes"
    );

    let changed = second
        .file_hashes
        .iter()
        .find(|(path, _)| path == &changed_path)
        .expect("the changed file is still within the budget");
    assert_ne!(
        changed.1, old_hash,
        "the changed file must be re-parsed, not served from a stale cache entry"
    );
    assert!(
        second.cache_hits > 0,
        "the unchanged tail should still come from the cache"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Comments carry the natural-language signal; identifiers alone cannot answer a question.
#[test]
fn graph_corpus_has_natural_language_issues_not_only_symbol_names() {
    let content = r#"
// Exponential backoff with jitter is used when the upstream refuses a connection.
pub fn retry_delay(attempt: u32) -> u64 {
    let base = 2u64.pow(attempt);
    base
}
"#;

    // The identifier extractor strips comments, so it cannot see the words a question would use.
    let identifiers = extract_reference_tokens("src/retry.rs", content);
    assert!(identifiers.contains("retry_delay"));
    for word in ["backoff", "jitter", "exponential"] {
        assert!(
            !identifiers.contains(word),
            "the identifier index must not contain comment words ({word})"
        );
    }

    // The comment index does, which is what makes a natural-language query matchable.
    let docs = extract_doc_tokens("src/retry.rs", content);
    for word in ["backoff", "jitter", "exponential", "upstream"] {
        assert!(
            docs.contains(word),
            "comment word '{word}' missing from the doc index: {docs:?}"
        );
    }

    // End to end: a natural-language query reaches the file through the tool.
    let root = scratch("natural");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/retry.rs"), content).unwrap();
    std::fs::write(
        root.join("src/unrelated.rs"),
        "pub fn parse_config() -> usize { 1 }\n",
    )
    .unwrap();
    let cap = WorkspaceCap::new(&root).unwrap();
    let out = execute_code_graph(
        &cap,
        &serde_json::json!({"query": "exponential backoff jitter", "max_nodes": 5}).to_string(),
    );
    assert_eq!(
        out.outcome,
        kroma_core::messages::ToolOutcome::Success,
        "code_graph failed: {:?}",
        out.content
    );
    assert!(
        out.content.contains("retry.rs"),
        "a natural-language query must reach the file whose comment answers it: {}",
        out.content
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// C00-11: when the byte budget excludes every candidate, the report must say "truncated", not
/// "empty workspace". The two are different facts and the caller acts on them differently.
#[test]
fn budget_excluded_workspace_is_reported_as_truncated_not_empty() {
    let root = scratch("truncated-empty");
    write_files(&root, 2, 512);
    let cap = WorkspaceCap::new(&root).unwrap();
    let mut cache = RepoGraphCache::default();
    let graph = RepoGraph::build_from_workspace_cached_with_limits(
        &cap,
        GraphLimits {
            max_files: 64,
            max_total_bytes: 0,
        },
        &mut cache,
    );
    assert!(
        graph.files().is_empty(),
        "nothing fits in a zero-byte budget"
    );

    let report = graph.query(&["file_0_entry".to_string()], &[], 5);
    assert!(report.ranked.is_empty());
    assert!(report.truncated, "an all-excluding budget is truncation");
    let rendered = report.render();
    assert!(
        rendered.contains("budget excluded every candidate"),
        "truncation must be named as such: {rendered}"
    );
    assert!(
        !rendered.contains("empty workspace or no indexable"),
        "a truncated build must not be reported as an empty workspace: {rendered}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
