//! SQLite persistence for project memory: schema, opening and mutations.
//!
//! Read methods live in `store_read.rs`. Every mutation runs inside an
//! IMMEDIATE transaction so writers serialise on an early reserved lock.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::model::*;

pub(crate) const SCHEMA_VERSION: i64 = 2;
pub(crate) const MAX_ROWS: i64 = 10_000;
pub(crate) const MAX_CANDIDATES: i64 = 200;
const MAX_IMPORT_NOTES: usize = 2_000;
const BUSY_TIMEOUT_MS: u64 = 1_000;

pub(crate) const STATE_PENDING: &str = "pending";
pub(crate) const STATE_CANDIDATE: &str = "candidate";
pub(crate) const STATE_ACCEPTED: &str = "accepted";
pub(crate) const STATE_FORGOTTEN: &str = "forgotten";
pub(crate) const STATE_PURGED: &str = "purged";
pub(crate) const STATE_DROPPED: &str = "dropped";
pub(crate) const STATE_EXPIRED: &str = "expired";

/// A remembered rule stays pending for one week waiting for the user to
/// restate its quote; a review note is active but expires after 30 days.
pub(crate) const PENDING_TTL_SECS: i64 = 7 * 24 * 60 * 60;
pub(crate) const REVIEW_TTL_SECS: i64 = 30 * 24 * 60 * 60;

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
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    version   INTEGER NOT NULL,
    body      TEXT NOT NULL,
    PRIMARY KEY (memory_id, version)
);
CREATE TABLE IF NOT EXISTS imports (
    source_id TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS tombstones (
    fingerprint TEXT PRIMARY KEY
);
"#;

const INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS memories_state_updated ON memories(state, updated_at DESC);
CREATE INDEX IF NOT EXISTS memories_state_expires ON memories(state, expires_at);
"#;

/// Columns added after schema version 1, applied as additive migrations.
const ADDED_COLUMNS: &[(&str, &str)] = &[
    ("source_quote", "TEXT"),
    ("expires_at", "INTEGER"),
    ("supersedes", "TEXT"),
];

pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    /// Open (or create) the workspace database and bind it to `scope_identity`.
    pub fn open(path: &Path, scope_identity: &str) -> Result<Store> {
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
        let store = Store { conn };
        store.init(scope_identity)?;
        Ok(store)
    }

    fn init(&self, scope_identity: &str) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            bail!(
                "Memory database schema version {version} is newer than supported version {SCHEMA_VERSION}"
            );
        }
        self.conn
            .execute_batch(SCHEMA)
            .context("Failed to create memory schema")?;
        migrate(&self.conn)?;
        self.conn
            .execute_batch(&format!("PRAGMA user_version={SCHEMA_VERSION};"))?;

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
            Some(_) => {}
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
            }
        }
        init_flag(&self.conn, META_USE, true)?;
        init_flag(&self.conn, META_CAPTURE, true)?;
        // Expire stale pending and review entries on every open. Best effort:
        // a failed cleanup must not make storage unavailable.
        let _ = expire_rows(&self.conn, now_ts());
        Ok(())
    }

    /// Store a memory. `accepted` promotes manual records; model proposals
    /// always land as candidates. Replaying identical text is idempotent.
    pub fn insert(&mut self, input: &NewMemory, accepted: bool) -> Result<Memory> {
        validate_input(input)?;
        let text = input.text.trim().to_string();
        let fingerprint = fingerprint(&input.kind, &text, &input.paths);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if is_tombstoned(&tx, &fingerprint)? {
            bail!("Identical content was forgotten or purged and cannot be re-created");
        }
        if let Some(existing) = load_by_fingerprint(&tx, &fingerprint)? {
            let memory = match existing.state.as_str() {
                STATE_PURGED => bail!("Identical content was purged and cannot be restored"),
                STATE_FORGOTTEN => {
                    bail!("Identical content was forgotten and cannot be re-created")
                }
                STATE_ACCEPTED => existing,
                _ if accepted && input.source.origin == "manual" => {
                    set_state(&tx, &existing, STATE_ACCEPTED)?
                }
                _ => existing,
            };
            tx.commit()?;
            return Ok(memory);
        }
        let row = NewRow {
            state: if accepted {
                STATE_ACCEPTED
            } else {
                STATE_CANDIDATE
            },
            source_quote: None,
            expires_at: None,
            supersedes: None,
        };
        let memory = insert_row(&tx, input, &text, fingerprint, &row)?;
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
            STATE_CANDIDATE => set_state(&tx, &existing, STATE_ACCEPTED)?,
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
            "UPDATE memories SET text=?1, fingerprint=?2, version=?3, updated_at=?4 WHERE id=?5",
            params![text, fingerprint, version as i64, now, existing.id],
        )?;
        let memory = Memory {
            text,
            version,
            updated_at: now,
            ..existing
        };
        put_snapshot(&tx, &memory)?;
        tx.commit()?;
        Ok(memory)
    }

    /// Tombstone a memory: text and history stay, the fingerprint blocks reuse.
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

    /// Purge body, source and history while keeping the fingerprint tombstone.
    pub fn purge(&mut self, id: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = require_memory(&tx, id)?;
        if existing.state == STATE_PURGED {
            tx.commit()?;
            return Ok(());
        }
        record_version_tombstones(&tx, &existing)?;
        let wiped = serde_json::to_string(&Source {
            origin: STATE_PURGED.to_string(),
            session_id: None,
            call_id: None,
        })?;
        tx.execute(
            "DELETE FROM versions WHERE memory_id=?1",
            params![existing.id],
        )?;
        tx.execute(
            "UPDATE memories SET state=?1, text='', paths='[]', evidence_refs='[]',
                    source=?2, updated_at=?3 WHERE id=?4",
            params![STATE_PURGED, wiped, now_ts(), existing.id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Import a validated legacy note list. All notes validate before any write;
    /// repeated `source_id` replays report `already_imported` without writes.
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
        let rows: i64 = tx.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
        if rows + notes.len() as i64 > MAX_ROWS {
            bail!("Memory store row limit reached");
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
                    expires_at: None,
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

    /// Durable write used by the model tool. `pending` entries wait for the
    /// user to restate the verbatim quote; `accepted` entries are active and
    /// carry an expiry (review notes). Replaying identical text is idempotent.
    pub fn remember(
        &mut self,
        input: &NewMemory,
        state: &str,
        quote: Option<&str>,
        expires_at: Option<i64>,
        supersedes: Option<&str>,
    ) -> Result<Memory> {
        validate_input(input)?;
        if state != STATE_PENDING && state != STATE_ACCEPTED {
            bail!("Unsupported memory state");
        }
        if let Some(quote) = quote {
            validate_quote(quote)?;
        }
        if supersedes.is_some_and(|id| id.trim().is_empty()) {
            bail!("Superseded memory id is empty");
        }
        let text = input.text.trim().to_string();
        let fingerprint = fingerprint(&input.kind, &text, &input.paths);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if is_tombstoned(&tx, &fingerprint)? {
            bail!("Identical content was forgotten or purged and cannot be re-created");
        }
        let row = NewRow {
            state,
            source_quote: quote,
            expires_at,
            supersedes,
        };
        if let Some(existing) = load_by_fingerprint(&tx, &fingerprint)? {
            let memory = match existing.state.as_str() {
                STATE_PURGED => bail!("Memory was purged"),
                STATE_FORGOTTEN => {
                    bail!("Identical content was forgotten and cannot be re-created")
                }
                STATE_ACCEPTED | STATE_PENDING => existing,
                _ => revive_row(&tx, &existing, &row)?,
            };
            tx.commit()?;
            return Ok(memory);
        }
        let memory = insert_row(&tx, input, &text, fingerprint, &row)?;
        tx.commit()?;
        Ok(memory)
    }

    /// Promote every pending entry whose quote appears verbatim (whitespace-
    /// and case-normalized) in one of `recent_users`; drop the rest. Applies
    /// `supersedes` on promotion. Idempotent: a replayed summary finds no
    /// pending rows and changes nothing.
    pub(crate) fn promote_pending(
        &mut self,
        recent_users: &[String],
        now: i64,
    ) -> Result<PromoteReport> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_rows(&tx, now)?;
        let sql = format!(
            "SELECT {COLUMNS} FROM memories WHERE state=?1 ORDER BY created_at ASC LIMIT ?2"
        );
        let pending = {
            let mut statement = tx.prepare(&sql)?;
            let rows =
                statement.query_map(params![STATE_PENDING, MAX_CANDIDATES], memory_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut report = PromoteReport::default();
        for memory in pending {
            let matched = memory
                .source_quote
                .as_deref()
                .is_some_and(|quote| quote_matches(quote, recent_users));
            if !matched {
                set_state(&tx, &memory, STATE_DROPPED)?;
                report.dropped += 1;
                continue;
            }
            let promoted = promote_row(&tx, &memory)?;
            report.promoted += 1;
            if let Some(target) = promoted.supersedes.as_deref() {
                if target != promoted.id {
                    if let Some(old) = load_memory(&tx, target)? {
                        if old.state != STATE_FORGOTTEN && old.state != STATE_PURGED {
                            record_version_tombstones(&tx, &old)?;
                            set_state(&tx, &old, STATE_FORGOTTEN)?;
                            report.superseded += 1;
                        }
                    }
                }
            }
        }
        tx.commit()?;
        Ok(report)
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

/// Columns written for one new or revived row.
pub(crate) struct NewRow<'a> {
    pub state: &'a str,
    pub source_quote: Option<&'a str>,
    pub expires_at: Option<i64>,
    pub supersedes: Option<&'a str>,
}

#[derive(Default)]
pub(crate) struct PromoteReport {
    pub promoted: u64,
    pub dropped: u64,
    pub superseded: u64,
}

fn insert_row(
    conn: &Connection,
    input: &NewMemory,
    text: &str,
    fingerprint: String,
    row: &NewRow<'_>,
) -> Result<Memory> {
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
    if rows >= MAX_ROWS {
        bail!("Memory store row limit reached");
    }
    if row.state != STATE_ACCEPTED {
        let queued: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE state IN (?1, ?2)",
            params![STATE_CANDIDATE, STATE_PENDING],
            |row| row.get(0),
        )?;
        if queued >= MAX_CANDIDATES {
            bail!("Candidate queue is full ({MAX_CANDIDATES}); review pending candidates first");
        }
    }
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
        expires_at: row.expires_at,
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
    put_snapshot(conn, &memory)?;
    Ok(memory)
}

/// Reuse an existing row's identity when the same content is remembered again
/// after expiring, being dropped or lingering as a legacy candidate.
fn revive_row(conn: &Connection, existing: &Memory, row: &NewRow<'_>) -> Result<Memory> {
    let version = existing.version.saturating_add(1);
    let now = now_ts();
    conn.execute(
        "UPDATE memories SET state=?1, version=?2, updated_at=?3, source_quote=?4,
                expires_at=?5, supersedes=?6 WHERE id=?7",
        params![
            row.state,
            version as i64,
            now,
            row.source_quote,
            row.expires_at,
            row.supersedes,
            existing.id,
        ],
    )?;
    let memory = Memory {
        state: row.state.to_string(),
        version,
        updated_at: now,
        source_quote: row.source_quote.map(str::to_string),
        expires_at: row.expires_at,
        supersedes: row.supersedes.map(str::to_string),
        ..existing.clone()
    };
    put_snapshot(conn, &memory)?;
    Ok(memory)
}

/// Promotion activates a pending entry and clears its pending expiry.
fn promote_row(conn: &Connection, existing: &Memory) -> Result<Memory> {
    let version = existing.version.saturating_add(1);
    let now = now_ts();
    conn.execute(
        "UPDATE memories SET state=?1, version=?2, updated_at=?3, expires_at=NULL WHERE id=?4",
        params![STATE_ACCEPTED, version as i64, now, existing.id],
    )?;
    let memory = Memory {
        state: STATE_ACCEPTED.to_string(),
        version,
        updated_at: now,
        expires_at: None,
        ..existing.clone()
    };
    put_snapshot(conn, &memory)?;
    Ok(memory)
}

/// Change lifecycle state, bumping the version and snapshotting the body.
fn set_state(conn: &Connection, existing: &Memory, state: &str) -> Result<Memory> {
    let version = existing.version.saturating_add(1);
    let now = now_ts();
    conn.execute(
        "UPDATE memories SET state=?1, version=?2, updated_at=?3 WHERE id=?4",
        params![state, version as i64, now, existing.id],
    )?;
    let memory = Memory {
        state: state.to_string(),
        version,
        updated_at: now,
        ..existing.clone()
    };
    put_snapshot(conn, &memory)?;
    Ok(memory)
}

fn put_snapshot(conn: &Connection, memory: &Memory) -> Result<()> {
    let body = serde_json::to_string(memory)?;
    conn.execute(
        "INSERT INTO versions (memory_id, version, body) VALUES (?1, ?2, ?3)
         ON CONFLICT(memory_id, version) DO UPDATE SET body=excluded.body",
        params![memory.id, memory.version as i64, body],
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

/// Record the current text and every retained version so forgetting cannot be
/// undone by re-creating an earlier corrected wording of the same note.
fn record_version_tombstones(conn: &Connection, memory: &Memory) -> Result<()> {
    remember_tombstone(conn, memory)?;
    let mut statement = conn.prepare("SELECT body FROM versions WHERE memory_id=?1")?;
    let bodies = statement.query_map(params![memory.id], |row| row.get::<_, String>(0))?;
    for body in bodies {
        let old: Memory = serde_json::from_str(&body?)?;
        remember_tombstone(conn, &old)?;
    }
    Ok(())
}

fn remember_tombstone(conn: &Connection, memory: &Memory) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO tombstones(fingerprint) VALUES (?1)",
        params![fingerprint(&memory.kind, &memory.text, &memory.paths)],
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

/// Forward-compatible, additive schema migration. A database written by an
/// older build gains the new columns; a database from a newer build is rejected
/// by the version check in `Store::init`.
fn migrate(conn: &Connection) -> Result<()> {
    for (name, kind) in ADDED_COLUMNS {
        if !column_exists(conn, name)? {
            conn.execute_batch(&format!("ALTER TABLE memories ADD COLUMN {name} {kind};"))
                .context("Failed to migrate the memory schema")?;
        }
    }
    conn.execute_batch(INDEXES)
        .context("Failed to create memory schema indexes")?;
    Ok(())
}

fn column_exists(conn: &Connection, name: &str) -> Result<bool> {
    let mut statement = conn.prepare("SELECT name FROM pragma_table_info('memories')")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let existing: String = row.get(0)?;
        if existing == name {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Expire active review notes and pending rules whose time ran out, and drop
/// pending rules that were never confirmed within a week.
pub(crate) fn expire_rows(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE memories SET state=?1, updated_at=?2
          WHERE state IN (?3, ?4) AND expires_at IS NOT NULL AND expires_at <= ?2",
        params![STATE_EXPIRED, now, STATE_ACCEPTED, STATE_PENDING],
    )?;
    conn.execute(
        "UPDATE memories SET state=?1, updated_at=?2 WHERE state=?3 AND created_at <= ?4",
        params![STATE_DROPPED, now, STATE_PENDING, now - PENDING_TTL_SECS],
    )?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn open(dir: &std::path::Path) -> Store {
        Store::open(
            &dir.join("workspaces").join("abc").join("memory.sqlite3"),
            "scope-1",
        )
        .expect("open store")
    }

    fn snapshots(store: &Store, id: &str) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM versions WHERE memory_id=?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn insert_accept_correct_forget_purge() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());

        let memory = store.insert(&input("use WAL", "manual"), true).unwrap();
        assert_eq!(memory.state, "accepted");
        assert_eq!(memory.version, 1);
        assert_eq!(snapshots(&store, &memory.id), 1);

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
        assert_eq!(snapshots(&store, &candidate.id), 3);

        store.forget(&memory.id).unwrap();
        let forgotten = load_memory(&store.conn, &memory.id).unwrap().unwrap();
        assert_eq!(forgotten.state, "forgotten");
        assert!(store.insert(&input("use WAL", "manual"), true).is_err());

        store.purge(&memory.id).unwrap();
        let purged = load_memory(&store.conn, &memory.id).unwrap().unwrap();
        assert_eq!(purged.state, "purged");
        assert!(purged.text.is_empty());
        assert_eq!(snapshots(&store, &memory.id), 0);
        let retained = fingerprint("decision", "use WAL", &["src/lib.rs".to_string()]);
        assert!(load_by_fingerprint(&store.conn, &retained)
            .unwrap()
            .is_some());
        assert!(store.insert(&input("use WAL", "manual"), true).is_err());
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

        assert!(Store::open(&dir.path().join("w2").join("m.sqlite3"), "other-scope").is_ok());
    }

    #[test]
    fn identity_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite3");
        drop(Store::open(&path, "scope-a").unwrap());
        assert!(Store::open(&path, "scope-b").is_err());
        assert!(Store::open(&path, "scope-a").is_ok());
    }
    fn note(text: &str) -> NewMemory {
        NewMemory {
            kind: "reference".to_string(),
            text: text.to_string(),
            paths: Vec::new(),
            evidence_refs: Vec::new(),
            source: Source {
                origin: "manual".to_string(),
                session_id: None,
                call_id: None,
            },
        }
    }

    #[test]
    fn forgetting_blocks_earlier_corrected_wording_and_import_replay() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path());
        let original = store
            .insert(&note("cache keys ignore generations"), true)
            .unwrap();
        let corrected = store
            .correct(&original.id, "cache keys include generations")
            .unwrap();
        assert!(corrected.version > original.version);

        store.forget(&corrected.id).unwrap();
        assert!(store
            .insert(&note("cache keys ignore generations"), true)
            .is_err());
        assert!(store
            .insert(&note("cache keys include generations"), true)
            .is_err());

        let report = store
            .import_legacy(
                &["cache keys ignore generations".to_string()],
                "replay.json",
            )
            .unwrap();
        assert_eq!(report.created, 0);
        assert_eq!(report.skipped, 1);

        let second = store
            .insert(&input("purge keeps every earlier wording", "manual"), true)
            .unwrap();
        store
            .correct(&second.id, "purge keeps every earlier wording too")
            .unwrap();
        store.purge(&second.id).unwrap();
        assert!(store
            .insert(&input("purge keeps every earlier wording", "manual"), true)
            .is_err());
        assert!(store
            .insert(
                &input("purge keeps every earlier wording too", "manual"),
                true
            )
            .is_err());
    }

    #[test]
    fn concurrent_connections_keep_every_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("workspaces")
            .join("abc")
            .join("memory.sqlite3");
        drop(Store::open(&path, "scope-1").unwrap());
        let mut handles = Vec::new();
        for worker in 0..2 {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                let mut store = Store::open(&path, "scope-1").unwrap();
                for index in 0..10 {
                    let text = format!("worker {worker} note {index}");
                    store.insert(&input(&text, "manual"), true).unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let store = Store::open(&path, "scope-1").unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 20);
    }
}
