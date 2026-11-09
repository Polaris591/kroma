//! SQLite persistence for project memory: schema, opening and mutations.
//!
//! Read methods live in `store_read.rs`. Every mutation runs inside an
//! IMMEDIATE transaction so writers serialise on an early reserved lock.
//!
//! There is one write path for a durable memory: a write is accepted
//! immediately, and the stored rule text is the user's verbatim quote. Nothing
//! in this module archives, expires or promotes a record on a timer, and no
//! storage quota silently drops a user rule.

use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::model::*;

pub(crate) const SCHEMA_VERSION: i64 = 4;
/// Bounds one legacy import request, not the size of the store.
const MAX_IMPORT_NOTES: usize = 2_000;
const BUSY_TIMEOUT_MS: u64 = 1_000;

pub(crate) const STATE_PENDING: &str = "pending";
pub(crate) const STATE_CANDIDATE: &str = "candidate";
pub(crate) const STATE_ACCEPTED: &str = "accepted";
pub(crate) const STATE_FORGOTTEN: &str = "forgotten";
pub(crate) const STATE_PURGED: &str = "purged";

const META_IDENTITY: &str = "identity";
const META_USE: &str = "use_enabled";
const META_CAPTURE: &str = "capture_enabled";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    text          TEXT NOT NULL,
    state         TEXT NOT NULL,
    version       INTEGER NOT NULL,
    paths         TEXT NOT NULL,
    evidence_refs TEXT NOT NULL,
    source        TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    fingerprint   TEXT NOT NULL UNIQUE,
    source_quote  TEXT,
    expires_at    INTEGER,
    supersedes    TEXT
);
CREATE TABLE IF NOT EXISTS metadata (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS versions (
    memory_id   TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    version     INTEGER NOT NULL,
    fingerprint TEXT NOT NULL,
    PRIMARY KEY (memory_id, version)
);
CREATE TABLE IF NOT EXISTS imports (
    source_id TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS tombstones (
    fingerprint TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS memory_files (
    memory_id     TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    root          TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    digest        TEXT NOT NULL,
    PRIMARY KEY (memory_id, root, relative_path)
);
CREATE TABLE IF NOT EXISTS core_memories (
    memory_id TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE
);
"#;

const INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS memories_state_updated ON memories(state, updated_at DESC);
"#;

/// Columns added after schema version 1, applied as additive migrations.
const ADDED_COLUMNS: &[(&str, &str)] = &[
    ("source_quote", "TEXT"),
    ("expires_at", "INTEGER"),
    ("supersedes", "TEXT"),
];

/// The full-text index and its triggers. It is verified when the store is
/// opened, so a search is a pure read and never takes a write lock.
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

pub struct Store {
    pub(crate) conn: Connection,
    /// Canonical worktree whose file fingerprints this connection evaluates.
    pub(crate) root: String,
}

thread_local! {
    /// Databases this process has already created, migrated, defaulted and
    /// repaired. The worker serves one request at a time, so a plain per-process
    /// set is enough and keeps the open path cheap.
    static INITIALIZED: std::cell::RefCell<std::collections::BTreeSet<(String, String)>> =
        const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

impl Store {
    /// Open (or create) the workspace database and bind it to `scope_identity`
    /// and one worktree `root`.
    pub fn open(path: &Path, scope_identity: &str, root: &str) -> Result<Store> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create {}", parent.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open memory database {}", path.display()))?;
        conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        let store = Store {
            conn,
            root: worktree_key(Path::new(root)),
        };
        store.init(scope_identity)?;
        Ok(store)
    }

    fn init(&self, scope_identity: &str) -> Result<()> {
        // The schema version is always checked: a database written by a newer
        // build must fail loudly even in a long-lived worker process.
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            bail!(
                "Memory database schema version {version} is newer than supported version {SCHEMA_VERSION}"
            );
        }
        // Creating tables, migrating, restoring retired states, wiping purged
        // rows and seeding the flags is one-time work per database per process.
        // A recall or a tool call must not pay for it again on the same file.
        let key = (self.root.clone(), scope_identity.to_string());
        let first = INITIALIZED.with(|done| done.borrow_mut().insert(key));
        if first {
            self.conn
                .execute_batch(SCHEMA)
                .context("Failed to create memory schema")?;
            migrate(&self.conn, version)?;
            self.conn
                .execute_batch(&format!("PRAGMA user_version={SCHEMA_VERSION};"))?;
            self.claim_identity(scope_identity)?;
            init_flag(&self.conn, META_USE, true)?;
            init_flag(&self.conn, META_CAPTURE, true)?;
        } else {
            self.verify_identity(scope_identity)?;
        }
        // A missing or desynchronised full-text index is repaired whenever it is
        // noticed; the check is three cheap queries.
        repair_index(&self.conn)?;
        Ok(())
    }

    /// Cheap identity re-check for a database this process already initialised.
    fn verify_identity(&self, scope_identity: &str) -> Result<()> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                params![META_IDENTITY],
                |row| row.get(0),
            )
            .optional()?;
        match stored.as_deref() {
            Some(value) if value != scope_identity => {
                bail!("Memory database belongs to a different workspace scope")
            }
            Some(_) => Ok(()),
            None => bail!("Memory database is missing its workspace identity"),
        }
    }

    /// Bind a fresh database to its workspace, refusing a foreign or unlabelled
    /// file that already holds memories.
    fn claim_identity(&self, scope_identity: &str) -> Result<()> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                params![META_IDENTITY],
                |row| row.get(0),
            )
            .optional()?;
        match stored {
            Some(value) if value != scope_identity => {
                bail!("Memory database belongs to a different workspace scope")
            }
            Some(_) => Ok(()),
            None => {
                let rows: i64 =
                    self.conn
                        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
                if rows > 0 {
                    bail!("Memory database is missing its workspace identity");
                }
                self.conn.execute(
                    "INSERT INTO metadata(key, value) VALUES (?1, ?2)",
                    params![META_IDENTITY, scope_identity],
                )?;
                Ok(())
            }
        }
    }
    /// Pin or unpin one project-wide rule. Pinning requires an active user
    /// preference or decision; unpinning is idempotent. Neither changes the
    /// rule body, so the version is left alone.
    pub fn set_core(&mut self, id: &str, core: bool) -> Result<Memory> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let memory = require_memory(&tx, id)?;
        if core {
            mark_core_row(&tx, id)?;
        } else {
            unpin_core_row(&tx, id)?;
        }
        tx.commit()?;
        Ok(memory)
    }

    pub fn mark_core(&mut self, id: &str) -> Result<()> {
        self.set_core(id, true).map(|_| ())
    }

    /// Explicit save: a manual command, a review note or a verified tool write.
    /// Identical content is idempotent, wording the user previously forgot or
    /// purged may be saved again on purpose, and `accepted=false` stores a
    /// candidate for the admin queue.
    pub fn insert(&mut self, input: &NewMemory, accepted: bool) -> Result<Memory> {
        validate_input(input)?;
        let text = input.text.trim().to_string();
        let fingerprint = fingerprint(&input.kind, &text, &input.paths);
        let row = NewRow {
            state: if accepted {
                STATE_ACCEPTED
            } else {
                STATE_CANDIDATE
            },
            source_quote: None,
            supersedes: None,
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let memory = match load_by_fingerprint(&tx, &fingerprint)? {
            Some(existing) if existing.state == STATE_ACCEPTED => existing,
            Some(existing) if accepted => revive_row(&tx, &existing, input, &text, &row)?,
            Some(existing) => existing,
            None => insert_row(&tx, input, &text, fingerprint, &row)?,
        };
        tx.commit()?;
        Ok(memory)
    }

    pub fn accept(&mut self, id: &str) -> Result<Memory> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = require_memory(&tx, id)?;
        let memory = match existing.state.as_str() {
            STATE_ACCEPTED => existing,
            STATE_CANDIDATE | STATE_PENDING => set_state(&tx, &existing, STATE_ACCEPTED)?,
            STATE_PURGED => bail!("Memory was purged"),
            _ => bail!("Only pending candidates can be accepted"),
        };
        tx.commit()?;
        Ok(memory)
    }

    pub fn correct(&mut self, id: &str, text: &str) -> Result<Memory> {
        validate_text(text)?;
        let text = text.trim().to_string();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = require_memory(&tx, id)?;
        if existing.state != STATE_CANDIDATE && existing.state != STATE_ACCEPTED {
            bail!("Memory is not editable");
        }
        let fingerprint = fingerprint(&existing.kind, &text, &existing.paths);
        if let Some(other) = load_by_fingerprint(&tx, &fingerprint)? {
            if other.id != existing.id {
                bail!("An identical memory already exists");
            }
        }
        let version = existing.version.saturating_add(1);
        let now = now_ts();
        tx.execute(
            "UPDATE memories SET text=?1, fingerprint=?2, version=?3, updated_at=?4, expires_at=NULL
             WHERE id=?5",
            params![text, fingerprint, version as i64, now, existing.id],
        )?;
        let memory = Memory {
            text,
            version,
            updated_at: now,
            expires_at: None,
            ..existing
        };
        put_version(&tx, &memory)?;
        tx.commit()?;
        Ok(memory)
    }

    /// Tombstone a memory: text and history stay, the fingerprint blocks legacy
    /// replays. An explicit save can still re-create the wording on purpose.
    pub fn forget(&mut self, id: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = require_memory(&tx, id)?;
        match existing.state.as_str() {
            STATE_PURGED => bail!("Memory was purged"),
            STATE_FORGOTTEN => {
                tx.commit()?;
                return Ok(());
            }
            _ => {}
        }
        record_version_tombstones(&tx, &existing)?;
        set_state(&tx, &existing, STATE_FORGOTTEN)?;
        tx.commit()?;
        Ok(())
    }

    /// Purge a memory: body, quote, source, evidence baselines, history and any
    /// pinning are wiped, so nothing about the record survives except the
    /// fingerprint that keeps legacy replays out.
    pub fn purge(&mut self, id: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = require_memory(&tx, id)?;
        if existing.state == STATE_PURGED {
            wipe_purged_row(&tx, &existing.id)?;
            tx.commit()?;
            return Ok(());
        }
        record_version_tombstones(&tx, &existing)?;
        wipe_purged_row(&tx, &existing.id)?;
        tx.commit()?;
        Ok(())
    }

    /// Undo an accidental forget. Only a forgotten row can be restored: its
    /// text is still stored, and clearing the tombstone for the current wording
    /// lets that exact wording become active again. Purged rows are gone.
    pub fn restore(&mut self, id: &str) -> Result<Memory> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = require_memory(&tx, id)?;
        match existing.state.as_str() {
            STATE_FORGOTTEN => {}
            STATE_PURGED => bail!("Memory was purged"),
            STATE_ACCEPTED => bail!("Memory is already active"),
            _ => bail!("Memory is not forgotten"),
        }
        tx.execute(
            "DELETE FROM tombstones WHERE fingerprint=?1",
            params![fingerprint(&existing.kind, &existing.text, &existing.paths)],
        )?;
        let memory = set_state(&tx, &existing, STATE_ACCEPTED)?;
        tx.commit()?;
        Ok(memory)
    }

    /// Import a validated legacy note list. All notes validate before any write;
    /// repeated `source_id` replays report `already_imported` without writes,
    /// and content the user forgot or purged stays out of a replay.
    pub fn import_legacy(&mut self, notes: &[String], source_id: &str) -> Result<ImportReport> {
        if source_id.trim().is_empty() {
            bail!("Legacy import source id is required");
        }
        if notes.len() > MAX_IMPORT_NOTES {
            bail!("Legacy import is limited to {MAX_IMPORT_NOTES} notes");
        }
        for note in notes {
            validate_text(note)?;
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seen: Option<String> = tx
            .query_row(
                "SELECT source_id FROM imports WHERE source_id=?1",
                params![source_id],
                |r| r.get(0),
            )
            .optional()?;
        if seen.is_some() {
            tx.commit()?;
            return Ok(ImportReport {
                created: 0,
                skipped: notes.len(),
                already_imported: true,
            });
        }
        let source = Source {
            origin: "legacy_import".to_string(),
            session_id: None,
            call_id: None,
        };
        let mut report = ImportReport {
            created: 0,
            skipped: 0,
            already_imported: false,
        };
        for note in notes {
            let text = note.trim().to_string();
            let input = NewMemory {
                kind: "reference".to_string(),
                text: text.clone(),
                paths: Vec::new(),
                evidence_refs: Vec::new(),
                source: source.clone(),
            };
            let fp = fingerprint(&input.kind, &text, &input.paths);
            if is_tombstoned(&tx, &fp)? || load_by_fingerprint(&tx, &fp)?.is_some() {
                report.skipped += 1;
                continue;
            }
            insert_row(
                &tx,
                &input,
                &text,
                fp,
                &NewRow {
                    state: STATE_ACCEPTED,
                    source_quote: None,
                    supersedes: None,
                },
            )?;
            report.created += 1;
        }
        tx.execute(
            "INSERT INTO imports(source_id) VALUES (?1)",
            params![source_id],
        )?;
        tx.commit()?;
        Ok(report)
    }

    /// Durable write used by the model tool. The row is accepted immediately,
    /// and when a verified `quote` is given the stored rule text is that quote
    /// verbatim, not the model's restatement. `core` pins (`Some(true)`),
    /// unpins (`Some(false)`) or inherits (`None`): a replacement transfers the
    /// replaced rule's pinning, an existing record keeps its own, and a
    /// brand-new preference is pinned so the user's stated preferences are the
    /// always-on rules while decisions stay intent-scoped. Replaying identical
    /// text is idempotent.
    pub fn remember(
        &mut self,
        input: &NewMemory,
        quote: Option<&str>,
        supersedes: Option<&str>,
        core: Option<bool>,
    ) -> Result<Memory> {
        if let Some(quote) = quote {
            validate_quote(quote)?;
        }
        if supersedes.is_some_and(|id| id.trim().is_empty()) {
            bail!("Superseded memory id is empty");
        }
        let quote_text = quote.map(str::trim);
        // The rule keeps the model's complete sentence; the quote stays as its
        // provenance so a reader can always see the user's own wording.
        let effective = input.clone();
        validate_input(&effective)?;
        let text = effective.text.trim().to_string();
        let fingerprint = fingerprint(&effective.kind, &text, &effective.paths);
        let row = NewRow {
            state: STATE_ACCEPTED,
            source_quote: quote_text,
            supersedes,
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut fresh = false;
        let memory = match load_by_fingerprint(&tx, &fingerprint)? {
            Some(existing) if existing.state == STATE_ACCEPTED => existing,
            Some(existing) => revive_row(&tx, &existing, &effective, &text, &row)?,
            None => {
                fresh = true;
                insert_row(&tx, &effective, &text, fingerprint, &row)?
            }
        };
        if let Some(old_id) = supersedes.filter(|old| *old != memory.id) {
            let old = load_memory(&tx, old_id)?.context("Superseded memory not found")?;
            match old.state.as_str() {
                STATE_ACCEPTED => {
                    let was_core = is_core_row(&tx, &old.id)?;
                    record_version_tombstones(&tx, &old)?;
                    set_state(&tx, &old, STATE_FORGOTTEN)?;
                    // A pin only transfers to another pinnable rule.
                    if core.is_none()
                        && was_core
                        && matches!(memory.kind.as_str(), "preference" | "decision")
                    {
                        pin_core_row(&tx, &memory.id)?;
                    }
                }
                // The rule is already gone because this same replacement ran
                // before: a retry succeeds without touching anything else.
                STATE_FORGOTTEN | STATE_PURGED => {}
                _ => bail!("Superseded memory is not an active rule"),
            }
        }
        match core {
            Some(true) => mark_core_row(&tx, &memory.id)?,
            Some(false) => unpin_core_row(&tx, &memory.id)?,
            // A new preference is pinned by default; a replacement or an
            // existing record keeps whatever pinning the user already set.
            None if fresh && supersedes.is_none() && memory.kind == "preference" => {
                mark_core_row(&tx, &memory.id)?
            }
            None => {}
        }
        tx.commit()?;
        Ok(memory)
    }

    /// Record a per-worktree fingerprint baseline for review evidence. Only a
    /// regular file can anchor evidence: a directory hint is navigation, and a
    /// rule keeps its path hints as navigation too, so a later edit can never
    /// turn a durable user rule inert.
    pub fn stamp_paths(&mut self, id: &str, root: &Path) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let memory = require_memory(&tx, id)?;
        if memory.state != STATE_ACCEPTED || memory.kind != "review" {
            return Ok(());
        }
        let root_key = worktree_key(root);
        for path in &memory.paths {
            if let PathState::File(digest) = fingerprint_path(root, path) {
                tx.execute(
                    "INSERT INTO memory_files(memory_id,root,relative_path,digest)
                    VALUES (?1,?2,?3,?4) ON CONFLICT(memory_id,root,relative_path)
                    DO UPDATE SET digest=excluded.digest",
                    params![id, root_key, path, digest],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_mode(
        &mut self,
        use_enabled: Option<bool>,
        capture_enabled: Option<bool>,
    ) -> Result<Settings> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_settings(&tx)?;
        let settings = Settings {
            use_enabled: use_enabled.unwrap_or(current.use_enabled),
            capture_enabled: capture_enabled.unwrap_or(current.capture_enabled),
        };
        set_flag(&tx, META_USE, settings.use_enabled)?;
        set_flag(&tx, META_CAPTURE, settings.capture_enabled)?;
        tx.commit()?;
        Ok(settings)
    }
}

/// Validate and pin one accepted rule as a core rule. Runs inside the caller's
/// IMMEDIATE transaction so a failed core write cannot leave a committed
/// non-core memory behind.
fn mark_core_row(conn: &Connection, id: &str) -> Result<()> {
    let memory = require_memory(conn, id)?;
    if memory.state != STATE_ACCEPTED || !matches!(memory.kind.as_str(), "preference" | "decision")
    {
        bail!("Only an accepted user preference or decision can be a core rule");
    }
    pin_core_row(conn, id)
}

fn pin_core_row(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO core_memories(memory_id) VALUES (?1)
         ON CONFLICT(memory_id) DO NOTHING",
        params![id],
    )?;
    Ok(())
}

fn unpin_core_row(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM core_memories WHERE memory_id=?1", params![id])?;
    Ok(())
}

pub(crate) fn is_core_row(conn: &Connection, id: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM core_memories WHERE memory_id=?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Columns written for one new or revived row.
pub(crate) struct NewRow<'a> {
    pub state: &'a str,
    pub source_quote: Option<&'a str>,
    pub supersedes: Option<&'a str>,
}

fn insert_row(
    conn: &Connection,
    input: &NewMemory,
    text: &str,
    fingerprint: String,
    row: &NewRow<'_>,
) -> Result<Memory> {
    let now = now_ts();
    let memory = Memory {
        id: new_id(),
        kind: input.kind.clone(),
        text: text.to_string(),
        state: row.state.to_string(),
        version: 1,
        paths: input.paths.clone(),
        evidence_refs: input.evidence_refs.clone(),
        source: input.source.clone(),
        created_at: now,
        updated_at: now,
        source_quote: row.source_quote.map(str::to_string),
        expires_at: None,
        supersedes: row.supersedes.map(str::to_string),
    };
    conn.execute(
        "INSERT INTO memories (id, kind, text, state, version, paths, evidence_refs, source,
                               created_at, updated_at, fingerprint, source_quote, expires_at,
                               supersedes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            memory.id,
            memory.kind,
            memory.text,
            memory.state,
            memory.version as i64,
            serde_json::to_string(&memory.paths)?,
            serde_json::to_string(&memory.evidence_refs)?,
            serde_json::to_string(&memory.source)?,
            memory.created_at,
            memory.updated_at,
            fingerprint,
            memory.source_quote,
            memory.expires_at,
            memory.supersedes,
        ],
    )?;
    put_version(conn, &memory)?;
    Ok(memory)
}

/// Reuse an existing row's identity when the same content is saved again after
/// being forgotten, purged or left as a candidate. Every column is rewritten,
/// because a purged row has no body left to reuse, and no evidence baseline can
/// outlive the record it was taken for.
fn revive_row(
    conn: &Connection,
    existing: &Memory,
    input: &NewMemory,
    text: &str,
    row: &NewRow<'_>,
) -> Result<Memory> {
    let version = existing.version.saturating_add(1);
    let now = now_ts();
    conn.execute(
        "DELETE FROM memory_files WHERE memory_id=?1",
        params![existing.id],
    )?;
    conn.execute(
        "UPDATE memories SET kind=?1, text=?2, state=?3, version=?4, paths=?5, evidence_refs=?6,
                source=?7, updated_at=?8, source_quote=?9, expires_at=NULL, supersedes=?10
          WHERE id=?11",
        params![
            input.kind,
            text,
            row.state,
            version as i64,
            serde_json::to_string(&input.paths)?,
            serde_json::to_string(&input.evidence_refs)?,
            serde_json::to_string(&input.source)?,
            now,
            row.source_quote,
            row.supersedes,
            existing.id,
        ],
    )?;
    let memory = Memory {
        kind: input.kind.clone(),
        text: text.to_string(),
        state: row.state.to_string(),
        version,
        paths: input.paths.clone(),
        evidence_refs: input.evidence_refs.clone(),
        source: input.source.clone(),
        updated_at: now,
        source_quote: row.source_quote.map(str::to_string),
        expires_at: None,
        supersedes: row.supersedes.map(str::to_string),
        ..existing.clone()
    };
    put_version(conn, &memory)?;
    Ok(memory)
}

/// Change lifecycle state, bumping the version, clearing any obsolete expiry
/// and recording the new wording's fingerprint.
fn set_state(conn: &Connection, existing: &Memory, state: &str) -> Result<Memory> {
    let version = existing.version.saturating_add(1);
    let now = now_ts();
    conn.execute(
        "UPDATE memories SET state=?1, version=?2, updated_at=?3, expires_at=NULL WHERE id=?4",
        params![state, version as i64, now, existing.id],
    )?;
    let memory = Memory {
        state: state.to_string(),
        version,
        updated_at: now,
        expires_at: None,
        ..existing.clone()
    };
    put_version(conn, &memory)?;
    Ok(memory)
}

/// History stores a fingerprint of each wording instead of a full body copy:
/// the body is what tombstoning needs, and the current body already lives in
/// `memories`. A snapshot per edit would otherwise grow without bound.
fn put_version(conn: &Connection, memory: &Memory) -> Result<()> {
    conn.execute(
        "INSERT INTO versions (memory_id, version, fingerprint) VALUES (?1, ?2, ?3)
         ON CONFLICT(memory_id, version) DO UPDATE SET fingerprint=excluded.fingerprint",
        params![
            memory.id,
            memory.version as i64,
            fingerprint(&memory.kind, &memory.text, &memory.paths)
        ],
    )?;
    Ok(())
}

fn require_memory(conn: &Connection, id: &str) -> Result<Memory> {
    load_memory(conn, id)?.ok_or_else(|| anyhow::anyhow!("Memory not found"))
}

pub(crate) fn load_memory(conn: &Connection, id: &str) -> Result<Option<Memory>> {
    let sql = format!("SELECT {COLUMNS} FROM memories WHERE id=?1");
    Ok(conn
        .query_row(&sql, params![id], memory_from_row)
        .optional()?)
}

fn is_tombstoned(conn: &Connection, fp: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM tombstones WHERE fingerprint=?1",
            params![fp],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Record the current text and every retained version so a legacy replay cannot
/// undo a forget by re-creating an earlier corrected wording of the same note.
fn record_version_tombstones(conn: &Connection, memory: &Memory) -> Result<()> {
    remember_tombstone(conn, &memory.kind, &memory.text, &memory.paths)?;
    let mut statement = conn.prepare(
        "SELECT fingerprint FROM versions WHERE memory_id=?1 AND fingerprint IS NOT NULL",
    )?;
    let fingerprints = statement.query_map(params![memory.id], |row| row.get::<_, String>(0))?;
    for fp in fingerprints {
        let fp = fp?;
        conn.execute(
            "INSERT OR IGNORE INTO tombstones(fingerprint) VALUES (?1)",
            params![fp],
        )?;
    }
    Ok(())
}

fn remember_tombstone(conn: &Connection, kind: &str, text: &str, paths: &[String]) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    conn.execute(
        "INSERT OR IGNORE INTO tombstones(fingerprint) VALUES (?1)",
        params![fingerprint(kind, text, paths)],
    )?;
    Ok(())
}

pub(crate) fn load_by_fingerprint(conn: &Connection, fp: &str) -> Result<Option<Memory>> {
    let sql = format!("SELECT {COLUMNS} FROM memories WHERE fingerprint=?1");
    Ok(conn
        .query_row(&sql, params![fp], memory_from_row)
        .optional()?)
}

pub(crate) const COLUMNS: &str = "id, kind, text, state, version, paths, evidence_refs, source, created_at, updated_at, source_quote, expires_at, supersedes";

pub(crate) fn memory_from_row(row: &Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        kind: row.get(1)?,
        text: row.get(2)?,
        state: row.get(3)?,
        version: row.get::<_, i64>(4)? as u32,
        paths: from_json(row, 5)?,
        evidence_refs: from_json(row, 6)?,
        source: from_json(row, 7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        source_quote: row.get(10)?,
        expires_at: row.get(11)?,
        supersedes: row.get(12)?,
    })
}

fn from_json<T: serde::de::DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let raw: String = row.get(index)?;
    serde_json::from_str(&raw).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(err))
    })
}

/// Erase everything a purged row must not keep. The fingerprint column stays:
/// it is what keeps the wording out of later legacy replays.
fn wipe_purged_row(conn: &Connection, id: &str) -> Result<()> {
    let wiped = serde_json::to_string(&Source {
        origin: STATE_PURGED.to_string(),
        session_id: None,
        call_id: None,
    })?;
    conn.execute("DELETE FROM versions WHERE memory_id=?1", params![id])?;
    conn.execute("DELETE FROM memory_files WHERE memory_id=?1", params![id])?;
    conn.execute("DELETE FROM core_memories WHERE memory_id=?1", params![id])?;
    conn.execute(
        "UPDATE memories SET state=?1, text='', paths='[]', evidence_refs='[]', source=?2,
                source_quote=NULL, supersedes=NULL, expires_at=NULL, updated_at=?3
          WHERE id=?4",
        params![STATE_PURGED, wiped, now_ts(), id],
    )?;
    Ok(())
}

/// Forward-compatible, additive schema migration. A database written by an
/// older build keeps its content; a database from a newer build is rejected by
/// the version check in `Store::init`.
fn migrate(conn: &Connection, from: i64) -> Result<()> {
    for (name, kind) in ADDED_COLUMNS {
        if !table_has_column(conn, "memories", name)? {
            conn.execute_batch(&format!("ALTER TABLE memories ADD COLUMN {name} {kind};"))
                .context("Failed to migrate the memory schema")?;
        }
    }
    if from < 3 {
        // Version 3 keeps fingerprints per worktree and stops inferring topic
        // conflicts. An old baseline cannot be attributed to a checkout, and a
        // paused rule has no author left to ask, so both are reset.
        conn.execute_batch(
            "DROP TABLE IF EXISTS memory_topics; DROP TABLE IF EXISTS memory_files;",
        )?;
        conn.execute_batch(SCHEMA)
            .context("Failed to create the version 3 tables")?;
        conn.execute(
            "UPDATE memories SET state=?1, updated_at=?2 WHERE state IN ('stale','disputed')",
            params![STATE_ACCEPTED, now_ts()],
        )?;
    }
    if from < 4 {
        migrate_versions(conn)?;
        // Per-worktree staleness verdicts and the expiry index are obsolete:
        // staleness is computed on demand today.
        conn.execute_batch(
            "DROP TABLE IF EXISTS memory_stale; DROP INDEX IF EXISTS memories_state_expires;",
        )?;
        restore_retired_states(conn)?;
        repair_purged_rows(conn)?;
        conn.execute(
            "DELETE FROM metadata WHERE key IN
                ('last_activity','wrote_memory','last_verified_user_turn',
                 'last_verified_user_turn_count')",
            [],
        )?;
    }
    conn.execute_batch(INDEXES)
        .context("Failed to create memory schema indexes")?;
    Ok(())
}

/// Replace full-body history snapshots with the fingerprint of the wording they
/// stood for. Every body is read and converted before the old table is dropped,
/// so no history is pruned before its fingerprint exists.
fn migrate_versions(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "versions", "body")? {
        return Ok(());
    }
    let converted = {
        let mut statement = conn.prepare("SELECT memory_id, version, body FROM versions")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut converted: Vec<(String, i64, Option<String>)> = Vec::new();
        for row in rows {
            let (id, version, body) = row?;
            let fp = serde_json::from_str::<Memory>(&body)
                .ok()
                .map(|memory| fingerprint(&memory.kind, &memory.text, &memory.paths));
            converted.push((id, version, fp));
        }
        converted
    };
    conn.execute_batch(
        "DROP TABLE IF EXISTS versions_v4;
         CREATE TABLE versions_v4 (
             memory_id   TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
             version     INTEGER NOT NULL,
             fingerprint TEXT,
             PRIMARY KEY (memory_id, version)
         );",
    )?;
    {
        let mut statement = conn.prepare(
            "INSERT OR REPLACE INTO versions_v4(memory_id, version, fingerprint) VALUES (?1, ?2, ?3)",
        )?;
        for (id, version, fp) in &converted {
            statement.execute(params![id, version, fp])?;
        }
    }
    conn.execute_batch("DROP TABLE versions; ALTER TABLE versions_v4 RENAME TO versions;")?;
    Ok(())
}

/// Bring rules the retired lifecycle hid back into reach. An archived or
/// expired rule keeps its content and is active again; a proposal the user
/// never confirmed becomes a candidate a human can still accept. Nothing here
/// activates an unverified proposal on its own.
fn restore_retired_states(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE memories SET state=?1 WHERE state='archived'",
        params![STATE_ACCEPTED],
    )?;
    conn.execute(
        "UPDATE memories SET state=?1
          WHERE state='expired'
            AND (kind='review' OR COALESCE(json_extract(source,'$.origin'),'') <> 'model_proposal')",
        params![STATE_ACCEPTED],
    )?;
    conn.execute(
        "UPDATE memories SET state=?1 WHERE state IN ('pending','dropped','expired')",
        params![STATE_CANDIDATE],
    )?;
    conn.execute(
        "UPDATE memories SET expires_at=NULL WHERE expires_at IS NOT NULL",
        [],
    )?;
    Ok(())
}

/// Wipe rows an older build purged only partially: source quote, replaced id,
/// evidence baselines, pinning and history must not survive a purge.
fn repair_purged_rows(conn: &Connection) -> Result<()> {
    let wiped = serde_json::to_string(&Source {
        origin: STATE_PURGED.to_string(),
        session_id: None,
        call_id: None,
    })?;
    conn.execute(
        "DELETE FROM versions WHERE memory_id IN (SELECT id FROM memories WHERE state=?1)",
        params![STATE_PURGED],
    )?;
    conn.execute(
        "DELETE FROM memory_files WHERE memory_id IN (SELECT id FROM memories WHERE state=?1)",
        params![STATE_PURGED],
    )?;
    conn.execute(
        "DELETE FROM core_memories WHERE memory_id IN (SELECT id FROM memories WHERE state=?1)",
        params![STATE_PURGED],
    )?;
    conn.execute(
        "UPDATE memories SET text='', paths='[]', evidence_refs='[]', source=?2,
                source_quote=NULL, supersedes=NULL, expires_at=NULL
          WHERE state=?1",
        params![STATE_PURGED, wiped],
    )?;
    Ok(())
}

/// Verify the full-text index when the store is opened instead of on every
/// search: a missing table, a missing trigger or a row-count mismatch is
/// rebuilt once here, so reads never take a write lock.
fn repair_index(conn: &Connection) -> Result<()> {
    let (table, triggers): (i64, i64) = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM sqlite_master
                  WHERE type='table' AND name='memories_fts'),
                (SELECT COUNT(*) FROM sqlite_master
                  WHERE type='trigger' AND name IN
                    ('memories_fts_insert','memories_fts_update','memories_fts_delete'))",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if table == 0 || triggers < 3 {
        conn.execute_batch(FTS_SCHEMA)?;
    }
    let rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE state=?1",
        params![STATE_ACCEPTED],
        |row| row.get(0),
    )?;
    let indexed: i64 = conn.query_row("SELECT COUNT(*) FROM memories_fts", [], |row| row.get(0))?;
    if table != 0 && triggers >= 3 && rows == indexed {
        return Ok(());
    }
    conn.execute("DELETE FROM memories_fts", [])?;
    conn.execute(
        "INSERT INTO memories_fts(rowid, text) SELECT rowid, text FROM memories WHERE state=?1",
        params![STATE_ACCEPTED],
    )?;
    Ok(())
}

fn table_has_column(conn: &Connection, table: &str, name: &str) -> Result<bool> {
    let mut statement = conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let existing: String = row.get(0)?;
        if existing == name {
            return Ok(true);
        }
    }
    Ok(false)
}

fn set_flag(conn: &Connection, key: &str, value: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, if value { "1" } else { "0" }],
    )?;
    Ok(())
}

fn init_flag(conn: &Connection, key: &str, value: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO NOTHING",
        params![key, if value { "1" } else { "0" }],
    )?;
    Ok(())
}

fn read_flag(conn: &Connection, key: &str) -> Result<bool> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.as_deref() != Some("0"))
}

pub(crate) fn read_settings(conn: &Connection) -> Result<Settings> {
    Ok(Settings {
        use_enabled: read_flag(conn, META_USE)?,
        capture_enabled: read_flag(conn, META_CAPTURE)?,
    })
}

pub(crate) fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or(0)
}

/// SHA256 over the JSON encoding of (kind, trimmed text, sorted paths).
pub(crate) fn fingerprint(kind: &str, text: &str, paths: &[String]) -> String {
    let mut sorted: Vec<String> = paths.to_vec();
    sorted.sort();
    let encoded = serde_json::to_string(&(kind, text.trim(), sorted)).unwrap_or_default();
    let digest = Sha256::digest(encoded.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest.iter() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Opaque, unguessable-enough identifier without extra dependencies.
fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tick = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(b"kroma-memory-id");
    hasher.update(nanos.to_le_bytes());
    hasher.update(tick.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update((&tick as *const u64 as usize).to_le_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// What one path hint can contribute: an evidence digest, plain navigation, or
/// no usable anchor at all.
#[derive(Debug, PartialEq, Eq)]
enum PathState {
    File(String),
    Directory,
    Unavailable,
}

// Repeated context hooks should not rehash large, unchanged review files.
// File identity + change times are checked before reusing a digest. Bound
// this ephemeral cache so a large repository never grows a permanent index.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanos: i64,
}

fn file_stamp(meta: &std::fs::Metadata) -> FileStamp {
    FileStamp {
        size: meta.len(),
        modified: meta.modified().ok(),
        #[cfg(unix)]
        device: meta.dev(),
        #[cfg(unix)]
        inode: meta.ino(),
        #[cfg(unix)]
        changed_seconds: meta.ctime(),
        #[cfg(unix)]
        changed_nanos: meta.ctime_nsec(),
    }
}

static FILE_HASH_CACHE: OnceLock<Mutex<Vec<(PathBuf, FileStamp, String)>>> = OnceLock::new();

/// Stable per-worktree key stored with a fingerprint.
fn worktree_key(root: &Path) -> String {
    std::fs::canonicalize(root)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| root.to_string_lossy().into_owned())
}

/// Evidence is stale here when no recorded baseline matches the current file
/// content in this worktree. A record with nothing to compare keeps its normal
/// standing instead: a hint that is only a directory, or a note written before
/// per-worktree baselines existed, has no content to re-verify.
pub(crate) fn review_is_stale(conn: &Connection, root: &Path, id: &str) -> Result<bool> {
    let root_key = worktree_key(root);
    let mut statement = conn
        .prepare("SELECT relative_path, digest FROM memory_files WHERE memory_id=?1 AND root=?2")?;
    let rows = statement.query_map(params![id, root_key], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut anchored = false;
    for row in rows {
        let (path, digest) = row?;
        anchored = true;
        if fingerprint_path(root, &path) != PathState::File(digest) {
            return Ok(true);
        }
    }
    if anchored {
        return Ok(false);
    }
    // A review anchored only in another worktree has no verified baseline here.
    let other: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_files WHERE memory_id=?1)",
        params![id],
        |row| row.get(0),
    )?;
    Ok(other != 0)
}

fn fingerprint_path(root: &Path, relative: &str) -> PathState {
    let Ok(root) = std::fs::canonicalize(root) else {
        return PathState::Unavailable;
    };
    let Ok(target) = std::fs::canonicalize(root.join(relative)) else {
        return PathState::Unavailable;
    };
    if !target.starts_with(&root) {
        return PathState::Unavailable;
    }
    if target.is_dir() {
        return PathState::Directory;
    }
    if !target.is_file() {
        return PathState::Unavailable;
    }
    let Ok(file) = std::fs::File::open(&target) else {
        return PathState::Unavailable;
    };
    let Ok(meta) = file.metadata() else {
        return PathState::Unavailable;
    };
    if meta.len() > 16 * 1024 * 1024 {
        return PathState::Unavailable;
    }
    let stamp = file_stamp(&meta);
    let cache = FILE_HASH_CACHE.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(entries) = cache.lock() {
        if let Some((_, _, digest)) = entries
            .iter()
            .find(|(path, old, _)| path == &target && old == &stamp)
        {
            return PathState::File(digest.clone());
        }
    }
    let mut bytes = Vec::new();
    if file
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > 16 * 1024 * 1024
    {
        return PathState::Unavailable;
    }
    let digest = Sha256::digest(&bytes);
    let mut out = String::with_capacity(64);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    if let Ok(mut entries) = cache.lock() {
        entries.retain(|(path, _, _)| path != &target);
        if entries.len() >= 128 {
            entries.remove(0);
        }
        entries.push((target, stamp, out.clone()));
    }
    PathState::File(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_cache_reuses_unchanged_files_but_detects_equal_length_edits() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mod.rs");
        std::fs::write(&file, "first").unwrap();
        let first = fingerprint_path(dir.path(), "mod.rs");
        assert_eq!(fingerprint_path(dir.path(), "mod.rs"), first);
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&file, "later").unwrap();
        let later = fingerprint_path(dir.path(), "mod.rs");
        assert_ne!(later, first);
        assert_eq!(fingerprint_path(dir.path(), "mod.rs"), later);
    }

    fn input(text: &str, origin: &str) -> NewMemory {
        NewMemory {
            kind: "decision".to_string(),
            text: text.to_string(),
            paths: vec!["src/lib.rs".to_string()],
            evidence_refs: Vec::new(),
            source: Source {
                origin: origin.to_string(),
                session_id: None,
                call_id: None,
            },
        }
    }

    fn rule(text: &str) -> NewMemory {
        NewMemory {
            paths: Vec::new(),
            ..input(text, "manual")
        }
    }
    /// A legacy-import shaped note: kind `reference`, no path hints.
    fn note(text: &str) -> NewMemory {
        NewMemory {
            kind: "reference".to_string(),
            paths: Vec::new(),
            ..input(text, "manual")
        }
    }

    fn open(dir: &std::path::Path) -> Store {
        Store::open(
            &dir.join("workspaces").join("abc").join("memory.sqlite3"),
            "scope-1",
            &dir.to_string_lossy(),
        )
        .expect("open store")
    }

    fn versions(store: &Store, id: &str) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM versions WHERE memory_id=?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn count(store: &Store, table: &str, id: &str) -> i64 {
        store
            .conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE memory_id=?1"),
                params![id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn state_of(store: &Store, id: &str) -> String {
        load_memory(&store.conn, id).unwrap().unwrap().state
    }

    #[test]
    fn insert_accept_correct_forget_purge() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());

        let memory = store.insert(&input("use WAL", "manual"), true).unwrap();
        assert_eq!(memory.state, "accepted");
        assert_eq!(memory.version, 1);
        assert_eq!(versions(&store, &memory.id), 1);

        let duplicate = store
            .insert(&input("use WAL", "model_proposal"), false)
            .unwrap();
        assert_eq!(duplicate.id, memory.id);

        let candidate = store
            .insert(&input("maybe cache", "model_proposal"), false)
            .unwrap();
        assert_eq!(candidate.state, "candidate");
        let accepted = store.accept(&candidate.id).unwrap();
        assert_eq!(accepted.state, "accepted");
        assert_eq!(accepted.version, 2);

        let corrected = store.correct(&candidate.id, "definitely cache").unwrap();
        assert_eq!(corrected.text, "definitely cache");
        assert_eq!(corrected.version, 3);
        assert_eq!(versions(&store, &candidate.id), 3);

        store.forget(&memory.id).unwrap();
        assert_eq!(state_of(&store, &memory.id), "forgotten");
        assert!(load_memory(&store.conn, &memory.id)
            .unwrap()
            .unwrap()
            .text
            .eq("use WAL"));

        store.purge(&memory.id).unwrap();
        let purged = load_memory(&store.conn, &memory.id).unwrap().unwrap();
        assert_eq!(purged.state, "purged");
        assert!(purged.text.is_empty());
        assert_eq!(versions(&store, &memory.id), 0);
        // The fingerprint stays so a legacy replay cannot resurrect the wording.
        let retained = fingerprint("decision", "use WAL", &["src/lib.rs".to_string()]);
        assert!(load_by_fingerprint(&store.conn, &retained)
            .unwrap()
            .is_some());
    }

    #[test]
    fn explicit_saves_revive_forgotten_and_purged_wording() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());
        let memory = store.insert(&rule("use WAL"), true).unwrap();

        store.forget(&memory.id).unwrap();
        let again = store.insert(&rule("use WAL"), true).unwrap();
        assert_eq!(again.id, memory.id);
        assert_eq!(again.state, "accepted");
        assert!(again.version > memory.version);
        assert_eq!(again.text, "use WAL");

        store.purge(&again.id).unwrap();
        let revived = store.insert(&rule("use WAL"), true).unwrap();
        assert_eq!(revived.id, memory.id);
        assert_eq!(revived.state, "accepted");
        assert_eq!(revived.text, "use WAL");
        assert_eq!(revived.paths, Vec::<String>::new());
        assert_eq!(revived.source.origin, "manual");
        assert!(revived.source_quote.is_none());
        assert!(revived.supersedes.is_none());
        assert_eq!(state_of(&store, &memory.id), "accepted");
    }

    #[test]
    fn every_active_rule_stays_active_without_archiving() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());
        for index in 0..250 {
            store
                .insert(&rule(&format!("distinct durable rule {index}")), true)
                .unwrap();
        }
        let active: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE state=?1",
                params![STATE_ACCEPTED],
                |row| row.get(0),
            )
            .unwrap();
        let archived: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE state='archived'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active, 250);
        assert_eq!(archived, 0);
        assert_eq!(store.stats().unwrap().accepted, 250);
    }

    #[test]
    fn import_is_bounded_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());
        let notes = vec![
            "first legacy note".to_string(),
            "second legacy note".to_string(),
        ];
        let report = store.import_legacy(&notes, "legacy-notes.json").unwrap();
        assert_eq!(report.created, 2);
        assert_eq!(report.skipped, 0);
        assert!(!report.already_imported);

        let replay = store.import_legacy(&notes, "legacy-notes.json").unwrap();
        assert_eq!(replay.created, 0);
        assert!(replay.already_imported);

        let again = store.import_legacy(&notes, "other-file.json").unwrap();
        assert_eq!(again.created, 0);
        assert_eq!(again.skipped, 2);

        assert!(store
            .import_legacy(&["   ".to_string()], "bad.json")
            .is_err());
    }

    #[test]
    fn legacy_replays_respect_tombstones_while_explicit_saves_revive() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());
        let original = store
            .insert(&note("cache keys ignore generations"), true)
            .unwrap();
        let corrected = store
            .correct(&original.id, "cache keys include generations")
            .unwrap();
        store.forget(&corrected.id).unwrap();

        // An earlier wording the user edited away stays out of a legacy replay.
        let report = store
            .import_legacy(
                &["cache keys ignore generations".to_string()],
                "replay.json",
            )
            .unwrap();
        assert_eq!(report.created, 0);
        assert_eq!(report.skipped, 1);

        // An explicit save of the forgotten wording is the user's own decision.
        let saved = store
            .insert(&note("cache keys include generations"), true)
            .unwrap();
        assert_eq!(saved.id, corrected.id);
        assert_eq!(saved.state, "accepted");
    }

    #[test]
    fn modes_default_on_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());
        let settings = store.set_mode(None, None).unwrap();
        assert!(settings.use_enabled && settings.capture_enabled);
        let off = store.set_mode(Some(false), None).unwrap();
        assert!(!off.use_enabled && off.capture_enabled);
        drop(store);

        let mut reopened = open(dir.path());
        let settings = reopened.set_mode(None, None).unwrap();
        assert!(!settings.use_enabled && settings.capture_enabled);

        assert!(Store::open(
            &dir.path().join("w2").join("m.sqlite3"),
            "other-scope",
            &dir.path().to_string_lossy()
        )
        .is_ok());
    }

    #[test]
    fn identity_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite3");
        let root = dir.path().to_string_lossy().into_owned();
        drop(Store::open(&path, "scope-a", &root).unwrap());
        assert!(Store::open(&path, "scope-b", &root).is_err());
        assert!(Store::open(&path, "scope-a", &root).is_ok());
    }

    #[test]
    fn concurrent_connections_keep_every_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("workspaces")
            .join("abc")
            .join("memory.sqlite3");
        let root = dir.path().to_string_lossy().into_owned();
        drop(Store::open(&path, "scope-1", &root).unwrap());
        let mut handles = Vec::new();
        for worker in 0..2 {
            let path = path.clone();
            let root = root.clone();
            handles.push(std::thread::spawn(move || {
                let mut store = Store::open(&path, "scope-1", &root).unwrap();
                for index in 0..10 {
                    let text = format!("worker {worker} note {index}");
                    store.insert(&rule(&text), true).unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let store = Store::open(&path, "scope-1", &root).unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 20);
    }

    #[test]
    fn remember_keeps_the_rule_text_and_its_user_quote() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());
        let message = "Please use pnpm for the workspace and commit the lockfile";
        let quote = crate::model::find_quote("use pnpm for the workspace", message).unwrap();
        let memory = store
            .remember(
                &input("use pnpm", "model_proposal"),
                Some(quote),
                None,
                None,
            )
            .unwrap();
        assert_eq!(memory.text, "use pnpm");
        assert_eq!(
            memory.source_quote.as_deref(),
            Some(quote),
            "the user0027s own words are kept as provenance"
        );
        assert_eq!(memory.state, STATE_ACCEPTED);
        assert!(memory.expires_at.is_none());
        assert_eq!(memory.source.origin, "model_proposal");

        // A repeated request is idempotent instead of creating a second rule.
        let replay = store
            .remember(
                &input("use pnpm", "model_proposal"),
                Some(quote),
                None,
                None,
            )
            .unwrap();
        assert_eq!(replay.id, memory.id);
        assert_eq!(replay.version, memory.version);

        // Review notes carry no quote and keep the text they were given.
        let review = NewMemory {
            kind: "review".to_string(),
            text: "parser lacks EOF handling".to_string(),
            paths: vec!["src/lib.rs".to_string()],
            evidence_refs: vec!["src/lib.rs:1".to_string()],
            source: Source {
                origin: "model_proposal".to_string(),
                session_id: None,
                call_id: None,
            },
        };
        let stored = store.remember(&review, None, None, None).unwrap();
        assert_eq!(stored.text, "parser lacks EOF handling");
        assert_eq!(stored.state, STATE_ACCEPTED);
        assert!(stored.source_quote.is_none());
    }

    #[test]
    fn core_pinning_is_explicit_and_inherited_only_on_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());
        let first = store
            .remember(&rule("use pnpm"), Some("use pnpm"), None, Some(true))
            .unwrap();
        assert!(is_core_row(&store.conn, &first.id).unwrap());

        // Some(false) unpins even on an idempotent replay.
        store
            .remember(&rule("use pnpm"), Some("use pnpm"), None, Some(false))
            .unwrap();
        assert!(!is_core_row(&store.conn, &first.id).unwrap());

        // set_core pins and unpins without touching the body or the version.
        let pinned = store.set_core(&first.id, true).unwrap();
        assert_eq!(pinned.version, first.version);
        assert!(is_core_row(&store.conn, &first.id).unwrap());
        store.set_core(&first.id, false).unwrap();
        assert!(!is_core_row(&store.conn, &first.id).unwrap());

        // A replacement inherits the pin when the caller is silent.
        store.set_core(&first.id, true).unwrap();
        let second = store
            .remember(&rule("use bun"), Some("use bun"), Some(&first.id), None)
            .unwrap();
        assert_eq!(state_of(&store, &first.id), STATE_FORGOTTEN);
        assert!(is_core_row(&store.conn, &second.id).unwrap());

        // An explicit Some(false) overrides that inheritance.
        let third = store
            .remember(
                &rule("use yarn"),
                Some("use yarn"),
                Some(&second.id),
                Some(false),
            )
            .unwrap();
        assert!(!is_core_row(&store.conn, &third.id).unwrap());

        // Pinning is not capped at four rules.
        for index in 0..6 {
            let extra = store
                .remember(
                    &rule(&format!("distinct pinned rule {index}")),
                    Some(&format!("distinct pinned rule {index}")),
                    None,
                    Some(true),
                )
                .unwrap();
            assert!(is_core_row(&store.conn, &extra.id).unwrap());
        }

        // Review findings can never be pinned.
        let review = NewMemory {
            kind: "review".to_string(),
            text: "parser lacks EOF handling".to_string(),
            paths: vec!["src/lib.rs".to_string()],
            evidence_refs: vec!["src/lib.rs:1".to_string()],
            source: Source {
                origin: "model_proposal".to_string(),
                session_id: None,
                call_id: None,
            },
        };
        assert!(store.remember(&review, None, None, Some(true)).is_err());
        // ... and a review replacement does not inherit a pin either.
        let review_without_pin = store
            .remember(&review, None, Some(&third.id), None)
            .unwrap();
        assert!(!is_core_row(&store.conn, &review_without_pin.id).unwrap());
    }

    #[test]
    fn replacement_retries_are_idempotent_once_the_old_rule_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());
        let old = store
            .remember(&rule("legacy rule"), Some("legacy rule"), None, None)
            .unwrap();
        let replacement = rule("new rule");
        let first = store
            .remember(&replacement, Some("new rule"), Some(&old.id), None)
            .unwrap();
        assert_eq!(state_of(&store, &old.id), STATE_FORGOTTEN);

        // The host retries the same request: the rule it replaced is already gone.
        let retry = store
            .remember(&replacement, Some("new rule"), Some(&old.id), None)
            .unwrap();
        assert_eq!(retry.id, first.id);
        assert_eq!(retry.version, first.version);

        // Replacing a rule with itself is a no-op; an unknown id is an error.
        let self_replace = store
            .remember(&replacement, Some("new rule"), Some(&first.id), None)
            .unwrap();
        assert_eq!(self_replace.id, first.id);
        assert_eq!(state_of(&store, &first.id), STATE_ACCEPTED);
        assert!(store
            .remember(&replacement, Some("new rule"), Some("missing-id"), None)
            .is_err());
    }

    #[test]
    fn purge_wipes_quote_replacement_baselines_pinning_and_history() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn main() {}\n").unwrap();
        let mut store = open(dir.path());

        let base = store
            .remember(&rule("older rule"), Some("older rule"), None, None)
            .unwrap();
        store.set_core(&base.id, true).unwrap();
        let review = NewMemory {
            kind: "review".to_string(),
            text: "parser lacks EOF handling".to_string(),
            paths: vec!["src/lib.rs".to_string()],
            evidence_refs: vec!["src/lib.rs:1".to_string()],
            source: Source {
                origin: "model_proposal".to_string(),
                session_id: None,
                call_id: None,
            },
        };
        let noted = store.remember(&review, None, Some(&base.id), None).unwrap();
        store.stamp_paths(&noted.id, dir.path()).unwrap();
        assert_eq!(count(&store, "memory_files", &noted.id), 1);
        assert_eq!(versions(&store, &noted.id), 1);

        for id in [&base.id, &noted.id] {
            store.purge(id).unwrap();
            let row = load_memory(&store.conn, id).unwrap().unwrap();
            assert_eq!(row.state, STATE_PURGED);
            assert!(row.text.is_empty());
            assert!(row.paths.is_empty());
            assert!(row.evidence_refs.is_empty());
            assert!(row.source_quote.is_none());
            assert!(row.supersedes.is_none());
            assert!(row.expires_at.is_none());
            assert_eq!(row.source.origin, STATE_PURGED);
            assert_eq!(versions(&store, id), 0);
            assert_eq!(count(&store, "memory_files", id), 0);
            assert_eq!(count(&store, "core_memories", id), 0);
            // Purging twice repairs instead of failing.
            store.purge(id).unwrap();
        }
        assert_eq!(state_of(&store, &base.id), STATE_PURGED);
    }

    fn review(text: &str, paths: &[&str]) -> NewMemory {
        NewMemory {
            kind: "review".to_string(),
            text: text.to_string(),
            paths: paths.iter().map(|path| path.to_string()).collect(),
            evidence_refs: vec!["src/lib.rs:1".to_string()],
            source: Source {
                origin: "model_proposal".to_string(),
                session_id: None,
                call_id: None,
            },
        }
    }

    #[test]
    fn review_evidence_is_needs_recheck_only_in_the_changed_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        let side = dir.path().join("side");
        for root in [&main, &side] {
            std::fs::create_dir_all(root.join("src")).unwrap();
        }
        std::fs::write(main.join("src/lib.rs"), "fn main() {}\n").unwrap();
        std::fs::write(side.join("src/lib.rs"), "fn main() { work(); }\n").unwrap();
        let path = dir.path().join("memory.sqlite3");

        let mut from_main = Store::open(&path, "scope-1", &main.to_string_lossy()).unwrap();
        let memory = from_main
            .remember(
                &review("parser lacks EOF handling", &["src/lib.rs"]),
                None,
                None,
                None,
            )
            .unwrap();
        from_main.stamp_paths(&memory.id, &main).unwrap();
        assert!(!from_main.is_stale(&memory.id).unwrap());
        assert_eq!(from_main.search("parser EOF", &[], 5).unwrap().len(), 1);

        // A sibling checkout computes its own verdict without a refresh pass.
        let mut from_side = Store::open(&path, "scope-1", &side.to_string_lossy()).unwrap();
        assert!(from_side.is_stale(&memory.id).unwrap());
        assert!(
            !from_main.is_stale(&memory.id).unwrap(),
            "a sibling checkout cannot change this verdict"
        );
        // A changed file never hides the note from search or read.
        assert_eq!(from_side.search("parser EOF", &[], 5).unwrap().len(), 1);
        assert_eq!(
            from_side
                .read(std::slice::from_ref(&memory.id))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(from_side.recall("parser EOF handling", 6).unwrap().len(), 1);

        // Re-anchoring this worktree makes its own evidence current again.
        from_side.stamp_paths(&memory.id, &side).unwrap();
        assert!(!from_side.is_stale(&memory.id).unwrap());

        // An older digest from the sibling cannot vouch for changed evidence.
        std::fs::write(main.join("src/lib.rs"), "fn main() { work(); }\n").unwrap();
        assert!(from_main.is_stale(&memory.id).unwrap());
        assert!(!from_side.is_stale(&memory.id).unwrap());
        assert_eq!(from_main.search("parser EOF", &[], 5).unwrap().len(), 1);
    }

    /// The schema a version 3 store was written with, kept verbatim so the
    /// migration test can build a database the old build would recognize.
    const V3_SCHEMA: &str = r#"
CREATE TABLE memories (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    text          TEXT NOT NULL,
    state         TEXT NOT NULL,
    version       INTEGER NOT NULL,
    paths         TEXT NOT NULL,
    evidence_refs TEXT NOT NULL,
    source        TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    fingerprint   TEXT NOT NULL UNIQUE,
    source_quote  TEXT,
    expires_at    INTEGER,
    supersedes    TEXT
);
CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE versions (
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    version   INTEGER NOT NULL,
    body      TEXT NOT NULL,
    PRIMARY KEY (memory_id, version)
);
CREATE TABLE imports (source_id TEXT PRIMARY KEY);
CREATE TABLE tombstones (fingerprint TEXT PRIMARY KEY);
CREATE TABLE memory_files (
    memory_id     TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    root          TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    digest        TEXT NOT NULL,
    PRIMARY KEY (memory_id, root, relative_path)
);
CREATE TABLE memory_stale (
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    root      TEXT NOT NULL,
    PRIMARY KEY (memory_id, root)
);
CREATE TABLE core_memories (memory_id TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE);
CREATE INDEX memories_state_updated ON memories(state, updated_at DESC);
CREATE INDEX memories_state_expires ON memories(state, expires_at);
"#;

    /// Build a version 3 database in the states the retired lifecycle produced.
    fn build_v3(path: &std::path::Path, root: &str) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(V3_SCHEMA).unwrap();
        let insert = |id: &str,
                      kind: &str,
                      text: &str,
                      state: &str,
                      expires: Option<i64>,
                      origin: &str| {
            let source = serde_json::to_string(&Source {
                origin: origin.to_string(),
                session_id: None,
                call_id: None,
            })
            .unwrap();
            conn.execute(
                "INSERT INTO memories (id, kind, text, state, version, paths, evidence_refs, source,
                                       created_at, updated_at, fingerprint, source_quote, expires_at,
                                       supersedes)
                 VALUES (?1, ?2, ?3, ?4, 1, '[]', '[]', ?5, 100, 100, ?6, ?7, ?8, ?9)",
                params![
                    id,
                    kind,
                    text,
                    state,
                    source,
                    fingerprint(kind, text, &[]),
                    format!("quote for {id}"),
                    expires,
                    "older-rule-id"
                ],
            )
            .unwrap();
        };
        insert(
            "m-archived",
            "decision",
            "archived rule text",
            "archived",
            None,
            "manual",
        );
        insert(
            "m-review",
            "review",
            "expired review text",
            "expired",
            Some(90),
            "model_proposal",
        );
        insert(
            "m-pending",
            "decision",
            "unverified proposal text",
            "pending",
            Some(4_102_444_800),
            "model_proposal",
        );
        insert(
            "m-dropped",
            "preference",
            "dropped proposal text",
            "dropped",
            None,
            "model_proposal",
        );
        insert(
            "m-manual",
            "decision",
            "expired manual rule text",
            "expired",
            Some(90),
            "manual",
        );
        insert(
            "m-purged",
            "reference",
            "purged body text",
            "purged",
            None,
            "legacy_import",
        );
        insert(
            "m-live",
            "decision",
            "current live wording",
            "accepted",
            None,
            "manual",
        );
        let snapshot = serde_json::json!({
            "id": "m-live",
            "kind": "decision",
            "text": "older live wording",
            "state": "accepted",
            "version": 1,
            "paths": [],
            "evidence_refs": [],
            "source": {"origin": "manual", "session_id": null, "call_id": null},
            "created_at": 100,
            "updated_at": 100
        })
        .to_string();
        conn.execute(
            "INSERT INTO versions(memory_id, version, body) VALUES ('m-live', 1, ?1)",
            params![snapshot],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO versions(memory_id, version, body) VALUES ('m-purged', 1, '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_files(memory_id, root, relative_path, digest)
             VALUES ('m-purged', ?1, 'src/lib.rs', 'digest')",
            params![root],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO core_memories(memory_id) VALUES ('m-purged')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_stale(memory_id, root) VALUES ('m-review', ?1)",
            params![root],
        )
        .unwrap();
        for (key, value) in [
            ("identity", "scope-1"),
            ("use_enabled", "0"),
            ("capture_enabled", "1"),
            ("last_activity", "{}"),
            ("wrote_memory", "1"),
            ("last_verified_user_turn", "hash"),
            ("last_verified_user_turn_count", "5"),
        ] {
            conn.execute(
                "INSERT INTO metadata(key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .unwrap();
        }
        conn.execute_batch("PRAGMA user_version=3;").unwrap();
    }

    #[test]
    fn migration_from_v3_restores_rules_without_activating_proposals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("workspaces")
            .join("abc")
            .join("memory.sqlite3");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        build_v3(&path, &root);

        let mut store = Store::open(&path, "scope-1", &root).expect("migrate");
        assert_eq!(state_of(&store, "m-archived"), STATE_ACCEPTED);
        assert_eq!(state_of(&store, "m-review"), STATE_ACCEPTED);
        assert_eq!(state_of(&store, "m-manual"), STATE_ACCEPTED);
        assert_eq!(state_of(&store, "m-live"), STATE_ACCEPTED);
        // Unverified proposals become candidates, never active rules.
        assert_eq!(state_of(&store, "m-pending"), STATE_CANDIDATE);
        assert_eq!(state_of(&store, "m-dropped"), STATE_CANDIDATE);
        // Existing content survives, obsolete expiries do not.
        let review = load_memory(&store.conn, "m-review").unwrap().unwrap();
        assert_eq!(review.text, "expired review text");
        assert!(review.expires_at.is_none());
        let live = load_memory(&store.conn, "m-live").unwrap().unwrap();
        assert_eq!(live.text, "current live wording");
        assert!(live.source_quote.as_deref() == Some("quote for m-live"));

        // Settings the user chose survive even though the retired project-wide
        // markers do not.
        assert!(!store.settings().unwrap().use_enabled);
        assert!(store.settings().unwrap().capture_enabled);

        // The restored review keeps its content even though this worktree's
        // baseline is unverifiable, so it stays reachable.
        store.set_mode(Some(true), None).unwrap();
        assert_eq!(store.search("expired review", &[], 5).unwrap().len(), 1);
        assert_eq!(store.search("archived rule", &[], 5).unwrap().len(), 1);
        assert!(store
            .search("unverified proposal", &[], 5)
            .unwrap()
            .is_empty());
        assert_eq!(
            store.read(&["m-review".to_string()]).unwrap().len(),
            1,
            "an unverifiable review is still readable"
        );

        // Project-wide markers from the retired lifecycle are gone; user
        // settings survive.
        for key in [
            "last_activity",
            "wrote_memory",
            "last_verified_user_turn",
            "last_verified_user_turn_count",
        ] {
            let found: Option<String> = store
                .conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert!(found.is_none(), "{key} must be cleared");
        }

        // The obsolete staleness table and expiry index are gone.
        let stale_table: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_stale'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_table, 0);
    }

    #[test]
    fn migration_from_v3_prunes_snapshot_bodies_and_wipes_purged_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("workspaces")
            .join("abc")
            .join("memory.sqlite3");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        build_v3(&path, &root);

        let mut store = Store::open(&path, "scope-1", &root).expect("migrate");
        // History keeps a fingerprint per wording and no full body copy.
        assert!(!table_has_column(&store.conn, "versions", "body").unwrap());
        let migrated: String = store
            .conn
            .query_row(
                "SELECT fingerprint FROM versions WHERE memory_id='m-live' AND version=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            migrated,
            fingerprint("decision", "older live wording", &[]),
            "the snapshot becomes the fingerprint of the wording it stored"
        );
        assert_eq!(store.stats().unwrap().versions, 1);

        // A row purged by an older build keeps nothing but its fingerprint.
        let purged = load_memory(&store.conn, "m-purged").unwrap().unwrap();
        assert_eq!(purged.state, STATE_PURGED);
        assert!(purged.text.is_empty());
        assert!(purged.source_quote.is_none());
        assert!(purged.supersedes.is_none());
        assert_eq!(purged.source.origin, STATE_PURGED);
        assert_eq!(versions(&store, "m-purged"), 0);
        assert_eq!(count(&store, "memory_files", "m-purged"), 0);
        assert_eq!(count(&store, "core_memories", "m-purged"), 0);
        // A purged wording cannot come back through a legacy replay either.
        let replay = store
            .import_legacy(&["purged body text".to_string()], "purged.json")
            .unwrap();
        assert_eq!((replay.created, replay.skipped), (0, 1));
        // The migrated snapshot fingerprint becomes a tombstone, so the earlier
        // wording stays out of a legacy replay after a forget.
        store.forget("m-live").unwrap();
        assert!(is_tombstoned(
            &store.conn,
            &fingerprint("decision", "older live wording", &[])
        )
        .unwrap());
    }
}
