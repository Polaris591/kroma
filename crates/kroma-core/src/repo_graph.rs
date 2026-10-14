//! Gate 7 (`v1.2.0`) — Symbol Dependency Graph & Personalized PageRank Navigation.
//!
//! A grep-only agent has to guess which files matter and burns turns re-reading them. This module
//! builds a deterministic *file ⇄ symbol* dependency graph from the workspace and ranks files with
//! Personalized PageRank seeded by the current task's query terms / focus files, so the first turn
//! already sees the cross-file call chain. The approach follows RepoGraph (ICLR 2025) and LocAgent
//! (ACL 2025).
//!
//! Everything is derived from workspace data through [`WorkspaceCap`], so sensitive paths, symlinks
//! and ignore rules are honoured exactly as in `find`/`grep`. No symbol index is written to disk and
//! nothing here mutates the workspace.

use crate::messages::ToolResultMeta;
use crate::sensitive_paths::SensitivePathGuard;
use crate::tools::find::{load_workspace_ignore_patterns, walk_workspace_files};
use crate::tools::ToolExecutionOutput;
use crate::types::ErrorCode;
use crate::workspace::WorkspaceCap;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::Path;
use std::time::Instant;

/// Hard cap on files indexed in one graph build (bounds worst-case latency on huge repositories).
pub const MAX_GRAPH_FILES: usize = 1200;
/// A single source file larger than this is skipped: minified bundles and generated blobs only add
/// noise to the reference graph.
pub const MAX_GRAPH_FILE_BYTES: u64 = 512 * 1024;
/// Total bytes read across all indexed files.
pub const MAX_GRAPH_TOTAL_BYTES: usize = 24 * 1024 * 1024;
/// PageRank damping factor for the cold-start walk (standard 0.85). With a uniform restart
/// vector this degenerates to degree centrality: "what is the core of this repository?".
pub const PAGERANK_DAMPING: f64 = 0.85;
/// Damping factor for an *explicitly seeded* walk. A repository's degree-central hubs (the
/// `lib.rs`/`types.rs` of the world) otherwise out-score the file a query actually names, so a
/// seeded walk restarts far more often (α = 0.65) and stays inside the seed's neighbourhood.
pub const PAGERANK_DAMPING_PERSONALIZED: f64 = 0.35;
/// Deterministic iteration count; the rank vector converges far below this for real repositories.
pub const PAGERANK_ITERATIONS: usize = 15;
/// Default number of ranked files returned by the `code_graph` tool.
pub const DEFAULT_MAX_NODES: usize = 12;
/// Personalization weights. They are large relative to the uniform `1.0` floor so that a query for
/// a symbol actually re-centres the random walk on the defining file and its call chain instead of
/// being drowned out by the repository's degree-central hubs.
const PATH_MATCH_WEIGHT: f64 = 60.0;
const SYMBOL_DEFINITION_WEIGHT: f64 = 150.0;
const FOCUS_FILE_WEIGHT: f64 = 400.0;
/// One-hop neighbours of a seed file (its callers and callees) are seeded too, so the ranked list
/// reads as "the file you asked about, then the call chain around it" rather than "the file, then
/// whichever file the repository mentions most".
const NEIGHBOUR_WEIGHT: f64 = 30.0;
/// Upper bound on `max_nodes` accepted from the model.
pub const MAX_MAX_NODES: usize = 40;
/// Signature lines rendered per ranked file.
const MAX_SYMBOLS_PER_FILE: usize = 6;
/// Neighbours rendered per ranked file.
const MAX_NEIGHBOURS_PER_FILE: usize = 4;

/// Source extensions the graph indexes, mirroring the Gate 4 language families.
pub const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "py", "pyi", "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "go", "c", "h", "cc",
    "cpp", "cxx", "hpp", "hh", "java",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Class,
    Interface,
    Type,
    Module,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "fn",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::Type => "type",
            SymbolKind::Module => "mod",
        }
    }
}

/// One symbol definition found in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDef {
    pub name: String,
    pub kind: SymbolKind,
    pub file_path: String,
    pub line: usize,
    pub signature: String,
}

impl SymbolDef {
    /// `12: fn authenticate_user(token: &str) -> Result<Session>`
    pub fn render(&self) -> String {
        format!("{}: {}", self.line, self.signature)
    }
}

/// A directed, weighted cross-file reference edge.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub symbol: String,
    pub weight: f64,
    /// B1 (`v1.4.0`): the kind of relationship this edge represents. Explicit `Import` edges carry
    /// more PageRank weight than heuristic `CallOrReference` edges, and `TypeOrSymbol` edges point
    /// at type definitions rather than call sites.
    pub kind: EdgeKind,
}

/// B1 (`v1.4.0`): typed dependency edge classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EdgeKind {
    /// The referencing file explicitly imports/uses the symbol (Rust `use`, Python `import`, TS
    /// `import`, C `#include`).
    Import,
    /// The symbol is referenced from an ordinary call or expression position (heuristic).
    #[default]
    CallOrReference,
    /// The symbol is a type-like definition (`struct` / `enum` / `trait` / `class` / `interface` /
    /// `type`).
    TypeOrSymbol,
}

#[derive(Debug, Clone)]
pub struct RankedFile {
    pub path: String,
    pub score: f64,
    pub symbols: Vec<SymbolDef>,
    pub called_by: Vec<String>,
    pub calls_into: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RepoGraphReport {
    pub query_terms: Vec<String>,
    pub focus_files: Vec<String>,
    pub ranked: Vec<RankedFile>,
    pub total_files: usize,
    pub total_symbols: usize,
    pub total_edges: usize,
    pub truncated: bool,
}

impl RepoGraphReport {
    pub fn render(&self) -> String {
        let mut out = format!(
            "[RepoGraph | files={} symbols={} edges={} | ranked={} | truncated={}]\n",
            self.total_files,
            self.total_symbols,
            self.total_edges,
            self.ranked.len(),
            self.truncated
        );
        if !self.query_terms.is_empty() {
            out.push_str(&format!("query_terms: {}\n", self.query_terms.join(", ")));
        }
        if !self.focus_files.is_empty() {
            out.push_str(&format!("focus_files: {}\n", self.focus_files.join(", ")));
        }
        if self.ranked.is_empty() {
            out.push_str("no ranked files (empty workspace or no indexable source files).\n");
            return out;
        }
        for (i, node) in self.ranked.iter().enumerate() {
            out.push_str(&format!(
                "rank {}: {} (score={:.4})\n",
                i + 1,
                node.path,
                node.score
            ));
            for sym in &node.symbols {
                out.push_str(&format!("  {} {}\n", sym.kind.as_str(), sym.render()));
            }
            if !node.called_by.is_empty() {
                out.push_str(&format!("  called_by: {}\n", node.called_by.join(", ")));
            }
            if !node.calls_into.is_empty() {
                out.push_str(&format!("  calls_into: {}\n", node.calls_into.join(", ")));
            }
        }
        out
    }
}

/// Deterministic file ⇄ symbol dependency graph over a workspace.
#[derive(Debug, Clone, Default)]
pub struct RepoGraph {
    files: Vec<String>,
    file_index: HashMap<String, usize>,
    symbols: Vec<SymbolDef>,
    edges: Vec<GraphEdge>,
    out_adj: Vec<Vec<(usize, f64)>>,
    in_adj: Vec<Vec<(usize, f64)>>,
    pub truncated: bool,
    /// Files skipped because they were sensitive, unreadable, oversized or not UTF-8.
    pub skipped_files: usize,
    /// B1 (`v1.4.0`): number of files served from the incremental parse cache instead of being
    /// re-parsed during this build.
    pub cache_hits: usize,
}

/// B1 (`v1.4.0`): the cached parse of a single source file, keyed by path. Reused when the file's
/// `(len, mtime_ns)` fingerprint is unchanged, so repeated graph builds do not re-parse the world.
#[derive(Debug, Clone)]
pub struct CachedFileParse {
    len: u64,
    mtime_ns: u64,
    /// Content hash, retained for diagnostics and future content-addressed reuse.
    content_sha256: String,
    byte_len: usize,
    symbols: Vec<SymbolDef>,
    tokens: BTreeSet<String>,
    import_tokens: BTreeSet<String>,
}

/// B1 (`v1.4.0`): an incremental per-file parse cache for [`RepoGraph::build_from_workspace_cached`].
#[derive(Debug, Clone, Default)]
pub struct RepoGraphCache {
    entries: HashMap<String, CachedFileParse>,
    /// Number of cache hits observed across all builds (telemetry only).
    pub hits: u64,
}

impl RepoGraphCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn get_fresh(
        &mut self,
        rel: &str,
        len: u64,
        mtime_ns: u64,
        content_sha256: &str,
    ) -> Option<&CachedFileParse> {
        // BUG-06: the content hash is part of the key, so a persistent cross-call cache cannot
        // serve a stale parse when a file's bytes changed but its `(len, mtime_ns)` fingerprint
        // happened to stay the same.
        let hit = self.entries.get(rel).is_some_and(|e| {
            e.len == len && e.mtime_ns == mtime_ns && e.content_sha256 == content_sha256
        });
        if hit {
            self.hits = self.hits.saturating_add(1);
            self.entries.get(rel)
        } else {
            None
        }
    }

    /// Content hash of a cached file's last parse (diagnostics / content-addressed reuse).
    pub fn content_sha256(&self, rel: &str) -> Option<&str> {
        self.entries.get(rel).map(|e| e.content_sha256.as_str())
    }

    fn insert(&mut self, rel: String, parsed: CachedFileParse) {
        self.entries.insert(rel, parsed);
    }

    /// Drop cache entries for files that no longer exist in the workspace.
    fn retain_paths(&mut self, files: &[String]) {
        if self.entries.is_empty() {
            return;
        }
        let live: std::collections::HashSet<&str> = files.iter().map(|s| s.as_str()).collect();
        self.entries.retain(|k, _| live.contains(k.as_str()));
    }
}

/// B1: nanoseconds since the Unix epoch for a file's modification time (0 when unavailable).
fn mtime_nanos(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// B1 (`v1.4.0`): tokens that appear in an explicit import/use/include statement, used to classify
/// graph edges as [`EdgeKind::Import`].
pub fn extract_import_tokens(rel: &str, content: &str) -> BTreeSet<String> {
    let Some(lang) = lang_of(rel) else {
        return BTreeSet::new();
    };
    let stripped = strip_comments_and_strings(lang, content);
    let mut tokens = BTreeSet::new();
    for raw_line in stripped.lines() {
        let line = raw_line.trim_start();
        let is_import = match lang {
            Lang::Rust => {
                line.starts_with("use ") || line.starts_with("pub use ") || line.starts_with("mod ")
            }
            Lang::Python => line.starts_with("import ") || line.starts_with("from "),
            Lang::TsJs => {
                line.starts_with("import ")
                    || line.starts_with("export ")
                    || line.contains("require(")
            }
            Lang::CLike | Lang::Java | Lang::Go => {
                line.starts_with("#include")
                    || line.starts_with("#import")
                    || line.starts_with("import ")
                    || line.starts_with("package ")
            }
        };
        if !is_import {
            continue;
        }
        let mut current = String::new();
        for ch in line.chars() {
            if ch.is_alphanumeric() || ch == '_' {
                current.push(ch);
            } else if current.len() >= 3 && !current.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                tokens.insert(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
        if current.len() >= 3 && !current.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            tokens.insert(current);
        }
    }
    tokens
}

impl RepoGraph {
    pub fn total_files(&self) -> usize {
        self.files.len()
    }

    pub fn total_symbols(&self) -> usize {
        self.symbols.len()
    }

    pub fn total_edges(&self) -> usize {
        self.edges.len()
    }

    pub fn files(&self) -> &[String] {
        &self.files
    }

    pub fn symbols(&self) -> &[SymbolDef] {
        &self.symbols
    }

    pub fn edges(&self) -> &[GraphEdge] {
        &self.edges
    }

    /// Build the graph by walking the workspace through [`WorkspaceCap`].
    ///
    /// Files are indexed in deterministic lexicographic order and the walk stops at `max_files`
    /// (or the byte caps), setting `truncated` so callers can report partial coverage honestly.
    pub fn build_from_workspace(cap: &WorkspaceCap, max_files: usize) -> RepoGraph {
        // BUG-06: reuse the per-workspace cache instead of building a throwaway one, so repeated
        // `code_graph` / `/graph` calls within a session do not re-parse unchanged files. The lock
        // is held only for this synchronous build; the cache is content-addressed, so sharing it
        // across threads / Env instances cannot return a stale graph.
        let mut cache = cap
            .repo_graph_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::build_from_workspace_cached(cap, max_files, &mut cache)
    }

    /// B1 (`v1.4.0`): like [`RepoGraph::build_from_workspace`], but reuses a persistent per-file
    /// parse cache. A file whose `(path, len, mtime_ns, content_sha256)` is unchanged is not
    /// re-parsed, so repeated `code_graph` / `/graph` calls within a session stay cheap.
    pub fn build_from_workspace_cached(
        cap: &WorkspaceCap,
        max_files: usize,
        cache: &mut RepoGraphCache,
    ) -> RepoGraph {
        let ignore_rules = load_workspace_ignore_patterns(cap);
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        let mut skipped_sensitive = 0usize;
        walk_workspace_files(
            cap,
            Path::new("."),
            &ignore_rules,
            &mut paths,
            &mut skipped_sensitive,
        );
        paths.sort();
        paths.dedup();

        let mut files: Vec<String> = Vec::new();
        let mut symbols: Vec<SymbolDef> = Vec::new();
        let mut file_symbols: Vec<Vec<usize>> = Vec::new();
        let mut file_tokens: Vec<BTreeSet<String>> = Vec::new();
        let mut file_import_tokens: Vec<BTreeSet<String>> = Vec::new();
        let mut skipped_files = skipped_sensitive;
        let mut truncated = false;
        let mut total_bytes = 0usize;
        let mut cache_hits = 0usize;

        for path in paths {
            let rel = path.to_string_lossy().replace('\\', "/");
            if !is_source_path(&rel) {
                continue;
            }
            if files.len() >= max_files {
                truncated = true;
                break;
            }
            if SensitivePathGuard::is_sensitive(Path::new(&rel)) {
                skipped_files += 1;
                continue;
            }
            let Ok((_, file)) = cap.open_existing_no_symlink(&rel) else {
                skipped_files += 1;
                continue;
            };
            let Ok(meta) = file.metadata() else {
                skipped_files += 1;
                continue;
            };
            if !meta.file_type().is_file() || meta.len() > MAX_GRAPH_FILE_BYTES {
                skipped_files += 1;
                continue;
            }
            let len = meta.len();
            let mtime_ns = mtime_nanos(&meta);

            let mut file = file;
            let mut bytes = Vec::with_capacity(len as usize);
            let mut limited = (&mut file).take(MAX_GRAPH_FILE_BYTES + 1);
            if limited.read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_GRAPH_FILE_BYTES
            {
                skipped_files += 1;
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                skipped_files += 1;
                continue;
            };
            let content_sha256 = crate::prompt::sha256_hex64(text.as_bytes());

            // B1: reuse the cached parse when the content hash matches, so a cache shared across
            // builds never serves a stale graph.
            if let Some(cached) = cache.get_fresh(&rel, len, mtime_ns, &content_sha256) {
                cache_hits += 1;
                total_bytes = total_bytes.saturating_add(cached.byte_len);
                let mut indices = Vec::with_capacity(cached.symbols.len());
                for def in &cached.symbols {
                    indices.push(symbols.len());
                    symbols.push(def.clone());
                }
                file_symbols.push(indices);
                file_tokens.push(cached.tokens.clone());
                file_import_tokens.push(cached.import_tokens.clone());
                files.push(rel);
                continue;
            }

            if total_bytes.saturating_add(text.len()) > MAX_GRAPH_TOTAL_BYTES {
                truncated = true;
                break;
            }
            total_bytes += text.len();

            let mut defs = Vec::new();
            extract_symbols_from_source(&rel, &text, &mut defs);
            let mut cached_symbols = Vec::with_capacity(defs.len());
            for mut def in defs {
                def.file_path = rel.clone();
                cached_symbols.push(def);
            }
            let tokens = extract_reference_tokens(&rel, &text);
            let import_tokens = extract_import_tokens(&rel, &text);

            let mut indices = Vec::with_capacity(cached_symbols.len());
            for def in &cached_symbols {
                indices.push(symbols.len());
                symbols.push(def.clone());
            }
            file_symbols.push(indices);
            file_tokens.push(tokens.clone());
            file_import_tokens.push(import_tokens.clone());
            files.push(rel.clone());

            cache.insert(
                rel,
                CachedFileParse {
                    len,
                    mtime_ns,
                    content_sha256,
                    byte_len: text.len(),
                    symbols: cached_symbols,
                    tokens,
                    import_tokens,
                },
            );
        }

        cache.retain_paths(&files);

        let mut graph = RepoGraph {
            file_index: files
                .iter()
                .enumerate()
                .map(|(i, f)| (f.clone(), i))
                .collect(),
            files,
            symbols,
            edges: Vec::new(),
            out_adj: Vec::new(),
            in_adj: Vec::new(),
            truncated,
            skipped_files,
            cache_hits: 0,
        };
        graph.cache_hits = cache_hits;
        graph.build_edges(&file_symbols, &file_tokens, &file_import_tokens);
        graph
    }

    fn build_edges(
        &mut self,
        file_symbols: &[Vec<usize>],
        file_tokens: &[BTreeSet<String>],
        file_import_tokens: &[BTreeSet<String>],
    ) {
        // symbol name -> indices of the symbols with that name.
        let mut definitions: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (idx, sym) in self.symbols.iter().enumerate() {
            if sym.name.len() < 3 {
                continue;
            }
            definitions.entry(sym.name.as_str()).or_default().push(idx);
        }

        let mut per_pair: BTreeMap<(usize, usize, String), f64> = BTreeMap::new();
        // B1: track whether the matched definition is a type-like symbol.
        let mut per_pair_is_type: BTreeMap<(usize, usize, String), bool> = BTreeMap::new();
        for (from_idx, tokens) in file_tokens.iter().enumerate() {
            let from_path = &self.files[from_idx];
            for token in tokens {
                let Some(defs) = definitions.get(token.as_str()) else {
                    continue;
                };
                let weight = specificity(token) / (defs.len() as f64);
                for &def_idx in defs {
                    let def = &self.symbols[def_idx];
                    if &def.file_path == from_path {
                        continue;
                    }
                    let Some(&to_idx) = self.file_index.get(&def.file_path) else {
                        continue;
                    };
                    *per_pair
                        .entry((from_idx, to_idx, token.clone()))
                        .or_insert(0.0) += weight;
                    let is_type = matches!(
                        def.kind,
                        SymbolKind::Struct
                            | SymbolKind::Enum
                            | SymbolKind::Trait
                            | SymbolKind::Class
                            | SymbolKind::Interface
                            | SymbolKind::Type
                    );
                    let entry = per_pair_is_type
                        .entry((from_idx, to_idx, token.clone()))
                        .or_insert(false);
                    *entry = *entry || is_type;
                }
            }
        }

        let mut out_adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); self.files.len()];
        let mut in_adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); self.files.len()];
        for ((from, to, symbol), raw_weight) in per_pair {
            // Hub guard: a single very common symbol must not dominate a file's rank contribution.
            let weight = raw_weight.min(8.0);
            if weight <= 0.0 {
                continue;
            }
            // B1 (`v1.4.0`): classify the edge and give explicit imports more PageRank weight than
            // heuristic name matches, so a real dependency outranks a coincidental identifier hit.
            let kind = if file_import_tokens
                .get(from)
                .is_some_and(|imports| imports.contains(&symbol))
            {
                EdgeKind::Import
            } else if per_pair_is_type
                .get(&(from, to, symbol.clone()))
                .copied()
                .unwrap_or(false)
            {
                EdgeKind::TypeOrSymbol
            } else {
                EdgeKind::CallOrReference
            };
            let weight = match kind {
                EdgeKind::Import => (weight * 1.5).min(8.0),
                EdgeKind::TypeOrSymbol => (weight * 1.2).min(8.0),
                EdgeKind::CallOrReference => weight,
            };
            if weight <= 0.0 {
                continue;
            }
            self.edges.push(GraphEdge {
                from: self.files[from].clone(),
                to: self.files[to].clone(),
                symbol,
                weight,
                kind,
            });
            out_adj[from].push((to, weight));
            in_adj[to].push((from, weight));
        }
        for list in out_adj.iter_mut().chain(in_adj.iter_mut()) {
            list.sort_by(|a, b| a.0.cmp(&b.0));
        }
        // `file_symbols` is kept only for symmetry with the walk; nothing else needs it here.
        let _ = file_symbols;
        self.out_adj = out_adj;
        self.in_adj = in_adj;
    }

    /// Rank files with Personalized PageRank.
    ///
    /// The personalization vector is seeded by (a) `seed_files` — usually files the agent just
    /// touched or was told to focus on — and (b) `query_terms` matched against file paths and symbol
    /// names. With no seeds at all the vector is uniform, which reduces PageRank to degree
    /// centrality: a sensible cold-start ordering for "what is the core of this repository?".
    pub fn query(
        &self,
        query_terms: &[String],
        seed_files: &[String],
        top_k: usize,
    ) -> RepoGraphReport {
        let n = self.files.len();
        if n == 0 {
            return RepoGraphReport {
                query_terms: query_terms.to_vec(),
                focus_files: seed_files.to_vec(),
                ranked: Vec::new(),
                total_files: 0,
                total_symbols: self.symbols.len(),
                total_edges: self.edges.len(),
                truncated: self.truncated,
            };
        }

        let lowered_terms: Vec<String> = query_terms
            .iter()
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .collect();

        let mut personal = vec![1.0f64; n];
        let mut seeds: Vec<usize> = Vec::new();
        let mark_seed =
            |personal: &mut Vec<f64>, seeds: &mut Vec<usize>, idx: usize, weight: f64| {
                personal[idx] += weight;
                if !seeds.contains(&idx) {
                    seeds.push(idx);
                }
            };
        for (idx, path) in self.files.iter().enumerate() {
            let lowered_path = path.to_ascii_lowercase();
            if lowered_terms.iter().any(|t| lowered_path.contains(t)) {
                mark_seed(&mut personal, &mut seeds, idx, PATH_MATCH_WEIGHT);
            }
            let defines_match = self.symbols.iter().any(|s| {
                s.file_path == *path
                    && lowered_terms
                        .iter()
                        .any(|t| s.name.to_ascii_lowercase().contains(t))
            });
            if defines_match {
                mark_seed(&mut personal, &mut seeds, idx, SYMBOL_DEFINITION_WEIGHT);
            }
        }
        for seed in seed_files {
            let normalized = seed.trim().trim_start_matches("./").replace('\\', "/");
            if let Some(&idx) = self.file_index.get(&normalized) {
                mark_seed(&mut personal, &mut seeds, idx, FOCUS_FILE_WEIGHT);
            } else if let Some(&idx) = self.file_index.get(seed.trim()) {
                mark_seed(&mut personal, &mut seeds, idx, FOCUS_FILE_WEIGHT);
            }
        }

        // One-hop expansion: the callers and callees of every seed.
        let mut neighbours: BTreeSet<usize> = BTreeSet::new();
        for &seed in &seeds {
            for (target, _) in &self.out_adj[seed] {
                neighbours.insert(*target);
            }
            for (source, _) in &self.in_adj[seed] {
                neighbours.insert(*source);
            }
        }
        for target in neighbours {
            if !seeds.contains(&target) {
                personal[target] += NEIGHBOUR_WEIGHT;
            }
        }

        let damping = if seeds.is_empty() {
            PAGERANK_DAMPING
        } else {
            PAGERANK_DAMPING_PERSONALIZED
        };

        let personal_sum: f64 = personal.iter().sum();
        if personal_sum <= 0.0 {
            personal = vec![1.0; n];
        }
        let personal_sum: f64 = personal.iter().sum();
        let personal: Vec<f64> = personal.into_iter().map(|v| v / personal_sum).collect();

        let scores = personalized_pagerank(n, &self.out_adj, &personal, damping);

        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            scores[b]
                .partial_cmp(&scores[a])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.files[a].cmp(&self.files[b]))
        });

        let top_k = top_k.max(1).min(n);
        let mut ranked = Vec::with_capacity(top_k);
        for &idx in order.iter().take(top_k) {
            let path = &self.files[idx];
            let mut symbols: Vec<SymbolDef> = self
                .symbols
                .iter()
                .filter(|s| s.file_path == *path)
                .cloned()
                .collect();
            symbols.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.name.cmp(&b.name)));
            symbols.truncate(MAX_SYMBOLS_PER_FILE);

            let called_by = self.neighbours(&self.in_adj, idx);
            let calls_into = self.neighbours(&self.out_adj, idx);
            ranked.push(RankedFile {
                path: path.clone(),
                score: scores[idx],
                symbols,
                called_by,
                calls_into,
            });
        }

        RepoGraphReport {
            query_terms: query_terms.to_vec(),
            focus_files: seed_files.to_vec(),
            ranked,
            total_files: n,
            total_symbols: self.symbols.len(),
            total_edges: self.edges.len(),
            truncated: self.truncated,
        }
    }

    fn neighbours(&self, adj: &[Vec<(usize, f64)>], idx: usize) -> Vec<String> {
        // A file pair is connected by one edge per referenced symbol, so the raw adjacency list
        // repeats the same neighbour once per symbol. Sum the weights per neighbour first: otherwise
        // `calls_into` / `called_by` list a path several times and crowd out other neighbours.
        let mut totals: BTreeMap<usize, f64> = BTreeMap::new();
        for &(peer, weight) in &adj[idx] {
            *totals.entry(peer).or_insert(0.0) += weight;
        }
        let mut list: Vec<(usize, f64)> = totals.into_iter().collect();
        list.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.files[a.0].cmp(&self.files[b.0]))
        });
        list.into_iter()
            .take(MAX_NEIGHBOURS_PER_FILE)
            .map(|(i, _)| self.files[i].clone())
            .collect()
    }
}

/// Deterministic Personalized PageRank with dangling-mass redistribution.
fn personalized_pagerank(
    n: usize,
    out_adj: &[Vec<(usize, f64)>],
    personal: &[f64],
    damping: f64,
) -> Vec<f64> {
    let mut out_weight = vec![0.0f64; n];
    for (i, list) in out_adj.iter().enumerate() {
        out_weight[i] = list.iter().map(|(_, w)| *w).sum();
    }
    let mut rank = personal.to_vec();
    let mut next = vec![0.0f64; n];
    for _ in 0..PAGERANK_ITERATIONS {
        let dangling: f64 = (0..n)
            .filter(|&i| out_weight[i] <= 0.0)
            .map(|i| rank[i])
            .sum();
        for slot in next.iter_mut() {
            *slot = 0.0;
        }
        for i in 0..n {
            if out_weight[i] <= 0.0 {
                continue;
            }
            let share = rank[i] / out_weight[i];
            for (target, weight) in &out_adj[i] {
                next[*target] += share * weight;
            }
        }
        for i in 0..n {
            next[i] = (1.0 - damping) * personal[i] + damping * (next[i] + dangling * personal[i]);
        }
        std::mem::swap(&mut rank, &mut next);
    }
    rank
}

/// Rarer (more specific) symbols carry more evidence about a real dependency.
fn specificity(name: &str) -> f64 {
    let base = if name.len() >= 8 {
        1.5
    } else if name.len() >= 5 {
        1.0
    } else {
        0.5
    };
    // All-lowercase short names are usually locals/fields, not cross-file symbols.
    if name.chars().all(|c| c.is_ascii_lowercase()) && name.len() < 6 {
        base * 0.5
    } else {
        base
    }
}

pub fn is_source_path(rel: &str) -> bool {
    let ext = rel
        .rsplit('/')
        .next()
        .unwrap_or(rel)
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    SOURCE_EXTENSIONS.contains(&ext.as_str())
}

// ---------------------------------------------------------------------------------------------
// Symbol extraction
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Python,
    TsJs,
    Go,
    CLike,
    Java,
}

fn lang_of(rel: &str) -> Option<Lang> {
    let ext = rel
        .rsplit('/')
        .next()
        .unwrap_or(rel)
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "rs" => Some(Lang::Rust),
        "py" | "pyi" => Some(Lang::Python),
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => Some(Lang::TsJs),
        "go" => Some(Lang::Go),
        "java" => Some(Lang::Java),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" => Some(Lang::CLike),
        _ => None,
    }
}

/// Line-oriented symbol extractor. Signatures are the trimmed declaration line up to the body
/// opener, which is enough for a model to decide whether the file is worth reading in full.
pub fn extract_symbols_from_source(rel: &str, content: &str, out: &mut Vec<SymbolDef>) {
    let Some(lang) = lang_of(rel) else {
        return;
    };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with('#') && lang != Lang::Python
        {
            continue;
        }
        let Some((name, kind)) = match_symbol(lang, trimmed) else {
            continue;
        };
        if name.len() < 3 || !seen.insert(name.clone()) {
            continue;
        }
        out.push(SymbolDef {
            name,
            kind,
            file_path: rel.to_string(),
            line: line_no,
            signature: trim_signature(trimmed),
        });
    }
}

fn trim_signature(trimmed: &str) -> String {
    let cut = trimmed
        .find('{')
        .or_else(|| trimmed.find("=>"))
        .unwrap_or(trimmed.len());
    let sig = trimmed[..cut].trim_end().trim_end_matches(':').trim_end();
    if sig.len() > 160 {
        format!("{}…", &sig[..floor_char_boundary(sig, 160)])
    } else {
        sig.to_string()
    }
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Strip common visibility / async / export prefixes so the declaration keyword is at position 0.
fn strip_decl_prefixes(mut line: &str) -> &str {
    loop {
        let before = line;
        for prefix in [
            "pub(crate) ",
            "pub(super) ",
            "pub ",
            "export default ",
            "export ",
            "default ",
            "public ",
            "private ",
            "protected ",
            "internal ",
            "static ",
            "final ",
            "abstract ",
            "sealed ",
            "open ",
            "async ",
            "unsafe ",
            "extern ",
        ] {
            if let Some(rest) = line.strip_prefix(prefix) {
                line = rest;
                break;
            }
        }
        if line == before {
            return line;
        }
    }
}

/// Remove balanced `<...>` groups so `impl<T> RepoGraph` and `impl<T: Trait> Foo<T>` both reduce to
/// the plain type name.
fn strip_angle_groups(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut depth = 0usize;
    for ch in input.chars() {
        match ch {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

fn ident_at(s: &str) -> Option<String> {
    let name: String = s
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() || name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        None
    } else {
        Some(name)
    }
}

fn match_symbol(lang: Lang, trimmed: &str) -> Option<(String, SymbolKind)> {
    let decl = strip_decl_prefixes(trimmed);
    match lang {
        Lang::Rust => {
            for (kw, kind) in [
                ("fn ", SymbolKind::Function),
                ("struct ", SymbolKind::Struct),
                ("enum ", SymbolKind::Enum),
                ("trait ", SymbolKind::Trait),
                ("type ", SymbolKind::Type),
                ("mod ", SymbolKind::Module),
                ("union ", SymbolKind::Struct),
            ] {
                if let Some(rest) = decl.strip_prefix(kw) {
                    return ident_at(rest).map(|n| (n, kind));
                }
            }
            // `impl WorkspaceCap {` / `impl Drop for InFlightTracker {` / `impl<T> RepoGraph {`:
            // the *type* being implemented is the cross-file anchor a type-name query wants, so it
            // is recorded as a definition even though Rust has no `impl` symbol of its own.
            if let Some(rest) = decl.strip_prefix("impl ") {
                let cleaned = strip_angle_groups(rest);
                let target = match cleaned.split_once(" for ") {
                    Some((_, target)) => target,
                    None => cleaned.as_str(),
                };
                let target = target
                    .trim_start()
                    .trim_start_matches('&')
                    .trim_start()
                    .strip_prefix("mut ")
                    .unwrap_or(target.trim_start());
                if let Some(name) = ident_at(target) {
                    return Some((name, SymbolKind::Type));
                }
            }
            None
        }
        Lang::Python => {
            for (kw, kind) in [
                ("def ", SymbolKind::Function),
                ("class ", SymbolKind::Class),
            ] {
                if let Some(rest) = decl.strip_prefix(kw) {
                    return ident_at(rest).map(|n| (n, kind));
                }
            }
            None
        }
        Lang::TsJs => {
            for (kw, kind) in [
                ("function ", SymbolKind::Function),
                ("class ", SymbolKind::Class),
                ("interface ", SymbolKind::Interface),
                ("type ", SymbolKind::Type),
                ("enum ", SymbolKind::Enum),
                ("namespace ", SymbolKind::Module),
            ] {
                if let Some(rest) = decl.strip_prefix(kw) {
                    return ident_at(rest).map(|n| (n, kind));
                }
            }
            // `const handler = (req) => ...` / `const handler = function ...`
            //
            // `ident_at` trims leading whitespace before collecting the identifier, so `rest` must
            // be trimmed first: otherwise `rest[name.len()..]` slices at an offset that ignores the
            // whitespace and panics inside a multi-byte identifier (`const  函数 = ...`).
            if let Some(rest) = decl.strip_prefix("const ").map(str::trim_start) {
                let name = ident_at(rest)?;
                let after = rest[name.len()..].trim_start();
                if after.starts_with("= (")
                    || after.starts_with("= async (")
                    || after.starts_with("= function")
                    || after.starts_with("= async function")
                {
                    return Some((name, SymbolKind::Function));
                }
            }
            None
        }
        Lang::Go => {
            if let Some(rest) = decl.strip_prefix("func ") {
                // `func (r *T) Name(` → method receiver, otherwise `func Name(`.
                let rest = if rest.starts_with('(') {
                    let close = rest.find(')')?;
                    rest[close + 1..].trim_start()
                } else {
                    rest
                };
                return ident_at(rest).map(|n| (n, SymbolKind::Function));
            }
            if let Some(rest) = decl.strip_prefix("type ").map(str::trim_start) {
                let name = ident_at(rest)?;
                let tail = rest[name.len()..].trim_start();
                let kind = if tail.starts_with("struct") {
                    SymbolKind::Struct
                } else if tail.starts_with("interface") {
                    SymbolKind::Interface
                } else {
                    SymbolKind::Type
                };
                return Some((name, kind));
            }
            None
        }
        Lang::Java => {
            for (kw, kind) in [
                ("class ", SymbolKind::Class),
                ("interface ", SymbolKind::Interface),
                ("enum ", SymbolKind::Enum),
                ("record ", SymbolKind::Struct),
            ] {
                if let Some(rest) = decl.strip_prefix(kw) {
                    return ident_at(rest).map(|n| (n, kind));
                }
            }
            java_like_method(decl)
        }
        Lang::CLike => {
            for (kw, kind) in [
                ("struct ", SymbolKind::Struct),
                ("class ", SymbolKind::Class),
                ("enum ", SymbolKind::Enum),
                ("namespace ", SymbolKind::Module),
                ("union ", SymbolKind::Struct),
            ] {
                if let Some(rest) = decl.strip_prefix(kw) {
                    return ident_at(rest).map(|n| (n, kind));
                }
            }
            if let Some(rest) = decl.strip_prefix("typedef ") {
                return rest
                    .trim_end_matches(';')
                    .rsplit(char::is_whitespace)
                    .find(|t| !t.is_empty() && t.chars().all(|c| c.is_alphanumeric() || c == '_'))
                    .map(|n| (n.to_string(), SymbolKind::Type));
            }
            java_like_method(decl)
        }
    }
}

/// Conservative C-like/Java method heuristic: `Type name(args)` with no trailing `;` and no
/// assignment. Deliberately rejects anything ambiguous so the graph stays low-noise.
fn java_like_method(decl: &str) -> Option<(String, SymbolKind)> {
    if decl.ends_with(';') || decl.contains('=') || decl.starts_with("return ") {
        return None;
    }
    let open = decl.find('(')?;
    let head = decl[..open].trim_end();
    if head.is_empty() || head.contains(['{', '}', '"', '\'']) {
        return None;
    }
    // Reject control flow that looks like a call.
    let first = head.split_whitespace().next().unwrap_or("");
    if matches!(
        first,
        "if" | "for" | "while" | "switch" | "catch" | "do" | "else" | "case"
    ) {
        return None;
    }
    let name = head
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|t| !t.is_empty())?;
    if name.len() < 3 {
        return None;
    }
    // Require a return-type-ish token before the name for C-like languages.
    if head.len() <= name.len() {
        return None;
    }
    Some((name.to_string(), SymbolKind::Function))
}

// ---------------------------------------------------------------------------------------------
// Reference token extraction
// ---------------------------------------------------------------------------------------------

/// Extract the identifier set a file references, with comments and string literals removed so a
/// symbol mentioned only in prose or in a log message does not create a dependency edge.
pub fn extract_reference_tokens(rel: &str, content: &str) -> BTreeSet<String> {
    let Some(lang) = lang_of(rel) else {
        return BTreeSet::new();
    };
    let stripped = strip_comments_and_strings(lang, content);
    let mut tokens = BTreeSet::new();
    let mut current = String::new();
    for ch in stripped.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            if current.len() >= 3 && !current.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                tokens.insert(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    if current.len() >= 3 && !current.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        tokens.insert(current);
    }
    tokens
}

/// Length (in chars) of a Rust char or byte literal starting at `chars[i]`, or `None` when the
/// `'` opens a lifetime (`'a`, `'static`) rather than a literal. A lifetime must not be treated as
/// a string opener: doing so erases the rest of the line (everything up to the next `'` or the
/// newline), which drops the symbols and call edges the graph is built from.
fn rust_char_literal_len(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i) != Some(&'\'') {
        return None;
    }
    let mut j = i + 1;
    if chars.get(j) == Some(&'\\') {
        j += 1;
        match chars.get(j) {
            // `'\u{1F600}'` / `'\u{41}'`.
            Some('u') if chars.get(j + 1) == Some(&'{') => {
                let mut k = j + 2;
                while k < chars.len() && chars[k] != '}' && chars[k] != '\n' {
                    k += 1;
                }
                return (chars.get(k) == Some(&'}') && chars.get(k + 1) == Some(&'\''))
                    .then(|| k + 2 - i);
            }
            // `'\x41'` (byte literal).
            Some('x')
                if chars.get(j + 1).is_some_and(|c| c.is_ascii_hexdigit())
                    && chars.get(j + 2).is_some_and(|c| c.is_ascii_hexdigit())
                    && chars.get(j + 3) == Some(&'\'') =>
            {
                return Some(j + 4 - i);
            }
            // `'\n'`, `'\''`, `'\\'`, …
            Some(c) if *c != '\n' && chars.get(j + 1) == Some(&'\'') => {
                return Some(j + 2 - i);
            }
            _ => return None,
        }
    }
    match chars.get(j) {
        Some(c) if *c != '\'' && *c != '\n' && chars.get(j + 1) == Some(&'\'') => Some(j + 2 - i),
        _ => None,
    }
}

fn strip_comments_and_strings(lang: Lang, content: &str) -> String {
    let hash_comments = matches!(lang, Lang::Python);
    let mut out = String::with_capacity(content.len());
    let bytes: Vec<char> = content.chars().collect();
    let mut i = 0usize;
    let n = bytes.len();
    while i < n {
        let c = bytes[i];
        // Line comment
        if (c == '/' && i + 1 < n && bytes[i + 1] == '/') || (hash_comments && c == '#') {
            while i < n && bytes[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // Block comment
        if c == '/' && i + 1 < n && bytes[i + 1] == '*' {
            i += 2;
            while i + 1 < n && !(bytes[i] == '*' && bytes[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(n);
            out.push(' ');
            continue;
        }
        // Rust char / byte literals (`'x'`, `'\n'`, `'\u{1F600}'`) are literals, but a lone `'`
        // is a lifetime (`fn f<'a>(x: &'a str)`, `where T: 'static`). Only consume the literal
        // form; leave the lifetime quote in place so the rest of the line is not erased.
        if lang == Lang::Rust && c == '\'' {
            if let Some(len) = rust_char_literal_len(&bytes, i) {
                i += len;
                out.push(' ');
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        // String literals (double, single, backtick). Triple-quoted Python strings degrade to a
        // sequence of single-quoted strings, which still removes the payload.
        if c == '"' || c == '\'' || (c == '`' && matches!(lang, Lang::TsJs)) {
            let quote = c;
            i += 1;
            while i < n {
                if bytes[i] == '\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                if bytes[i] == '\n' && quote != '`' {
                    break;
                }
                i += 1;
            }
            out.push(' ');
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// `code_graph` kernel tool
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeGraphArgs {
    pub query: Option<String>,
    pub focus_files: Option<Vec<String>>,
    pub max_nodes: Option<usize>,
}

pub fn execute_code_graph(workspace: &WorkspaceCap, raw_args: &str) -> ToolExecutionOutput {
    let started = Instant::now();
    let args: CodeGraphArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                format!("invalid JSON arguments for `code_graph`: {}", e),
                ToolResultMeta::default(),
            );
        }
    };

    let max_nodes = args
        .max_nodes
        .unwrap_or(DEFAULT_MAX_NODES)
        .clamp(1, MAX_MAX_NODES);

    let mut focus_files = Vec::new();
    if let Some(list) = args.focus_files {
        if list.len() > 32 {
            return ToolExecutionOutput::error(
                ErrorCode::InvalidArguments,
                "`focus_files` accepts at most 32 entries".to_string(),
                ToolResultMeta::default(),
            );
        }
        for entry in list {
            let normalized = entry.trim().trim_start_matches("./").replace('\\', "/");
            if normalized.is_empty() {
                continue;
            }
            if let Err(e) = WorkspaceCap::validate_relative_components(&normalized) {
                return ToolExecutionOutput::error(
                    e.error_code(),
                    format!("invalid `focus_files` entry '{}': {}", entry, e),
                    ToolResultMeta::default(),
                );
            }
            focus_files.push(normalized);
        }
    }

    let query_terms: Vec<String> = args
        .query
        .as_deref()
        .unwrap_or("")
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '.'))
        .map(str::trim)
        .filter(|t| t.len() >= 2)
        .map(str::to_string)
        .take(16)
        .collect();

    let graph = RepoGraph::build_from_workspace(workspace, MAX_GRAPH_FILES);
    let report = graph.query(&query_terms, &focus_files, max_nodes);

    let mut body = report.render();
    body.push_str(&format!(
        "[graph build: {} files indexed, {} skipped (sensitive/oversized/binary), {} ms]\n",
        graph.total_files(),
        graph.skipped_files,
        started.elapsed().as_millis()
    ));

    ToolExecutionOutput::success(
        body,
        ToolResultMeta {
            relative_path: None,
            duration_ms: started.elapsed().as_millis() as u64,
            ..Default::default()
        },
    )
}
