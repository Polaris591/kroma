//! Read-side operations for the project memory store: admin lookup, model
//! reads, search, admin listing, settings and statistics.
//!
//! Every statement here is parameterised and `search` never forwards raw user
//! text to FTS5. Matching runs in two bounded stages:
//!
//! * an FTS5 (`unicode61`) index over accepted memory text, ranked by `bm25`;
//! * a literal substring scan (`instr`) for recall the tokenizer cannot give:
//!   two-character CJK queries (a CJK run is one FTS token, so `中文` never
//!   matches `不支持中文检索`) and queries built from punctuation/operators.
//!
//! The literal stage uses `instr`, so `%` and `_` are ordinary characters and
//! LIKE wildcard escaping is unnecessary; quotes, `NEAR`, `*` and `-` are plain
//! text. It stops once the result limit is filled.
//!
//! Reads never take a write lock: the full-text index is verified when the
//! store is opened, and review evidence is measured on demand instead of being
//! cached in a table. Path hints on a memory are historical navigation data:
//! neither `read` nor `search` touches the filesystem or treats a hint as an
//! access grant.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Result};
use rusqlite::{params, Row};

use crate::model::{validate_paths, Memory, SearchHit, Settings, Stats, MAX_TEXT_CHARS};
use crate::store::{
    load_memory, memory_from_row, now_ts, read_settings, review_is_stale, Store, COLUMNS,
    STATE_ACCEPTED, STATE_CANDIDATE, STATE_FORGOTTEN, STATE_PENDING, STATE_PURGED,
};

/// Largest batch `read` will accept from the model.
const MAX_READ_IDS: usize = 8;
const MAX_SEARCH_HITS: usize = 20;
const MAX_LIST_LIMIT: usize = 20;
const MAX_RECALL: usize = 6;
/// One bound for every query string the store accepts, matching the host's own
/// command and tool limits.
const MAX_QUERY_CHARS: usize = 512;
const MAX_QUERY_TOKENS: usize = 8;
const MAX_ID_CHARS: usize = 128;
const MAX_SUMMARY_CHARS: usize = 160;
/// `model::validate_paths` allows at most eight path hints.
const MAX_PATH_FILTERS: usize = 8;

/// ?1 query, ?2 state, ?3 path-filter flag, ?4..?11 padded hints, ?12 limit,
/// ?13 now.
/// Padding uses the empty string, which `validate_paths` never produces, so
/// unused slots cannot match a stored hint.
const ACTIVE_ROW: &str = "(m.expires_at IS NULL OR m.expires_at > ?13)";

const PATH_FILTER: &str = "AND (?3 = 0 OR EXISTS (SELECT 1 FROM json_each(m.paths) hp \
     WHERE hp.value IN (?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)))";

impl Store {
    pub fn is_core(&self, id: &str) -> Result<bool> {
        crate::store::is_core_row(&self.conn, id)
    }

    /// Only explicit, user-pinned rules bypass task-keyword matching. Review
    /// notes and general memories never enter this always-on pack.
    pub fn core_memories(&self) -> Result<Vec<Memory>> {
        if !read_settings(&self.conn)?.use_enabled {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.id FROM memories m JOIN core_memories c ON m.id=c.memory_id
             WHERE m.state=?1 AND (m.expires_at IS NULL OR m.expires_at>?2)
             ORDER BY m.created_at ASC",
        )?;
        let ids = stmt
            .query_map(params![STATE_ACCEPTED, now_ts()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut result = Vec::new();
        for id in ids {
            if let Some(memory) = load_memory(&self.conn, &id)? {
                result.push(memory);
            }
        }
        Ok(result)
    }

    /// Admin lookup: active records and candidates are visible, tombstones are
    /// not. This is the only read that may return a candidate body.
    pub fn get(&self, id: &str) -> Result<Memory> {
        let id = check_id(id)?;
        let memory =
            load_memory(&self.conn, id)?.ok_or_else(|| anyhow::anyhow!("Memory not found"))?;
        match memory.state.as_str() {
            STATE_ACCEPTED | STATE_CANDIDATE | STATE_PENDING => Ok(memory),
            STATE_FORGOTTEN => bail!("Memory was forgotten"),
            STATE_PURGED => bail!("Memory was purged"),
            _ => bail!("Memory not found"),
        }
    }

    /// Model-facing read. Every requested id that is still active is returned;
    /// ids that are unknown, deleted or no longer active are reported instead of
    /// failing the batch, so one stale citation cannot blank a whole answer.
    pub fn read_partial(&self, ids: &[String]) -> Result<(Vec<Memory>, Vec<String>)> {
        if !read_settings(&self.conn)?.use_enabled {
            return Ok((Vec::new(), Vec::new()));
        }
        if ids.len() > MAX_READ_IDS {
            bail!("At most {MAX_READ_IDS} memories can be read in one request");
        }
        let mut unique: Vec<&str> = Vec::with_capacity(ids.len());
        let mut seen: HashSet<&str> = HashSet::new();
        for id in ids {
            let id = check_id(id)?;
            if seen.insert(id) {
                unique.push(id);
            }
        }
        let now = now_ts();
        let mut memories = Vec::with_capacity(unique.len());
        let mut missing = Vec::new();
        for id in unique {
            match load_memory(&self.conn, id)? {
                Some(memory) if is_active(&memory, now) => memories.push(memory),
                _ => missing.push(id.to_string()),
            }
        }
        Ok((memories, missing))
    }

    /// Model-facing read of the active memories in one batch.
    pub fn read(&self, ids: &[String]) -> Result<Vec<Memory>> {
        Ok(self.read_partial(ids)?.0)
    }

    /// Search accepted memories. Disabled `use` returns nothing. Path hints are
    /// a navigation filter applied in SQL before the row limit, never a
    /// filesystem check or permission.
    pub fn search(&self, query: &str, paths: &[String], limit: usize) -> Result<Vec<SearchHit>> {
        if !read_settings(&self.conn)?.use_enabled {
            return Ok(Vec::new());
        }
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            bail!("Search query is longer than {MAX_QUERY_CHARS} characters");
        }
        validate_paths(paths)?;
        let limit = limit.clamp(1, MAX_SEARCH_HITS);

        let mut hits: Vec<SearchHit> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(expression) = fts_expression(query) {
            for hit in self.ranked_hits(&expression, paths, limit)? {
                if seen.insert(hit.id.clone()) {
                    hits.push(hit);
                }
            }
        }
        if hits.len() < limit {
            for hit in self.literal_hits(query, paths, limit)? {
                if hits.len() >= limit {
                    break;
                }
                if seen.insert(hit.id.clone()) {
                    hits.push(hit);
                }
            }
        }
        Ok(hits)
    }

    /// Recall for `request_context`: only evidence relevant to this intent.
    /// Never fill an otherwise empty pack with unrelated recent notes: those
    /// would pollute every user task and invalidate the plugin's promise that a
    /// non-match is byte-identical to no plugin.
    ///
    /// The whole bounded intent is scored, not its first words, and an intent
    /// made only of instruction words recalls nothing.
    pub fn recall(&self, intent: &str, limit: usize) -> Result<Vec<Memory>> {
        if !read_settings(&self.conn)?.use_enabled {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, MAX_RECALL);
        let now = now_ts();
        let query: String = intent.chars().take(MAX_TEXT_CHARS).collect();
        let mut memories: Vec<Memory> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for hit in self.fuzzy_hits(&query, &[], limit)? {
            if memories.len() >= limit {
                break;
            }
            if let Some(memory) = load_memory(&self.conn, &hit.id)? {
                if is_active(&memory, now) && seen.insert(memory.id.clone()) {
                    memories.push(memory);
                }
            }
        }
        Ok(memories)
    }

    /// Recall by the distinctive terms of the whole bounded query. The ASCII arm
    /// is one ranked FTS query over every kept term (prefix-matched, so `merge`
    /// still finds `merging`), the CJK arm is one literal scan over the query's
    /// bigrams, and the merged result is truncated to a single global limit.
    pub fn fuzzy_hits(
        &self,
        query: &str,
        paths: &[String],
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let limit = limit.clamp(1, MAX_SEARCH_HITS);
        let terms = recall_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        validate_paths(paths)?;
        let mut hits: Vec<SearchHit> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(expression) = prefix_expression(&terms.ascii) {
            for hit in self.ranked_hits(&expression, paths, limit)? {
                if seen.insert(hit.id.clone()) {
                    hits.push(hit);
                }
            }
        }
        if hits.len() < limit {
            for hit in self.bigram_hits(&terms.cjk, paths, limit)? {
                if hits.len() >= limit {
                    break;
                }
                if seen.insert(hit.id.clone()) {
                    hits.push(hit);
                }
            }
        }
        hits.truncate(limit);
        Ok(hits)
    }

    /// Admin listing of live states, newest first. Any other state (including
    /// tombstones) is rejected instead of being filtered after the limit.
    pub fn list(&self, state: &str, limit: usize, offset: usize) -> Result<Vec<Memory>> {
        if state != STATE_ACCEPTED && state != STATE_CANDIDATE && state != STATE_PENDING {
            bail!("List state must be accepted, candidate or pending");
        }
        let limit = limit.clamp(1, MAX_LIST_LIMIT) as i64;
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        let sql = format!(
            "SELECT {COLUMNS} FROM memories WHERE state=?1 \
             ORDER BY updated_at DESC, id DESC LIMIT ?2 OFFSET ?3"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![state, limit, offset], memory_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn settings(&self) -> Result<Settings> {
        read_settings(&self.conn)
    }

    /// `forgotten` covers both tombstones (forgotten and purged).
    pub fn stats(&self) -> Result<Stats> {
        let (accepted, pending, candidates, forgotten) = self.conn.query_row(
            "SELECT COALESCE(SUM(state=?1), 0), COALESCE(SUM(state=?2), 0),
                    COALESCE(SUM(state=?3), 0), COALESCE(SUM(state IN (?4, ?5)), 0)
               FROM memories",
            params![
                STATE_ACCEPTED,
                STATE_PENDING,
                STATE_CANDIDATE,
                STATE_FORGOTTEN,
                STATE_PURGED
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?;
        let versions: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM versions", [], |row| row.get(0))?;
        Ok(Stats {
            accepted: accepted as u64,
            pending: pending as u64,
            candidates: candidates as u64,
            forgotten: forgotten as u64,
            versions: versions as u64,
        })
    }

    /// Whether this worktree can still vouch for a review note's evidence. The
    /// verdict is computed from this worktree's recorded baselines on demand, so
    /// no refresh pass is needed before a read, and a changed file only marks
    /// the note as needing a re-check: it never hides the note from search, read
    /// or recall.
    pub fn is_stale(&self, id: &str) -> Result<bool> {
        let Some(memory) = load_memory(&self.conn, id)? else {
            return Ok(false);
        };
        if memory.state != STATE_ACCEPTED || memory.kind != "review" {
            return Ok(false);
        }
        review_is_stale(&self.conn, Path::new(&self.root), id)
    }

    fn ranked_hits(
        &self,
        expression: &str,
        paths: &[String],
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let sql = format!(
            "SELECT m.id, m.kind, m.text, m.version, m.paths
               FROM memories_fts
               JOIN memories m ON m.rowid = memories_fts.rowid
              WHERE memories_fts MATCH ?1
                AND m.state = ?2
                AND {ACTIVE_ROW}
                {PATH_FILTER}
              ORDER BY bm25(memories_fts) ASC, m.updated_at DESC
              LIMIT ?12"
        );
        self.collect_hits(&sql, expression, paths, limit)
    }

    fn literal_hits(&self, query: &str, paths: &[String], limit: usize) -> Result<Vec<SearchHit>> {
        let sql = format!(
            "SELECT m.id, m.kind, m.text, m.version, m.paths
               FROM memories m
              WHERE m.state = ?2
                AND {ACTIVE_ROW}
                AND instr(lower(m.text), lower(?1)) > 0
                {PATH_FILTER}
              ORDER BY m.updated_at DESC
              LIMIT ?12"
        );
        self.collect_hits(&sql, query, paths, limit)
    }

    /// Literal hits for a set of CJK bigrams, best overlap first. One statement
    /// covers the whole intent and stops at the row limit.
    fn bigram_hits(
        &self,
        bigrams: &[String],
        paths: &[String],
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        if bigrams.is_empty() {
            return Ok(Vec::new());
        }
        let needles = serde_json::to_string(bigrams)?;
        let sql = format!(
            "SELECT m.id, m.kind, m.text, m.version, m.paths
               FROM memories m
              WHERE m.state = ?2
                AND {ACTIVE_ROW}
                AND (SELECT COUNT(*) FROM json_each(?1) t WHERE instr(m.text, t.value) > 0) > 0
                {PATH_FILTER}
              ORDER BY (SELECT COUNT(*) FROM json_each(?1) t
                         WHERE instr(m.text, t.value) > 0) DESC,
                       m.updated_at DESC
              LIMIT ?12"
        );
        self.collect_hits(&sql, &needles, paths, limit)
    }

    fn collect_hits(
        &self,
        sql: &str,
        needle: &str,
        paths: &[String],
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let padded = padded_paths(paths);
        let filter = i64::from(!paths.is_empty());
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(
            params![
                needle,
                STATE_ACCEPTED,
                filter,
                padded[0],
                padded[1],
                padded[2],
                padded[3],
                padded[4],
                padded[5],
                padded[6],
                padded[7],
                limit as i64,
                now_ts(),
            ],
            hit_from_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// Active means accepted and not past a legacy expiry.
fn is_active(memory: &Memory, now: i64) -> bool {
    memory.state == STATE_ACCEPTED && memory.expires_at.is_none_or(|at| at > now)
}

fn check_id(id: &str) -> Result<&str> {
    let id = id.trim();
    if id.is_empty() || id.chars().count() > MAX_ID_CHARS {
        bail!("Memory id is not a valid identifier");
    }
    Ok(id)
}

fn padded_paths(paths: &[String]) -> [&str; MAX_PATH_FILTERS] {
    let mut padded = [""; MAX_PATH_FILTERS];
    for (slot, path) in padded.iter_mut().zip(paths) {
        *slot = path.as_str();
    }
    padded
}

/// Distinctive terms of one bounded intent: ASCII words and CJK bigrams.
/// Generic instruction words are dropped, because recalling on them would
/// inject an unrelated rule into every task.
struct RecallTerms {
    ascii: Vec<String>,
    cjk: Vec<String>,
}

fn recall_terms(query: &str) -> RecallTerms {
    let mut words: Vec<String> = Vec::new();
    let mut runs: Vec<Vec<char>> = Vec::new();
    let mut word = String::new();
    let mut run: Vec<char> = Vec::new();
    let flush_word = |word: &mut String, out: &mut Vec<String>| {
        if !word.is_empty() {
            out.push(std::mem::take(word));
        }
    };
    let flush_run = |run: &mut Vec<char>, out: &mut Vec<Vec<char>>| {
        if !run.is_empty() {
            out.push(std::mem::take(run));
        }
    };
    for ch in query.chars() {
        if is_cjk(ch) {
            flush_word(&mut word, &mut words);
            run.push(ch);
        } else if ch.is_ascii_alphanumeric() {
            flush_run(&mut run, &mut runs);
            word.push(ch.to_ascii_lowercase());
        } else {
            flush_word(&mut word, &mut words);
            flush_run(&mut run, &mut runs);
        }
    }
    flush_word(&mut word, &mut words);
    flush_run(&mut run, &mut runs);

    // The whole bounded intent is scored, so every distinct term is kept. A
    // rule that shares only a late or short word must still be found; bounding
    // the list here would silently drop exactly those terms.
    let mut ascii: Vec<String> = Vec::new();
    for word in words {
        if word.len() < 3 || GENERIC_TERMS.contains(&word.as_str()) {
            continue;
        }
        if !ascii.iter().any(|known| known == &word) {
            ascii.push(word);
        }
    }
    let mut cjk: Vec<String> = Vec::new();
    for run in runs {
        for pair in run.windows(2) {
            let bigram: String = pair.iter().collect();
            if GENERIC_CJK.contains(&bigram.as_str()) {
                continue;
            }
            if !cjk.iter().any(|known| known == &bigram) {
                cjk.push(bigram);
            }
        }
    }
    RecallTerms { ascii, cjk }
}

impl RecallTerms {
    fn is_empty(&self) -> bool {
        self.ascii.is_empty() && self.cjk.is_empty()
    }
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// `term* OR ...` for ASCII tokens only, which FTS5 accepts as prefix queries
/// without quoting. Each term also contributes its inflected stem, because a
/// suffix moves the word past a plain prefix match: `merge` does not prefix
/// `merging` (the `e` is dropped), so both sides supply a stemmed prefix.
fn prefix_expression(terms: &[String]) -> Option<String> {
    let mut out = String::new();
    for term in terms {
        for variant in prefix_variants(term) {
            if !out.is_empty() {
                out.push_str(" OR ");
            }
            out.push_str(&variant);
            out.push('*');
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The term plus its common inflections trimmed to a shared stem, never shorter
/// than four characters so a stem cannot match half the store.
fn prefix_variants(term: &str) -> Vec<String> {
    let mut out = vec![term.to_string()];
    for suffix in ["ing", "ed", "es", "s", "e"] {
        if let Some(base) = term.strip_suffix(suffix) {
            if base.len() >= 4 && !out.iter().any(|known| known.as_str() == base) {
                out.push(base.to_string());
            }
        }
    }
    out.retain(|variant| variant.chars().all(|ch| ch.is_ascii_alphanumeric()));
    out
}

/// Words that describe the act of asking, not the subject of the rule.
const GENERIC_TERMS: &[&str] = &[
    "the",
    "and",
    "for",
    "not",
    "are",
    "was",
    "how",
    "can",
    "now",
    "this",
    "that",
    "with",
    "from",
    "have",
    "using",
    "use",
    "used",
    "please",
    "make",
    "change",
    "configure",
    "project",
    "about",
    "should",
    "would",
    "could",
    "does",
    "did",
    "also",
    "then",
    "than",
    "into",
    "just",
    "like",
    "only",
    "some",
    "very",
    "when",
    "what",
    "where",
    "which",
    "while",
    "your",
    "ours",
    "their",
    "there",
    "here",
    "more",
    "most",
    "other",
    "same",
    "such",
    "take",
    "give",
    "still",
    "even",
    "run",
    "runs",
    "test",
    "tests",
    "code",
    "file",
    "files",
    "fix",
    "fixes",
    "build",
    "error",
    "errors",
    "check",
    "checks",
    "add",
    "remove",
    "update",
    "work",
    "need",
    "want",
];

/// Function-word bigrams; they appear in any sentence and prove nothing.
const GENERIC_CJK: &[&str] = &[
    "这个", "那个", "一下", "什么", "我们", "你们", "他们", "可以", "已经", "然后", "就是", "不是",
    "这样", "那样", "时候", "东西", "怎么", "如何", "现在", "帮我", "给我", "一个", "还是", "如果",
    "因为", "所以",
];

/// Build an FTS5 MATCH expression from alphanumeric tokens only. `None` when
/// the query has no tokens, because FTS5 rejects an empty match expression.
fn fts_expression(query: &str) -> Option<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in query.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
            if tokens.len() == MAX_QUERY_TOKENS {
                break;
            }
        }
    }
    if !current.is_empty() && tokens.len() < MAX_QUERY_TOKENS {
        tokens.push(current);
    }
    if tokens.is_empty() {
        return None;
    }
    let mut out = String::new();
    for token in &tokens {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push('"');
        for ch in token.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
    }
    Some(out)
}

fn hit_from_row(row: &Row<'_>) -> rusqlite::Result<SearchHit> {
    let text: String = row.get(2)?;
    let raw_paths: String = row.get(4)?;
    let paths: Vec<String> = serde_json::from_str(&raw_paths).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(err))
    })?;
    Ok(SearchHit {
        id: row.get(0)?,
        kind: row.get(1)?,
        summary: summarize(&text),
        version: row.get::<_, i64>(3)? as u32,
        paths,
    })
}

/// One-line, length-bounded preview of untrusted memory text.
fn summarize(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_SUMMARY_CHARS {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(MAX_SUMMARY_CHARS - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NewMemory, Source};
    use std::path::Path;

    fn open(dir: &Path, name: &str) -> Store {
        let path = dir.join("workspaces").join(name).join("memory.sqlite3");
        Store::open(&path, name, &path.to_string_lossy()).expect("open store")
    }

    fn add(store: &mut Store, kind: &str, text: &str, paths: &[&str], accepted: bool) -> Memory {
        let input = NewMemory {
            kind: kind.to_string(),
            text: text.to_string(),
            paths: paths.iter().map(|path| path.to_string()).collect(),
            evidence_refs: Vec::new(),
            source: Source {
                origin: if accepted { "manual" } else { "model_proposal" }.to_string(),
                session_id: None,
                call_id: None,
            },
        };
        store.insert(&input, accepted).expect("insert memory")
    }

    fn hit_ids(hits: &[SearchHit]) -> Vec<String> {
        hits.iter().map(|hit| hit.id.clone()).collect()
    }

    fn hints(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn get_and_list_cover_admin_states_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let accepted = add(
            &mut store,
            "decision",
            "cache invalidation runs on rename",
            &["src/lib.rs"],
            true,
        );
        let candidate = add(
            &mut store,
            "pitfall",
            "maybe cache keys collide",
            &[],
            false,
        );

        assert_eq!(store.get(&accepted.id).unwrap().text, accepted.text);
        assert_eq!(store.get(&candidate.id).unwrap().state, "candidate");

        assert_eq!(store.list("accepted", 20, 0).unwrap().len(), 1);
        assert_eq!(store.list("candidate", 20, 0).unwrap().len(), 1);
        for state in ["forgotten", "purged", "all", "", "Accepted"] {
            assert!(
                store.list(state, 20, 0).is_err(),
                "state {state} must be rejected"
            );
        }

        assert_eq!(store.list("accepted", 0, 0).unwrap().len(), 1);
        assert!(store.list("accepted", 20, 50).unwrap().is_empty());
        assert!(store
            .list("accepted", usize::MAX, usize::MAX)
            .unwrap()
            .is_empty());

        store.forget(&accepted.id).unwrap();
        assert!(store.get(&accepted.id).is_err());
        assert!(store.list("accepted", 20, 0).unwrap().is_empty());
        store.purge(&candidate.id).unwrap();
        assert!(store.get(&candidate.id).is_err());
    }

    #[test]
    fn stats_counts_states_and_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let first = add(&mut store, "decision", "keep a version per edit", &[], true);
        add(
            &mut store,
            "reference",
            "queued note about versions",
            &[],
            false,
        );
        let third = add(
            &mut store,
            "pitfall",
            "older note about versions",
            &[],
            true,
        );

        let stats = store.stats().unwrap();
        assert_eq!(
            (stats.accepted, stats.candidates, stats.forgotten),
            (2, 1, 0)
        );
        assert_eq!(stats.versions, 3);

        store.forget(&first.id).unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(
            (stats.accepted, stats.candidates, stats.forgotten),
            (1, 1, 1)
        );
        assert_eq!(stats.versions, 4);

        store.purge(&third.id).unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(
            (stats.accepted, stats.candidates, stats.forgotten),
            (0, 1, 2)
        );
        assert_eq!(stats.versions, 3, "purge wipes stored history");

        let settings = store.settings().unwrap();
        assert!(settings.use_enabled && settings.capture_enabled);
        let off = store.set_mode(Some(false), Some(false)).unwrap();
        assert!(!off.use_enabled && !off.capture_enabled);
        assert!(!store.settings().unwrap().use_enabled);
    }

    #[test]
    fn read_returns_active_memories_in_requested_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let first = add(
            &mut store,
            "decision",
            "first accepted note",
            &["src/a.rs"],
            true,
        );
        let second = add(
            &mut store,
            "decision",
            "second accepted note",
            &["src/b.rs"],
            true,
        );

        assert!(store.read(&[]).unwrap().is_empty());
        let memories = store.read(&[second.id.clone(), first.id.clone()]).unwrap();
        assert_eq!(memories.len(), 2);
        assert_eq!(memories[0].id, second.id);
        assert_eq!(memories[1].id, first.id);

        let repeated = store.read(&[first.id.clone(), first.id.clone()]).unwrap();
        assert_eq!(repeated.len(), 1);

        let too_many: Vec<String> = (0..9).map(|n| format!("{n:032x}")).collect();
        assert!(store.read(&too_many).is_err());
        assert!(store.read_partial(&too_many).is_err());
    }

    #[test]
    fn read_returns_known_active_entries_and_reports_missing_ids() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let accepted = add(
            &mut store,
            "decision",
            "accepted note about reads",
            &[],
            true,
        );
        let candidate = add(
            &mut store,
            "pitfall",
            "candidate note about reads",
            &[],
            false,
        );
        let forgotten = add(
            &mut store,
            "decision",
            "forgotten note about reads",
            &[],
            true,
        );
        let purged = add(&mut store, "decision", "purged note about reads", &[], true);
        store.forget(&forgotten.id).unwrap();
        store.purge(&purged.id).unwrap();

        let (memories, missing) = store
            .read_partial(&[
                accepted.id.clone(),
                candidate.id.clone(),
                forgotten.id.clone(),
                purged.id.clone(),
                "0".repeat(32),
            ])
            .unwrap();
        assert_eq!(hit_ids_memories(&memories), vec![accepted.id.clone()]);
        assert_eq!(
            missing,
            vec![
                candidate.id.clone(),
                forgotten.id.clone(),
                purged.id.clone(),
                "0".repeat(32),
            ]
        );
        // The same batch read without the missing set still answers in full.
        assert_eq!(
            store
                .read(&[accepted.id.clone(), purged.id.clone()])
                .unwrap()
                .len(),
            1
        );
        // A malformed id is a request error, not a missing memory.
        assert!(store.read(&[String::new()]).is_err());
        assert!(store.read(&["   ".to_string()]).is_err());
    }

    fn hit_ids_memories(memories: &[Memory]) -> Vec<String> {
        memories.iter().map(|memory| memory.id.clone()).collect()
    }

    #[test]
    fn disabled_use_mode_returns_no_model_reads_or_hits() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let memory = add(
            &mut store,
            "decision",
            "journal mode stays WAL",
            &["src/lib.rs"],
            true,
        );
        assert_eq!(store.search("WAL", &hints(&[]), 5).unwrap().len(), 1);

        store.set_mode(Some(false), None).unwrap();
        assert!(store
            .read(std::slice::from_ref(&memory.id))
            .unwrap()
            .is_empty());
        assert!(store.search("WAL", &hints(&[]), 5).unwrap().is_empty());
        assert!(store.search("中文", &hints(&[]), 5).unwrap().is_empty());
        assert_eq!(store.get(&memory.id).unwrap().id, memory.id);
        assert_eq!(store.list("accepted", 5, 0).unwrap().len(), 1);

        store.set_mode(Some(true), None).unwrap();
        assert_eq!(
            store.read(std::slice::from_ref(&memory.id)).unwrap().len(),
            1
        );
        assert_eq!(store.search("WAL", &hints(&[]), 5).unwrap().len(), 1);
    }

    #[test]
    fn automatic_recall_never_backfills_unrelated_recent_memories() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        add(&mut store, "preference", "use pnpm for packages", &[], true);
        add(
            &mut store,
            "decision",
            "use SQLite WAL for transactions",
            &[],
            true,
        );

        assert!(store
            .recall("configure an unrelated CSS flex layout", 6)
            .unwrap()
            .is_empty());
        let hits = store.recall("pnpm packages", 6).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("pnpm"));
        assert!(store.recall("", 6).unwrap().is_empty());
    }

    #[test]
    fn recall_finds_a_cjk_rule_from_a_sentence() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let rule = add(
            &mut store,
            "decision",
            "提交前运行共享测试，失败则禁止合并",
            &[],
            true,
        );
        // The tokenizer cannot segment Chinese, so a sentence can only match
        // through bigrams. This is the case that used to recall nothing.
        let hits = store.recall("提交前要跑共享测试再合并", 6).unwrap();
        assert_eq!(hits.len(), 1, "a Chinese sentence must recall the rule");
        assert_eq!(hits[0].id, rule.id);
        assert!(store.recall("这个功能怎么做", 6).unwrap().is_empty());
    }

    #[test]
    fn recall_matches_word_forms_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let rule = add(
            &mut store,
            "decision",
            "run the shared test before merging",
            &[],
            true,
        );
        let hits = store.recall("how does the merge work", 6).unwrap();
        assert_eq!(hits.len(), 1, "merge must find merging");
        assert_eq!(hits[0].id, rule.id);
    }

    #[test]
    fn recall_ignores_instruction_words_that_would_match_anything() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        add(
            &mut store,
            "decision",
            "run the shared test before merging",
            &[],
            true,
        );
        // Precision beats recall here: injecting an unrelated rule into every
        // task is worse than missing a convenience reminder.
        assert!(store
            .recall("please run the tests again", 6)
            .unwrap()
            .is_empty());
        assert!(store
            .fuzzy_hits("please run the tests again", &hints(&[]), 20)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn recall_scores_the_whole_bounded_intent() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let rule = add(
            &mut store,
            "decision",
            "the flibbertigibbet cache must persist across restarts",
            &[],
            true,
        );
        // The only word that identifies the rule sits far past the first 256
        // characters, which the old prefix-only scan never looked at.
        let mut intent = "lorem ipsum dolor sit amet ".repeat(40);
        intent.push_str("and finally flibbertigibbet");
        assert!(intent.chars().count() > 512);
        let hits = store.recall(&intent, 6).unwrap();
        assert_eq!(hits.len(), 1, "a late term must still recall the rule");
        assert_eq!(hits[0].id, rule.id);
    }

    #[test]
    fn fuzzy_recall_respects_one_global_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        for index in 0..4 {
            add(
                &mut store,
                "reference",
                &format!("zebra record {index}"),
                &[],
                true,
            );
            add(
                &mut store,
                "reference",
                &format!("quokka record {index}"),
                &[],
                true,
            );
        }
        let hits = store.fuzzy_hits("zebra quokka", &hints(&[]), 3).unwrap();
        assert_eq!(hits.len(), 3, "the merged result is capped once, globally");
        assert_eq!(store.recall("zebra quokka", 2).unwrap().len(), 2);
    }

    #[test]
    fn recall_handles_an_intent_with_many_distinct_terms() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let rule = add(
            &mut store,
            "decision",
            "quantumbadger expands every checkpoint manifest",
            &[],
            true,
        );
        let filler: Vec<String> = (0..40).map(|n| format!("placeholder{n}wording")).collect();
        let intent = format!("{} quantumbadger", filler.join(" "));
        assert!(intent.chars().count() > 512);
        let hits = store.recall(&intent, 6).unwrap();
        assert_eq!(hits.len(), 1, "one global query must cover every term");
        assert_eq!(hits[0].id, rule.id);
    }

    #[test]
    fn search_sees_accepted_memories_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let live = add(
            &mut store,
            "decision",
            "cache invalidation runs on rename",
            &[],
            true,
        );
        let candidate = add(
            &mut store,
            "pitfall",
            "cache invalidation candidate idea",
            &[],
            false,
        );
        assert_eq!(candidate.state, "candidate");
        let dropped = add(
            &mut store,
            "decision",
            "cache invalidation older rule",
            &[],
            true,
        );
        let wiped = add(
            &mut store,
            "decision",
            "cache invalidation third rule",
            &[],
            true,
        );
        store.forget(&dropped.id).unwrap();
        store.purge(&wiped.id).unwrap();

        let hits = store.search("cache", &hints(&[]), 20).unwrap();
        assert_eq!(hit_ids(&hits), vec![live.id.clone()]);
        assert_eq!(hits[0].kind, "decision");
        assert_eq!(hits[0].version, 1);

        assert!(store
            .search("candidate", &hints(&[]), 20)
            .unwrap()
            .is_empty());
        assert!(store.search("older", &hints(&[]), 20).unwrap().is_empty());
        assert!(store.search("", &hints(&[]), 20).unwrap().is_empty());
        assert!(store.search("   ", &hints(&[]), 20).unwrap().is_empty());
        assert!(store
            .search(&"x".repeat(MAX_QUERY_CHARS + 1), &hints(&[]), 20)
            .is_err());
        assert!(store
            .search(&"x".repeat(MAX_QUERY_CHARS), &hints(&[]), 20)
            .is_ok());
    }

    #[test]
    fn search_finds_two_character_cjk_without_tokenizer_support() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let cjk = add(
            &mut store,
            "decision",
            "提交前运行共享测试，失败则禁止合并",
            &["src/cjk.rs"],
            true,
        );
        let english = add(
            &mut store,
            "decision",
            "run the shared test before merging",
            &[],
            true,
        );

        for query in ["测试", "共享", "合并", "提交前运行共享测试，失败则禁止合并"]
        {
            let hits = store.search(query, &hints(&[]), 20).unwrap();
            assert_eq!(hit_ids(&hits), vec![cjk.id.clone()], "query {query}");
        }
        assert!(store.search("测试", &hints(&[]), 20).unwrap()[0]
            .summary
            .contains("共享测试"));
        assert_eq!(
            hit_ids(&store.search("merging", &hints(&[]), 20).unwrap()),
            vec![english.id.clone()]
        );
    }

    #[test]
    fn search_treats_wildcards_quotes_and_operators_literally() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let literal = add(
            &mut store,
            "reference",
            "rate 50%_off for \"quoted\" exports",
            &[],
            true,
        );
        let wildish = add(&mut store, "reference", "budget 50ZZoff marker", &[], true);
        let star = add(
            &mut store,
            "reference",
            "glob *.rs matches everything",
            &[],
            true,
        );
        add(&mut store, "reference", "nothing relevant here", &[], true);

        assert_eq!(
            hit_ids(&store.search("50%_off", &hints(&[]), 20).unwrap()),
            vec![literal.id.clone()]
        );
        assert_eq!(
            hit_ids(&store.search("%_off", &hints(&[]), 20).unwrap()),
            vec![literal.id.clone()]
        );
        assert_eq!(
            hit_ids(&store.search("\"quoted\"", &hints(&[]), 20).unwrap()),
            vec![literal.id.clone()]
        );
        assert_eq!(
            hit_ids(&store.search("RATE", &hints(&[]), 20).unwrap()),
            vec![literal.id.clone()]
        );
        assert_eq!(
            hit_ids(&store.search("*", &hints(&[]), 20).unwrap()),
            vec![star.id.clone()]
        );
        assert_eq!(
            hit_ids(&store.search("*.rs", &hints(&[]), 20).unwrap()),
            vec![star.id.clone()]
        );

        assert!(store
            .search("NEAR(rate budget)", &hints(&[]), 20)
            .unwrap()
            .is_empty());
        assert_eq!(
            hit_ids(&store.search("-off", &hints(&[]), 20).unwrap()),
            vec![literal.id.clone()]
        );
        assert!(store
            .search("\"unterminated", &hints(&[]), 20)
            .unwrap()
            .is_empty());
        assert!(!hit_ids(&store.search("50%_off", &hints(&[]), 20).unwrap()).contains(&wildish.id));
    }

    #[test]
    fn search_applies_path_hints_before_the_row_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let wanted = add(
            &mut store,
            "decision",
            "alpha rollout order",
            &["src/lib.rs"],
            true,
        );
        for slot in 0..3 {
            add(
                &mut store,
                "decision",
                &format!("alpha rollout order variant {slot}"),
                &["other/x.rs"],
                true,
            );
        }

        let unfiltered = store.search("alpha", &hints(&[]), 20).unwrap();
        assert_eq!(unfiltered.len(), 4);
        let filtered = store.search("alpha", &hints(&["src/lib.rs"]), 1).unwrap();
        assert_eq!(hit_ids(&filtered), vec![wanted.id.clone()]);
        assert_eq!(
            store
                .search("alpha", &hints(&["src/lib.rs"]), 20)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store.search("alpha", &hints(&["src"]), 20).unwrap().len(),
            0
        );
        assert!(
            store.search("alpha", &hints(&["other/x.rs"]), 20).unwrap()[0].paths
                == vec!["other/x.rs".to_string()]
        );

        let cjk = add(
            &mut store,
            "decision",
            "冲突时以测试结果为准",
            &["src/cjk.rs"],
            true,
        );
        assert_eq!(
            hit_ids(&store.search("测试", &hints(&["src/cjk.rs"]), 5).unwrap()),
            vec![cjk.id]
        );
        assert!(store
            .search("测试", &hints(&["other/x.rs"]), 5)
            .unwrap()
            .is_empty());

        assert!(store.search("alpha", &hints(&["../escape"]), 5).is_err());
        assert!(store.search("alpha", &hints(&["/abs/path"]), 5).is_err());
        let many: Vec<String> = (0..9).map(|n| format!("src/{n}.rs")).collect();
        assert!(store.search("alpha", &many, 5).is_err());
        assert!(
            store
                .search("alpha", &hints(&["src/lib.rs"]), 100)
                .unwrap()
                .len()
                <= MAX_SEARCH_HITS
        );
    }

    #[test]
    fn search_bounds_hits_limits_and_summaries() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        for slot in 0..24 {
            add(
                &mut store,
                "reference",
                &format!("needle entry number {slot}"),
                &[],
                true,
            );
        }
        let long_text = format!("{} tail", "needle ".repeat(120));
        add(&mut store, "reference", long_text.trim(), &[], true);

        let hits = store.search("needle", &hints(&[]), 100).unwrap();
        assert_eq!(hits.len(), MAX_SEARCH_HITS);
        for hit in &hits {
            assert!(hit.summary.chars().count() <= MAX_SUMMARY_CHARS);
            assert!(!hit.summary.contains('\n'));
        }
        assert_eq!(store.search("needle", &hints(&[]), 0).unwrap().len(), 1);
        assert_eq!(store.search("needle", &hints(&[]), 1).unwrap().len(), 1);

        let long_hit = store.search("tail", &hints(&[]), 1).unwrap();
        assert_eq!(long_hit[0].summary.chars().count(), MAX_SUMMARY_CHARS);
        assert!(long_hit[0].summary.ends_with('…'));
    }

    #[test]
    fn search_recovers_a_missing_or_desynced_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let live = add(&mut store, "decision", "prebuilt index note", &[], true);
        add(
            &mut store,
            "reference",
            "queued index candidate",
            &[],
            false,
        );
        assert_eq!(
            hit_ids(&store.search("prebuilt", &hints(&[]), 20).unwrap()),
            vec![live.id.clone()]
        );

        // The index lost every row while its triggers survived.
        store.conn.execute("DELETE FROM memories_fts", []).unwrap();
        drop(store);
        let mut reopened = open(dir.path(), "s1");
        assert_eq!(
            hit_ids(&reopened.search("prebuilt", &hints(&[]), 20).unwrap()),
            vec![live.id.clone()],
            "opening the store repairs the index"
        );
        let later = add(&mut reopened, "decision", "indexed after reopen", &[], true);
        assert_eq!(
            hit_ids(&reopened.search("reopen", &hints(&[]), 20).unwrap()),
            vec![later.id.clone()]
        );

        // A destroyed table and its triggers are recreated from scratch.
        reopened
            .conn
            .execute_batch(
                "DROP TABLE memories_fts;
                 DROP TRIGGER memories_fts_insert;
                 DROP TRIGGER memories_fts_update;
                 DROP TRIGGER memories_fts_delete;",
            )
            .unwrap();
        drop(reopened);
        let mut again = open(dir.path(), "s1");
        assert_eq!(again.search("prebuilt", &hints(&[]), 20).unwrap().len(), 1);
        let fresh = add(&mut again, "pitfall", "recreated trigger note", &[], true);
        assert_eq!(
            hit_ids(&again.search("recreated", &hints(&[]), 20).unwrap()),
            vec![fresh.id.clone()]
        );
        let promoted = add(&mut again, "pitfall", "promoted later", &[], false);
        again.accept(&promoted.id).unwrap();
        assert_eq!(again.search("promoted", &hints(&[]), 20).unwrap().len(), 1);
    }

    #[test]
    fn invalid_and_foreign_ids_are_rejected_or_reported_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let mut other = open(dir.path(), "s2");
        let local = add(&mut store, "decision", "local note", &[], true);
        let foreign = add(&mut other, "decision", "foreign note", &[], true);

        assert_eq!(store.get(&local.id).unwrap().id, local.id);
        assert!(store.get(&foreign.id).is_err());
        assert!(store
            .read(std::slice::from_ref(&foreign.id))
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .read_partial(std::slice::from_ref(&foreign.id))
                .unwrap()
                .1,
            vec![foreign.id.clone()]
        );
        assert!(store.get("").is_err());
        assert!(store.get("   ").is_err());
        assert!(store.get(&"z".repeat(MAX_ID_CHARS + 1)).is_err());
        assert!(store.read(&["not-an-id".to_string()]).is_ok());
        assert_eq!(store.search("local", &hints(&[]), 20).unwrap().len(), 1);
        assert!(store.search("foreign", &hints(&[]), 20).unwrap().is_empty());
    }

    #[test]
    fn search_tracks_correction_and_forget() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let memory = add(&mut store, "decision", "alpha cache is enabled", &[], true);
        assert_eq!(store.search("alpha", &hints(&[]), 8).unwrap().len(), 1);

        store.correct(&memory.id, "beta cache is enabled").unwrap();
        assert_eq!(store.search("beta", &hints(&[]), 8).unwrap().len(), 1);
        assert!(store.search("alpha", &hints(&[]), 8).unwrap().is_empty());

        store.forget(&memory.id).unwrap();
        assert!(store.search("beta", &hints(&[]), 8).unwrap().is_empty());
        assert!(store.get(&memory.id).is_err());
    }
}
