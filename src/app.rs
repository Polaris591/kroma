//! Default, dependency-free application composition.
use crate::{
    agent::Agent,
    config::Config,
    context::{BuiltinContextFactory, ContextConfig, ContextFactory},
    events::EventSink,
    history_tools::{HistoryRead, HistorySearch},
    mcp,
    model::ModelBackend,
    plugins::PluginCatalog,
    prompt::PromptComposer,
    provider::ModelClient,
    skills::{LoadSkill, SkillCatalog},
    tools::ToolRegistry,
};
use anyhow::Result;
use std::sync::Arc;

/// The one startup composition point for CLI and future plugin hosts.
/// An extension changes components here; it never edits Agent's loop.
pub struct RuntimeComponents {
    // A model and its context budget must change together. Keep these
    // private so an extension cannot accidentally use the previous model's
    // window after swapping the backend.
    model: Arc<dyn ModelBackend>,
    context_budget: ContextConfig,
    pub tools: ToolRegistry,
    pub context_factory: Arc<dyn ContextFactory>,
    pub prompt: PromptComposer,
    pub events: Option<Arc<dyn EventSink>>,
    /// TUI welcome does not materialize a new empty default session.
    pub defer_initial_session: bool,
}

impl RuntimeComponents {
    pub fn model(&self) -> Arc<dyn ModelBackend> {
        self.model.clone()
    }

    pub fn replace_model(&mut self, model: Arc<dyn ModelBackend>, budget: ContextConfig) {
        self.model = model;
        self.context_budget = budget;
    }
}

pub struct DefaultRuntime {
    pub agent: Agent,
    pub skills: SkillCatalog,
    pub plugins: PluginCatalog,
    pub startup_warnings: Vec<String>,
    pub plugin_warnings: Vec<String>,
    pub selected_model_name: Option<String>,
    /// Unmodified application components for idle plugin reloads. Share
    /// existing MCP/Skill adapters; never reconnect or rebuild Agent Loop.
    pub base_tools: ToolRegistry,
    pub base_prompt: PromptComposer,
}

pub async fn default_runtime_with(
    config: Config,
    customize: impl FnOnce(&mut RuntimeComponents) -> Result<()>,
) -> Result<DefaultRuntime> {
    let plugins = PluginCatalog::discover(&config.workdir)?;
    let override_model = plugins.checked_model_override()?;
    let mut plugin_warnings = Vec::new();
    let (model, budget, selected_model_name) = if let Some((backend, budget, name)) = override_model
    {
        (backend, budget, Some(name))
    } else {
        let (model, budget) = configured_model(&config).await?;
        (Arc::new(model) as Arc<dyn ModelBackend>, budget, None)
    };
    let skills = SkillCatalog::discover(&config.workdir)?;
    let mut tools = ToolRegistry::builtin();
    tools.register(Arc::new(HistorySearch))?;
    tools.register(Arc::new(HistoryRead))?;
    if !skills.is_empty() {
        tools.register(Arc::new(LoadSkill(skills.clone())))?;
    }
    let mcp_status = mcp::register_configured_report(&mut tools, &config.workdir).await?;
    if mcp_status.tools > 0 {
        crate::runtime_log!("[mcp] registered {} tool(s)", mcp_status.tools);
    }
    let mut prompt = PromptComposer::default();
    if let Some(metadata) = skills.metadata_prompt() {
        prompt = prompt.with_trailing(metadata)?;
    }
    let mut components = RuntimeComponents {
        model,
        context_budget: budget,
        tools,
        context_factory: Arc::new(BuiltinContextFactory),
        prompt,
        events: None,
        defer_initial_session: false,
    };
    let base_tools = components.tools.clone();
    let base_prompt = components.prompt.clone();
    plugin_warnings.splice(0..0, plugins.warnings.clone());
    plugin_warnings.extend(plugins.contribute(&mut components.tools, &mut components.prompt));
    plugin_warnings.extend(plugins.replace_context(&mut components.context_factory));
    customize(&mut components)?;
    components.model = plugins.model_hooks(components.model, components.context_budget);
    components.events = plugins.wrap_events(components.events)?;
    let agent = Agent::with_runtime_budget_mode(
        components.model,
        components.tools,
        components.context_factory,
        components.prompt,
        components.events,
        config.workdir,
        components.context_budget,
        components.defer_initial_session,
    )?;
    Ok(DefaultRuntime {
        agent,
        skills,
        plugins,
        startup_warnings: mcp_status
            .unavailable
            .into_iter()
            .chain(mcp_status.warnings)
            .collect(),
        plugin_warnings,
        selected_model_name,
        base_tools,
        base_prompt,
    })
}

pub async fn default_runtime(config: Config) -> Result<DefaultRuntime> {
    default_runtime_with(config, |_| Ok(())).await
}

pub async fn model_from_profile(
    profile: &crate::profiles::ProviderProfile,
) -> Result<(Arc<dyn ModelBackend>, ContextConfig)> {
    let config = Config::from_profile(profile, Default::default(), None)?;
    let (model, budget) = configured_model(&config).await?;
    Ok((Arc::new(model), budget))
}

async fn configured_model(config: &Config) -> Result<(ModelClient, ContextConfig)> {
    let mut model = ModelClient::new(
        &config.base_url,
        config.api_key.clone(),
        config.model.clone(),
    )?
    .with_protocol(&config.api_protocol)?
    .with_reasoning(
        config.reasoning_effort.clone(),
        config.deepseek_thinking.clone(),
    )
    .with_output_tokens(config.max_output_tokens)
    .with_thinking_levels(config.thinking_levels.clone());
    // A configured Chat context is enough to avoid the optional /models call.
    // Other protocols can also skip it if every discoverable limit and
    // thinking tier has been specified. Otherwise preserve capability
    // discovery (including image support) rather than guessing.
    let has_context =
        config.context_window.is_some() || std::env::var_os("KROMA_CONTEXT_WINDOW").is_some();
    let configured_fully = config.max_output_tokens.is_some() && !config.thinking_levels.is_empty();
    let limits = if has_context && (config.api_protocol == "chat" || configured_fully) {
        crate::runtime_log!("[model] using configured limits; skipping optional metadata lookup");
        None
    } else {
        model.discover_model().await
    };
    // Precedence: environment override, then the saved profile, then upstream
    // metadata, then Kroma's conservative default.
    let budget = ContextConfig::from_env_with_limits(
        config
            .context_window
            .or_else(|| limits.as_ref().and_then(|m| m.context_window)),
        model.effective_output_tokens(),
    )?;
    Ok((model, budget))
}

pub async fn default_agent(config: Config) -> Result<Agent> {
    Ok(default_runtime(config).await?.agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_model_and_context_budget_are_one_operation() {
        let first: Arc<dyn ModelBackend> =
            Arc::new(ModelClient::new("http://127.0.0.1:9/v1", None, "first".into()).unwrap());
        let next: Arc<dyn ModelBackend> =
            Arc::new(ModelClient::new("http://127.0.0.1:9/v1", None, "next".into()).unwrap());
        let initial = ContextConfig::with_output_reserve(16_384, 1024).unwrap();
        let desired = ContextConfig::with_output_reserve(262_144, 8192).unwrap();
        let mut components = RuntimeComponents {
            model: first,
            context_budget: initial,
            tools: ToolRegistry::builtin(),
            context_factory: Arc::new(BuiltinContextFactory),
            prompt: PromptComposer::default(),
            events: None,
            defer_initial_session: false,
        };
        components.replace_model(next.clone(), desired);
        assert!(Arc::ptr_eq(&components.model(), &next));
        assert_eq!(components.context_budget.window(), 262_144);
        assert_eq!(components.context_budget.output_reserve(), 8192);
    }

    #[tokio::test]
    async fn profile_limits_and_levels_reach_the_runtime() {
        // Port 9 refuses connections immediately, so model discovery falls
        // back to the profile exactly as it does for endpoints without
        // metadata.
        let profile = crate::profiles::ProviderProfile {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "mock".into(),
            api_key: None,
            deepseek_thinking: None,
            api_protocol: Some("chat".into()),
            reasoning_effort: Some("high".into()),
            context_window: Some(500_000),
            max_output_tokens: Some(128_000),
            thinking_levels: Some(vec!["medium".into(), "high".into(), "xhigh".into()]),
        };
        let (model, budget) = model_from_profile(&profile).await.unwrap();
        assert_eq!(budget.window(), 500_000);
        assert_eq!(budget.output_reserve(), 32_768);
        assert_eq!(budget.available_input(), 467_232);
        assert_eq!(model.reasoning_level().as_deref(), Some("high"));
        assert!(model.set_reasoning_level("low").is_err());
        model.set_reasoning_level("xhigh").unwrap();
        assert_eq!(model.reasoning_level().as_deref(), Some("xhigh"));
    }

    #[tokio::test]
    async fn fully_specified_non_chat_profiles_skip_optional_metadata_lookup() {
        use std::net::TcpListener;
        for protocol in ["responses", "anthropic"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let profile = crate::profiles::ProviderProfile {
                base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
                model: "mock".into(),
                api_key: None,
                deepseek_thinking: None,
                api_protocol: Some(protocol.into()),
                reasoning_effort: Some("high".into()),
                context_window: Some(240_000),
                max_output_tokens: Some(8192),
                thinking_levels: Some(vec!["medium".into(), "high".into()]),
            };
            let (_, budget) = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                model_from_profile(&profile),
            )
            .await
            .expect("explicit profile must skip metadata network call")
            .unwrap();
            assert_eq!(budget.window(), 240_000);
            assert_eq!(budget.output_reserve(), 8192);
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock,
                "{protocol} must not call /models when fully configured"
            );
        }
    }

    #[tokio::test]
    async fn complete_chat_profile_skips_slow_optional_model_discovery() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let profile = crate::profiles::ProviderProfile {
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            model: "deepseek".into(),
            api_key: None,
            deepseek_thinking: None,
            api_protocol: Some("chat".into()),
            reasoning_effort: Some("high".into()),
            context_window: Some(50_000),
            max_output_tokens: None,
            thinking_levels: None,
        };
        let (_, budget) = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            model_from_profile(&profile),
        )
        .await
        .expect("fully configured profile should not wait for /models")
        .unwrap();
        assert_eq!(budget.window(), 50_000);
        assert_eq!(budget.output_reserve(), 4096);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "metadata endpoint should not be contacted"
        );
    }
}
