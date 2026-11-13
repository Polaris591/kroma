//! Minimal session selection. Each conversation keeps its own snapshot, WAL,
//! lock, compression state and shell capture references; no session database.
use crate::{session::SessionStore, usage::UsageMeter};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(0);

pub struct SessionCatalog {
    default: PathBuf,
    prefix: String,
}

/// Model selection belongs to a conversation, not to the global profile.
/// Only a profile name and thinking level are saved, never credentials.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionModel {
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

/// UI-only usage ledger. Never enters provider messages or Agent context.
/// Older sessions without this sidecar legitimately have unavailable metrics.
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionUsage {
    pub coding: UsageMeter,
    pub title: UsageMeter,
    pub apec: UsageMeter,
    pub context_input: Option<u64>,
    pub context_estimated: bool,
}

pub struct SessionEntry {
    pub id: String,
    pub modified: Option<SystemTime>,
    pub name: Option<String>,
}

/// Read-only, bounded local search. Never index provider reasoning or tool
/// results, and never modify a session while another process owns it.
#[derive(Clone, Debug, Serialize)]
pub struct SessionTextMatch {
    pub id: String,
    pub snippet: String,
}

/// One hit in a conversation that the Desktop can navigate to. `message_index`
/// is the absolute index in the complete replayed history, so it stays valid
/// when the UI shows only its newest page.
#[derive(Clone, Debug, Serialize)]
pub struct SessionMessageMatch {
    pub id: String,
    /// `user` or `assistant`; the Desktop only ever searches visible text.
    pub role: String,
    pub message_index: usize,
    pub snippet: String,
}

/// Back up per-session display/model metadata beside the archived history.
/// Existing archives without this file still restore their transcripts.
#[derive(Default, Serialize, Deserialize)]
struct TrashedSessionMetadata {
    name: Option<String>,
    model: Option<SessionModel>,
    usage: Option<SessionUsage>,
    capabilities: Option<crate::capabilities::CapabilityOverrides>,
}

impl SessionCatalog {
    /// Derive a sidecar from the default snapshot path. `.json` snapshots keep
    /// their historical `main.names.json` companions. For a custom suffix such
    /// as `foo.session`, append instead of replacing the extension: replacing
    /// it made `foo.session` and `foo.other` share `foo.names.json`, leaking
    /// one default session's title into the other.
    fn sidecar(&self, suffix: &str) -> PathBuf {
        let file_name = self.default.file_name().unwrap_or_default();
        if file_name
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with(".json")
        {
            return self.default.with_extension(suffix);
        }
        let mut name = self.default.as_os_str().to_os_string();
        name.push(".");
        name.push(suffix);
        PathBuf::from(name)
    }

    fn capabilities_path(&self) -> PathBuf {
        self.sidecar("capabilities.json")
    }

    fn capabilities_map(
        &self,
    ) -> Result<std::collections::BTreeMap<String, crate::capabilities::CapabilityOverrides>> {
        crate::capabilities::read_or_default(&self.capabilities_path())
    }

    pub fn capability_overrides(
        &self,
        id: &str,
    ) -> Result<crate::capabilities::CapabilityOverrides> {
        if id == "unknown" {
            return Ok(Default::default());
        }
        self.path_for(id)?;
        Ok(self
            .capabilities_map()?
            .get(id)
            .cloned()
            .unwrap_or_default())
    }

    pub fn patch_capability_items(
        &self,
        id: &str,
        workdir: &Path,
        group: crate::capabilities::CapabilityGroup,
        items: &std::collections::BTreeMap<String, Option<bool>>,
    ) -> Result<()> {
        self.resolve(id)?;
        crate::catalog_file::update(&self.default, &self.capabilities_path(), || {
            let mut map = self.capabilities_map()?;
            let settings = map.entry(id.into()).or_default();
            settings.expand_legacy(workdir)?;
            settings.items.patch(group, items, false)?;
            if settings == &Default::default() {
                map.remove(id);
            }
            Ok(map)
        })
    }

    pub fn set_capability_overrides(
        &self,
        id: &str,
        settings: crate::capabilities::CapabilityOverrides,
    ) -> Result<()> {
        self.resolve(id)?;
        crate::catalog_file::update(&self.default, &self.capabilities_path(), || {
            let mut map = self.capabilities_map()?;
            if settings == Default::default() {
                map.remove(id);
            } else {
                map.insert(id.into(), settings);
            }
            Ok(map)
        })
    }

    fn last_path(&self) -> PathBuf {
        self.sidecar("last.json")
    }

    /// Explicit session navigation is more precise than filesystem mtimes,
    /// which can tie when sessions are created close together.
    pub fn remember_active(&self, id: &str) -> Result<()> {
        self.resolve(id)?;
        let path = self.last_path();
        crate::catalog_file::update(&self.default, &path, || Ok(id))
    }

    pub fn recent(&self) -> Result<Option<String>> {
        if let Ok(raw) = fs::read(self.last_path()) {
            if let Ok(id) = serde_json::from_slice::<String>(&raw) {
                if self.resolve(&id).is_ok() {
                    return Ok(Some(id));
                }
            }
        }
        Ok(self.list()?.into_iter().next().map(|entry| entry.id))
    }

    fn usage_path(&self) -> PathBuf {
        self.sidecar("usage.json")
    }

    fn usage_map(&self) -> Result<std::collections::BTreeMap<String, SessionUsage>> {
        match fs::read(self.usage_path()) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(map) => Ok(map),
                Err(error) => {
                    // Usage is display-only; a damaged sidecar cannot make an
                    // otherwise healthy WAL/session unusable.
                    crate::runtime_log!("[session] ignoring invalid usage sidecar: {error}");
                    Ok(Default::default())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn usage(&self, id: &str) -> Result<SessionUsage> {
        self.resolve(id)?;
        Ok(self.usage_map()?.remove(id).unwrap_or_default())
    }

    /// Apply observed usage to the newest ledger in one catalog transaction.
    /// Reading the whole map and writing it back separately would let a
    /// concurrent writer erase an update made between those two steps.
    pub fn add_usage(
        &self,
        id: &str,
        coding: &[crate::model::Usage],
        compaction: &[(usize, crate::model::Usage)],
    ) -> Result<SessionUsage> {
        self.resolve(id)?;
        crate::catalog_file::update_and_return(&self.default, &self.usage_path(), || {
            let mut entries = self.usage_map()?;
            let mut entry = entries.remove(id).unwrap_or_default();
            for measure in coding {
                entry.coding.record(Some(measure));
            }
            for (requests, measure) in compaction {
                entry.apec.record_batch(*requests, Some(measure));
            }
            entries.insert(id.to_owned(), entry.clone());
            Ok((entries, entry))
        })
    }
    pub fn set_usage(&self, id: &str, usage: SessionUsage) -> Result<()> {
        self.resolve(id)?;
        crate::catalog_file::update(&self.default, &self.usage_path(), || {
            let mut entries = self.usage_map()?;
            entries.insert(id.to_owned(), usage);
            Ok(entries)
        })
    }

    /// Title requests may complete after the user switches sessions. Record
    /// their usage under the originating session without changing UI totals.
    pub fn add_title_usage(
        &self,
        id: &str,
        requests: usize,
        usage: crate::model::Usage,
    ) -> Result<()> {
        self.resolve(id)?;
        crate::catalog_file::update(&self.default, &self.usage_path(), || {
            let mut entries = self.usage_map()?;
            entries
                .entry(id.to_owned())
                .or_default()
                .title
                .record_batch(requests, Some(&usage));
            Ok(entries)
        })
    }

    fn models_path(&self) -> PathBuf {
        self.sidecar("models.json")
    }

    fn models(&self) -> Result<std::collections::BTreeMap<String, SessionModel>> {
        match fs::read(self.models_path()) {
            Ok(raw) => match serde_json::from_slice(&raw) {
                Ok(map) => Ok(map),
                Err(error) => {
                    crate::runtime_log!("[session] ignoring invalid model sidecar: {error}");
                    Ok(Default::default())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn model_choice(&self, id: &str) -> Result<Option<SessionModel>> {
        self.resolve(id)?;
        Ok(self.models()?.remove(id))
    }

    pub fn set_model_choice(&self, id: &str, choice: SessionModel) -> Result<()> {
        self.resolve(id)?;
        if choice.profile.trim().is_empty() {
            bail!("session model profile is empty");
        }
        crate::catalog_file::update(&self.default, &self.models_path(), || {
            let mut models = self.models()?;
            models.insert(id.into(), choice);
            Ok(models)
        })
    }

    pub fn copy_model_choice(&self, from: &str, to: &str) -> Result<()> {
        if let Some(choice) = self.model_choice(from)? {
            self.set_model_choice(to, choice)?;
        }
        self.set_capability_overrides(to, self.capability_overrides(from)?)?;
        Ok(())
    }
    fn names_path(&self) -> PathBuf {
        self.sidecar("names.json")
    }
    fn names(&self) -> Result<std::collections::BTreeMap<String, String>> {
        match fs::read(self.names_path()) {
            Ok(raw) => match serde_json::from_slice(&raw) {
                Ok(map) => Ok(map),
                Err(error) => {
                    crate::runtime_log!("[session] ignoring invalid names sidecar: {error}");
                    Ok(Default::default())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(error) => Err(error.into()),
        }
    }
    pub fn name_session(&self, id: &str, title: &str) -> Result<()> {
        if title.trim().is_empty() || title.chars().count() > 80 {
            bail!("session name must be 1–80 characters");
        }
        self.resolve(id)?;
        crate::catalog_file::update(&self.default, &self.names_path(), || {
            let mut names = self.names()?;
            names.insert(id.to_owned(), title.trim().to_owned());
            Ok(names)
        })
    }

    /// Background AI naming must never replace a title set by the user.
    /// The check and insertion are atomic across Kroma processes.
    pub fn name_session_if_unnamed(&self, id: &str, title: &str) -> Result<bool> {
        if title.trim().is_empty() || title.chars().count() > 80 {
            bail!("session name must be 1–80 characters");
        }
        self.resolve(id)?;
        let mut changed = false;
        crate::catalog_file::update(&self.default, &self.names_path(), || {
            let mut names = self.names()?;
            if !names.contains_key(id) {
                names.insert(id.to_owned(), title.trim().to_owned());
                changed = true;
            }
            Ok(names)
        })?;
        Ok(changed)
    }

    pub fn has_name(&self, id: &str) -> Result<bool> {
        Ok(self.names()?.contains_key(id))
    }

    pub fn title_matches(&self, text: &str) -> Result<bool> {
        Ok(self.names()?.values().any(|name| name == text))
    }

    pub fn resolve_name(&self, text: &str) -> Result<String> {
        if text == "default" || self.resolve(text).is_ok() {
            return Ok(text.to_owned());
        }
        let names = self.names()?;
        let matches = names
            .iter()
            .filter(|(_, name)| name.as_str() == text)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            bail!("unknown or ambiguous session name; use /sessions");
        }
        Ok(matches[0].clone())
    }
    pub fn new(workdir: &Path) -> Result<Self> {
        Self::from_default(SessionStore::default_path(workdir)?)
    }

    pub(crate) fn from_default(default: PathBuf) -> Result<Self> {
        let filename = default
            .file_name()
            .context("session filename is missing")?
            .to_string_lossy();
        let prefix = filename
            .strip_suffix(".json")
            .unwrap_or(&filename)
            .to_owned();
        if prefix.is_empty() {
            bail!("session filename cannot be empty");
        }
        Ok(Self {
            default,
            prefix: format!("{prefix}--"),
        })
    }

    pub fn default_path(&self) -> &Path {
        &self.default
    }

    pub fn id_for_path<'a>(&self, path: &'a Path) -> Option<&'a str> {
        if path == self.default {
            return Some("default");
        }
        let filename = path.file_name()?.to_str()?;
        let id = filename.strip_prefix(&self.prefix)?.strip_suffix(".json")?;
        if valid_id(id) {
            Some(id)
        } else {
            None
        }
    }

    fn named_path(&self, id: &str) -> Result<PathBuf> {
        if !valid_id(id) || id == "default" {
            bail!("invalid session ID; use /sessions to see available IDs");
        }
        Ok(self
            .default
            .with_file_name(format!("{}{}.json", self.prefix, id)))
    }

    pub fn path_for(&self, id: &str) -> Result<PathBuf> {
        if id == "default" {
            return Ok(self.default.clone());
        }
        self.named_path(id)
    }

    pub fn resolve(&self, id: &str) -> Result<PathBuf> {
        let path = if id == "default" {
            self.default.clone()
        } else {
            self.named_path(id)?
        };
        // A missing target must never be silently created by /resume.
        if !path.is_file() {
            bail!("session '{id}' does not exist; use /sessions to see saved conversations");
        }
        Ok(path)
    }

    /// Move a saved conversation and its durable journal out of the catalog.
    /// Keep a recoverable local copy instead of permanently destroying a
    /// user's history. Acquire the same OS lock as an Agent first, so another
    /// running Kroma process cannot have this session open.
    pub fn trash_session(&self, id: &str) -> Result<()> {
        let path = self.resolve(id)?;
        // The caller must have released any live Agent lock before deleting.
        let _lock = SessionStore::open_at(path.clone(), Path::new("."))?;
        let parent = path.parent().context("session directory is missing")?;
        let trash_dir = parent.join(".deleted");
        fs::create_dir_all(&trash_dir)?;
        let original_name = path
            .file_name()
            .context("missing session file name")?
            .to_string_lossy();
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let destination = trash_dir.join(format!("{stamp}-{original_name}"));
        let metadata_path = destination.with_extension("meta.json");
        let metadata = TrashedSessionMetadata {
            name: self.names()?.get(id).cloned(),
            model: self.models()?.get(id).cloned(),
            usage: self.usage_map()?.get(id).cloned(),
            capabilities: self.capabilities_map()?.get(id).cloned(),
        };
        fs::write(&metadata_path, serde_json::to_vec(&metadata)?)
            .context("saving recoverable session metadata")?;
        // Moving the snapshot first makes the session disappear from lists.
        if let Err(error) = fs::rename(&path, &destination) {
            let _ = fs::remove_file(&metadata_path);
            return Err(error).with_context(|| format!("could not trash session {id}"));
        }
        let journal = crate::session::journal_path(&path);
        if journal.exists() {
            let dest_journal = crate::session::journal_path(&destination);
            if let Err(error) = fs::rename(&journal, &dest_journal) {
                // Restore the checkpoint so an incomplete deletion cannot
                // strand an existing WAL without its matching snapshot.
                let _ = fs::rename(&destination, &path);
                let _ = fs::remove_file(&metadata_path);
                return Err(error).context("could not move session journal");
            }
        }
        // The durable archive is now outside the active catalog. Remove
        // obsolete metadata too, but never report a false deletion failure
        // if an optional display sidecar cannot be rewritten.
        let cleanup = (|| -> Result<()> {
            crate::catalog_file::update(&self.default, &self.names_path(), || {
                let mut names = self.names()?;
                names.remove(id);
                Ok(names)
            })?;
            crate::catalog_file::update(&self.default, &self.models_path(), || {
                let mut models = self.models()?;
                models.remove(id);
                Ok(models)
            })?;
            crate::catalog_file::update(&self.default, &self.usage_path(), || {
                let mut usage = self.usage_map()?;
                usage.remove(id);
                Ok(usage)
            })?;
            crate::catalog_file::update(&self.default, &self.capabilities_path(), || {
                let mut capabilities = self.capabilities_map()?;
                capabilities.remove(id);
                Ok(capabilities)
            })
        })();
        if let Err(error) = cleanup {
            crate::runtime_log!("[session] unable to clean trashed session metadata: {error:#}");
        }
        Ok(())
    }

    /// Undo a Desktop deletion by moving the most recent archived snapshot
    /// and its journal back to their original names. Refuse to overwrite any
    /// live session or journal; earlier archives remain available on disk.
    pub fn restore_session(&self, id: &str) -> Result<()> {
        let path = self.path_for(id)?;
        let _lock = crate::session::acquire_session_lock(&path)?;
        let journal = crate::session::journal_path(&path);
        if path.exists() {
            bail!("cannot restore: a session with this ID already exists");
        }
        // A crash between moving the snapshot and WAL during deletion may
        // leave only the WAL at its original path. Restore the archived
        // snapshot and pair it with that journal rather than strand history.
        let interrupted_trash = journal.exists();
        let filename = path
            .file_name()
            .context("missing session filename")?
            .to_string_lossy();
        let suffix = format!("-{filename}");
        let trash = path
            .parent()
            .context("missing session directory")?
            .join(".deleted");
        let archived = fs::read_dir(&trash)?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let prefix = name.strip_suffix(&suffix)?;
                let stamp = prefix.parse::<u128>().ok()?;
                entry
                    .file_type()
                    .ok()?
                    .is_file()
                    .then_some((stamp, entry.path()))
            })
            .max_by_key(|(stamp, _)| *stamp)
            .map(|(_, path)| path)
            .context("no recoverable archive for this session")?;
        let archived_journal = crate::session::journal_path(&archived);
        if interrupted_trash && archived_journal.exists() {
            bail!("cannot restore: both the live and archived session journals exist");
        }
        let metadata_path = archived.with_extension("meta.json");
        let metadata = fs::read(&metadata_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<TrashedSessionMetadata>(&bytes).ok());
        fs::rename(&archived, &path).context("restoring session snapshot")?;
        if archived_journal.exists() {
            if let Err(error) = fs::rename(&archived_journal, &journal) {
                let _ = fs::rename(&path, &archived);
                return Err(error).context("restoring session journal");
            }
        }
        if let Some(metadata) = metadata {
            let restore_metadata = (|| -> Result<()> {
                if let Some(name) = metadata.name {
                    self.name_session(id, &name)?;
                }
                if let Some(model) = metadata.model {
                    self.set_model_choice(id, model)?;
                }
                if let Some(usage) = metadata.usage {
                    self.set_usage(id, usage)?;
                }
                if let Some(capabilities) = metadata.capabilities {
                    self.set_capability_overrides(id, capabilities)?;
                }
                Ok(())
            })();
            if let Err(error) = restore_metadata {
                crate::runtime_log!(
                    "[session] recovered history but metadata needs review: {error:#}"
                );
            } else {
                let _ = fs::remove_file(metadata_path);
            }
        }
        Ok(())
    }

    pub fn new_path(&self) -> Result<(String, PathBuf)> {
        for _ in 0..10 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock is before Unix epoch")?
                .as_nanos();
            let serial = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
            let id = format!("s{nanos:x}-{serial:x}-{:x}", std::process::id());
            let path = self.named_path(&id)?;
            if !path.exists() && !crate::session::journal_path(&path).exists() {
                return Ok((id, path));
            }
        }
        bail!("unable to allocate a unique session ID")
    }

    /// Look for a phrase in user-authored messages of the most recent
    /// conversations. Snapshot + WAL replay is read-only. Very large archives
    /// are skipped so typing in the Desktop search box stays responsive.
    pub fn search_user_messages(
        &self,
        query: &str,
        max_sessions: usize,
        max_hits: usize,
    ) -> Result<Vec<SessionTextMatch>> {
        Ok(self
            .search_messages(query, max_sessions, max_hits, SearchScope::UserOnly)?
            .into_iter()
            .map(|hit| SessionTextMatch {
                id: hit.id,
                snippet: hit.snippet,
            })
            .collect())
    }

    /// The same bounded scan, also covering assistant answers that the
    /// transcript renders. Tool output, system prompts and reasoning are never
    /// searched. `search_user_messages` stays as the compatible user-only form.
    pub fn search_conversation_messages(
        &self,
        query: &str,
        max_sessions: usize,
        max_hits: usize,
    ) -> Result<Vec<SessionMessageMatch>> {
        self.search_messages(query, max_sessions, max_hits, SearchScope::Conversation)
    }

    fn search_messages(
        &self,
        query: &str,
        max_sessions: usize,
        max_hits: usize,
        scope: SearchScope,
    ) -> Result<Vec<SessionMessageMatch>> {
        const MAX_ARCHIVE_BYTES: u64 = 12 * 1024 * 1024;
        let term = query.trim().to_lowercase();
        if term.chars().count() < 2 || term.chars().count() > 120 || max_hits == 0 {
            return Ok(Vec::new());
        }
        let mut matches = Vec::new();
        for entry in self.list()?.into_iter().take(max_sessions.min(40)) {
            let path = self.path_for(&entry.id)?;
            let journal = crate::session::journal_path(&path);
            let size = fs::metadata(&path)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
                + fs::metadata(&journal)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
            if size > MAX_ARCHIVE_BYTES {
                continue;
            }
            let messages = match crate::session::read_archive_messages(&path) {
                Ok(messages) => messages,
                Err(error) => {
                    crate::runtime_log!("[session-search] ignoring unreadable archive: {error:#}");
                    continue;
                }
            };
            for (index, message) in messages.iter().enumerate().rev() {
                if !scope.matches(message) {
                    continue;
                }
                let Some(content) = message.content.as_deref() else {
                    continue;
                };
                if content.len() > 512_000 {
                    continue;
                }
                let lower = content.to_lowercase();
                let Some(position) = lower.find(&term) else {
                    continue;
                };
                // Count Unicode scalar values rather than byte offsets so
                // Chinese project/task names can be centered in the preview.
                let start = lower[..position].chars().count().saturating_sub(32);
                let snippet = content
                    .chars()
                    .skip(start)
                    .take(145)
                    .filter(|character| !('\u{e000}'..='\u{f8ff}').contains(character))
                    .collect::<String>()
                    .replace(['\r', '\n'], " ");
                matches.push(SessionMessageMatch {
                    id: entry.id.clone(),
                    role: message.role.clone(),
                    message_index: index,
                    snippet: format!(
                        "{}{}{}",
                        if start > 0 { "…" } else { "" },
                        snippet,
                        if content.chars().count() > start + 145 {
                            "…"
                        } else {
                            ""
                        }
                    ),
                });
                break;
            }
            if matches.len() >= max_hits.min(30) {
                break;
            }
        }
        Ok(matches)
    }

    pub fn list(&self) -> Result<Vec<SessionEntry>> {
        let mut entries = Vec::new();
        let names = self.names()?;
        let Some(parent) = self.default.parent() else {
            bail!("session directory is missing");
        };
        if !parent.exists() {
            return Ok(entries);
        }
        for entry in fs::read_dir(parent).context("failed to list saved sessions")? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            if let Some(id) = self.id_for_path(&path) {
                // The JSON checkpoint can be old while the WAL contains the
                // latest turns; order by the most recent durable write.
                let wal = crate::session::journal_path(&path);
                let modified = entry
                    .metadata()?
                    .modified()
                    .ok()
                    .into_iter()
                    .chain(fs::metadata(wal).and_then(|meta| meta.modified()).ok())
                    .max();
                entries.push(SessionEntry {
                    id: id.to_owned(),
                    modified,
                    name: names.get(id).cloned(),
                });
            }
        }
        entries.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
        Ok(entries)
    }
}

/// Which visible transcript text a local search may read.
#[derive(Clone, Copy)]
enum SearchScope {
    UserOnly,
    Conversation,
}

impl SearchScope {
    fn matches(self, message: &crate::protocol::Message) -> bool {
        if crate::protocol::is_actual_user_message(message) {
            return true;
        }
        if !matches!(self, Self::Conversation) || message.role != "assistant" {
            return false;
        }
        // These assistant messages are not shown as answer text; matching
        // them would report a hit the transcript never renders.
        !message
            .provider_items
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("kroma_context_reference" | "kroma_continuation")
                )
            })
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_patches_migrate_legacy_reset_and_survive_session_lifecycle() -> Result<()> {
        use crate::capabilities::{Capabilities, CapabilityGroup, CapabilityOverrides};
        use std::collections::BTreeMap;
        let root = std::env::temp_dir().join(format!(
            "kroma-item-sessions-{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let path = root.join("main.json");
        let catalog = SessionCatalog::from_default(path.clone())?;
        let store = SessionStore::open_at(path.clone(), &root)?;
        store.save(
            &[crate::protocol::Message::new("system", "test")],
            &crate::context_state::CompactionState::default(),
        )?;
        drop(store);
        catalog.set_capability_overrides(
            "default",
            CapabilityOverrides {
                tools: Some(false),
                ..Default::default()
            },
        )?;
        catalog.patch_capability_items(
            "default",
            &root,
            CapabilityGroup::Tools,
            &BTreeMap::from([("shell".into(), None)]),
        )?;
        let migrated = catalog.capability_overrides("default")?;
        assert_eq!(migrated.tools, None);
        assert!(!migrated.items.tools.contains_key("shell"));
        assert_eq!(migrated.items.tools.get("history_read"), Some(&false));
        let defaults = Capabilities::default();
        assert!(migrated.effective(defaults.clone()).enabled(
            CapabilityGroup::Tools,
            "shell",
            true
        ));
        assert!(!migrated.effective(defaults.clone()).enabled(
            CapabilityGroup::Tools,
            "history_read",
            true
        ));
        catalog.patch_capability_items(
            "default",
            &root,
            CapabilityGroup::Skills,
            &BTreeMap::from([("one".into(), Some(false)), ("two".into(), Some(true))]),
        )?;
        catalog.patch_capability_items(
            "default",
            &root,
            CapabilityGroup::Mcp,
            &BTreeMap::from([("server".into(), Some(false))]),
        )?;
        catalog.patch_capability_items(
            "default",
            &root,
            CapabilityGroup::Plugins,
            &BTreeMap::from([("plugin".into(), Some(true))]),
        )?;
        let settings = catalog.capability_overrides("default")?;
        assert_eq!(settings.items.tools, migrated.items.tools);
        assert_eq!(settings.items.skills.get("two"), Some(&true));
        let before = fs::read(catalog.capabilities_path())?;
        assert!(catalog
            .patch_capability_items(
                "default",
                &root,
                CapabilityGroup::Tools,
                &BTreeMap::from([("".into(), Some(true))])
            )
            .is_err());
        assert_eq!(fs::read(catalog.capabilities_path())?, before);
        let reopened = SessionCatalog::from_default(path)?;
        assert_eq!(reopened.capability_overrides("default")?, settings);
        let (branch, branch_path) = reopened.new_path()?;
        let store = SessionStore::open_at(branch_path, &root)?;
        store.save(
            &[crate::protocol::Message::new("system", "test")],
            &crate::context_state::CompactionState::default(),
        )?;
        drop(store);
        reopened.copy_model_choice("default", &branch)?;
        assert_eq!(reopened.capability_overrides(&branch)?, settings);
        reopened.trash_session(&branch)?;
        assert_eq!(
            reopened.capability_overrides(&branch)?,
            CapabilityOverrides::default()
        );
        reopened.restore_session(&branch)?;
        assert_eq!(reopened.capability_overrides(&branch)?, settings);
        reopened.patch_capability_items(
            &branch,
            &root,
            CapabilityGroup::Plugins,
            &BTreeMap::from([("plugin".into(), None)]),
        )?;
        assert!(!reopened
            .capability_overrides(&branch)?
            .items
            .plugins
            .contains_key("plugin"));
        assert_eq!(reopened.capability_overrides("default")?, settings);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn capability_overrides_survive_reopen_fork_trash_and_restore() -> Result<()> {
        use crate::capabilities::{Capabilities, CapabilityOverrides};
        let root = std::env::temp_dir().join(format!(
            "kroma-session-capabilities-{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let path = root.join("main.json");
        let catalog = SessionCatalog::from_default(path.clone())?;
        let store = SessionStore::open_at(path.clone(), &root)?;
        store.save(
            &[crate::protocol::Message::new("system", "test")],
            &crate::context_state::CompactionState::default(),
        )?;
        drop(store);
        assert_eq!(
            catalog.capability_overrides("default")?,
            CapabilityOverrides::default()
        );
        let settings = CapabilityOverrides {
            tools: Some(false),
            mcp: Some(true),
            skills: None,
            plugins: Some(false),
            ..Default::default()
        };
        catalog.set_capability_overrides("default", settings.clone())?;
        let reopened = SessionCatalog::from_default(path)?;
        assert_eq!(reopened.capability_overrides("default")?, settings);
        let defaults = Capabilities {
            skills: false,
            ..Capabilities::default()
        };
        assert_eq!(
            reopened
                .capability_overrides("default")?
                .effective(defaults.clone()),
            Capabilities {
                tools: false,
                mcp: true,
                skills: false,
                plugins: false,
                ..Default::default()
            }
        );
        let (branch, branch_path) = reopened.new_path()?;
        let store = SessionStore::open_at(branch_path, &root)?;
        store.save(
            &[crate::protocol::Message::new("system", "test")],
            &crate::context_state::CompactionState::default(),
        )?;
        drop(store);
        reopened.copy_model_choice("default", &branch)?;
        assert_eq!(reopened.capability_overrides(&branch)?, settings);
        reopened.trash_session(&branch)?;
        assert_eq!(
            reopened.capability_overrides(&branch)?,
            CapabilityOverrides::default()
        );
        reopened.restore_session(&branch)?;
        assert_eq!(reopened.capability_overrides(&branch)?, settings);
        reopened.set_capability_overrides("default", CapabilityOverrides::default())?;
        assert_eq!(
            reopened
                .capability_overrides("default")?
                .effective(defaults.clone()),
            defaults
        );
        assert!(reopened
            .set_capability_overrides("unknown", settings)
            .is_err());
        assert!(reopened.capability_overrides("../../bad").is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn search_only_user_messages_in_recent_archives() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-session-search-{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let catalog = SessionCatalog::from_default(root.join("main.json"))?;
        let (id, path) = catalog.new_path()?;
        let store = SessionStore::open_at(path, &root)?;
        store.save(
            &[
                crate::protocol::Message::new("system", "instructions"),
                crate::protocol::Message::new("user", "请帮我部署 Kroma Desktop"),
                crate::protocol::Message::new("tool", "PRIVATE-TOOL-OUTPUT"),
                crate::protocol::Message::new("assistant", "已经完成"),
            ],
            &crate::context_state::CompactionState::default(),
        )?;
        drop(store);
        let hits = catalog.search_user_messages("desktop", 5, 10)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        assert!(hits[0].snippet.contains("Desktop"));
        assert!(catalog
            .search_user_messages("PRIVATE-TOOL-OUTPUT", 5, 10)?
            .is_empty());
        assert!(catalog.search_user_messages("deploy", 5, 10)?.is_empty());
        assert!(catalog.search_user_messages("", 5, 10)?.is_empty());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn conversation_search_covers_answers_unicode_and_absolute_indexes() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-session-answer-search-{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let catalog = SessionCatalog::from_default(root.join("main.json"))?;
        let (id, path) = catalog.new_path()?;
        let mut messages = vec![
            crate::protocol::Message::new("system", "instructions"),
            crate::protocol::Message::new("user", "请帮我检查 Kroma 的搜索"),
            crate::protocol::Message::new("assistant", "已经完成 Search 功能"),
            crate::protocol::Message::new("tool", "PRIVATE-TOOL-OUTPUT"),
            crate::protocol::Message {
                provider_items: Some(vec![serde_json::json!({"type":"kroma_continuation"})]),
                reasoning_content: Some("hidden reasoning".into()),
                ..crate::protocol::Message::new("assistant", "hidden continuation")
            },
        ];
        for round in 0..20 {
            messages.push(crate::protocol::Message::new(
                "user",
                format!("继续第 {round} 步"),
            ));
            messages.push(crate::protocol::Message::new(
                "assistant",
                format!("完成第 {round} 步"),
            ));
        }
        messages.push(crate::protocol::Message::new("assistant", "最终回答"));
        let last_index = messages.len() - 1;
        // Only the first entries are in the snapshot; the answer lives in WAL.
        let store = SessionStore::open_at(path, &root)?;
        store.save(
            &messages[..3],
            &crate::context_state::CompactionState::default(),
        )?;
        let mut live = messages[..3].to_vec();
        for message in &messages[3..] {
            live.push(message.clone());
            store.append(&live, &crate::context_state::CompactionState::default())?;
        }
        drop(store);
        let hits = catalog.search_conversation_messages("search", 5, 10)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        assert_eq!(hits[0].role, "assistant");
        assert_eq!(hits[0].message_index, 2);
        assert!(hits[0].snippet.contains("Search"));
        // Unicode text is found and previewed, not only ASCII.
        let unicode = catalog.search_conversation_messages("搜索", 5, 10)?;
        assert_eq!(unicode.len(), 1);
        assert_eq!(unicode[0].role, "user");
        assert!(unicode[0].snippet.contains("搜索"));
        // A WAL-only answer keeps its absolute index.
        let latest = catalog.search_conversation_messages("最终", 5, 10)?;
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].message_index, last_index);
        assert_eq!(latest[0].role, "assistant");
        // Tool output, reasoning and continuation text are never searched.
        assert!(catalog
            .search_conversation_messages("PRIVATE-TOOL-OUTPUT", 5, 10)?
            .is_empty());
        assert!(catalog
            .search_conversation_messages("hidden", 5, 10)?
            .is_empty());
        // The compatible user-only search keeps its old semantics.
        let user_only = catalog.search_user_messages("完成第", 5, 10)?;
        assert!(user_only.is_empty());
        assert_eq!(catalog.search_user_messages("kroma", 5, 10)?.len(), 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn trash_saved_session_is_recoverable_and_refuses_live_locks() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-trash-session-{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let catalog = SessionCatalog::from_default(root.join("main.json"))?;
        let (id, path) = catalog.new_path()?;
        let store = SessionStore::open_at(path.clone(), &root)?;
        store.save(
            &[crate::protocol::Message::new("system", "instructions")],
            &crate::context_state::CompactionState::default(),
        )?;
        catalog.name_session(&id, "To be deleted")?;
        catalog.set_model_choice(
            &id,
            SessionModel {
                profile: "test-provider".into(),
                thinking: Some("high".into()),
            },
        )?;
        catalog.set_usage(
            &id,
            SessionUsage {
                context_input: Some(1234),
                ..Default::default()
            },
        )?;
        assert_eq!(catalog.list()?.len(), 1);
        assert!(
            catalog.trash_session(&id).is_err(),
            "a live lock must prevent deletion"
        );
        assert!(path.exists());
        drop(store);
        catalog.trash_session(&id)?;
        assert!(catalog.list()?.is_empty());
        assert!(!catalog.names()?.contains_key(&id));
        assert!(!path.exists());
        let trash = root.join(".deleted");
        let archived = fs::read_dir(trash)?
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert!(archived
            .iter()
            .any(|item| item.extension().is_some_and(|ext| ext == "json")));
        catalog.restore_session(&id)?;
        assert!(path.exists(), "undo must restore the original session ID");
        assert_eq!(catalog.list()?.len(), 1);
        assert_eq!(catalog.list()?[0].name.as_deref(), Some("To be deleted"));
        assert_eq!(catalog.model_choice(&id)?.unwrap().profile, "test-provider");
        assert_eq!(catalog.usage(&id)?.context_input, Some(1234));
        assert!(
            catalog.restore_session(&id).is_err(),
            "never overwrite a live session"
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn restore_recovers_snapshot_moved_before_its_wal_on_crash() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-interrupted-trash-{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let catalog = SessionCatalog::from_default(root.join("main.json"))?;
        let (id, path) = catalog.new_path()?;
        let store = SessionStore::open_at(path.clone(), &root)?;
        let context = crate::context_state::CompactionState::default();
        let mut messages = vec![crate::protocol::Message::new("system", "instructions")];
        store.save(&messages, &context)?;
        messages.push(crate::protocol::Message::new("user", "persisted in WAL"));
        store.append(&messages, &context)?;
        drop(store);
        let journal = crate::session::journal_path(&path);
        assert!(journal.exists());
        let trash = root.join(".deleted");
        fs::create_dir_all(&trash)?;
        let destination = trash.join(format!("1-{}", path.file_name().unwrap().to_string_lossy()));
        fs::rename(&path, &destination)?;
        assert!(!path.exists());
        assert!(journal.exists());
        catalog.restore_session(&id)?;
        assert!(path.exists());
        assert!(journal.exists());
        let reopened = SessionStore::open_at(path.clone(), &root)?;
        let recovered = reopened.load()?.expect("snapshot and WAL paired");
        assert_eq!(recovered.messages.len(), 2);
        assert_eq!(
            recovered.messages[1].content.as_deref(),
            Some("persisted in WAL")
        );
        drop(reopened);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn corrupt_display_sidecars_do_not_block_transcript_or_new_metrics() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-corrupt-sidecars-{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let catalog = SessionCatalog::from_default(root.join("main.json"))?;
        fs::write(catalog.default_path(), "{}")?;
        fs::write(catalog.names_path(), "not JSON")?;
        fs::write(catalog.usage_path(), "not JSON")?;
        fs::write(catalog.models_path(), "not JSON")?;
        assert_eq!(catalog.list()?.len(), 1);
        assert!(catalog.list()?[0].name.is_none());
        assert_eq!(catalog.usage("default")?.coding.calls, 0);
        assert!(catalog.model_choice("default")?.is_none());
        catalog.name_session("default", "Recovered")?;
        assert_eq!(catalog.list()?[0].name.as_deref(), Some("Recovered"));
        let saved = catalog.add_usage(
            "default",
            &[crate::model::Usage {
                input: Some(14),
                ..Default::default()
            }],
            &[],
        )?;
        assert_eq!(saved.coding.calls, 1);
        assert_eq!(saved.coding.input, 14);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn ai_title_can_only_fill_an_unnamed_session() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-auto-title-{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let catalog = SessionCatalog::from_default(root.join("main.json"))?;
        let (id, file) = catalog.new_path()?;
        fs::write(file, "{}")?;
        assert!(catalog.name_session_if_unnamed(&id, "AI title")?);
        assert!(!catalog.name_session_if_unnamed(&id, "AI overwrite")?);
        catalog.name_session(&id, "Manual title")?;
        assert!(!catalog.name_session_if_unnamed(&id, "AI late result")?);
        assert_eq!(
            catalog.list()?.into_iter().next().unwrap().name.as_deref(),
            Some("Manual title")
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn model_selection_is_per_session_and_copied_to_branches() -> Result<()> {
        let root = std::env::temp_dir().join(format!("kroma-model-choices-{}", std::process::id()));
        fs::create_dir_all(&root)?;
        let catalog = SessionCatalog::from_default(root.join("main.json"))?;
        fs::write(catalog.default_path(), "{}")?;
        let (branch, file) = catalog.new_path()?;
        fs::write(file, "{}")?;
        let choice = SessionModel {
            profile: "example".into(),
            thinking: Some("high".into()),
        };
        catalog.set_model_choice("default", choice.clone())?;
        catalog.copy_model_choice("default", &branch)?;
        assert_eq!(catalog.model_choice(&branch)?, Some(choice));
        catalog.set_model_choice(
            "default",
            SessionModel {
                profile: "other".into(),
                thinking: None,
            },
        )?;
        assert_eq!(catalog.model_choice(&branch)?.unwrap().profile, "example");
        catalog.remember_active(&branch)?;
        assert_eq!(catalog.recent()?.as_deref(), Some(branch.as_str()));
        catalog.remember_active("default")?;
        assert_eq!(catalog.recent()?.as_deref(), Some("default"));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn non_json_default_uses_distinct_sidecars_and_appended_wal() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-custom-session-{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let first = SessionCatalog::from_default(root.join("foo.session"))?;
        let second = SessionCatalog::from_default(root.join("foo.other"))?;
        assert_ne!(first.names_path(), second.names_path());
        assert_eq!(first.names_path(), root.join("foo.session.names.json"));
        assert_eq!(
            crate::session::journal_path(first.default_path()),
            root.join("foo.session.wal")
        );
        fs::write(first.default_path(), b"{}")?;
        first.name_session("default", "FIRST SESSION TITLE")?;
        assert!(second.names()?.is_empty());

        // A newer WAL must update list ordering even when the snapshot name
        // does not end in `.json`.
        let catalog = SessionCatalog::from_default(root.join("main.json"))?;
        assert_eq!(
            catalog.names_path(),
            root.join("main.names.json"),
            ".json snapshots keep their existing sidecar names"
        );
        fs::write(catalog.default_path(), b"{}")?;
        let wal = crate::session::journal_path(catalog.default_path());
        fs::write(&wal, b"journal")?;
        let old = UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let new = UNIX_EPOCH + std::time::Duration::from_secs(2_000);
        fs::File::options()
            .write(true)
            .open(catalog.default_path())?
            .set_times(std::fs::FileTimes::new().set_modified(old))?;
        fs::File::options()
            .write(true)
            .open(&wal)?
            .set_times(std::fs::FileTimes::new().set_modified(new))?;
        let entry = catalog
            .list()?
            .into_iter()
            .find(|entry| entry.id == "default")
            .expect("default session");
        assert_eq!(entry.modified, Some(new));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn concurrent_model_updates_do_not_lose_or_fail_writes() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-catalog-concurrency-{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let catalog = std::sync::Arc::new(SessionCatalog::from_default(root.join("main.json"))?);
        fs::write(catalog.default_path(), b"{}")?;
        let ids = (0..8)
            .map(|_| {
                let (id, path) = catalog.new_path()?;
                fs::write(path, b"{}")?;
                Ok(id)
            })
            .collect::<Result<Vec<_>>>()?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(ids.len()));
        let workers = ids
            .into_iter()
            .map(|id| {
                let catalog = catalog.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || -> Result<()> {
                    barrier.wait();
                    for round in 0..8 {
                        catalog.set_model_choice(
                            &id,
                            SessionModel {
                                profile: format!("{id}-{round}"),
                                thinking: None,
                            },
                        )?;
                    }
                    Ok(())
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().expect("catalog writer panicked")?;
        }
        let models = catalog.models()?;
        assert_eq!(models.len(), 8);
        assert!(models.values().all(|choice| choice.profile.ends_with("-7")));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn catalog_ids_are_scoped_and_cannot_escape_directory() {
        let root = std::env::temp_dir().join(format!(
            "kroma-catalog-{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let catalog = SessionCatalog::from_default(root.join("project.json")).unwrap();
        assert_eq!(catalog.id_for_path(catalog.default_path()), Some("default"));
        assert!(catalog.resolve("../../etc/passwd").is_err());
        assert!(catalog.resolve("unknown").is_err());
        let fresh = "fresh-name";
        assert!(catalog.resolve_name(fresh).is_err());
        assert!(!catalog.title_matches(fresh).unwrap());
        assert!(catalog.path_for(fresh).is_ok());
        let (id, path) = catalog.new_path().unwrap();
        assert_eq!(catalog.id_for_path(&path), Some(id.as_str()));
        assert!(catalog.resolve(&id).is_err());
        fs::write(&path, "test").unwrap();
        fs::write(root.join("unrelated.json"), "test").unwrap();
        assert_eq!(catalog.resolve(&id).unwrap(), path);
        assert_eq!(catalog.list().unwrap().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_names_are_metadata_and_resolve_only_unique_matches() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-session-names-{}",
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let catalog = SessionCatalog::from_default(root.join("session.json"))?;
        fs::write(catalog.default_path(), b"{}")?;
        let (id, path) = catalog.new_path()?;
        fs::write(path, b"{}")?;
        catalog.name_session("default", "Agent loop")?;
        catalog.name_session(&id, "压缩优化")?;
        assert_eq!(catalog.resolve_name("压缩优化")?, id);
        assert_eq!(catalog.resolve_name("Agent loop")?, "default");
        assert!(catalog
            .list()?
            .iter()
            .any(|session| session.name.as_deref() == Some("压缩优化")));
        catalog.name_session(&id, "Agent loop")?;
        assert!(catalog.resolve_name("Agent loop").is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
