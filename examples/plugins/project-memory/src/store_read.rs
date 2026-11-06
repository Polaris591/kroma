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
//! text. Its cost is bounded by the store's `MAX_ROWS` cap and it stops once
//! the result limit is filled.
//!
//! Path hints on a memory are historical navigation data: neither `read` nor
//! `search` touches the filesystem or treats a hint as an access grant.

use std::collections::HashSet;

use anyhow::{bail, Result};
use rusqlite::{params, Row};

use crate::model::{validate_paths, Memory, SearchHit, Settings, Stats};
use crate::store::{
    load_memory, memory_from_row, now_ts, read_settings, Store, COLUMNS, MAX_ROWS, STATE_ACCEPTED,
    STATE_CANDIDATE, STATE_FORGOTTEN, STATE_PENDING, STATE_PURGED,
};

/// Largest batch `read` will accept from the model.
const MAX_READ_IDS: usize = 8;
const MAX_SEARCH_HITS: usize = 20;
const MAX_LIST_LIMIT: usize = 20;
const MAX_RECALL: usize = 6;
const MAX_QUERY_CHARS: usize = 256;
const MAX_QUERY_TOKENS: usize = 8;
const MAX_ID_CHARS: usize = 128;
const MAX_SUMMARY_CHARS: usize = 160;
/// `model::validate_paths` allows at most eight path hints.
const MAX_PATH_FILTERS: usize = 8;

/// ?1 query, ?2 state, ?3 path-filter flag, ?4..?11 padded hints, ?12 limit.
/// Padding uses the empty string, which `validate_paths` never produces, so
/// unused slots cannot match a stored hint.
const ACTIVE_ROW: &str = "(m.expires_at IS NULL OR m.expires_at > ?13)";

const PATH_FILTER: &str = "AND (?3 = 0 OR EXISTS (SELECT 1 FROM json_each(m.paths) hp \
     WHERE hp.value IN (?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)))";

const FTS_SCHEMA: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(text, tokenize='unicode61');
CREATE TRIGGER IF NOT EXISTS memories_fts_insert AFTER INSERT ON memories
WHEN new.state = 'accepted' BEGIN
    INSERT INTO memories_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TRIGGER IF NOT EXISTS memories_fts_update AFTER UPDATE ON memories BEGIN
    DELETE FROM memories_fts WHERE rowid = old.rowid;
    INSERT INTO memories_fts(rowid, text) SELECT new.rowid, new.text WHERE new.state = 'accepted';
END;
CREATE TRIGGER IF NOT EXISTS memories_fts_delete AFTER DELETE ON memories BEGIN
    DELETE FROM memories_fts WHERE rowid = old.rowid;
END;
"#;

impl Store {
    /// Admin lookup: accepted and pending candidates are visible, tombstones
    /// are not. This is the only read that may return a candidate body.
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

    /// Model-facing read of accepted memories only. Disabled `use` returns
    /// nothing; an unknown, candidate or tombstoned id fails the whole batch so
    /// a request can never be answered partially.
    pub fn read(&self, ids: &[String]) -> Result<Vec<Memory>> {
        if !read_settings(&self.conn)?.use_enabled {
            return Ok(Vec::new());
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
        for id in unique {
            let memory =
                load_memory(&self.conn, id)?.ok_or_else(|| anyhow::anyhow!("Memory not found"))?;
            if !is_active(&memory, now) {
                bail!("Only active memories can be read");
            }
            memories.push(memory);
        }
        Ok(memories)
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
        self.ensure_index()?;

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

    /// Admin listing of live states, newest first. Any other state (including
    /// tombstones) is rejected instead of being filtered after the limit.
    pub fn list(&self, state: &str, limit: usize, offset: usize) -> Result<Vec<Memory>> {
        if state != STATE_ACCEPTED && state != STATE_CANDIDATE && state != STATE_PENDING {
            bail!("List state must be accepted, candidate or pending");
        }
        let limit = limit.clamp(1, MAX_LIST_LIMIT) as i64;
        let offset = offset.min(MAX_ROWS as usize) as i64;
        let sql = format!(
            "SELECT {COLUMNS} FROM memories WHERE state=?1 \
             ORDER BY updated_at DESC, id DESC LIMIT ?2 OFFSET ?3"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![state, limit, offset], memory_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Recall for `request_context`: only evidence relevant to this intent.
    /// Never fill an otherwise empty pack with unrelated recent notes: those
    /// would pollute every user task and invalidate the zero-maintenance
    /// plugin's promise that a non-match is byte-identical to no plugin.
    pub fn recall(&self, intent: &str, limit: usize) -> Result<Vec<Memory>> {
        if !read_settings(&self.conn)?.use_enabled {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, MAX_RECALL);
        let now = now_ts();
        let query: String = intent.chars().take(MAX_QUERY_CHARS).collect();
        let mut memories: Vec<Memory> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for hit in self.search(&query, &[], limit)? {
            if let Some(memory) = load_memory(&self.conn, &hit.id)? {
                if is_active(&memory, now) && seen.insert(memory.id.clone()) {
                    memories.push(memory);
                }
            }
        }
        // A new task often contains surrounding words absent from the stored
        // rule ("pnpm dependency" vs "use pnpm for packages"). Exact multi-
        // token FTS requires *all* words. Retry distinctive single terms,
        // never the unrelated latest-N fallback, and bound the work per task.
        // Ignore generic instruction words that would recall nearly anything.
        if memories.len() < limit {
            let mut terms = Vec::new();
            for word in query.split(|ch: char| !ch.is_alphanumeric()) {
                let word = word.to_lowercase();
                // Three-letter tooling tokens such as bun, npm, git and SQL
                // can be decisive; do not discard them as generic noise.
                if word.chars().count() < 3
                    || matches!(
                        word.as_str(),
                        "the"
                            | "and"
                            | "for"
                            | "not"
                            | "use"
                            | "are"
                            | "was"
                            | "how"
                            | "can"
                            | "now"
                            | "this"
                            | "that"
                            | "with"
                            | "from"
                            | "have"
                            | "using"
                            | "please"
                            | "make"
                            | "change"
                            | "configure"
                            | "project"
                            | "about"
                            | "should"
                            | "would"
                            | "could"
                    )
                {
                    continue;
                }
                if !terms.contains(&word) {
                    terms.push(word);
                }
                if terms.len() == MAX_QUERY_TOKENS {
                    break;
                }
            }
            for term in terms {
                for hit in self.search(&term, &[], limit)? {
                    if memories.len() >= limit {
                        break;
                    }
                    if seen.contains(&hit.id) {
                        continue;
                    }
                    if let Some(memory) = load_memory(&self.conn, &hit.id)? {
                        if is_active(&memory, now) && seen.insert(memory.id.clone()) {
                            memories.push(memory);
                        }
                    }
                }
                if memories.len() >= limit {
                    break;
                }
            }
        }
        Ok(memories)
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

    /// Create the FTS table, its triggers and the initial index. Idempotent, so
    /// it is safe on every search and on a store written before the first one.
    fn ensure_index(&self) -> Result<()> {
        let existing: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memories_fts'",
            [],
            |row| row.get(0),
        )?;
        self.conn.execute_batch(FTS_SCHEMA)?;
        if existing == 0 {
            self.conn.execute(
                "INSERT INTO memories_fts (rowid, text)
                 SELECT m.rowid, m.text FROM memories m
                  WHERE m.state = ?1
                    AND (m.expires_at IS NULL OR m.expires_at > ?3)
                    AND NOT EXISTS (SELECT 1 FROM memories_fts f WHERE f.rowid = m.rowid)
                  ORDER BY m.rowid LIMIT ?2",
                params![STATE_ACCEPTED, MAX_ROWS, now_ts()],
            )?;
        }
        Ok(())
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

/// Active means accepted and not past its expiry.
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
        Store::open(&path, name).expect("open store")
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
    fn stats_counts_states_and_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let first = add(&mut store, "decision", "keep a version per edit", &[], true);
        add(
            &mut store,
            "reference",
            "pending note about versions",
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
        assert_eq!(stats.versions, 3, "purge wipes stored snapshots");

        let settings = store.settings().unwrap();
        assert!(settings.use_enabled && settings.capture_enabled);
        let off = store.set_mode(Some(false), Some(false)).unwrap();
        assert!(!off.use_enabled && !off.capture_enabled);
        assert!(!store.settings().unwrap().use_enabled);
    }

    #[test]
    fn read_returns_accepted_memories_in_requested_order() {
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
    }

    #[test]
    fn read_fails_whole_batch_on_unknown_candidate_or_tombstone() {
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

        assert_eq!(
            store
                .read(std::slice::from_ref(&accepted.id))
                .unwrap()
                .len(),
            1
        );
        for blocked in [&candidate.id, &forgotten.id, &purged.id] {
            assert!(store.read(std::slice::from_ref(blocked)).is_err());
            let mixed = store.read(&[accepted.id.clone(), blocked.clone()]);
            assert!(
                mixed.is_err(),
                "one unavailable id must fail the whole read"
            );
        }
        assert!(store.read(&["0".repeat(32)]).is_err());
        assert!(store.read(&[String::new()]).is_err());
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

        assert!(store.search("alpha", &hints(&[]), 100).unwrap().len() <= MAX_SEARCH_HITS);
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
    fn search_backfills_memories_written_before_the_index_existed() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let first = add(&mut store, "decision", "prebuilt index note", &[], true);
        add(&mut store, "pitfall", "prebuilt candidate note", &[], false);
        assert_eq!(store.search("prebuilt", &hints(&[]), 20).unwrap().len(), 1);

        drop(store);
        let mut reopened = open(dir.path(), "s1");
        let later = add(&mut reopened, "decision", "indexed after reopen", &[], true);
        assert_eq!(
            hit_ids(&reopened.search("reopen", &hints(&[]), 20).unwrap()),
            vec![later.id.clone()]
        );
        assert_eq!(
            hit_ids(&reopened.search("prebuilt", &hints(&[]), 20).unwrap()),
            vec![first.id.clone()]
        );

        let mut again = open(dir.path(), "s1");
        let promoted = add(&mut again, "pitfall", "promoted later", &[], false);
        again.accept(&promoted.id).unwrap();
        assert_eq!(again.search("promoted", &hints(&[]), 20).unwrap().len(), 1);
    }

    #[test]
    fn invalid_and_foreign_ids_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "s1");
        let mut other = open(dir.path(), "s2");
        let local = add(&mut store, "decision", "local note", &[], true);
        let foreign = add(&mut other, "decision", "foreign note", &[], true);

        assert_eq!(store.get(&local.id).unwrap().id, local.id);
        assert!(store.get(&foreign.id).is_err());
        assert!(store.read(std::slice::from_ref(&foreign.id)).is_err());
        assert!(store.get("").is_err());
        assert!(store.get("   ").is_err());
        assert!(store.get(&"z".repeat(MAX_ID_CHARS + 1)).is_err());
        assert!(store.read(&["not-an-id".to_string()]).is_err());
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
