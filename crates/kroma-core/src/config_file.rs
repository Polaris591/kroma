//! Kroma v1.3.0 — persistent, layered configuration.
//!
//! Two files provide durable settings so `kroma` runs with zero environment variables:
//!
//! * the **user global** file at `~/.kroma/config.toml` (Windows: `%USERPROFILE%\.kroma\config.toml`),
//!   written atomically with `0600` permissions on Unix, and
//! * the **workspace** file at `<workspace>/.kroma.toml`.
//!
//! ## Trust boundary
//!
//! A workspace file lives inside an untrusted repository, so it is restricted to an explicit
//! allow-list of benign defaults ([`WORKSPACE_ALLOWED_KEYS`]). Credentials and security switches
//! ([`SECURITY_SENSITIVE_KEYS`]) are only ever honored from the user global file, `KROMA_*`
//! environment variables, or explicit CLI flags. Every key a workspace file tried to set outside
//! the allow-list is dropped and reported in [`ResolvedConfig::rejected_workspace_keys`], so the
//! guard is observable rather than silent.
//!
//! ## Precedence (highest first)
//!
//! `CLI flags` > `KROMA_* environment variables` > `workspace .kroma.toml` >
//! `selected profile in the user global file` > `top-level defaults in the user global file`.
//!
//! ## Format
//!
//! The parser implements the TOML subset this schema needs (comments, `key = "string"`,
//! `key = 123`, `key = true`, `[table]` and `[table.sub]` headers) so Kroma keeps zero extra
//! dependencies, matching the hand-rolled base64 reader in [`crate::tools::read`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Hard cap on the size of a workspace `.kroma.toml`.
///
/// The file lives inside an untrusted repository, so a symlink to an oversized file must not be
/// able to exhaust memory while the process starts. 64 KiB is far more than the allow-listed key
/// set can ever need.
pub const MAX_WORKSPACE_CONFIG_BYTES: u64 = 64 * 1024;

/// Top-level keys a workspace `.kroma.toml` is allowed to contribute. Everything else — including
/// credentials, endpoint overrides, verification commands, hooks and policy switches — is ignored.
pub const WORKSPACE_ALLOWED_KEYS: &[&str] = &[
    "model",
    "protocol",
    "reasoning_effort",
    "max_subagents",
    "projection_profile",
    "adaptive_tournament",
];

/// Keys that change security posture or carry credentials. A workspace file may never set these;
/// they are only trusted from the user global file, `KROMA_*` environment variables, or CLI flags.
pub const SECURITY_SENSITIVE_KEYS: &[&str] = &[
    "api_key",
    "base_url",
    "verify_command",
    "policy",
    "policy_hook",
    "observer_hook",
    "allow_write",
    "allow_shell",
];

/// Environment variable name overrides, in the same order as the config fields.
const ENV_MODEL: &str = "KROMA_MODEL";
const ENV_BASE_URL: &str = "KROMA_BASE_URL";
const ENV_API_KEY: &str = "KROMA_API_KEY";
const ENV_PROTOCOL: &str = "KROMA_PROTOCOL";
const ENV_REASONING_EFFORT: &str = "KROMA_REASONING_EFFORT";
const ENV_VERIFY_COMMAND: &str = "KROMA_VERIFY_COMMAND";
const ENV_MAX_SUBAGENTS: &str = "KROMA_MAX_SUBAGENTS";
const ENV_PROFILE: &str = "KROMA_PROFILE";
/// B2 (`v1.4.0`): context projection profile (`baseline`/`p1`/`p2`/`p3`).
const ENV_PROJECTION_PROFILE: &str = "KROMA_PROJECTION_PROFILE";
/// Item #3 (`v1.4.0`): adaptive tournament escalation candidate count.
const ENV_ADAPTIVE_TOURNAMENT: &str = "KROMA_ADAPTIVE_TOURNAMENT";
/// Conventional OpenAI-compatible variables accepted as fallbacks for the endpoint/credential.
/// They must be collected by [`collect_env`] or [`ResolvedConfig::load`] would never see them.
const ENV_OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";
const ENV_OPENAI_API_KEY: &str = "OPENAI_API_KEY";
const ENV_ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";

/// Placeholder shown instead of a real credential by [`ResolvedConfig::redacted_summary`].
pub const REDACTED_API_KEY: &str = "sk-***";

/// One `[profiles.<name>]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigProfile {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub protocol: Option<String>,
    pub reasoning_effort: Option<String>,
}

impl ConfigProfile {
    fn is_empty(&self) -> bool {
        self.api_key.is_none()
            && self.base_url.is_none()
            && self.model.is_none()
            && self.protocol.is_none()
            && self.reasoning_effort.is_none()
    }
}

/// A single parsed configuration file. Missing optional fields stay `None` so layer merging can
/// distinguish "unset" from "explicitly set".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KromaConfigFile {
    pub default_profile: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub protocol: Option<String>,
    pub reasoning_effort: Option<String>,
    pub policy: Option<String>,
    pub verify_command: Option<String>,
    pub max_subagents: Option<usize>,
    /// B2 (`v1.4.0`): context projection profile (`baseline`/`p1`/`p2`/`p3`). A safe key that a
    /// workspace `.kroma.toml` may set.
    pub projection_profile: Option<String>,
    /// Item #3 (`v1.4.0`): number of candidates to race when the single agent stalls (`0` = off).
    pub adaptive_tournament: Option<usize>,
    pub profiles: BTreeMap<String, ConfigProfile>,
    /// Every top-level key name present in the file (typed or not), sorted and deduplicated.
    pub raw_keys: Vec<String>,
    /// Every table header present (for example `profiles` and `profiles.gemini`).
    pub raw_sections: Vec<String>,
}

impl KromaConfigFile {
    /// Path of the user global configuration file, or `None` when no home directory is known.
    pub fn user_config_path() -> Option<PathBuf> {
        user_config_dir().map(|dir| dir.join("config.toml"))
    }

    /// Path of the workspace configuration file for `workspace_root`.
    pub fn workspace_config_path(workspace_root: &Path) -> PathBuf {
        workspace_root.join(".kroma.toml")
    }

    /// Parse the supported TOML subset. Unsupported syntax is an error; unknown keys are retained
    /// in [`Self::raw_keys`] so the trust guard can report them.
    pub fn parse_str(text: &str) -> Result<Self, String> {
        let entries = parse_toml_subset(text)?;
        let mut file = KromaConfigFile::default();
        let mut keys: BTreeSet<String> = BTreeSet::new();
        let mut sections: BTreeSet<String> = BTreeSet::new();

        for (table, key, value) in entries {
            if table.is_empty() {
                keys.insert(key.clone());
                match key.as_str() {
                    "default_profile" => file.default_profile = Some(value.as_string()?),
                    "model" => file.model = Some(value.as_string()?),
                    "base_url" => file.base_url = Some(value.as_string()?),
                    "api_key" => file.api_key = Some(value.as_string()?),
                    "protocol" => file.protocol = Some(value.as_string()?),
                    "reasoning_effort" => file.reasoning_effort = Some(value.as_string()?),
                    "policy" => file.policy = Some(value.as_string()?),
                    "verify_command" => file.verify_command = Some(value.as_string()?),
                    "max_subagents" => {
                        file.max_subagents = Some(value.as_positive_usize(&key)?);
                    }
                    "adaptive_tournament" => {
                        file.adaptive_tournament = Some(value.as_positive_usize(&key)?);
                    }
                    "projection_profile" => {
                        let raw = value.as_string()?;
                        // Validate eagerly so a typo never silently changes behaviour.
                        crate::context_projector::ProjectionProfile::parse(&raw)
                            .map_err(|e| format!("{}: {}", key, e))?;
                        file.projection_profile = Some(raw);
                    }
                    // Unknown top-level keys are ignored but remembered for the trust guard.
                    _ => {}
                }
                continue;
            }

            if table.len() == 2 && table[0] == "profiles" {
                let name = table[1].clone();
                sections.insert(table.join("."));
                let profile = file.profiles.entry(name).or_default();
                match key.as_str() {
                    "api_key" => profile.api_key = Some(value.as_string()?),
                    "base_url" => profile.base_url = Some(value.as_string()?),
                    "model" => profile.model = Some(value.as_string()?),
                    "protocol" => profile.protocol = Some(value.as_string()?),
                    "reasoning_effort" => profile.reasoning_effort = Some(value.as_string()?),
                    _ => {}
                }
                continue;
            }

            // Any other table (including `[profiles]` itself) is remembered but contributes nothing.
            sections.insert(table.join("."));
        }

        file.profiles.retain(|_, profile| !profile.is_empty());
        file.raw_keys = keys.into_iter().collect();
        file.raw_sections = sections.into_iter().collect();
        Ok(file)
    }

    /// Load a configuration file. A missing file yields the empty default; an unreadable or
    /// malformed file is an error so a typo never silently changes behaviour.
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse_str(&text)
                .map_err(|e| format!("invalid configuration file {}: {}", path.display(), e)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(format!(
                "cannot read configuration file {}: {}",
                path.display(),
                err
            )),
        }
    }

    /// Load the user global configuration file, if one exists.
    pub fn load_user_global() -> Result<Self, String> {
        match Self::user_config_path() {
            Some(path) => Self::load(&path),
            None => Ok(Self::default()),
        }
    }

    /// Load the workspace configuration file, if one exists.
    ///
    /// Unlike the trusted user global file, `<workspace>/.kroma.toml` comes from an untrusted
    /// repository, so it is read defensively: a missing file yields the empty default, but a
    /// symlink, a non-regular file (a FIFO would hang startup; a device could block forever), a
    /// file larger than [`MAX_WORKSPACE_CONFIG_BYTES`], or non-UTF-8 content is an explicit error
    /// rather than a silent ignore, so a hostile repository is visible instead of quietly changing
    /// behaviour. The workspace config is deliberately *not* added to [`crate::sensitive_paths`]:
    /// the agent must still be able to edit its own project configuration.
    pub fn load_workspace(workspace_root: &Path) -> Result<Self, String> {
        let path = Self::workspace_config_path(workspace_root);
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(format!(
                    "cannot read configuration file {}: {}",
                    path.display(),
                    err
                ))
            }
        };
        if meta.file_type().is_symlink() {
            return Err(format!(
                "refusing to load configuration file {}: it is a symbolic link (a workspace config must be a regular file)",
                path.display()
            ));
        }
        if !meta.file_type().is_file() {
            return Err(format!(
                "refusing to load configuration file {}: it is not a regular file",
                path.display()
            ));
        }
        if meta.len() > MAX_WORKSPACE_CONFIG_BYTES {
            return Err(workspace_config_too_large(&path, meta.len()));
        }

        // Bound the read as well: the file could grow between the `stat` above and the read, and a
        // regular file can be arbitrarily large on disk.
        let mut file = fs::File::open(&path)
            .map_err(|err| format!("cannot read configuration file {}: {}", path.display(), err))?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_WORKSPACE_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| format!("cannot read configuration file {}: {}", path.display(), err))?;
        if bytes.len() as u64 > MAX_WORKSPACE_CONFIG_BYTES {
            return Err(workspace_config_too_large(&path, bytes.len() as u64));
        }
        let text = String::from_utf8(bytes).map_err(|_| {
            format!(
                "cannot read configuration file {}: content is not valid UTF-8",
                path.display()
            )
        })?;
        Self::parse_str(&text)
            .map_err(|e| format!("invalid configuration file {}: {}", path.display(), e))
    }

    /// Serialize back to the supported TOML subset.
    pub fn to_toml_string(&self) -> String {
        let mut out = String::from("# Kroma configuration — generated by `kroma /config set`.\n");
        out.push_str(
            "# Credentials in this file are trusted; workspace `.kroma.toml` files are not.\n\n",
        );
        if let Some(v) = &self.default_profile {
            out.push_str(&format!("default_profile = {}\n", toml_quote(v)));
        }
        for (label, value) in [
            ("model", self.model.as_ref()),
            ("base_url", self.base_url.as_ref()),
            ("api_key", self.api_key.as_ref()),
            ("protocol", self.protocol.as_ref()),
            ("reasoning_effort", self.reasoning_effort.as_ref()),
            ("policy", self.policy.as_ref()),
            ("verify_command", self.verify_command.as_ref()),
            ("projection_profile", self.projection_profile.as_ref()),
        ] {
            if let Some(v) = value {
                out.push_str(&format!("{} = {}\n", label, toml_quote(v)));
            }
        }
        if let Some(n) = self.max_subagents {
            out.push_str(&format!("max_subagents = {}\n", n));
        }
        if let Some(n) = self.adaptive_tournament {
            out.push_str(&format!("adaptive_tournament = {}\n", n));
        }
        for (name, profile) in &self.profiles {
            out.push_str(&format!("\n[profiles.{}]\n", name));
            for (label, value) in [
                ("base_url", profile.base_url.as_ref()),
                ("api_key", profile.api_key.as_ref()),
                ("model", profile.model.as_ref()),
                ("protocol", profile.protocol.as_ref()),
                ("reasoning_effort", profile.reasoning_effort.as_ref()),
            ] {
                if let Some(v) = value {
                    out.push_str(&format!("{} = {}\n", label, toml_quote(v)));
                }
            }
        }
        out
    }

    /// Atomically write this configuration to the user global path with `0600` permissions on Unix.
    pub fn save_user_global(&self) -> Result<PathBuf, String> {
        let path = Self::user_config_path().ok_or_else(|| {
            "cannot determine the user home directory for ~/.kroma/config.toml".to_string()
        })?;
        let dir = path
            .parent()
            .ok_or_else(|| format!("invalid configuration path {}", path.display()))?;
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
        restrict_dir_permissions(dir)?;
        write_file_atomically(&path, self.to_toml_string().as_bytes())?;
        Ok(path)
    }

    /// Read-modify-write a single top-level key in the user global file.
    ///
    /// `key` accepts the same names as the file format (`model`, `base_url`, `api_key`, `protocol`,
    /// `reasoning_effort`, `policy`, `verify_command`, `default_profile`, `max_subagents`,
    /// `projection_profile`, `adaptive_tournament`).
    pub fn set_user_global_key(key: &str, value: &str) -> Result<PathBuf, String> {
        let mut file = Self::load_user_global()?;
        let key = key.trim();
        if key.is_empty() {
            return Err("missing configuration key".to_string());
        }
        match key {
            "default_profile" => file.default_profile = Some(value.to_string()),
            "model" => file.model = Some(value.to_string()),
            "base_url" => file.base_url = Some(value.to_string()),
            "api_key" => file.api_key = Some(value.to_string()),
            "protocol" => file.protocol = Some(value.to_string()),
            "reasoning_effort" => file.reasoning_effort = Some(value.to_string()),
            "policy" => file.policy = Some(value.to_string()),
            "verify_command" => file.verify_command = Some(value.to_string()),
            "max_subagents" => {
                let parsed = value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| format!("'{}' is not a valid max_subagents value", value))?;
                file.max_subagents = Some(parsed);
            }
            "projection_profile" => file.projection_profile = Some(value.to_string()),
            "adaptive_tournament" => {
                let parsed = value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| format!("'{}' is not a valid adaptive_tournament value", value))?;
                file.adaptive_tournament = Some(parsed);
            }
            other => {
                return Err(format!(
                    "unknown configuration key '{}'; expected one of: default_profile, model, base_url, api_key, protocol, reasoning_effort, policy, verify_command, max_subagents, projection_profile, adaptive_tournament",
                    other
                ));
            }
        }
        file.save_user_global()
    }
}

/// Explicit CLI overrides. These sit above every file and environment layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOverrides {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub protocol: Option<String>,
    pub reasoning_effort: Option<String>,
    pub verify_command: Option<String>,
    pub max_subagents: Option<usize>,
    /// B2 (`v1.4.0`): context projection profile override.
    pub projection_profile: Option<String>,
    /// Item #3 (`v1.4.0`): adaptive tournament escalation candidate count.
    pub adaptive_tournament: Option<usize>,
}

/// Fully merged configuration plus the provenance needed to explain it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedConfig {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub protocol: Option<String>,
    pub reasoning_effort: Option<String>,
    pub policy: Option<String>,
    pub verify_command: Option<String>,
    pub max_subagents: Option<usize>,
    /// B2 (`v1.4.0`): resolved context projection profile.
    pub projection_profile: Option<String>,
    /// Item #3 (`v1.4.0`): resolved adaptive tournament escalation candidate count.
    pub adaptive_tournament: Option<usize>,
    pub active_profile: Option<String>,
    /// Keys a workspace `.kroma.toml` tried to set outside [`WORKSPACE_ALLOWED_KEYS`].
    pub rejected_workspace_keys: Vec<String>,
    pub user_config_path: Option<PathBuf>,
    pub workspace_config_path: Option<PathBuf>,
}

impl ResolvedConfig {
    /// Merge every layer for `workspace_root` using the real environment and the supplied CLI
    /// overrides.
    pub fn load(workspace_root: &Path, overrides: &ConfigOverrides) -> Result<Self, String> {
        let user = KromaConfigFile::load_user_global()?;
        let workspace = KromaConfigFile::load_workspace(workspace_root)?;
        let env = collect_env();
        Ok(Self::resolve(
            &user,
            &workspace,
            &env,
            overrides,
            KromaConfigFile::user_config_path(),
            KromaConfigFile::workspace_config_path(workspace_root),
        ))
    }

    /// Pure merge, used by tests and by [`Self::load`].
    pub fn resolve(
        user: &KromaConfigFile,
        workspace: &KromaConfigFile,
        env: &BTreeMap<String, String>,
        overrides: &ConfigOverrides,
        user_config_path: Option<PathBuf>,
        workspace_config_path: PathBuf,
    ) -> Self {
        // Trust guard: only the allow-list survives from the untrusted workspace file.
        let mut rejected_workspace_keys: Vec<String> = workspace
            .raw_keys
            .iter()
            .filter(|k| !WORKSPACE_ALLOWED_KEYS.contains(&k.as_str()))
            .cloned()
            .collect();
        // `[profiles.*]` tables are a credential channel, so a workspace file may never declare
        // them either; report the section (not its individual keys) as the rejected entry.
        if workspace
            .raw_sections
            .iter()
            .any(|section| section == "profiles" || section.starts_with("profiles."))
        {
            rejected_workspace_keys.push("profiles".to_string());
        }
        rejected_workspace_keys.sort();
        rejected_workspace_keys.dedup();

        let active_profile = overrides
            .profile
            .clone()
            .or_else(|| env_get(env, ENV_PROFILE))
            .or_else(|| user.default_profile.clone());
        let profile = active_profile
            .as_ref()
            .and_then(|name| user.profiles.get(name));

        let env_string = |name: &str| env_get(env, name);

        Self {
            model: overrides
                .model
                .clone()
                .or_else(|| env_string(ENV_MODEL))
                .or_else(|| workspace.model.clone())
                .or_else(|| profile.and_then(|p| p.model.clone()))
                .or_else(|| user.model.clone()),
            base_url: overrides
                .base_url
                .clone()
                .or_else(|| env_string(ENV_BASE_URL))
                .or_else(|| env_string(ENV_OPENAI_BASE_URL))
                .or_else(|| profile.and_then(|p| p.base_url.clone()))
                .or_else(|| user.base_url.clone()),
            api_key: overrides
                .api_key
                .clone()
                .or_else(|| env_string(ENV_API_KEY))
                .or_else(|| env_string(ENV_OPENAI_API_KEY))
                .or_else(|| env_string(ENV_ANTHROPIC_API_KEY))
                .or_else(|| profile.and_then(|p| p.api_key.clone()))
                .or_else(|| user.api_key.clone()),
            protocol: overrides
                .protocol
                .clone()
                .or_else(|| env_string(ENV_PROTOCOL))
                .or_else(|| workspace.protocol.clone())
                .or_else(|| profile.and_then(|p| p.protocol.clone()))
                .or_else(|| user.protocol.clone()),
            reasoning_effort: overrides
                .reasoning_effort
                .clone()
                .or_else(|| env_string(ENV_REASONING_EFFORT))
                .or_else(|| workspace.reasoning_effort.clone())
                .or_else(|| profile.and_then(|p| p.reasoning_effort.clone()))
                .or_else(|| user.reasoning_effort.clone()),
            // `policy`, `verify_command` and `max_subagents` are security-relevant: the workspace
            // file never contributes them.
            policy: overrides_absent(&user.policy, env_string("KROMA_POLICY")),
            verify_command: overrides
                .verify_command
                .clone()
                .or_else(|| env_string(ENV_VERIFY_COMMAND))
                .or_else(|| user.verify_command.clone()),
            max_subagents: overrides
                .max_subagents
                .or_else(|| env_string(ENV_MAX_SUBAGENTS).and_then(|v| v.parse::<usize>().ok()))
                .or_else(|| workspace.max_subagents)
                .or_else(|| user.max_subagents),
            // B2: a safe, non-security key — the workspace file may set it.
            projection_profile: overrides
                .projection_profile
                .clone()
                .or_else(|| env_string(ENV_PROJECTION_PROFILE))
                .or_else(|| workspace.projection_profile.clone())
                .or_else(|| user.projection_profile.clone()),
            // Item #3: a safe, non-security key — the workspace file may set it.
            adaptive_tournament: overrides
                .adaptive_tournament
                .or_else(|| {
                    env_string(ENV_ADAPTIVE_TOURNAMENT).and_then(|v| v.parse::<usize>().ok())
                })
                .or_else(|| workspace.adaptive_tournament)
                .or_else(|| user.adaptive_tournament),
            active_profile,
            rejected_workspace_keys,
            user_config_path,
            workspace_config_path: Some(workspace_config_path),
        }
    }

    /// True when a workspace file tried to set a key outside the allow-list.
    pub fn workspace_guard_triggered(&self) -> bool {
        !self.rejected_workspace_keys.is_empty()
    }

    /// Human-readable view for `/config show`, with the credential redacted.
    pub fn redacted_summary(&self) -> String {
        let show = |v: &Option<String>| v.clone().unwrap_or_else(|| "<unset>".to_string());
        let mut out = String::new();
        out.push_str(&format!(
            "profile:          {}\n",
            show(&self.active_profile)
        ));
        out.push_str(&format!("model:            {}\n", show(&self.model)));
        out.push_str(&format!("base_url:         {}\n", show(&self.base_url)));
        out.push_str(&format!(
            "api_key:          {}\n",
            if self.api_key.is_some() {
                REDACTED_API_KEY
            } else {
                "<unset>"
            }
        ));
        out.push_str(&format!("protocol:         {}\n", show(&self.protocol)));
        out.push_str(&format!(
            "reasoning_effort: {}\n",
            show(&self.reasoning_effort)
        ));
        out.push_str(&format!("policy:           {}\n", show(&self.policy)));
        out.push_str(&format!(
            "verify_command:   {}\n",
            show(&self.verify_command)
        ));
        out.push_str(&format!(
            "max_subagents:    {}\n",
            self.max_subagents
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<unset>".to_string())
        ));
        out.push_str(&format!(
            "user config:      {}\n",
            self.user_config_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<none>".to_string())
        ));
        out.push_str(&format!(
            "workspace config: {}\n",
            self.workspace_config_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<none>".to_string())
        ));
        if self.workspace_guard_triggered() {
            out.push_str(&format!(
                "ignored (untrusted workspace keys): {}\n",
                self.rejected_workspace_keys.join(", ")
            ));
        }
        out
    }
}

fn overrides_absent(user_value: &Option<String>, env_value: Option<String>) -> Option<String> {
    env_value.or_else(|| user_value.clone())
}

fn workspace_config_too_large(path: &Path, len: u64) -> String {
    format!(
        "refusing to load configuration file {}: it is {} bytes, exceeding the {} byte workspace config limit",
        path.display(),
        len,
        MAX_WORKSPACE_CONFIG_BYTES
    )
}

fn env_get(env: &BTreeMap<String, String>, name: &str) -> Option<String> {
    env.get(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Snapshot the `KROMA_*` environment variables that participate in configuration.
pub fn collect_env() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for name in [
        ENV_MODEL,
        ENV_BASE_URL,
        ENV_API_KEY,
        ENV_PROTOCOL,
        ENV_REASONING_EFFORT,
        ENV_VERIFY_COMMAND,
        ENV_MAX_SUBAGENTS,
        ENV_PROFILE,
        "KROMA_POLICY",
        ENV_PROJECTION_PROFILE,
        ENV_ADAPTIVE_TOURNAMENT,
        ENV_OPENAI_BASE_URL,
        ENV_OPENAI_API_KEY,
        ENV_ANTHROPIC_API_KEY,
    ] {
        if let Ok(value) = std::env::var(name) {
            out.insert(name.to_string(), value);
        }
    }
    out
}

/// `~/.kroma` (or `%USERPROFILE%\.kroma`).
///
/// Resolution order: `KROMA_HOME` > `HOME` > `USERPROFILE` > the OS account database (Unix). The
/// last fallback matters because services and daemons frequently run without `HOME` set.
pub fn user_config_dir() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("KROMA_HOME") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(home_dir_from_account_database)?;
    Some(PathBuf::from(home).join(".kroma"))
}

/// Unix-only fallback that reads the home directory from the account database (`getpwuid`).
#[cfg(unix)]
fn home_dir_from_account_database() -> Option<String> {
    // SAFETY: `getpwuid` returns a pointer into a static buffer owned by libc; it is only read
    // immediately, and `geteuid` cannot fail.
    unsafe {
        let uid = libc::geteuid();
        let pw = libc::getpwuid(uid);
        if pw.is_null() {
            return None;
        }
        let dir = (*pw).pw_dir;
        if dir.is_null() {
            return None;
        }
        let cstr = std::ffi::CStr::from_ptr(dir);
        let value = cstr.to_string_lossy().trim().to_string();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }
}

#[cfg(not(unix))]
fn home_dir_from_account_database() -> Option<String> {
    None
}

fn write_file_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    write_file_atomically_with_suffix(path, bytes, &uuid::Uuid::new_v4().to_string())
}

/// Atomic write with an explicit temp-file suffix.
///
/// The suffix is random in production so a same-user attacker cannot predict the temp name and
/// pre-plant a symlink on it. `File::create` (`O_CREAT | O_TRUNC`, no `O_NOFOLLOW`) would have
/// *followed* such a symlink and truncated the link target before the later permission check could
/// refuse it; [`create_new_config_file_no_symlink`] instead creates the temp file exclusively with
/// `O_NOFOLLOW`, so an existing entry (symlink or otherwise) makes the open fail with `EEXIST`/
/// `ELOOP` and nothing is truncated. The suffix is a parameter so the hostile-entry path can be
/// exercised deterministically in tests.
fn write_file_atomically_with_suffix(
    path: &Path,
    bytes: &[u8],
    suffix: &str,
) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let base = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");

    let (tmp, mut file) = {
        let mut created: Option<(PathBuf, fs::File)> = None;
        for attempt in 0..16u32 {
            // First attempt honours the caller's suffix; a collision (a leftover temp file from a
            // crashed write) is retried with a fresh random suffix so it cannot wedge the save.
            let attempt_suffix = if attempt == 0 {
                suffix.to_string()
            } else {
                uuid::Uuid::new_v4().to_string()
            };
            let candidate = dir.join(format!(
                ".{}.tmp-{}-{}",
                base,
                std::process::id(),
                attempt_suffix
            ));
            match create_new_config_file_no_symlink(&candidate) {
                Ok(file) => {
                    created = Some((candidate, file));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // A regular leftover is a stale temp file we may skip; anything else (symlink,
                    // directory, special file) is not ours to clobber, so refuse loudly rather than
                    // silently retrying past an attacker's plant.
                    match fs::symlink_metadata(&candidate) {
                        Ok(meta) if meta.file_type().is_file() => continue,
                        _ => {
                            return Err(format!(
                                "refusing to write through the existing entry at {}: {}",
                                candidate.display(),
                                e
                            ))
                        }
                    }
                }
                Err(e) => return Err(format!("cannot create {}: {}", candidate.display(), e)),
            }
        }
        created.ok_or_else(|| {
            format!(
                "cannot create a temporary file next to {}: too many name collisions",
                path.display()
            )
        })?
    };

    let write_result = (|| -> Result<(), String> {
        file.write_all(bytes)
            .map_err(|e| format!("cannot write {}: {}", tmp.display(), e))?;
        file.sync_all()
            .map_err(|e| format!("cannot flush {}: {}", tmp.display(), e))?;
        Ok(())
    })();
    drop(file);
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot replace {}: {}", path.display(), e)
    })?;
    // The temp file was created with mode 0600, so the rename already carries that mode; the
    // explicit pass is kept because `mode` is masked by the process umask and is a no-op on Windows.
    restrict_file_permissions(path)?;
    Ok(())
}
/// Create a brand-new config file exclusively and without following a symlink, at mode `0600` on
/// Unix.
///
/// Thin wrapper over [`crate::secure_fs::create_new_no_symlink`] so the hardening policy lives in
/// one place: `create_new(true)` refuses a pre-planted symlink with `EEXIST` rather than following
/// it, and `O_NOFOLLOW` closes the swap race.
fn create_new_config_file_no_symlink(path: &Path) -> std::io::Result<fs::File> {
    crate::secure_fs::create_new_no_symlink(path, 0o600)
}

/// Restrict a state file to `0600` without ever `chmod`-ing it by path.
#[cfg(unix)]
fn restrict_file_permissions(path: &Path) -> Result<(), String> {
    crate::secure_fs::restrict_file_no_follow(path, 0o600)
        .map_err(|e| format!("cannot restrict permissions on {}: {}", path.display(), e))
}

#[cfg(not(unix))]
fn restrict_file_permissions(path: &Path) -> Result<(), String> {
    crate::secure_fs::restrict_file_no_follow(path, 0o600)
        .map_err(|e| format!("cannot restrict permissions on {}: {}", path.display(), e))
}

/// Directory counterpart of [`restrict_file_permissions`]: open with `O_NOFOLLOW | O_DIRECTORY` and
/// `fchmod`, so a symlinked final component is refused rather than traversed.
#[cfg(unix)]
fn restrict_dir_permissions(path: &Path) -> Result<(), String> {
    crate::secure_fs::restrict_dir_no_follow(path, 0o700)
        .map_err(|e| format!("cannot restrict permissions on {}: {}", path.display(), e))
}

#[cfg(not(unix))]
fn restrict_dir_permissions(path: &Path) -> Result<(), String> {
    crate::secure_fs::restrict_dir_no_follow(path, 0o700)
        .map_err(|e| format!("cannot restrict permissions on {}: {}", path.display(), e))
}

// ---------------------------------------------------------------------------------------------
// Minimal TOML subset parser
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum TomlValue {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl TomlValue {
    fn as_string(&self) -> Result<String, String> {
        match self {
            TomlValue::Str(s) => Ok(s.clone()),
            TomlValue::Int(n) => Ok(n.to_string()),
            TomlValue::Bool(b) => Ok(b.to_string()),
        }
    }

    fn as_positive_usize(&self, key: &str) -> Result<usize, String> {
        match self {
            TomlValue::Int(n) if *n >= 0 => Ok(*n as usize),
            TomlValue::Int(n) => Err(format!("key '{}' must not be negative (got {})", key, n)),
            _ => Err(format!("key '{}' must be a non-negative integer", key)),
        }
    }
}

type TomlEntry = (Vec<String>, String, TomlValue);

fn parse_toml_subset(text: &str) -> Result<Vec<TomlEntry>, String> {
    let mut entries = Vec::new();
    let mut table: Vec<String> = Vec::new();
    for (index, raw_line) in text.lines().enumerate() {
        let lineno = index + 1;
        let line = strip_comment(raw_line);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            if rest.starts_with('[') {
                return Err(format!("line {}: array-of-tables is not supported", lineno));
            }
            let inner = rest
                .strip_suffix(']')
                .ok_or_else(|| format!("line {}: unterminated table header", lineno))?;
            table = split_dotted_key(inner.trim(), lineno)?;
            if table.is_empty() {
                return Err(format!("line {}: empty table header", lineno));
            }
            continue;
        }
        let (key_part, value_part) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: expected `key = value`", lineno))?;
        let key = parse_key(key_part.trim(), lineno)?;
        let value = parse_value(value_part.trim(), lineno)?;
        entries.push((table.clone(), key, value));
    }
    Ok(entries)
}

/// Strip a `#` comment, honoring `#` inside double-quoted strings.
fn strip_comment(line: &str) -> String {
    let mut out = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in line.chars() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }
        if ch == '#' {
            break;
        }
        out.push(ch);
    }
    out
}

fn split_dotted_key(raw: &str, lineno: usize) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    for part in raw.split('.') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!(
                "line {}: empty key segment in table header",
                lineno
            ));
        }
        parts.push(unquote_key(part, lineno)?);
    }
    Ok(parts)
}

fn parse_key(raw: &str, lineno: usize) -> Result<String, String> {
    if raw.is_empty() {
        return Err(format!("line {}: empty key", lineno));
    }
    unquote_key(raw, lineno)
}

fn unquote_key(raw: &str, lineno: usize) -> Result<String, String> {
    if raw.starts_with('"') {
        match parse_quoted(raw, lineno)? {
            (s, rest) if rest.is_empty() => Ok(s),
            _ => Err(format!(
                "line {}: trailing characters after quoted key",
                lineno
            )),
        }
    } else if raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Ok(raw.to_string())
    } else {
        Err(format!("line {}: invalid bare key '{}'", lineno, raw))
    }
}

fn parse_value(raw: &str, lineno: usize) -> Result<TomlValue, String> {
    if raw.is_empty() {
        return Err(format!("line {}: missing value", lineno));
    }
    if raw.starts_with('"') {
        let (s, rest) = parse_quoted(raw, lineno)?;
        if !rest.trim().is_empty() {
            return Err(format!(
                "line {}: trailing characters after string value",
                lineno
            ));
        }
        return Ok(TomlValue::Str(s));
    }
    match raw {
        "true" => return Ok(TomlValue::Bool(true)),
        "false" => return Ok(TomlValue::Bool(false)),
        _ => {}
    }
    if let Ok(n) = raw.parse::<i64>() {
        return Ok(TomlValue::Int(n));
    }
    Err(format!(
        "line {}: unsupported value '{}'; expected a quoted string, integer, or boolean",
        lineno, raw
    ))
}

/// Parse a double-quoted string, returning the decoded value and the unconsumed remainder.
fn parse_quoted(raw: &str, lineno: usize) -> Result<(String, &str), String> {
    let bytes = raw.as_bytes();
    debug_assert_eq!(bytes.first(), Some(&b'"'));
    let mut out = String::new();
    let mut chars = raw.char_indices().skip(1);
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => return Ok((out, &raw[idx + 1..])),
            '\\' => {
                let (_, esc) = chars
                    .next()
                    .ok_or_else(|| format!("line {}: unterminated escape sequence", lineno))?;
                match esc {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    other => {
                        return Err(format!(
                            "line {}: unsupported escape sequence '\\{}'",
                            lineno, other
                        ))
                    }
                }
            }
            other => out.push(other),
        }
    }
    Err(format!("line {}: unterminated string", lineno))
}

/// Quote a value for serialization.
fn toml_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(all(test, unix))]
mod permission_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kroma-config-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_restrict_file_permissions_refuses_a_symlinked_leaf() {
        let dir = scratch("file-symlink");
        let victim = dir.join("victim");
        fs::write(&victim, b"secret").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        restrict_file_permissions(&link)
            .expect_err("a symlinked config leaf must be refused, not chmod-ed through");
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o644,
            "a refused chmod must leave the symlink target's own permissions untouched"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_restrict_file_permissions_sets_0600_on_a_regular_file() {
        let dir = scratch("file-regular");
        let file = dir.join("config.toml");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

        restrict_file_permissions(&file).unwrap();
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_restrict_dir_permissions_refuses_a_symlinked_leaf() {
        let dir = scratch("dir-symlink");
        let victim = dir.join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.join("kroma");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        restrict_dir_permissions(&link)
            .expect_err("a symlinked config directory must be refused, not chmod-ed through");
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o755,
            "a refused chmod must leave the symlink target's own permissions untouched"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_restrict_dir_permissions_sets_0700_on_a_real_directory() {
        let dir = scratch("dir-real");
        let sub = dir.join("kroma");
        fs::create_dir(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();

        restrict_dir_permissions(&sub).unwrap();
        assert_eq!(
            fs::metadata(&sub).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A symlink pre-planted on the temp path must make the write fail without following the link,
    /// so the victim file is never truncated. The old `File::create` (`O_CREAT | O_TRUNC`, no
    /// `O_NOFOLLOW`) followed the link and truncated the target before the permission check ran.
    #[test]
    fn test_write_file_atomically_refuses_a_symlinked_temp_path_and_keeps_the_target_intact() {
        let dir = scratch("tmp-symlink");
        let victim = dir.join("victim");
        fs::write(&victim, b"ORIGINAL").unwrap();
        let config = dir.join("config.toml");
        let suffix = "fixed-suffix";
        let planted = dir.join(format!(
            ".config.toml.tmp-{}-{}",
            std::process::id(),
            suffix
        ));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();

        write_file_atomically_with_suffix(&config, b"new-config-content", suffix)
            .expect_err("a symlinked temp path must be refused, not followed");

        assert_eq!(
            fs::read(&victim).unwrap(),
            b"ORIGINAL",
            "the symlink target must not be truncated"
        );
        assert!(
            !config.exists(),
            "a refused write must not leave a config behind"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A stale *regular* temp file is not an attack: the write retries with a fresh name and still
    /// succeeds, so a crashed previous write cannot permanently wedge `save_config`.
    #[test]
    fn test_write_file_atomically_retries_past_a_stale_regular_temp_file() {
        let dir = scratch("tmp-stale");
        let config = dir.join("config.toml");
        let suffix = "fixed-suffix";
        let stale = dir.join(format!(
            ".config.toml.tmp-{}-{}",
            std::process::id(),
            suffix
        ));
        fs::write(&stale, b"leftover").unwrap();

        write_file_atomically_with_suffix(&config, b"payload", suffix)
            .expect("a stale regular temp file must be retried past");
        assert_eq!(fs::read(&config).unwrap(), b"payload");
        assert_eq!(
            fs::read(&stale).unwrap(),
            b"leftover",
            "the stale file must be left untouched, not clobbered"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_file_atomically_writes_and_restricts_permissions() {
        let dir = scratch("atomic-ok");
        let config = dir.join("config.toml");

        write_file_atomically(&config, b"model = \"x\"\n").unwrap();
        assert_eq!(fs::read(&config).unwrap(), b"model = \"x\"\n");
        assert_eq!(
            fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
