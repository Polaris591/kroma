#[cfg(test)]
use crate::context_state::CompactionState;
use crate::{context_state::PersistedContext, ports::ContextCommit, protocol::Message};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
// Bound the recovery work and checkpoint frequency, but do not force a full
// rewrite at every user / model / tool message.
const JOURNAL_MAX_ENTRIES: usize = 256;
const JOURNAL_MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct SavedSession {
    version: u32,
    workdir: String,
    #[serde(default)]
    generation: u64,
    messages: Vec<Message>,
    #[serde(default)]
    compression: PersistedContext,
}

pub struct LoadedSession {
    pub messages: Vec<Message>,
    pub context: PersistedContext,
}

/// Read the checkpoint plus valid journal entries without taking the
/// session lock. The agent already holds that lock for the live session.
pub(crate) fn read_archive_messages(path: &Path) -> Result<Vec<Message>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let journal = journal_name(path);
            if journal.exists() && fs::metadata(&journal)?.len() > 0 {
                bail!("session snapshot missing while a journal still exists; refusing to discard history");
            }
            return Ok(Vec::new());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading session {}", path.display()))
        }
    };
    let mut session: SavedSession = serde_json::from_slice(&data)
        .with_context(|| format!("invalid session {}", path.display()))?;
    if !matches!(session.version, 1..=3) {
        bail!("unsupported session format version {}", session.version);
    }
    replay_journal(
        &journal_name(path),
        session.generation,
        &mut session.messages,
        &mut session.compression,
    )?;
    if session
        .messages
        .first()
        .is_none_or(|msg| msg.role != "system")
    {
        bail!("saved session has no system message");
    }
    // This is a read-only projection of durable history. Session recovery is
    // separate: only SessionStore::load may synthesize unknown results for an
    // interrupted tool call.
    Ok(session.messages)
}

struct JournalReplay {
    entries: usize,
    complete_bytes: usize,
    total_bytes: usize,
}

/// Parse and validate complete journal records. An interrupted final record is
/// counted as unreadable but is not an error; `SessionStore::load` decides
/// whether to truncate it. Generation mismatches and records already present
/// in the snapshot are skipped, exactly as in normal session recovery.
fn replay_journal(
    journal_path: &Path,
    generation: u64,
    messages: &mut Vec<Message>,
    compression: &mut PersistedContext,
) -> Result<JournalReplay> {
    let contents = match fs::read(journal_path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let total_bytes = contents.len();
    let mut complete_bytes = 0usize;
    let mut entries = 0usize;
    for (i, byte) in contents.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let line = &contents[complete_bytes..i];
        complete_bytes = i + 1;
        let split = line
            .iter()
            .rposition(|b| *b == b'\t')
            .context("corrupt session journal: missing checksum")?;
        let (payload, trailer) = line.split_at(split);
        let saved_checksum = std::str::from_utf8(&trailer[1..])
            .ok()
            .and_then(|value| u64::from_str_radix(value, 16).ok())
            .context("corrupt session journal checksum")?;
        if checksum(payload) != saved_checksum {
            bail!("corrupt session journal: checksum mismatch");
        }
        let event: JournalAppend =
            serde_json::from_slice(payload).context("invalid session journal event")?;
        if event.generation != generation {
            // A checkpoint from /clear can commit before the old journal is
            // truncated. Never replay another generation.
            continue;
        }
        entries += 1;
        match event.index.cmp(&messages.len()) {
            std::cmp::Ordering::Less => {
                if serde_json::to_value(&messages[event.index])?
                    != serde_json::to_value(&event.message)?
                {
                    bail!("session journal disagrees with durable checkpoint");
                }
            }
            std::cmp::Ordering::Equal => {
                messages.push(event.message);
                *compression = event.compression;
            }
            std::cmp::Ordering::Greater => {
                bail!("session journal has a missing message index");
            }
        }
    }
    Ok(JournalReplay {
        entries,
        complete_bytes,
        total_bytes,
    })
}

pub struct SessionStore {
    path: PathBuf,
    journal_path: PathBuf,
    workdir: String,
    // Keep an OS advisory lock for the entire lifetime of the session.
    _lock: fs::File,
    journal: Mutex<JournalCursor>,
}

/// Only the Agent can create this handle; an injected context policy receives
/// the limited ContextCommit trait, never an unrestricted SessionStore.
pub(crate) struct ContextCommitter<'a> {
    session: &'a SessionStore,
    authoritative: &'a [Message],
}

impl SessionStore {
    pub(crate) fn context_committer<'a>(
        &'a self,
        authoritative: &'a [Message],
    ) -> ContextCommitter<'a> {
        ContextCommitter {
            session: self,
            authoritative,
        }
    }
}

impl ContextCommit for ContextCommitter<'_> {
    fn commit(&self, state: &PersistedContext) -> Result<()> {
        state.validate(self.authoritative)?;
        self.session.save(self.authoritative, state)
    }
}

#[derive(Default)]
struct JournalCursor {
    generation: u64,
    message_count: usize,
    entries: usize,
    bytes: u64,
    // A failed write may have left an incomplete or not-yet-durable record.
    // The current process must not append over that uncertain tail. Reopen
    // the session to run checksum/tail recovery before accepting more work.
    poisoned: bool,
    #[cfg(test)]
    fail_partial_at: Option<usize>,
}

#[derive(Serialize, Deserialize)]
struct JournalAppend {
    generation: u64,
    index: usize,
    message: Message,
    compression: PersistedContext,
}

fn new_generation() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    nanos ^ (u64::from(std::process::id()) << 32) ^ NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
}

fn journal_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".wal");
    PathBuf::from(name)
}

// Stable FNV-1a: detects incomplete/corrupt journal entries without a crate.
// The journal is not an authenticated security boundary.
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

impl SessionStore {
    pub(crate) fn default_path(workdir: &Path) -> Result<PathBuf> {
        let path = if let Some(path) = env::var_os("KROMA_SESSION_FILE") {
            let path = PathBuf::from(path);
            if path.is_relative() {
                env::current_dir()
                    .context("failed to resolve relative session path")?
                    .join(path)
            } else {
                path
            }
        } else {
            let home = env::var_os("HOME")
                .or_else(|| env::var_os("USERPROFILE"))
                .context("HOME is unset; set KROMA_SESSION_FILE to choose a history path")?;

            // Stable file name; verify the full workdir when loading to avoid hash collisions.
            let hash = workdir
                .to_string_lossy()
                .as_bytes()
                .iter()
                .fold(0xcbf29ce484222325u64, |hash, byte| {
                    (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
                });
            PathBuf::from(home)
                .join(".kroma")
                .join("sessions")
                .join(format!("{hash:016x}.json"))
        };
        if path.as_os_str().is_empty() {
            bail!("KROMA_SESSION_FILE must not be empty");
        }
        Ok(path)
    }

    pub(crate) fn open_at(path: PathBuf, workdir: &Path) -> Result<Self> {
        let parent = path.parent().context("invalid session path")?;
        #[cfg(unix)]
        let parent_existed = parent.exists();
        fs::create_dir_all(parent).context("failed to create session directory")?;
        #[cfg(unix)]
        if !parent_existed {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let file_name = path.file_name().context("invalid session file name")?;
        let lock_path = parent.join(format!(".{}.lock", file_name.to_string_lossy()));
        let mut opts = fs::OpenOptions::new();
        opts.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            opts.share_mode(0);
        }
        let lock_file = opts.open(&lock_path).with_context(|| {
            format!(
                "session already in use or lock inaccessible: {}",
                path.display()
            )
        })?;
        #[cfg(unix)]
        {
            let status =
                unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if status != 0 {
                bail!(
                    "session already open in another Kroma process ({}): {}",
                    path.display(),
                    std::io::Error::last_os_error()
                );
            }
        }
        Ok(Self {
            journal_path: journal_name(&path),
            path,
            workdir: workdir.to_string_lossy().into_owned(),
            _lock: lock_file,
            journal: Mutex::new(JournalCursor {
                generation: new_generation(),
                ..JournalCursor::default()
            }),
        })
    }

    pub fn archive_path(&self) -> &Path {
        &self.path
    }

    pub fn ensure_writable(&self) -> Result<()> {
        if self
            .journal
            .lock()
            .expect("session journal lock poisoned")
            .poisoned
        {
            bail!("session persistence previously failed; stop this session and restart Kroma to recover its WAL before continuing");
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn inject_partial_append_failure_at(&self, index: usize) {
        self.journal
            .lock()
            .expect("session journal lock poisoned")
            .fail_partial_at = Some(index);
    }

    #[cfg(test)]
    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    pub fn load(&self) -> Result<Option<LoadedSession>> {
        let data = match fs::read(&self.path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if self.journal_path.exists() && fs::metadata(&self.journal_path)?.len() > 0 {
                    bail!("session snapshot missing while a journal still exists; refusing to discard history");
                }
                return Ok(None);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read session {}", self.path.display()))
            }
        };
        let mut session: SavedSession = serde_json::from_slice(&data)
            .with_context(|| format!("invalid saved session {}", self.path.display()))?;
        if !matches!(session.version, 1..=3) {
            bail!("unsupported session format version {}", session.version);
        }
        if session.workdir != self.workdir {
            bail!("saved session belongs to a different working directory");
        }
        let migrate_generation = session.generation == 0;
        {
            let mut cursor = self.journal.lock().expect("session journal lock poisoned");
            if !migrate_generation {
                cursor.generation = session.generation;
            }
            cursor.message_count = session.messages.len();
            let replay = replay_journal(
                &self.journal_path,
                session.generation,
                &mut session.messages,
                &mut session.compression,
            )?;
            cursor.message_count = session.messages.len();
            cursor.entries = replay.entries;
            cursor.bytes = replay.complete_bytes as u64;
            if replay.complete_bytes < replay.total_bytes {
                // The final write was interrupted before its newline. Its
                // command outcome is unknown; recover_pending_calls handles
                // that conservatively, without replaying shell commands.
                let file = fs::OpenOptions::new()
                    .write(true)
                    .open(&self.journal_path)?;
                file.set_len(replay.complete_bytes as u64)?;
                file.sync_all()?;
            }
        }
        if session
            .messages
            .first()
            .is_none_or(|msg| msg.role != "system")
        {
            bail!("saved session has no system message");
        }
        let migrated = match &mut session.compression {
            PersistedContext::Builtin(state) => state.migrate_legacy()?,
            PersistedContext::External(state) => {
                state.validate()?;
                false
            }
        };
        session.compression.validate(&session.messages)?;
        let recovered = recover_pending_calls(&mut session.messages);
        if migrated || recovered || session.version != 3 || migrate_generation {
            self.save(&session.messages, &session.compression)?;
        }
        Ok(Some(LoadedSession {
            messages: session.messages,
            context: session.compression,
        }))
    }

    pub fn save<S: Clone + Into<PersistedContext>>(
        &self,
        messages: &[Message],
        state: &S,
    ) -> Result<()> {
        self.save_snapshot(messages, &state.clone().into(), false)
    }

    pub fn reset<S: Clone + Into<PersistedContext>>(
        &self,
        messages: &[Message],
        state: &S,
    ) -> Result<()> {
        self.save_snapshot(messages, &state.clone().into(), true)
    }

    /// Append one individually durable message instead of rewriting history.
    /// The caller passes the full slice only for occasional checkpoints.
    pub fn append<S: Clone + Into<PersistedContext>>(
        &self,
        messages: &[Message],
        compression: &S,
    ) -> Result<()> {
        let last = messages
            .last()
            .context("cannot append empty conversation")?;
        let mut cursor = self.journal.lock().expect("session journal lock poisoned");
        if cursor.poisoned {
            bail!(
                "session persistence previously failed; restart Kroma to recover before continuing"
            );
        }
        if messages.len() != cursor.message_count + 1 {
            bail!("session append is not sequential; use a new checkpoint for replacements");
        }
        let persisted: PersistedContext = compression.clone().into();
        let payload = serde_json::to_vec(&JournalAppend {
            generation: cursor.generation,
            index: cursor.message_count,
            message: last.clone(),
            compression: persisted.clone(),
        })?;
        #[cfg(test)]
        let fail_partial = if cursor.fail_partial_at == Some(cursor.message_count) {
            cursor.fail_partial_at = None;
            true
        } else {
            false
        };
        let appended = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let new_file = !self.journal_path.exists();
            let mut file = options
                .open(&self.journal_path)
                .context("failed to open session append journal")?;
            #[cfg(test)]
            if fail_partial {
                file.write_all(&payload[..payload.len() / 2])?;
                file.sync_all()?;
                bail!("injected partial WAL append failure");
            }
            file.write_all(&payload)?;
            file.write_all(format!("\t{:016x}\n", checksum(&payload)).as_bytes())?;
            file.sync_all()
                .context("failed to sync appended session result")?;
            if new_file {
                sync_parent(&self.path)?;
            }
            Ok(())
        })();
        if let Err(error) = appended {
            cursor.poisoned = true;
            return Err(error.context(
                "session append failed; further writes blocked until recovery on restart",
            ));
        }
        cursor.message_count += 1;
        cursor.entries += 1;
        cursor.bytes += payload.len() as u64 + 18;
        if cursor.entries >= JOURNAL_MAX_ENTRIES || cursor.bytes >= JOURNAL_MAX_BYTES {
            if let Err(error) = self.checkpoint_locked(messages, &persisted, &mut cursor) {
                cursor.poisoned = true;
                return Err(error.context("session checkpoint failed; restart before continuing"));
            }
        }
        Ok(())
    }

    fn save_snapshot(
        &self,
        messages: &[Message],
        compression: &PersistedContext,
        fresh_generation: bool,
    ) -> Result<()> {
        let mut cursor = self.journal.lock().expect("session journal lock poisoned");
        if cursor.poisoned {
            bail!("session persistence previously failed; restart Kroma to recover before saving");
        }
        if fresh_generation {
            let generation = new_generation();
            // Commit generation only after the snapshot is durable.
            if let Err(error) = self.write_snapshot(messages, compression, generation) {
                cursor.poisoned = true;
                return Err(error.context("session snapshot failed; restart before continuing"));
            }
            cursor.generation = generation;
            cursor.message_count = messages.len();
            cursor.entries = 0;
            cursor.bytes = 0;
            if let Err(error) = self.clear_journal() {
                cursor.poisoned = true;
                return Err(error.context("session WAL reset failed; restart before continuing"));
            }
            return Ok(());
        }
        if let Err(error) = self.checkpoint_locked(messages, compression, &mut cursor) {
            cursor.poisoned = true;
            return Err(error.context("session checkpoint failed; restart before continuing"));
        }
        Ok(())
    }

    fn checkpoint_locked(
        &self,
        messages: &[Message],
        compression: &PersistedContext,
        cursor: &mut JournalCursor,
    ) -> Result<()> {
        self.write_snapshot(messages, compression, cursor.generation)?;
        cursor.message_count = messages.len();
        cursor.entries = 0;
        cursor.bytes = 0;
        self.clear_journal()
    }

    fn clear_journal(&self) -> Result<()> {
        match fs::OpenOptions::new().write(true).open(&self.journal_path) {
            Ok(file) => {
                file.set_len(0)?;
                file.sync_all()?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        Ok(())
    }

    fn write_snapshot(
        &self,
        messages: &[Message],
        compression: &PersistedContext,
        generation: u64,
    ) -> Result<()> {
        let saved = SavedSession {
            version: 3,
            workdir: self.workdir.clone(),
            generation,
            messages: messages.to_vec(),
            compression: compression.clone(),
        };
        let bytes = serde_json::to_vec(&saved).context("failed to encode chat history")?;
        let parent = self.path.parent().context("invalid session file path")?;
        #[cfg(unix)]
        let parent_existed = parent.exists();
        fs::create_dir_all(parent).context("failed to create chat history directory")?;
        #[cfg(unix)]
        if !parent_existed {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }

        let name = self
            .path
            .file_name()
            .context("invalid session file name")?
            .to_string_lossy();
        let temp = parent.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temp)
                .context("failed to create session temp file")?;
            file.write_all(&bytes)
                .context("failed to write chat history")?;
            file.sync_all().context("failed to sync chat history")?;
            crate::atomic_file::replace(&temp, &self.path)?;
            sync_parent(&self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path.parent().context("session path has no parent")?)?
        .sync_all()
        .context("failed to sync session directory")?;
    // Windows has no directory fsync: the write-through move in
    // atomic_file::replace already makes the new snapshot durable.
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

// An interrupted process must never replay a tool call whose side effects may
// already have happened. Tell the model the result is unknown instead.
pub fn recover_pending_calls(messages: &mut Vec<Message>) -> bool {
    let Some(index) = messages.iter().rposition(|msg| msg.role != "tool") else {
        return false;
    };
    if messages[index].role != "assistant" {
        return false;
    }
    let Some(calls) = messages[index].tool_calls.clone() else {
        return false;
    };
    let mut recovered = false;
    for call in calls {
        if !messages[index + 1..]
            .iter()
            .any(|msg| msg.tool_call_id.as_deref() == Some(call.id.as_str()))
        {
            messages.push(Message::tool(
                call.id,
                "{\"error\":\"Session interrupted. Tool result is unknown; do not assume the command succeeded or automatically repeat it.\"}".to_owned(),
            ));
            recovered = true;
        }
    }
    recovered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FunctionCall, ToolCall};
    use std::time::Instant;

    #[test]
    fn partially_written_tool_result_blocks_session_until_reopen_and_marks_unknown() {
        let dir = env::temp_dir().join(format!("kroma-wal-fault-{}", new_generation()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        let store = SessionStore::open_at(path.clone(), &dir).unwrap();
        let state = CompactionState::default();
        let mut messages = vec![Message::new("system", "system")];
        store.save(&messages, &state).unwrap();
        messages.push(Message::new("user", "do two commands"));
        store.append(&messages, &state).unwrap();
        messages.push(Message {
            role: "assistant".into(),
            content: None,
            reasoning_content: None,
            tool_calls: Some(
                (1..=2)
                    .map(|n| ToolCall {
                        id: format!("call_{n}"),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "shell".into(),
                            arguments: format!("{{\"command\":\"echo {n}\"}}"),
                        },
                    })
                    .collect(),
            ),
            tool_call_id: None,
            provider_items: None,
        });
        store.append(&messages, &state).unwrap();
        messages.push(Message::tool("call_1".into(), "finished first".into()));
        store.append(&messages, &state).unwrap();
        store.inject_partial_append_failure_at(messages.len());
        messages.push(Message::tool("call_2".into(), "finished second".into()));
        let failed = store.append(&messages, &state).unwrap_err();
        assert!(failed.to_string().contains("restart"));
        assert!(store.ensure_writable().is_err());
        assert!(store.append(&messages, &state).is_err());
        assert!(store.save(&messages, &state).is_err());
        drop(store);

        let recovered_store = SessionStore::open_at(path, &dir).unwrap();
        let recovered = recovered_store.load().unwrap().unwrap().messages;
        assert_eq!(recovered.len(), 5);
        assert_eq!(recovered[3].content.as_deref(), Some("finished first"));
        assert_eq!(recovered[4].tool_call_id.as_deref(), Some("call_2"));
        assert!(recovered[4].content.as_deref().unwrap().contains("unknown"));
        assert!(recovered_store.ensure_writable().is_ok());
        drop(recovered_store);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn journal_round_trip_preserves_reasoning_and_tool_call_order() {
        let dir = env::temp_dir().join(format!("kroma-thinking-wal-{}", new_generation()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        let store = SessionStore::open_at(path.clone(), &dir).unwrap();
        let mut messages = vec![Message::new("system", "base")];
        store.save(&messages, &CompactionState::default()).unwrap();
        messages.push(Message::new("user", "inspect"));
        store
            .append(&messages, &CompactionState::default())
            .unwrap();
        messages.push(Message {
            role: "assistant".into(),
            content: Some(String::new()),
            reasoning_content: Some("thinking fragments joined verbatim".into()),
            tool_calls: Some(vec![ToolCall {
                id: "call-1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "shell".into(),
                    arguments: r#"{"command":"pwd"}"#.into(),
                },
            }]),
            tool_call_id: None,
            provider_items: None,
        });
        store
            .append(&messages, &CompactionState::default())
            .unwrap();
        messages.push(Message::tool("call-1".into(), "result".into()));
        store
            .append(&messages, &CompactionState::default())
            .unwrap();
        drop(store);
        let store = SessionStore::open_at(path, &dir).unwrap();
        let restored = store.load().unwrap().unwrap().messages;
        assert_eq!(restored.len(), 4);
        assert_eq!(restored[2].reasoning_content, messages[2].reasoning_content);
        assert_eq!(restored[2].tool_calls.as_ref().unwrap()[0].id, "call-1");
        assert_eq!(restored[3].tool_call_id.as_deref(), Some("call-1"));
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn archive_reader_merges_snapshot_and_new_journal_like_session_load() {
        let dir = env::temp_dir().join(format!("kroma-archive-wal-{}", new_generation()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        let store = SessionStore::open_at(path.clone(), &dir).unwrap();
        let state = CompactionState::default();
        let mut messages = vec![
            Message::new("system", "system"),
            Message::new("user", "snapshot task"),
        ];
        store.save(&messages, &state).unwrap();
        messages.push(Message {
            role: "assistant".into(),
            content: Some("collecting latest evidence".into()),
            reasoning_content: None,
            tool_calls: Some(vec![ToolCall {
                id: "latest-call".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "shell".into(),
                    arguments: r#"{"command":"cat latest.log"}"#.into(),
                },
            }]),
            tool_call_id: None,
            provider_items: None,
        });
        store.append(&messages, &state).unwrap();
        messages.push(Message::tool(
            "latest-call".into(),
            "latest evidence from WAL".into(),
        ));
        store.append(&messages, &state).unwrap();

        let archived = read_archive_messages(&path).unwrap();
        assert_eq!(archived.len(), messages.len());
        assert_eq!(
            archived[2].tool_calls.as_ref().unwrap()[0].id,
            "latest-call"
        );
        assert_eq!(
            archived[3].content.as_deref(),
            Some("latest evidence from WAL")
        );

        let loaded = store.load().unwrap().unwrap().messages;
        assert_eq!(
            serde_json::to_vec(&archived).unwrap(),
            serde_json::to_vec(&loaded).unwrap(),
            "history reading must expose the same durable messages as a completed session load"
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn archive_reader_handles_empty_duplicate_and_old_generation_journals() {
        let dir = env::temp_dir().join(format!("kroma-archive-generation-{}", new_generation()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        let store = SessionStore::open_at(path.clone(), &dir).unwrap();
        let state = CompactionState::default();
        let mut messages = vec![
            Message::new("system", "system"),
            Message::new("user", "snapshot only"),
        ];
        store.save(&messages, &state).unwrap();
        assert_eq!(read_archive_messages(&path).unwrap().len(), 2);

        // A record already included in the snapshot is validated but not
        // appended a second time.
        messages.push(Message::new("user", "already checkpointed"));
        store.append(&messages, &state).unwrap();
        let generation = store.journal.lock().unwrap().generation;
        let persisted: PersistedContext = state.clone().into();
        store
            .write_snapshot(&messages, &persisted, generation)
            .unwrap();
        let archived = read_archive_messages(&path).unwrap();
        assert_eq!(archived.len(), 3);
        assert_eq!(archived[2].content.as_deref(), Some("already checkpointed"));

        // A stale journal from before a reset belongs to another generation.
        let stale = fs::read(journal_name(&path)).unwrap();
        store.save(&messages, &state).unwrap();
        assert_eq!(read_archive_messages(&path).unwrap().len(), messages.len());
        assert_eq!(fs::metadata(journal_name(&path)).unwrap().len(), 0);
        store.reset(&messages, &state).unwrap();
        fs::write(journal_name(&path), stale).unwrap();
        let archived = read_archive_messages(&path).unwrap();
        assert_eq!(archived.len(), messages.len());
        assert_eq!(
            archived
                .iter()
                .filter(|message| message.content.as_deref() == Some("already checkpointed"))
                .count(),
            1,
            "a stale-generation record must not be replayed"
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "local performance benchmark; run with --ignored --nocapture"]
    fn bench_large_session_save() {
        let dir = env::temp_dir().join(format!("kroma-io-bench-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let store = SessionStore::open_at(dir.join("session.json"), &dir).unwrap();
        let mut messages = vec![Message::new("system", "benchmark")];
        for n in 0..64 {
            messages.push(Message::new(
                "assistant",
                format!("{n}:{}", "x".repeat(65_536)),
            ));
        }
        store.save(&messages, &CompactionState::default()).unwrap();
        let original_size = fs::metadata(store.archive_path()).unwrap().len();
        let started = Instant::now();
        for n in 0..12 {
            messages.push(Message::tool(format!("call-{n}"), format!("result-{n}")));
            store.save(&messages, &CompactionState::default()).unwrap();
        }
        let full_rewrite_ms = started.elapsed().as_millis();
        messages.truncate(65);
        store.save(&messages, &CompactionState::default()).unwrap();
        let started = Instant::now();
        for n in 0..12 {
            messages.push(Message::tool(format!("call-{n}"), format!("result-{n}")));
            store
                .append(&messages, &CompactionState::default())
                .unwrap();
        }
        println!(
            "session-io size={}B count=12 full_rewrite_ms={} wal_append_ms={} final_messages={}",
            original_size,
            full_rewrite_ms,
            started.elapsed().as_millis(),
            messages.len()
        );
        assert_eq!(
            store.load().unwrap().unwrap().messages.len(),
            messages.len()
        );
        drop(store);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn journal_appends_and_crash_recovery_keep_batch_results() {
        let dir = env::temp_dir().join(format!("kroma-journal-test-{}", new_generation()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        let store = SessionStore::open_at(path.clone(), &dir).unwrap();
        let mut messages = vec![Message::new("system", "base")];
        store.save(&messages, &CompactionState::default()).unwrap();
        let snapshot_bytes = fs::metadata(&path).unwrap().len();
        messages.push(Message::new("user", "do batch"));
        store
            .append(&messages, &CompactionState::default())
            .unwrap();
        messages.push(Message {
            role: "assistant".into(),
            content: None,
            reasoning_content: None,
            tool_calls: Some(
                (1..=3)
                    .map(|n| ToolCall {
                        id: format!("id-{n}"),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "shell".into(),
                            arguments: format!("{{\"command\":\"touch marker-{n}\"}}"),
                        },
                    })
                    .collect(),
            ),
            tool_call_id: None,
            provider_items: None,
        });
        store
            .append(&messages, &CompactionState::default())
            .unwrap();
        messages.push(Message::tool("id-1".into(), "done".into()));
        store
            .append(&messages, &CompactionState::default())
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), snapshot_bytes);
        assert!(fs::metadata(store.journal_path()).unwrap().len() > 0);
        drop(store);

        let store = SessionStore::open_at(path, &dir).unwrap();
        let recovered = store.load().unwrap().unwrap().messages;
        assert_eq!(recovered.len(), 6);
        assert_eq!(recovered[3].content.as_deref(), Some("done"));
        assert_eq!(recovered[4].tool_call_id.as_deref(), Some("id-2"));
        assert_eq!(recovered[5].tool_call_id.as_deref(), Some("id-3"));
        assert!(recovered[4].content.as_deref().unwrap().contains("unknown"));
        assert_eq!(fs::metadata(store.journal_path()).unwrap().len(), 0);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn truncated_journal_tail_does_not_replay_unknown_shell_command() {
        let dir = env::temp_dir().join(format!("kroma-truncated-test-{}", new_generation()));
        fs::create_dir_all(&dir).unwrap();
        let store = SessionStore::open_at(dir.join("session.json"), &dir).unwrap();
        let mut messages = vec![Message::new("system", "base")];
        store.save(&messages, &CompactionState::default()).unwrap();
        messages.push(Message::new("user", "do work"));
        store
            .append(&messages, &CompactionState::default())
            .unwrap();
        messages.push(Message {
            role: "assistant".into(),
            content: None,
            reasoning_content: None,
            tool_calls: Some(vec![ToolCall {
                id: "id-1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "shell".into(),
                    arguments: r#"{"command":"touch marker"}"#.into(),
                },
            }]),
            tool_call_id: None,
            provider_items: None,
        });
        store
            .append(&messages, &CompactionState::default())
            .unwrap();
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(store.journal_path())
            .unwrap();
        file.write_all(br#"{"generation":"unfinished""#).unwrap();
        file.sync_all().unwrap();
        drop(file);
        // Simulate recovery using the locked store; the incomplete WAL tail
        // must be discarded without executing the pending shell command.
        let recovered = store.load().unwrap().unwrap().messages;
        assert_eq!(recovered.len(), 4);
        assert_eq!(recovered[3].tool_call_id.as_deref(), Some("id-1"));
        assert!(recovered[3].content.as_deref().unwrap().contains("unknown"));
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_partial_wal_append_blocks_reuse_and_recovers_unknown_tool() {
        let dir = env::temp_dir().join(format!("kroma-fail-wal-{}", new_generation()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        let store = SessionStore::open_at(path.clone(), &dir).unwrap();
        let state = CompactionState::default();
        let mut history = vec![Message::new("system", "base")];
        store.save(&history, &state).unwrap();
        history.push(Message::new("user", "run two tools"));
        store.append(&history, &state).unwrap();
        history.push(Message {
            role: "assistant".into(),
            content: None,
            reasoning_content: None,
            tool_calls: Some(
                (1..=2)
                    .map(|id| ToolCall {
                        id: format!("tool-{id}"),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "shell".into(),
                            arguments: format!("{{\"command\":\"touch marker-{id}\"}}"),
                        },
                    })
                    .collect(),
            ),
            tool_call_id: None,
            provider_items: None,
        });
        store.append(&history, &state).unwrap();
        history.push(Message::tool("tool-1".into(), "first succeeded".into()));
        store.append(&history, &state).unwrap();
        // Simulate disk-full during the second tool result. Some JSON bytes
        // land in the WAL, but its checksum/terminating newline never do.
        store.inject_partial_append_failure_at(history.len());
        history.push(Message::tool("tool-2".into(), "second succeeded".into()));
        let error = store.append(&history, &state).unwrap_err();
        assert!(error.to_string().contains("session append failed"));
        assert!(store.ensure_writable().is_err());
        assert!(store.append(&history, &state).is_err());
        assert!(store.save(&history, &state).is_err());
        drop(store);

        let reopened = SessionStore::open_at(path, &dir).unwrap();
        let recovered = reopened.load().unwrap().unwrap().messages;
        assert_eq!(recovered.len(), 5);
        assert_eq!(recovered[3].content.as_deref(), Some("first succeeded"));
        assert_eq!(recovered[4].tool_call_id.as_deref(), Some("tool-2"));
        assert!(recovered[4].content.as_deref().unwrap().contains("unknown"));
        assert!(reopened.ensure_writable().is_ok());
        let mut continued = recovered;
        continued.push(Message::new("user", "inspect state without replay"));
        reopened.append(&continued, &state).unwrap();
        assert_eq!(reopened.load().unwrap().unwrap().messages.len(), 6);
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snapshot_io_failure_prevents_further_mutations() {
        let dir = env::temp_dir().join(format!("kroma-snapshot-failure-{}", new_generation()));
        fs::create_dir_all(&dir).unwrap();
        let store = SessionStore::open_at(dir.join("history.json"), &dir).unwrap();
        let state = CompactionState::default();
        let mut messages = vec![Message::new("system", "keep me")];
        store.save(&messages, &state).unwrap();
        // Making the snapshot path itself a directory is a deterministic real
        // I/O failure on every platform (no atomic replace can overwrite a
        // directory) and holds even when tests run as root. Removing the
        // session directory instead fails on Windows, where the store's
        // exclusive lock file keeps that directory undeletable.
        let snapshot = dir.join("history.json");
        fs::remove_file(&snapshot).unwrap();
        fs::create_dir(&snapshot).unwrap();
        assert!(store.save(&messages, &state).is_err());
        assert!(store.ensure_writable().is_err());
        messages.push(Message::new("user", "must not proceed"));
        assert!(store.append(&messages, &state).is_err());
        drop(store);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resetting_history_cannot_replay_old_journal_generation() {
        let dir = env::temp_dir().join(format!("kroma-reset-journal-{}", new_generation()));
        fs::create_dir_all(&dir).unwrap();
        let store = SessionStore::open_at(dir.join("session.json"), &dir).unwrap();
        let mut messages = vec![Message::new("system", "old")];
        store.save(&messages, &CompactionState::default()).unwrap();
        messages.push(Message::new("user", "old task"));
        store
            .append(&messages, &CompactionState::default())
            .unwrap();
        let old_journal = fs::read(store.journal_path()).unwrap();
        let reset = vec![Message::new("system", "new")];
        store.reset(&reset, &CompactionState::default()).unwrap();
        // Simulate crash between checkpoint rename and journal truncation:
        // a surviving old journal generation must be ignored.
        fs::write(store.journal_path(), old_journal).unwrap();
        drop(store);
        let store = SessionStore::open_at(dir.join("session.json"), &dir).unwrap();
        let resumed = store.load().unwrap().unwrap();
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(resumed.messages[0].content.as_deref(), Some("new"));
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saves_loads_and_clears_history() {
        let dir = env::temp_dir().join(format!("kroma-session-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let store = SessionStore::open_at(dir.join("history.json"), &dir).unwrap();
        let history = vec![
            Message::new("system", "System"),
            Message::new("user", "Hello"),
        ];
        store.save(&history, &CompactionState::default()).unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().messages[1]
                .content
                .as_deref(),
            Some("Hello")
        );
        store
            .save(&history[..1], &CompactionState::default())
            .unwrap();
        assert_eq!(store.load().unwrap().unwrap().messages.len(), 1);
        drop(store);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn interrupted_tool_call_is_marked_unknown() {
        let mut history = vec![Message::new("system", "System")];
        history.push(Message {
            role: "assistant".into(),
            content: None,
            reasoning_content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "shell".into(),
                    arguments: "{\"command\":\"echo ok\"}".into(),
                },
            }]),
            tool_call_id: None,
            provider_items: None,
        });
        assert!(recover_pending_calls(&mut history));
        assert_eq!(history[2].tool_call_id.as_deref(), Some("call_1"));
        assert!(!recover_pending_calls(&mut history));
    }

    #[test]
    fn batch_crash_preserves_completed_results_and_marks_only_unfinished() {
        let dir = env::temp_dir().join(format!("kroma-batch-recovery-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let store = SessionStore::open_at(dir.join("batch.json"), &dir).unwrap();
        let calls = (1..=3)
            .map(|n| ToolCall {
                id: format!("call_{n}"),
                kind: "function".into(),
                function: FunctionCall {
                    name: "shell".into(),
                    arguments: format!("{{\"command\":\"echo {n}\"}}"),
                },
            })
            .collect();
        let assistant = Message {
            role: "assistant".into(),
            content: None,
            reasoning_content: None,
            tool_calls: Some(calls),
            tool_call_id: None,
            provider_items: None,
        };
        let mut history = vec![
            Message::new("system", "system"),
            Message::new("user", "run three"),
            assistant,
            Message::tool("call_1".into(), "completed one".into()),
        ];
        store.save(&history, &CompactionState::default()).unwrap();
        let loaded = store.load().unwrap().unwrap();
        history = loaded.messages;
        // Loading the session already performs crash recovery and persists it.
        assert!(!recover_pending_calls(&mut history));
        assert_eq!(history.len(), 6);
        assert_eq!(history[3].content.as_deref(), Some("completed one"));
        assert_eq!(history[4].tool_call_id.as_deref(), Some("call_2"));
        assert_eq!(history[5].tool_call_id.as_deref(), Some("call_3"));
        assert!(history[4].content.as_deref().unwrap().contains("unknown"));
        assert!(!recover_pending_calls(&mut history));
        store.save(&history, &CompactionState::default()).unwrap();
        let resumed = store.load().unwrap().unwrap().messages;
        assert_eq!(resumed.len(), 6);
        assert_eq!(resumed[3].content.as_deref(), Some("completed one"));
        drop(store);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn batch_cancel_never_replays_pending_side_effects() {
        let mut history = vec![
            Message::new("system", "system"),
            Message {
                role: "assistant".into(),
                content: None,
                reasoning_content: None,
                tool_calls: Some(
                    (1..=3)
                        .map(|n| ToolCall {
                            id: format!("call_{n}"),
                            kind: "function".into(),
                            function: FunctionCall {
                                name: "shell".into(),
                                arguments: format!("{{\"command\":\"touch marker_{n}\"}}"),
                            },
                        })
                        .collect(),
                ),
                tool_call_id: None,
                provider_items: None,
            },
            Message::tool("call_1".into(), "ok".into()),
            Message::tool("call_2".into(), "ok".into()),
        ];
        assert!(recover_pending_calls(&mut history));
        assert_eq!(history[4].tool_call_id.as_deref(), Some("call_3"));
        assert!(history[4].content.as_deref().unwrap().contains("unknown"));
        assert!(!recover_pending_calls(&mut history));
    }
}
