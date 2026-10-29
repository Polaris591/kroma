//! Minimal session selection. Each conversation keeps its own snapshot, WAL,
//! lock, compression state and shell capture references; no session database.
use crate::session::SessionStore;
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

pub struct SessionEntry {
    pub id: String,
    pub modified: Option<SystemTime>,
    pub name: Option<String>,
}

impl SessionCatalog {
    fn last_path(&self) -> PathBuf {
        self.default.with_extension("last.json")
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

    fn models_path(&self) -> PathBuf {
        self.default.with_extension("models.json")
    }

    fn models(&self) -> Result<std::collections::BTreeMap<String, SessionModel>> {
        match fs::read(self.models_path()) {
            Ok(raw) => serde_json::from_slice(&raw).context("invalid session model choices"),
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
        Ok(())
    }
    fn names_path(&self) -> PathBuf {
        self.default.with_extension("names.json")
    }
    fn names(&self) -> Result<std::collections::BTreeMap<String, String>> {
        match fs::read(self.names_path()) {
            Ok(raw) => Ok(serde_json::from_slice(&raw)?),
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

    pub fn new_path(&self) -> Result<(String, PathBuf)> {
        for _ in 0..10 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock is before Unix epoch")?
                .as_nanos();
            let serial = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
            let id = format!("s{nanos:x}-{serial:x}-{:x}", std::process::id());
            let path = self.named_path(&id)?;
            if !path.exists() && !path.with_extension("json.wal").exists() {
                return Ok((id, path));
            }
        }
        bail!("unable to allocate a unique session ID")
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
                let wal = path.with_extension("json.wal");
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
