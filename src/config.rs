use crate::{
    model_catalog::FieldSource,
    profiles::{self, Profiles},
};
use anyhow::{bail, Context, Result};
use std::{env, path::PathBuf};

pub struct Config {
    pub base_url: String,
    pub api_key: Option<String>,
    /// Saved environment-variable reference, if a profile uses one.
    pub api_key_env: Option<String>,
    pub model: String,
    pub workdir: PathBuf,
    /// Explicitly enabled only for providers that understand DeepSeek's
    /// nonstandard thinking field. Other providers retain the plain payload.
    pub deepseek_thinking: Option<String>,
    pub api_protocol: String,
    pub reasoning_effort: String,
    pub max_output_tokens: Option<usize>,
    pub max_output_tokens_source: Option<FieldSource>,
    /// Per-profile context capacity; `KROMA_CONTEXT_WINDOW` still wins.
    pub context_window: Option<usize>,
    pub context_window_source: Option<FieldSource>,
    /// Explicit profile levels; absent levels may fall back to the catalog.
    pub thinking_levels: Vec<String>,
    pub thinking_levels_source: Option<FieldSource>,
    /// Explicit `provider/model` opt-in for a proxy or renamed model. Without
    /// this, only an exact known provider host and exact model ID may match.
    pub catalog_model: Option<String>,
    pub catalog_model_source: Option<FieldSource>,
    /// Endpoint capability opt-in: send the Responses server-side
    /// `web_search` tool. Off unless the saved profile enables it.
    pub native_web_search: bool,
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
        // Changing KROMA_BASE_URL must not silently send credentials stored
        // for a different profile endpoint. An explicit KROMA_API_KEY still
        // belongs to the environment override and takes precedence.
        let profile_key_matches_endpoint = profile
            .map(|saved| credentials_match_endpoint(saved, &base_url))
            .transpose()?
            .unwrap_or(false);
        let nonempty_key = |key: String| (!key.trim().is_empty()).then_some(key);
        let api_key = connection_env("KROMA_API_KEY")
            .filter(|key| !key.trim().is_empty())
            .or_else(|| {
                profile
                    .filter(|_| profile_key_matches_endpoint)
                    .and_then(|p| p.api_key.clone())
                    .filter(|key| !key.trim().is_empty())
            })
            .or_else(|| {
                profile
                    .filter(|_| profile_key_matches_endpoint)
                    .and_then(|p| p.api_key_env.as_deref())
                    .and_then(|name| env::var(name).ok())
                    .and_then(nonempty_key)
            });
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
        let (max_output_tokens, max_output_tokens_source) =
            match env::var("KROMA_MAX_OUTPUT_TOKENS") {
                Ok(raw) => {
                    let n: usize = raw
                        .parse()
                        .context("KROMA_MAX_OUTPUT_TOKENS must be an integer")?;
                    if !(1..=2_000_000).contains(&n) {
                        bail!("KROMA_MAX_OUTPUT_TOKENS must be between 1 and 2000000");
                    }
                    (Some(n), Some(FieldSource::Environment))
                }
                Err(env::VarError::NotPresent) => (
                    profile.and_then(|p| p.output_limit()),
                    profile
                        .and_then(|p| p.output_limit())
                        .map(|_| FieldSource::Profile),
                ),
                Err(error) => return Err(error.into()),
            };
        // Resolve the context override here, not only inside ContextConfig,
        // so its source is visible and a known hard cap can be applied once.
        let (context_window, context_window_source) = match env::var("KROMA_CONTEXT_WINDOW") {
            Ok(raw) => {
                let n: usize = raw
                    .parse()
                    .context("KROMA_CONTEXT_WINDOW must be an integer token count")?;
                (
                    Some(profiles::validate_context_window(n)?),
                    Some(FieldSource::Environment),
                )
            }
            Err(env::VarError::NotPresent) => (
                profile.and_then(|p| p.context_limit()),
                profile
                    .and_then(|p| p.context_limit())
                    .map(|_| FieldSource::Profile),
            ),
            Err(error) => return Err(error.into()),
        };
        let (thinking_levels, thinking_levels_source) = match profile {
            Some(profile) if profile.thinking_levels.is_some() => {
                (profile.levels()?, Some(FieldSource::Profile))
            }
            _ => (Vec::new(), None),
        };
        let (catalog_model, catalog_model_source) = match env::var("KROMA_MODEL_CATALOG_MODEL") {
            Ok(raw) => {
                let value = raw.trim();
                if value.is_empty() {
                    bail!("KROMA_MODEL_CATALOG_MODEL cannot be empty");
                }
                (Some(value.to_owned()), Some(FieldSource::Environment))
            }
            Err(env::VarError::NotPresent) => (
                profile.and_then(|p| p.catalog_model.clone()),
                profile
                    .and_then(|p| p.catalog_model.as_ref())
                    .map(|_| FieldSource::Profile),
            ),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            base_url,
            api_key,
            api_key_env: profile
                .filter(|_| profile_key_matches_endpoint)
                .and_then(|p| p.api_key_env.clone()),
            model: model.trim().to_owned(),
            workdir,
            deepseek_thinking,
            api_protocol,
            reasoning_effort,
            max_output_tokens,
            max_output_tokens_source,
            context_window,
            context_window_source,
            thinking_levels,
            thinking_levels_source,
            catalog_model,
            catalog_model_source,
            // A capability switch, not a guess from the model name or host.
            native_web_search: profile.is_some_and(|p| p.native_web_search()),
            selected_profile,
        })
    }

    /// Resolve configured + catalog fallback without contacting the endpoint
    /// or executing credential commands. Used by `config show` and diagnostics.
    pub fn resolved_limits_without_probe(
        &self,
    ) -> Result<(
        crate::context::ContextConfig,
        crate::model_catalog::ModelReport,
    )> {
        let catalog = crate::model_catalog::select(
            &self.base_url,
            &self.model,
            self.catalog_model.as_deref(),
        )?;
        let inputs = crate::model_catalog::ModelInputs {
            model: &self.model,
            base_url: &self.base_url,
            context_window: self.context_window,
            context_source: self.context_window_source.unwrap_or(FieldSource::Unknown),
            requested_output: self.max_output_tokens,
            requested_output_source: self
                .max_output_tokens_source
                .unwrap_or(FieldSource::Unknown),
            thinking_levels: &self.thinking_levels,
            thinking_levels_source: self.thinking_levels_source.unwrap_or(FieldSource::Unknown),
        };
        let resolved = crate::model_catalog::resolve(
            &inputs,
            &crate::model_catalog::UpstreamLimits::default(),
            crate::model_catalog::MetadataState::NotQueried,
            None,
            catalog,
        );
        let resolved_output = resolved.effective_turn_output;
        let effective_output = crate::model_catalog::effective_wire_output(
            &self.api_protocol,
            resolved.effective_turn_output,
        );
        let mut budget =
            crate::context::ContextConfig::from_limits(resolved.context_window, effective_output)?;
        if let Some(limit) = resolved.catalog_max_input_tokens {
            budget = budget.with_input_limit(limit)?;
        }
        let reserve_source = if env::var_os("KROMA_OUTPUT_RESERVE_TOKENS").is_some() {
            FieldSource::Environment
        } else if effective_output.is_some() {
            FieldSource::Computed
        } else {
            FieldSource::Default
        };
        let mut report = crate::model_catalog::annotate_wire_output(
            resolved.report,
            &self.api_protocol,
            resolved_output,
        );
        if self.api_protocol != "chat" && self.max_output_tokens.is_none() {
            report.requested_output_tokens.note =
                Some("provider default; Kroma sends an explicit native output cap".into());
        }
        let report =
            crate::model_catalog::finish_report(report, budget.output_reserve(), reserve_source);
        Ok((budget, report))
    }
}

fn needs_saved_profiles(
    model: Option<&str>,
    base_url: Option<&str>,
    requested_profile: Option<&str>,
) -> bool {
    requested_profile.is_some() || model.is_none() || base_url.is_none()
}

fn credentials_match_endpoint(profile: &profiles::ProviderProfile, endpoint: &str) -> Result<bool> {
    Ok(profiles::normalize_endpoint(&profile.base_url)? == endpoint)
}

#[cfg(test)]
mod tests {
    use super::{credentials_match_endpoint, needs_saved_profiles, Config};
    use crate::profiles::ProviderProfile;
    use std::env;

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
    fn an_empty_stored_key_falls_back_to_its_environment_variable() {
        let name = "KROMA_TEST_EMPTY_KEY_FALLBACK";
        env::set_var(name, "from-environment");
        let profile = ProviderProfile {
            base_url: "http://localhost/v1".into(),
            model: "test".into(),
            api_key: Some(String::new()),
            api_key_env: Some(name.into()),
            ..ProviderProfile::default()
        };
        let config = Config::resolve(
            Some(&profile),
            None,
            std::env::current_dir().unwrap(),
            false,
            None,
        )
        .unwrap();
        assert_eq!(config.api_key.as_deref(), Some("from-environment"));
        env::remove_var(name);
    }

    #[test]
    fn profile_key_is_bound_to_normalized_endpoint() -> anyhow::Result<()> {
        let profile = ProviderProfile {
            base_url: "https://old.example/v1/".into(),
            model: "test".into(),
            api_key: Some("stored-key".into()),
            api_key_env: Some("OLD_KEY_ENV".into()),
            ..ProviderProfile::default()
        };
        let config = Config::resolve(
            Some(&profile),
            None,
            std::env::current_dir().unwrap(),
            false,
            None,
        )
        .unwrap();
        assert_eq!(config.base_url, "https://old.example/v1");
        assert_eq!(config.api_key.as_deref(), Some("stored-key"));
        assert_eq!(config.api_key_env.as_deref(), Some("OLD_KEY_ENV"));
        assert!(credentials_match_endpoint(
            &profile,
            "https://old.example/v1"
        )?);
        assert!(!credentials_match_endpoint(
            &profile,
            "https://new.example/v1"
        )?);
        Ok(())
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
