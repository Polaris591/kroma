use crate::profiles::{self, Profiles};
use anyhow::{bail, Context, Result};
use std::{env, path::PathBuf};

pub struct Config {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub workdir: PathBuf,
    /// Explicitly enabled only for providers that understand DeepSeek's
    /// nonstandard thinking field. Other providers retain the plain payload.
    pub deepseek_thinking: Option<String>,
    pub api_protocol: String,
    pub reasoning_effort: String,
    pub max_output_tokens: Option<usize>,
    /// Per-profile context capacity; `KROMA_CONTEXT_WINDOW` still wins.
    pub context_window: Option<usize>,
    /// Per-profile reasoning levels; empty means every level is allowed.
    pub thinking_levels: Vec<String>,
    /// Saved profile name, not credentials, for session-scoped TUI restoration.
    pub selected_profile: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_env_for_workdir(Self::workdir_from_env()?)
    }

    /// GUI hosts choose the project without changing process-wide environment.
    /// Provider selection and overrides remain identical to the CLI.
    pub fn from_env_for_workdir(workdir: PathBuf) -> Result<Self> {
        let workdir = workdir
            .canonicalize()
            .context("project path does not exist")?;
        if !workdir.is_dir() {
            bail!("project must be a directory");
        }
        let requested = env::var("KROMA_PROVIDER_PROFILE").ok();
        let env_model = env::var("KROMA_MODEL").ok();
        let env_url = env::var("KROMA_BASE_URL").ok();
        let plugin_identity = if env_model.is_none() && requested.is_none() {
            crate::plugins::PluginCatalog::discover(&workdir)?
                .enabled_model_identity()
                .map(|(model, url)| (model.to_owned(), url.to_owned()))
        } else {
            None
        };
        // A complete environment-only setup must not depend on an unrelated
        // saved profile. In particular, an old config with bad JSON or file
        // permissions must not prevent an explicitly configured agent.
        // Explicit profile selection still validates the saved profile.
        let saved = if needs_saved_profiles(
            env_model
                .as_deref()
                .or(plugin_identity.as_ref().map(|(model, _)| model.as_str())),
            env_url
                .as_deref()
                .or(plugin_identity.as_ref().map(|(_, url)| url.as_str())),
            requested.as_deref(),
        ) {
            Some(Profiles::load()?)
        } else {
            None
        };
        let selected = match saved.as_ref() {
            Some(profiles) => profiles.selected(requested.as_deref())?,
            None => None,
        };
        let profile = selected.map(|(_, p)| p);
        // A direct environment override may not correspond to any saved
        // profile. Do not persist a misleading profile name for this model.
        let selected_profile = if [
            "KROMA_MODEL",
            "KROMA_BASE_URL",
            "KROMA_API_KEY",
            "KROMA_API_PROTOCOL",
            "KROMA_DEEPSEEK_THINKING",
            "KROMA_REASONING_EFFORT",
        ]
        .iter()
        .any(|key| env::var_os(key).is_some())
        {
            None
        } else {
            selected.map(|(name, _)| name.to_owned())
        };
        Self::resolve(profile, selected_profile, workdir, true, plugin_identity)
    }

    /// Explicit model selection changes connection identity, but retains
    /// process-wide reasoning/output overrides just like initial startup.
    pub fn from_profile(
        profile: &profiles::ProviderProfile,
        workdir: PathBuf,
        name: Option<String>,
    ) -> Result<Self> {
        Self::resolve(Some(profile), name, workdir, false, None)
    }

    fn workdir_from_env() -> Result<PathBuf> {
        let workdir = env::var("KROMA_WORKDIR")
            .map(PathBuf::from)
            .or_else(|_| env::current_dir())
            .context("failed to locate working directory")?
            .canonicalize()
            .context("KROMA_WORKDIR does not exist")?;
        if !workdir.is_dir() {
            bail!("KROMA_WORKDIR must be a directory");
        }
        Ok(workdir)
    }

    fn resolve(
        profile: Option<&profiles::ProviderProfile>,
        selected_profile: Option<String>,
        workdir: PathBuf,
        connection_overrides: bool,
        plugin_identity: Option<(String, String)>,
    ) -> Result<Self> {
        let connection_env = |key| {
            if connection_overrides {
                env::var(key).ok()
            } else {
                None
            }
        };
        let model = connection_env("KROMA_MODEL")
            .or_else(|| profile.map(|p| p.model.clone()))
            .or_else(|| plugin_identity.as_ref().map(|(name, _)| name.clone()))
            .context("KROMA_MODEL, an enabled model plugin or a saved profile is required")?;
        if model.trim().is_empty() {
            bail!("KROMA_MODEL cannot be empty");
        }
        let base_url = profiles::normalize_endpoint(
            &connection_env("KROMA_BASE_URL")
                .or_else(|| profile.map(|p| p.base_url.clone()))
                // A process-protocol model has no HTTP endpoint. This URL is
                // an unused Config fallback; model_override supplies the
                // actual external backend before any HTTP client is started.
                .or_else(|| {
                    plugin_identity
                        .as_ref()
                        .and_then(|(_, url)| (!url.trim().is_empty()).then(|| url.clone()))
                })
                .unwrap_or_else(|| "https://api.openai.com/v1".into()),
        )?;
        let api_key = connection_env("KROMA_API_KEY")
            .or_else(|| profile.and_then(|p| p.api_key.clone()))
            .filter(|key| !key.trim().is_empty());
        let api_protocol = profiles::normalize_protocol(
            &connection_env("KROMA_API_PROTOCOL")
                .or_else(|| profile.and_then(|p| p.api_protocol.clone()))
                .unwrap_or_else(|| "chat".into()),
        )?;
        let deepseek_thinking = profiles::normalize_thinking(
            &env::var("KROMA_DEEPSEEK_THINKING")
                .ok()
                .or_else(|| profile.and_then(|p| p.deepseek_thinking.clone()))
                .unwrap_or_else(|| "off".into()),
        )?;
        let reasoning_effort = profiles::normalize_reasoning(
            &env::var("KROMA_REASONING_EFFORT")
                .ok()
                .or_else(|| profile.and_then(|p| p.reasoning_effort.clone()))
                .unwrap_or_else(|| "auto".into()),
        )?;
        let max_output_tokens = match env::var("KROMA_MAX_OUTPUT_TOKENS") {
            Ok(raw) => {
                let n: usize = raw
                    .parse()
                    .context("KROMA_MAX_OUTPUT_TOKENS must be an integer")?;
                if !(1..=2_000_000).contains(&n) {
                    bail!("KROMA_MAX_OUTPUT_TOKENS must be between 1 and 2000000");
                }
                Some(n)
            }
            Err(env::VarError::NotPresent) => profile.and_then(|p| p.output_limit()),
            Err(error) => return Err(error.into()),
        };
        let context_window = profile.and_then(|p| p.context_limit());
        let thinking_levels = profile.map(|p| p.levels()).transpose()?.unwrap_or_default();
        Ok(Self {
            base_url,
            api_key,
            model: model.trim().to_owned(),
            workdir,
            deepseek_thinking,
            api_protocol,
            reasoning_effort,
            max_output_tokens,
            context_window,
            thinking_levels,
            selected_profile,
        })
    }
}

fn needs_saved_profiles(
    model: Option<&str>,
    base_url: Option<&str>,
    requested_profile: Option<&str>,
) -> bool {
    requested_profile.is_some() || model.is_none() || base_url.is_none()
}

#[cfg(test)]
mod tests {
    use super::{needs_saved_profiles, Config};

    #[test]
    fn standalone_process_model_needs_no_http_endpoint_or_saved_profile() {
        let config = Config::resolve(
            None,
            None,
            std::env::current_dir().unwrap(),
            false,
            Some(("external-process".into(), "".into())),
        )
        .unwrap();
        assert_eq!(config.model, "external-process");
        assert!(
            !config.base_url.is_empty(),
            "internal Config keeps a normalized fallback"
        );
        assert!(config.api_key.is_none());
    }

    #[test]
    fn complete_env_is_independent_from_saved_profiles() {
        assert!(!needs_saved_profiles(
            Some("model"),
            Some("http://localhost/v1"),
            None
        ));
        assert!(needs_saved_profiles(Some("model"), None, None));
        assert!(needs_saved_profiles(
            None,
            Some("http://localhost/v1"),
            None
        ));
        assert!(needs_saved_profiles(None, None, None));
        // Explicitly selecting a named saved profile is always intentional.
        assert!(needs_saved_profiles(
            Some("model"),
            Some("http://localhost/v1"),
            Some("workbuddy")
        ));
        // Empty environment overrides are invalid, not a reason to load a
        // different model from disk without the user's knowledge.
        assert!(!needs_saved_profiles(Some(""), Some(""), None));
    }
}
