//! Minimal persistent provider profiles for the new Rust Kroma.
//!
//! This file is deliberately different from the old Kroma's config.toml:
//! neither tool needs to edit or migrate the other's credentials.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub base_url: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepseek_thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Context capacity this model accepts, in tokens. The
    /// `KROMA_CONTEXT_WINDOW` environment override still wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// Requested completion maximum, in tokens. The
    /// `KROMA_MAX_OUTPUT_TOKENS` environment override still wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
    /// Reasoning levels this model offers, in display order. Unset means every
    /// level Kroma knows about is allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_levels: Option<Vec<String>>,
}

impl ProviderProfile {
    /// Per-profile context window, unless an environment override exists.
    pub fn context_limit(&self) -> Option<usize> {
        self.context_window
    }

    /// Per-profile completion maximum, unless an environment override exists.
    pub fn output_limit(&self) -> Option<usize> {
        self.max_output_tokens
    }

    /// Normalized levels this model accepts. An empty list means no profile
    /// restriction, so the full Kroma level set remains available.
    pub fn levels(&self) -> Result<Vec<String>> {
        match &self.thinking_levels {
            Some(levels) => normalize_levels(levels),
            None => Ok(Vec::new()),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Profiles {
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProviderProfile>,
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(value) = env::var_os("KROMA_CONFIG_FILE") {
        if value.is_empty() {
            bail!("KROMA_CONFIG_FILE cannot be empty");
        }
        return Ok(PathBuf::from(value));
    }
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .context("HOME is unavailable; specify KROMA_CONFIG_FILE")?;
    Ok(PathBuf::from(home).join(".kroma").join("config.json"))
}

pub fn normalize_endpoint(value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches('/');
    let parsed = reqwest::Url::parse(value).context("invalid API base URL")?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        bail!("API base URL must be http(s) and include a host");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("API base URL cannot include a query or fragment");
    }
    Ok(value.to_owned())
}

pub fn normalize_thinking(value: &str) -> Result<Option<String>> {
    match value.trim() {
        "" | "off" | "disabled" => Ok(None),
        "enabled" | "high" | "max" => Ok(Some(value.trim().to_owned())),
        _ => bail!("thinking must be off, enabled, high, or max"),
    }
}

pub fn normalize_protocol(value: &str) -> Result<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "chat" | "completions" => Ok("chat".into()),
        "responses" | "response" => Ok("responses".into()),
        "anthropic" | "messages" => Ok("anthropic".into()),
        _ => bail!("API protocol must be chat, responses, or anthropic"),
    }
}

pub fn normalize_reasoning(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "none" => Ok("off".into()),
        "auto" | "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => Ok(value),
        _ => bail!(
            "reasoning effort must be auto, off, none, minimal, low, medium, high, xhigh, or max"
        ),
    }
}

/// Reject a context capacity the runtime cannot plan against.
pub fn validate_context_window(tokens: usize) -> Result<usize> {
    if !(1_024..=2_000_000).contains(&tokens) {
        bail!("context window must be between 1024 and 2000000 tokens");
    }
    Ok(tokens)
}

/// Reject a completion maximum the runtime cannot request.
pub fn validate_output_tokens(tokens: usize) -> Result<usize> {
    if !(1..=2_000_000).contains(&tokens) {
        bail!("maximum output tokens must be between 1 and 2000000");
    }
    Ok(tokens)
}

/// Normalize a declared level list, keeping the author's order and rejecting
/// duplicates rather than silently reordering a model's advertised tiers.
pub fn normalize_levels(levels: &[String]) -> Result<Vec<String>> {
    let mut result: Vec<String> = Vec::with_capacity(levels.len());
    for raw in levels {
        let level = normalize_reasoning(raw)?;
        if result.iter().any(|seen| seen == &level) {
            bail!("duplicate thinking level '{level}'");
        }
        result.push(level);
    }
    if result.is_empty() {
        bail!("thinking levels must list at least one level; remove the field to allow all");
    }
    Ok(result)
}

fn valid_name(name: &str) -> bool {
    (1..=48).contains(&name.len())
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

impl Profiles {
    pub fn load() -> Result<Self> {
        Self::load_at(&config_path()?)
    }

    pub fn load_at(path: &Path) -> Result<Self> {
        let metadata = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error).context("failed to inspect Kroma profile file"),
        };
        if !metadata.file_type().is_file() {
            bail!("Kroma profile file must resolve to a regular file");
        }
        let bytes = fs::read(path).context("failed to read Kroma profile file")?;
        let saved: Self = serde_json::from_slice(&bytes).context("invalid Kroma profile JSON")?;
        saved.validate()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0
                && saved.profiles.values().any(|p| p.api_key.is_some())
            {
                crate::runtime_log!(
                    "[config] warning: {} is readable by other users and stores an API key; consider chmod 600",
                    path.display()
                );
            }
        }
        Ok(saved)
    }

    fn validate(&self) -> Result<()> {
        for (name, profile) in &self.profiles {
            if !valid_name(name) {
                bail!("invalid Kroma profile name (use letters, numbers, -, _ or .)");
            }
            normalize_endpoint(&profile.base_url)?;
            if profile.model.trim().is_empty() {
                bail!("Kroma profile '{name}' has an empty model ID");
            }
            normalize_thinking(profile.deepseek_thinking.as_deref().unwrap_or("off"))?;
            normalize_protocol(profile.api_protocol.as_deref().unwrap_or("chat"))?;
            let default =
                normalize_reasoning(profile.reasoning_effort.as_deref().unwrap_or("auto"))?;
            if let Some(window) = profile.context_window {
                validate_context_window(window)?;
            }
            if let Some(output) = profile.max_output_tokens {
                validate_output_tokens(output)?;
                if let Some(window) = profile.context_window {
                    if output >= window {
                        bail!(
                            "Kroma profile '{name}' requests {output} output tokens, which must be smaller than its {window} token context window"
                        );
                    }
                }
            }
            let levels = profile.levels()?;
            // "auto" always means "let the provider decide", so it never has to
            // appear in the advertised list. A concrete default must.
            if !levels.is_empty()
                && default != "auto"
                && !levels.iter().any(|level| level == &default)
            {
                bail!(
                    "Kroma profile '{name}' default reasoning '{default}' is not one of its thinking levels ({})",
                    levels.join(", ")
                );
            }
        }
        if let Some(active) = &self.active {
            if !self.profiles.contains_key(active) {
                bail!("selected Kroma profile does not exist");
            }
        }
        Ok(())
    }

    pub fn selected<'a>(
        &'a self,
        name: Option<&str>,
    ) -> Result<Option<(&'a str, &'a ProviderProfile)>> {
        let selected = name.or(self.active.as_deref()).or_else(|| {
            (self.profiles.len() == 1)
                .then(|| self.profiles.keys().next())
                .flatten()
                .map(String::as_str)
        });
        match selected {
            None => Ok(None),
            Some(name) => {
                let (stored, profile) = self
                    .profiles
                    .get_key_value(name)
                    .context("requested Kroma profile does not exist; use 'kroma config list'")?;
                Ok(Some((stored.as_str(), profile)))
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_at(&config_path()?)
    }

    pub fn save_at(&self, path: &Path) -> Result<()> {
        self.validate()?;
        // Respect explicit config symlinks (dotfile managers, shared config
        // folders) rather than rejecting or replacing the symlink itself.
        let resolved = if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
        {
            Some(fs::canonicalize(path).context("cannot follow Kroma config symlink")?)
        } else {
            None
        };
        let path = resolved.as_deref().unwrap_or(path);
        let parent = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        #[cfg(unix)]
        let existed = parent.exists();
        fs::create_dir_all(parent).context("failed to create Kroma config directory")?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        if let Ok(meta) = fs::symlink_metadata(path) {
            if !meta.file_type().is_file() {
                bail!("refusing to replace a non-regular Kroma profile file");
            }
        }
        let data = {
            let mut data = serde_json::to_vec_pretty(self)?;
            data.push(b'\n');
            data
        };
        let filename = path
            .file_name()
            .context("invalid Kroma profile path")?
            .to_string_lossy();
        let temp = parent.join(format!(
            ".{filename}.{}-{}.tmp",
            std::process::id(),
            NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        let save = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temp)
                .context("failed to create private profile file")?;
            file.write_all(&data)?;
            file.sync_all()?;
            drop(file);
            crate::atomic_file::replace(&temp, path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::File::open(parent)?.sync_all()?;
                // The temporary file was created with 0600; rename preserves it.
                debug_assert_eq!(fs::metadata(path)?.permissions().mode() & 0o077, 0);
            }
            Ok(())
        })();
        if save.is_err() {
            let _ = fs::remove_file(temp);
        }
        save
    }
}

fn prompt_value(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut value = String::new();
    if io::stdin().read_line(&mut value)? == 0 {
        bail!("configuration cancelled (stdin closed)");
    }
    let trimmed = value.trim();
    Ok(if trimmed.is_empty() {
        default.to_owned()
    } else {
        trimmed.to_owned()
    })
}

/// Optional numeric profile field: Enter keeps the stored value, "-" clears it.
fn prompt_optional_tokens(label: &str, current: Option<usize>) -> Result<Option<usize>> {
    let shown = current
        .map(|value| value.to_string())
        .unwrap_or_else(|| "auto".into());
    print!("{label} [{shown}] (Enter=keep, '-'=auto): ");
    io::stdout().flush()?;
    let mut value = String::new();
    if io::stdin().read_line(&mut value)? == 0 {
        bail!("configuration cancelled (stdin closed)");
    }
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(current);
    }
    if trimmed == "-" || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    trimmed
        .parse()
        .map(Some)
        .with_context(|| format!("{label} must be an integer token count"))
}

/// Optional level list: Enter keeps the stored value, "-" allows every level.
fn prompt_levels(current: Option<&[String]>) -> Result<Option<Vec<String>>> {
    let shown = current
        .map(|levels| levels.join(","))
        .filter(|levels| !levels.is_empty())
        .unwrap_or_else(|| "all".into());
    print!("Thinking levels, comma separated (Enter=keep, '-'=all) [{shown}]: ");
    io::stdout().flush()?;
    let mut value = String::new();
    if io::stdin().read_line(&mut value)? == 0 {
        bail!("configuration cancelled (stdin closed)");
    }
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(current.map(<[String]>::to_vec));
    }
    if trimmed == "-" || trimmed.eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    let levels = trimmed
        .split(',')
        .map(str::trim)
        .filter(|level| !level.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    Ok(Some(normalize_levels(&levels)?))
}

#[cfg(unix)]
struct HiddenInput {
    previous: libc::termios,
}

#[cfg(unix)]
impl HiddenInput {
    fn enter() -> Result<Self> {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            bail!(
                "API key entry needs an interactive terminal (it is never read as an echoed line)"
            );
        }
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error()).context("failed to inspect terminal");
        }
        let previous = unsafe { original.assume_init() };
        let mut hidden = previous;
        hidden.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &hidden) } != 0 {
            return Err(io::Error::last_os_error()).context("failed to hide terminal input");
        }
        Ok(Self { previous })
    }
}

#[cfg(unix)]
impl Drop for HiddenInput {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.previous) };
    }
}

// Windows has no termios: clear the console's echo flag instead, which also
// keeps the key out of the terminal scrollback. The guard is only held for its
// `Drop`, hence the underscore.
#[cfg(windows)]
struct HiddenInput {
    _console: crate::winsys::HiddenConsoleInput,
}

#[cfg(windows)]
impl HiddenInput {
    fn enter() -> Result<Self> {
        Ok(Self {
            _console: crate::winsys::HiddenConsoleInput::enter().context(
                "API key entry needs an interactive terminal (it is never read as an echoed line)",
            )?,
        })
    }
}

// Any other target cannot hide input; the caller warns instead of pretending.
#[cfg(not(any(unix, windows)))]
struct HiddenInput;

#[cfg(not(any(unix, windows)))]
impl HiddenInput {
    fn enter() -> Result<Self> {
        Ok(Self)
    }
}

fn prompt_key(existing: bool) -> Result<Option<String>> {
    let status = if existing { "stored" } else { "not set" };
    print!("API key ({status}; Enter=keep, '-'=clear): ");
    io::stdout().flush()?;
    let guard = HiddenInput::enter()?;
    #[cfg(not(any(unix, windows)))]
    eprintln!("API key input will be visible on this terminal");
    let mut input = String::new();
    let result = io::stdin().read_line(&mut input);
    drop(guard);
    println!();
    if result? == 0 {
        bail!("configuration cancelled (stdin closed)");
    }
    match input.trim() {
        "" => Ok(None),
        "-" => Ok(Some(String::new())),
        value => Ok(Some(value.to_owned())),
    }
}

pub fn cli(args: &[String]) -> Result<()> {
    match args {
        [] => list_profiles(),
        [cmd] if cmd == "list" => list_profiles(),
        [cmd] if cmd == "show" => {
            let config = crate::config::Config::from_env()?;
            let host = reqwest::Url::parse(&config.base_url)?
                .host_str()
                .unwrap_or("")
                .to_owned();
            let context = env::var("KROMA_CONTEXT_WINDOW")
                .ok()
                .or_else(|| config.context_window.map(|value| value.to_string()))
                .unwrap_or_else(|| "auto".into());
            let output = env::var("KROMA_MAX_OUTPUT_TOKENS")
                .ok()
                .or_else(|| config.max_output_tokens.map(|value| value.to_string()))
                .unwrap_or_else(|| "auto".into());
            println!(
                "model={} | host={} | protocol={} | reasoning={} | deepseek_thinking={} | context={} | output={} | levels={} | key={} (environment overrides saved profile)",
                config.model,
                host,
                config.api_protocol,
                config.reasoning_effort,
                config.deepseek_thinking.as_deref().unwrap_or("off"),
                context,
                output,
                display_levels((!config.thinking_levels.is_empty()).then_some(&config.thinking_levels[..])),
                if config.api_key.is_some() {
                    "configured"
                } else {
                    "none"
                }
            );
            Ok(())
        }
        [cmd, name] if cmd == "use" => {
            let mut profiles = Profiles::load()?;
            if !profiles.profiles.contains_key(name) {
                bail!("unknown profile; use 'kroma config list'");
            }
            profiles.active = Some(name.clone());
            profiles.save()?;
            println!("Active profile: {name} (takes effect on next Kroma launch)");
            Ok(())
        }
        [cmd, name] if cmd == "set" => {
            if !valid_name(name) {
                bail!("invalid profile name");
            }
            let mut profiles = Profiles::load()?;
            let original = profiles.profiles.get(name).cloned().unwrap_or_default();
            let base_url = normalize_endpoint(&prompt_value(
                "API base URL",
                if original.base_url.is_empty() {
                    "https://api.openai.com/v1"
                } else {
                    &original.base_url
                },
            )?)?;
            let model = prompt_value("Model ID", &original.model)?;
            if model.trim().is_empty() {
                bail!("model ID cannot be empty");
            }
            let deepseek_thinking = normalize_thinking(&prompt_value(
                "DeepSeek thinking (off/enabled/high/max)",
                original.deepseek_thinking.as_deref().unwrap_or("off"),
            )?)?;
            let api_protocol = normalize_protocol(&prompt_value(
                "API protocol (chat/responses/anthropic)",
                original.api_protocol.as_deref().unwrap_or("chat"),
            )?)?;
            let reasoning_effort = normalize_reasoning(&prompt_value(
                "Reasoning (auto/off/low/medium/high/xhigh/max)",
                original.reasoning_effort.as_deref().unwrap_or("auto"),
            )?)?;
            let context_window =
                prompt_optional_tokens("Context window in tokens", original.context_window)?
                    .map(validate_context_window)
                    .transpose()?;
            let max_output_tokens =
                prompt_optional_tokens("Maximum output tokens", original.max_output_tokens)?
                    .map(validate_output_tokens)
                    .transpose()?;
            let thinking_levels = prompt_levels(original.thinking_levels.as_deref())?;
            let api_key = match prompt_key(original.api_key.is_some())? {
                None => original.api_key,
                Some(key) if key.is_empty() => None,
                Some(key) => Some(key),
            };
            profiles.profiles.insert(
                name.clone(),
                ProviderProfile {
                    base_url,
                    model,
                    api_key,
                    deepseek_thinking,
                    api_protocol: Some(api_protocol),
                    reasoning_effort: Some(reasoning_effort),
                    context_window,
                    max_output_tokens,
                    thinking_levels,
                },
            );
            if profiles.active.is_none() {
                profiles.active = Some(name.clone());
            }
            profiles.save()?;
            println!("Saved profile '{name}' to private Kroma config. Key value not displayed.");
            Ok(())
        }
        _ => bail!("Usage: kroma config [list|show|set <name>|use <name>]"),
    }
}

fn display_limit(value: Option<usize>) -> String {
    value.map_or_else(|| "auto".into(), |value| value.to_string())
}

fn display_levels(levels: Option<&[String]>) -> String {
    match levels {
        Some(levels) if !levels.is_empty() => levels.join(","),
        _ => "all".into(),
    }
}

fn list_profiles() -> Result<()> {
    let profiles = Profiles::load()?;
    let active = profiles.selected(None)?.map(|(name, _)| name);
    if profiles.profiles.is_empty() {
        println!("No saved profiles. Run 'kroma config set <name>' to create one.");
    } else {
        for (name, profile) in &profiles.profiles {
            let selected = if active == Some(name.as_str()) {
                "*"
            } else {
                " "
            };
            let host = reqwest::Url::parse(&profile.base_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| "(invalid)".to_owned());
            println!(
                "{selected} {name} | model={} | host={} | protocol={} | reasoning={} | thinking={} | context={} | output={} | levels={} | key={}",
                profile.model,
                host,
                profile.api_protocol.as_deref().unwrap_or("chat"),
                profile.reasoning_effort.as_deref().unwrap_or("auto"),
                profile.deepseek_thinking.as_deref().unwrap_or("off"),
                display_limit(profile.context_window),
                display_limit(profile.max_output_tokens),
                display_levels(profile.thinking_levels.as_deref()),
                if profile.api_key.is_some() {
                    "stored"
                } else {
                    "none"
                }
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Profiles {
        let mut profiles = Profiles {
            active: Some("workbuddy".into()),
            ..Default::default()
        };
        profiles.profiles.insert(
            "workbuddy".into(),
            ProviderProfile {
                base_url: "https://provider.example/v1".into(),
                model: "deepseek".into(),
                api_key: Some("non-production-dummy-key".into()),
                deepseek_thinking: Some("high".into()),
                api_protocol: None,
                reasoning_effort: None,
                context_window: None,
                max_output_tokens: None,
                thinking_levels: None,
            },
        );
        profiles
    }

    #[test]
    fn save_and_reload_with_private_permissions_and_distinct_legacy_path() {
        let parent = env::temp_dir().join(format!(
            "kroma-profile-test-{}-{}",
            std::process::id(),
            NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        let path = parent.join("config.json");
        let legacy = parent.join("config.toml");
        fs::create_dir_all(&parent).unwrap();
        fs::write(&legacy, b"old data (must remain unchanged)").unwrap();
        let settings = sample();
        settings.save_at(&path).unwrap();
        let loaded = Profiles::load_at(&path).unwrap();
        assert_eq!(loaded.selected(None).unwrap().unwrap().0, "workbuddy");
        assert_eq!(
            loaded.selected(None).unwrap().unwrap().1.api_key.as_deref(),
            Some("non-production-dummy-key")
        );
        assert_eq!(
            fs::read(&legacy).unwrap(),
            b"old data (must remain unchanged)"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        }
        let mut second = loaded;
        second.active = Some("workbuddy".to_owned());
        second.save_at(&path).unwrap();
        assert_eq!(Profiles::load_at(&path).unwrap().profiles.len(), 1);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn model_limits_and_levels_round_trip_and_stay_optional() {
        let parent = env::temp_dir().join(format!(
            "kroma-profile-limits-{}-{}",
            std::process::id(),
            NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        let path = parent.join("config.json");
        fs::create_dir_all(&parent).unwrap();
        let mut settings = sample();
        let profile = settings.profiles.get_mut("workbuddy").unwrap();
        profile.reasoning_effort = Some("high".into());
        profile.context_window = Some(500_000);
        profile.max_output_tokens = Some(128_000);
        profile.thinking_levels = Some(vec!["medium".into(), "high".into(), "xhigh".into()]);
        settings.save_at(&path).unwrap();
        let loaded = Profiles::load_at(&path).unwrap();
        let profile = loaded.profiles.get("workbuddy").unwrap();
        assert_eq!(profile.context_limit(), Some(500_000));
        assert_eq!(profile.output_limit(), Some(128_000));
        assert_eq!(
            profile.levels().unwrap(),
            vec!["medium".to_owned(), "high".to_owned(), "xhigh".to_owned()]
        );
        // A profile written before these fields existed still loads with the
        // conservative "let the provider decide" defaults.
        fs::write(
            &path,
            br#"{"active":"workbuddy","profiles":{"workbuddy":{"base_url":"https://provider.example/v1","model":"deepseek"}}}"#,
        )
        .unwrap();
        let legacy = Profiles::load_at(&path).unwrap();
        let profile = legacy.profiles.get("workbuddy").unwrap();
        assert_eq!(profile.context_limit(), None);
        assert_eq!(profile.output_limit(), None);
        assert!(profile.levels().unwrap().is_empty());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn invalid_limits_and_levels_are_rejected() {
        let mut profiles = sample();
        let set = |profiles: &mut Profiles, apply: &dyn Fn(&mut ProviderProfile)| {
            apply(profiles.profiles.get_mut("workbuddy").unwrap());
        };
        set(&mut profiles, &|profile| profile.context_window = Some(512));
        assert!(profiles.validate().is_err());
        set(&mut profiles, &|profile| {
            profile.context_window = Some(500_000);
            profile.max_output_tokens = Some(500_000);
        });
        assert!(profiles.validate().is_err());
        set(&mut profiles, &|profile| {
            profile.max_output_tokens = Some(128_000);
            profile.reasoning_effort = Some("low".into());
            profile.thinking_levels = Some(vec!["medium".into(), "high".into(), "xhigh".into()]);
        });
        let error = profiles.validate().unwrap_err().to_string();
        assert!(error.contains("thinking levels"), "{error}");
        // "auto" always means "provider default", so it needs no listing.
        set(&mut profiles, &|profile| {
            profile.reasoning_effort = Some("auto".into())
        });
        assert!(profiles.validate().is_ok());
        set(&mut profiles, &|profile| {
            profile.thinking_levels = Some(vec!["high".into(), "high".into()])
        });
        assert!(profiles.validate().is_err());
        set(&mut profiles, &|profile| {
            profile.thinking_levels = Some(vec!["turbo".into()])
        });
        assert!(profiles.validate().is_err());
        set(&mut profiles, &|profile| {
            profile.thinking_levels = Some(Vec::new())
        });
        assert!(profiles.validate().is_err());
        set(&mut profiles, &|profile| {
            profile.thinking_levels = Some(vec!["none".into(), "high".into()])
        });
        assert_eq!(
            profiles
                .profiles
                .get("workbuddy")
                .unwrap()
                .levels()
                .unwrap(),
            vec!["off".to_owned(), "high".to_owned()]
        );
        set(&mut profiles, &|profile| {
            profile.reasoning_effort = Some(" NONE ".into());
            profile.thinking_levels = Some(vec![" none ".into(), " HIGH ".into()]);
        });
        assert!(
            profiles.validate().is_ok(),
            "normalize defaults before validating allowed tiers"
        );
        set(&mut profiles, &|profile| {
            profile.thinking_levels = Some(vec!["off".into(), "none".into()]);
        });
        assert!(
            profiles.validate().is_err(),
            "reject aliases that duplicate after normalization"
        );
    }

    #[test]
    fn selection_validation_and_missing_file_are_explicit() {
        let mut profiles = sample();
        assert!(profiles.selected(Some("missing")).is_err());
        profiles
            .profiles
            .insert("wrong/name".to_owned(), ProviderProfile::default());
        assert!(profiles.validate().is_err());
        profiles.profiles.remove("wrong/name");
        profiles.active = Some("unavailable".to_owned());
        assert!(profiles.validate().is_err());
        assert!(normalize_endpoint("https://host/path?token=sensitive").is_err());
        assert!(normalize_thinking("always").is_err());
        assert!(valid_name("deepseek-v4.1"));
        assert!(!valid_name("../escape"));
    }

    #[cfg(unix)]
    #[test]
    fn non_private_file_and_symlink_are_allowed_with_private_new_writes() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let parent = env::temp_dir().join(format!(
            "kroma-permission-test-{}-{}",
            std::process::id(),
            NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&parent).unwrap();
        let path = parent.join("config.json");
        fs::write(&path, serde_json::to_vec(&sample()).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Profiles::load_at(&path).is_ok());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Profiles::load_at(&path).is_ok());
        let link = parent.join("link.json");
        symlink(&path, &link).unwrap();
        assert!(Profiles::load_at(&link).is_ok());
        sample().save_at(&link).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(Profiles::load_at(&link).is_ok());
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        fs::remove_dir_all(parent).unwrap();
    }
}
