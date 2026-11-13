//! Default, dependency-free application composition.
use crate::{
    agent::Agent,
    child_policy::ChildPolicy,
    config::Config,
    context::{BuiltinContextFactory, ContextConfig, ContextFactory},
    events::EventSink,
    file_edit::FileEdit,
    file_read::FileRead,
    history_tools::{HistoryRead, HistorySearch},
    mcp,
    model::ModelBackend,
    model_catalog::{self, FieldSource, MetadataState, ModelInputs, ModelReport, UpstreamLimits},
    plugins::PluginCatalog,
    prompt::PromptComposer,
    provider::{MetadataStatus, ModelClient},
    skills::{LoadSkill, SkillCatalog},
    tools::ToolRegistry,
};
use anyhow::Result;
use std::sync::Arc;

/// The one startup composition point for CLI and future plugin hosts.
/// An extension changes components here; it never edits Agent's loop.
#[derive(Clone)]
pub struct RuntimeComponents {
    // A model and its context budget must change together. Keep these
    // private so an extension cannot accidentally use the previous model's
    // window after swapping the backend.
    pub(crate) model: Arc<dyn ModelBackend>,
    pub(crate) context_budget: ContextConfig,
    /// Limits resolved for `model`. Diagnostics never inherit the previous
    /// model report after a replacement of unknown provenance.
    pub(crate) model_report: Option<ModelReport>,
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

    /// Effective limits resolved for the current model, when the host knows
    /// them. A replacement without a report leaves `None`.
    pub fn model_report(&self) -> Option<&ModelReport> {
        self.model_report.as_ref()
    }

    /// Swapping to a model without a resolved report clears the old one
    /// instead of advertising another model window and output reserve.
    pub fn replace_model(&mut self, model: Arc<dyn ModelBackend>, budget: ContextConfig) {
        self.model = model;
        self.context_budget = budget;
        self.model_report = None;
    }

    /// Replace a model whose limits the host resolved explicitly.
    pub fn replace_model_with_report(
        &mut self,
        model: Arc<dyn ModelBackend>,
        budget: ContextConfig,
        report: ModelReport,
    ) {
        self.model = model;
        self.context_budget = budget;
        self.model_report = Some(report);
    }
}

pub struct DefaultRuntime {
    pub agent: Agent,
    pub skills: SkillCatalog,
    pub plugins: PluginCatalog,
    pub startup_warnings: Vec<String>,
    pub plugin_warnings: Vec<String>,
    pub selected_model_name: Option<String>,
    /// Host components (including `customize` contributions) captured before
    /// plugin tool/prompt overlays, so an idle reload re-applies plugins onto
    /// the same base instead of stacking them twice. Share existing MCP/Skill
    /// adapters; never reconnect or rebuild Agent Loop.
    pub base_tools: ToolRegistry,
    pub base_prompt: PromptComposer,
    recipe: RuntimeRecipe,
    capabilities: crate::capabilities::Capabilities,
}

type RuntimeModel = (Arc<dyn ModelBackend>, ContextConfig, Option<ModelReport>);

struct RuntimeRecipe {
    config: Config,
    host: RuntimeComponents,
    fallback_model: Option<RuntimeModel>,
    host_model: Option<RuntimeModel>,
}

struct PreparedRuntime {
    components: RuntimeComponents,
    skills: SkillCatalog,
    plugins: PluginCatalog,
    base_tools: ToolRegistry,
    base_prompt: PromptComposer,
    startup_warnings: Vec<String>,
    plugin_warnings: Vec<String>,
}

pub fn builtin_tool_names() -> Vec<String> {
    builtin_tools()
        .expect("unique builtin tool names")
        .definitions()
        .into_iter()
        .filter_map(|definition| definition["function"]["name"].as_str().map(str::to_owned))
        .collect()
}

fn builtin_tools() -> Result<ToolRegistry> {
    let mut tools = ToolRegistry::builtin();
    tools.register(Arc::new(HistorySearch))?;
    tools.register(Arc::new(HistoryRead))?;
    tools.register(Arc::new(FileRead))?;
    tools.register(Arc::new(FileEdit))?;
    Ok(tools)
}

impl RuntimeRecipe {
    async fn prepare(
        &mut self,
        capabilities: crate::capabilities::Capabilities,
        plugins: PluginCatalog,
    ) -> Result<PreparedRuntime> {
        let mut components = self.host.clone();
        if let Some((model, budget, report)) = self.host_model.clone() {
            components.model = model;
            components.context_budget = budget;
            components.model_report = report;
        } else if let Some((model, budget, name)) = plugins.checked_model_override()? {
            components.model_report = Some(model_catalog::plugin_report(
                &name,
                budget.window(),
                budget.output_reserve(),
                model.accepts_images(),
                model.thinking_levels(),
            ));
            components.model = model;
            components.context_budget = budget;
        } else {
            if self.fallback_model.is_none() {
                let (model, budget, report) = configured_model(&self.config).await?;
                self.fallback_model = Some((Arc::new(model), budget, Some(report)));
            }
            let (model, budget, report) =
                self.fallback_model.as_ref().expect("fallback initialized");
            components.model = model.clone();
            if self.host_model.is_none() {
                self.host.model = model.clone();
            }
            components.context_budget = *budget;
            components.model_report = report.clone();
        }
        let mut skills = SkillCatalog::discover(&self.config.workdir)?;
        skills.retain_enabled(&capabilities);
        if !skills.is_empty() {
            components
                .tools
                .register(Arc::new(LoadSkill(skills.clone())))?;
            if let Some(metadata) = skills.metadata_prompt() {
                components.prompt = components.prompt.with_trailing(metadata)?;
            }
        }
        let mut startup_warnings = Vec::new();
        let status =
            mcp::register_selected(&mut components.tools, &self.config.workdir, &capabilities)
                .await?;
        startup_warnings.extend(status.unavailable);
        startup_warnings.extend(status.warnings);
        let base_tools = components.tools.clone();
        let base_prompt = components.prompt.clone();
        let mut plugin_warnings = plugins.warnings.clone();
        plugin_warnings.extend(plugins.contribute(&mut components.tools, &mut components.prompt));
        components
            .tools
            .apply_capabilities(&self.host.tools, &capabilities);
        if components.context_factory.is_builtin() {
            components.context_factory = plugins.builtin_context_factory(&capabilities);
        }
        plugin_warnings.extend(plugins.replace_context(&mut components.context_factory));
        if let Some(policy) = ChildPolicy::from_env()? {
            policy.apply(&mut components.tools, &mut components.prompt)?;
        }
        components.model = plugins.model_hooks(components.model, components.context_budget);
        components.events = plugins.wrap_events(components.events)?;
        Ok(PreparedRuntime {
            components,
            skills,
            plugins,
            base_tools,
            base_prompt,
            startup_warnings,
            plugin_warnings,
        })
    }
}

impl DefaultRuntime {
    pub fn capabilities(&self) -> crate::capabilities::Capabilities {
        self.capabilities.clone()
    }

    pub fn capability_tool_definitions(&self) -> Vec<serde_json::Value> {
        let mut definitions = self.recipe.host.tools.definitions();
        definitions.extend(self.base_tools.definitions());
        definitions.extend(self.plugins.capability_tool_inventory());
        definitions
    }

    pub fn prepare_plugin_reload(
        &self,
        next: &mut PluginCatalog,
    ) -> Result<(ToolRegistry, PromptComposer)> {
        let (mut tools, prompt) =
            self.plugins
                .prepare_idle_reload_from(next, &self.base_tools, &self.base_prompt)?;
        tools.apply_capabilities(&self.recipe.host.tools, &self.capabilities);
        if let Some(policy) = ChildPolicy::from_env()? {
            let mut prompt = prompt;
            policy.apply(&mut tools, &mut prompt)?;
            return Ok((tools, prompt));
        }
        Ok((tools, prompt))
    }

    pub async fn resume_session(&mut self, id: &str) -> Result<()> {
        let next = crate::capabilities::effective_for(self.agent.workdir(), id)?;
        let previous = self.capabilities.clone();
        if next != previous {
            self.set_capabilities(next.clone()).await?;
        }
        if let Err(error) = self.agent.resume_session(id) {
            if next != previous {
                self.set_capabilities(previous).await?;
            }
            return Err(error);
        }
        self.plugins.clear_session_ui();
        Ok(())
    }

    pub async fn new_session(&mut self) -> Result<String> {
        let next = crate::capabilities::load_defaults()?;
        let previous = self.capabilities.clone();
        if next != previous {
            self.set_capabilities(next.clone()).await?;
        }
        match self.agent.new_session() {
            Ok(id) => {
                self.plugins.clear_session_ui();
                Ok(id)
            }
            Err(error) => {
                if next != previous {
                    self.set_capabilities(previous).await?;
                }
                Err(error)
            }
        }
    }

    pub async fn set_capabilities(
        &mut self,
        capabilities: crate::capabilities::Capabilities,
    ) -> Result<()> {
        if self.agent.is_running() {
            anyhow::bail!("A task is running; capability changes were not applied");
        }
        self.set_capabilities_with_plugin_defaults(capabilities, &Default::default())
            .await
    }

    pub async fn set_capabilities_with_plugin_defaults(
        &mut self,
        capabilities: crate::capabilities::Capabilities,
        plugin_defaults: &std::collections::BTreeMap<String, bool>,
    ) -> Result<()> {
        if self.agent.is_running() {
            anyhow::bail!("A task is running; capability changes were not applied");
        }
        self.update_capabilities(capabilities, plugin_defaults, || Ok(()))
            .await
    }

    /// Prepare and validate once, persist, then commit without a fallible step.
    /// A failed write leaves the original runtime and context untouched.
    pub async fn update_capabilities(
        &mut self,
        capabilities: crate::capabilities::Capabilities,
        plugin_defaults: &std::collections::BTreeMap<String, bool>,
        persist: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        if self.agent.is_running() {
            anyhow::bail!("A task is running; capability changes were not applied");
        }
        let mut plugins = PluginCatalog::discover(self.agent.workdir())?;
        plugins.preview_default_items(plugin_defaults)?;
        plugins.apply_capabilities(&capabilities);
        let prepared = self.recipe.prepare(capabilities.clone(), plugins).await?;
        let context = self.agent.prepare_runtime_context(&prepared.components)?;
        persist()?;
        self.install_prepared_context(prepared, capabilities, context);
        Ok(())
    }

    #[cfg(test)]
    fn install_prepared(
        &mut self,
        prepared: PreparedRuntime,
        capabilities: crate::capabilities::Capabilities,
    ) -> Result<()> {
        let context = self.agent.prepare_runtime_context(&prepared.components)?;
        self.install_prepared_context(prepared, capabilities, context);
        Ok(())
    }

    fn install_prepared_context(
        &mut self,
        prepared: PreparedRuntime,
        capabilities: crate::capabilities::Capabilities,
        context: Box<dyn crate::context::ContextPolicy>,
    ) {
        let PreparedRuntime {
            components,
            skills,
            plugins,
            base_tools,
            base_prompt,
            startup_warnings,
            plugin_warnings,
        } = prepared;
        let selected_model_name = components.model_report.as_ref().and_then(|report| {
            (report.model != self.recipe.config.model).then(|| report.model.clone())
        });
        self.agent
            .replace_runtime(components, capabilities.clone(), context);
        self.agent
            .attach_turn_gate(plugins.turn_end_gate().map(Arc::new));
        self.skills = skills;
        self.plugins = plugins;
        self.base_tools = base_tools;
        self.base_prompt = base_prompt;
        self.startup_warnings = startup_warnings;
        self.plugin_warnings = plugin_warnings;
        self.selected_model_name = selected_model_name;
        self.capabilities = capabilities;
    }
}

pub async fn default_runtime_with(
    config: Config,
    customize: impl FnOnce(&mut RuntimeComponents) -> Result<()>,
) -> Result<DefaultRuntime> {
    let capabilities = crate::capabilities::effective_for(&config.workdir, "default")?;
    default_runtime_with_capabilities(config, capabilities, customize).await
}

pub async fn default_runtime_with_capabilities(
    config: Config,
    capabilities: crate::capabilities::Capabilities,
    customize: impl FnOnce(&mut RuntimeComponents) -> Result<()>,
) -> Result<DefaultRuntime> {
    let mut plugins = PluginCatalog::discover(&config.workdir)?;
    plugins.apply_capabilities(&capabilities);
    let override_model = plugins.checked_model_override()?;
    let plugin_model = override_model.is_some();
    let (model, budget, report) = if let Some((model, budget, name)) = override_model {
        let report = model_catalog::plugin_report(
            &name,
            budget.window(),
            budget.output_reserve(),
            model.accepts_images(),
            model.thinking_levels(),
        );
        (model, budget, report)
    } else {
        let (model, budget, report) = configured_model(&config).await?;
        (Arc::new(model) as Arc<dyn ModelBackend>, budget, report)
    };
    let original_model = model.clone();
    let fallback_model = (!plugin_model).then(|| (model.clone(), budget, Some(report.clone())));
    let mut components = RuntimeComponents {
        model,
        context_budget: budget,
        model_report: Some(report),
        tools: builtin_tools()?,
        context_factory: Arc::new(BuiltinContextFactory),
        prompt: PromptComposer::default(),
        events: None,
        defer_initial_session: false,
    };
    customize(&mut components)?;
    let host_model = (!Arc::ptr_eq(&components.model, &original_model)).then(|| {
        (
            components.model.clone(),
            components.context_budget,
            components.model_report.clone(),
        )
    });
    let mut recipe = RuntimeRecipe {
        config,
        host: components,
        fallback_model,
        host_model,
    };
    let PreparedRuntime {
        components,
        skills,
        plugins,
        base_tools,
        base_prompt,
        startup_warnings,
        plugin_warnings,
    } = recipe.prepare(capabilities.clone(), plugins).await?;
    let selected_model_name = components.model_report.as_ref().and_then(|report| {
        (plugin_model || report.model != recipe.config.model).then(|| report.model.clone())
    });
    let model_report = components.model_report.clone();
    let mut agent = Agent::with_capability_runtime(
        components,
        recipe.config.workdir.clone(),
        capabilities.clone(),
    )?;
    if let Some(policy) = ChildPolicy::from_env()? {
        agent.set_execution_limits(policy.limits());
    }
    if let Some(report) = model_report {
        agent.set_model_report(report);
    }
    agent.attach_turn_gate(plugins.turn_end_gate().map(Arc::new));
    agent.enable_checkpoints();
    Ok(DefaultRuntime {
        agent,
        skills,
        plugins,
        startup_warnings,
        plugin_warnings,
        selected_model_name,
        base_tools,
        base_prompt,
        recipe,
        capabilities,
    })
}

pub async fn default_runtime(config: Config) -> Result<DefaultRuntime> {
    default_runtime_with(config, |_| Ok(())).await
}

pub async fn model_from_profile(
    profile: &crate::profiles::ProviderProfile,
) -> Result<(Arc<dyn ModelBackend>, ContextConfig, ModelReport)> {
    let config = Config::from_profile(profile, Default::default(), None)?;
    let (model, budget, report) = configured_model(&config).await?;
    Ok((Arc::new(model), budget, report))
}

async fn configured_model(config: &Config) -> Result<(ModelClient, ContextConfig, ModelReport)> {
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
    .with_thinking_levels(config.thinking_levels.clone())
    // Endpoint capability, never inferred from the model name.
    .with_native_web_search(config.native_web_search)
    // Explicit, validated profile cache controls; defaults are unchanged.
    .with_cache_policy(config.cache_policy.clone());
    // Validate an explicit alias before doing any optional network work.
    let catalog = model_catalog::select(
        &config.base_url,
        &config.model,
        config.catalog_model.as_deref(),
    )?;
    // Native protocols need capabilities beyond user-configured token limits
    // (for example Anthropic thinking types). Never treat configured tiers as
    // proof that a mode is supported by the endpoint.
    let configured_native =
        config.max_output_tokens.is_some() && !config.thinking_levels.is_empty();
    let limits = if config.context_window.is_some()
        && (config.api_protocol == "chat"
            || (config.api_protocol == "responses" && configured_native))
    {
        crate::runtime_log!("[model] using configured limits; skipping optional metadata lookup");
        None
    } else {
        model.discover_model().await
    };
    let metadata = match model.metadata_status() {
        MetadataStatus::NotQueried => MetadataState::NotQueried,
        MetadataStatus::Available => MetadataState::Available,
        MetadataStatus::Unavailable => MetadataState::Unavailable,
    };
    let upstream = UpstreamLimits {
        context_window: limits.as_ref().and_then(|info| info.context_window),
        max_output_tokens: limits.as_ref().and_then(|info| info.max_output_tokens),
        image_input: limits.as_ref().and_then(|info| info.image_input),
    };
    let inputs = ModelInputs {
        model: &config.model,
        base_url: &config.base_url,
        context_window: config.context_window,
        context_source: config.context_window_source.unwrap_or(FieldSource::Unknown),
        requested_output: config.max_output_tokens,
        requested_output_source: config
            .max_output_tokens_source
            .unwrap_or(FieldSource::Unknown),
        thinking_levels: &config.thinking_levels,
        thinking_levels_source: config
            .thinking_levels_source
            .unwrap_or(FieldSource::Unknown),
    };
    let resolved = model_catalog::resolve(
        &inputs,
        &upstream,
        metadata,
        model.metadata_detail().map(str::to_owned),
        catalog,
    );
    // Catalog values only fill fields the endpoint did not provide. They are
    // also used by ModelClient for image acceptance and output hard caps.
    model = model.with_catalog_defaults(
        resolved.catalog_max_output_tokens,
        resolved.catalog_image_input,
    );
    model = model.with_thinking_levels(
        resolved
            .report
            .thinking_levels
            .value
            .clone()
            .unwrap_or_default(),
    );
    // Responses and Anthropic always send an explicit output limit. Even
    // when /models has no hard maximum, their default wire value is 4096;
    // reserve that same amount instead of an unrelated window/8 heuristic.
    let resolved_output = resolved.effective_turn_output;
    let effective_output =
        model_catalog::effective_wire_output(&config.api_protocol, resolved.effective_turn_output);
    let mut report =
        model_catalog::annotate_wire_output(resolved.report, &config.api_protocol, resolved_output);
    if config.api_protocol != "chat" && config.max_output_tokens.is_none() {
        report.requested_output_tokens.note =
            Some("provider default; Kroma sends an explicit native output cap".into());
    }
    let mut budget = ContextConfig::from_limits(resolved.context_window, effective_output)?;
    if let Some(limit) = resolved.catalog_max_input_tokens {
        budget = budget.with_input_limit(limit)?;
    }
    let reserve_source = if std::env::var_os("KROMA_OUTPUT_RESERVE_TOKENS").is_some() {
        FieldSource::Environment
    } else if effective_output.is_some() {
        FieldSource::Computed
    } else {
        FieldSource::Default
    };
    let report = model_catalog::finish_report(report, budget.output_reserve(), reserve_source);
    Ok((model, budget, report))
}

pub async fn default_agent(config: Config) -> Result<Agent> {
    Ok(default_runtime(config).await?.agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabling_plugins_removes_model_context_hooks_tools_and_ui() -> Result<()> {
        use crate::capabilities::Capabilities;
        use std::{
            fs,
            time::{SystemTime, UNIX_EPOCH},
        };
        let root = std::env::temp_dir().join(format!(
            "kroma-runtime-plugin-capabilities-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let workdir = root.join("project");
        let plugin_dir = root.join("plugins/probe");
        fs::create_dir_all(&workdir)?;
        fs::create_dir_all(&plugin_dir)?;
        fs::write(
            plugin_dir.join("plugin.toml"),
            r#"
api_version=2
id='probe'
name='Probe'
command='node'
args=['main.js']
prompt='CAPABILITY-PLUGIN-PROMPT'
hooks=['before_model_request']
[model]
protocol='plugin'
model='capability-plugin-model'
context_window=32768
output_reserve=2048
[context]
version=2
[ui]
status='CAPABILITY-PLUGIN-UI'
[[tools]]
name='capability_plugin_tool'
description='Probe'
[tools.parameters]
type='object'
[[tools]]
name='shell'
description='Plugin-owned replacement, independent of builtin tools'
replace=true
[tools.parameters]
type='object'
"#,
        )?;
        fs::write(
            plugin_dir.join("main.js"),
            "throw new Error('this fixture must remain lazy');\n",
        )?;
        let state = root.join("plugins.toml");
        let catalog = PluginCatalog::discover_at(root.join("plugins"), state.clone(), &workdir)?;
        catalog.set_enabled("probe", true).map_err(|error| {
            anyhow::anyhow!("{error}; discovery warnings: {:?}", catalog.warnings)
        })?;
        let profile = crate::profiles::ProviderProfile {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "fallback-model".into(),
            api_key: None,
            api_key_env: None,
            deepseek_thinking: None,
            api_protocol: Some("chat".into()),
            reasoning_effort: Some("auto".into()),
            context_window: Some(65536),
            max_output_tokens: Some(4096),
            thinking_levels: None,
            catalog_model: None,
            native_web_search: None,
            anthropic_cache_ttl: None,
            anthropic_stable_breakpoint: None,
            responses_prompt_cache_key: None,
        };
        let config = Config::from_profile(&profile, workdir.clone(), None)?;
        let disabled = Capabilities {
            tools: false,
            mcp: false,
            skills: false,
            plugins: false,
            ..Default::default()
        };
        let mut runtime =
            default_runtime_with_capabilities(config, disabled.clone(), |components| {
                components.defer_initial_session = true;
                Ok(())
            })
            .await?;
        let catalog = PluginCatalog::discover_at(root.join("plugins"), state, &workdir)?;
        let mut enabled = disabled.clone();
        enabled.items.plugins.insert("probe".into(), true);
        let mut catalog = catalog;
        catalog.apply_capabilities(&enabled);
        let prepared = runtime.recipe.prepare(enabled.clone(), catalog).await?;
        runtime.install_prepared(prepared, enabled.clone())?;
        assert_eq!(
            runtime.agent.model_report().unwrap().model,
            "capability-plugin-model"
        );
        let names = runtime.agent.available_tool_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"shell".to_owned()));
        assert!(names.contains(&"capability_plugin_tool".to_owned()));
        let mut reloaded =
            PluginCatalog::discover_at(root.join("plugins"), root.join("plugins.toml"), &workdir)?;
        let (tools, _) = runtime.prepare_plugin_reload(&mut reloaded)?;
        assert_eq!(tools.definitions().len(), 2);
        assert!(!tools
            .definitions()
            .iter()
            .any(|tool| tool["function"]["name"] == "history_read"));
        assert!(!runtime.plugins.ui_statuses().is_empty());
        let old = runtime.agent.new_session()?;
        let old_path = runtime.agent.archive_path().to_path_buf();
        let snapshot: serde_json::Value = serde_json::from_slice(&fs::read(&old_path)?)?;
        assert_eq!(snapshot["compression"]["strategy"], "probe");
        runtime.set_capabilities(disabled.clone()).await?;
        assert_eq!(
            runtime.agent.model_report().unwrap().model,
            "fallback-model"
        );
        assert!(runtime.agent.available_tool_names().is_empty());
        assert!(runtime.plugins.ui_statuses().is_empty());
        let mut projection = vec![crate::protocol::Message::new("system", "base")];
        runtime.base_prompt.apply(&mut projection);
        assert!(!projection[0]
            .content
            .as_ref()
            .unwrap()
            .contains("CAPABILITY-PLUGIN-PROMPT"));
        runtime.agent.enter_welcome()?;
        runtime.agent.resume_session(&old)?;
        assert_eq!(runtime.agent.session_id(), old);
        // Old external-context archives reopen with plugins disabled without
        // changing their persisted strategy or dropping their transcript, and
        // preserve that metadata across subsequent saves and re-enabling.
        let snapshot: serde_json::Value = serde_json::from_slice(&fs::read(&old_path)?)?;
        assert_eq!(snapshot["compression"]["strategy"], "probe");
        let _ = runtime.agent.compact().await.ok();
        let snapshot: serde_json::Value = serde_json::from_slice(&fs::read(&old_path)?)?;
        assert_eq!(snapshot["compression"]["strategy"], "probe");
        runtime.set_capabilities(enabled).await?;
        runtime.agent.resume_session(&old)?;
        assert_eq!(runtime.agent.session_id(), old);
        let snapshot: serde_json::Value = serde_json::from_slice(&fs::read(&old_path)?)?;
        assert_eq!(snapshot["compression"]["strategy"], "probe");
        drop(runtime);
        fs::remove_file(&old_path)?;
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn replacement_model_and_context_budget_are_one_operation() {
        let first: Arc<dyn ModelBackend> =
            Arc::new(ModelClient::new("http://127.0.0.1:9/v1", None, "first".into()).unwrap());
        let next: Arc<dyn ModelBackend> =
            Arc::new(ModelClient::new("http://127.0.0.1:9/v1", None, "next".into()).unwrap());
        let initial = ContextConfig::with_output_reserve(16_384, 1024).unwrap();
        let desired = ContextConfig::with_output_reserve(262_144, 8192).unwrap();
        let report = model_catalog::plugin_report("first", 16_384, 1024, None, &[]);
        let mut components = RuntimeComponents {
            model: first,
            context_budget: initial,
            model_report: Some(report),
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
        assert!(
            components.model_report().is_none(),
            "an unreported replacement must not keep the previous report"
        );
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
            api_key_env: None,
            deepseek_thinking: None,
            api_protocol: Some("chat".into()),
            reasoning_effort: Some("high".into()),
            context_window: Some(500_000),
            max_output_tokens: Some(128_000),
            thinking_levels: Some(vec!["medium".into(), "high".into(), "xhigh".into()]),
            catalog_model: None,
            native_web_search: None,
            anthropic_cache_ttl: None,
            anthropic_stable_breakpoint: None,
            responses_prompt_cache_key: None,
        };
        let (model, budget, report) = model_from_profile(&profile).await.unwrap();
        assert_eq!(budget.window(), 500_000);
        assert_eq!(budget.output_reserve(), 32_768);
        assert_eq!(budget.available_input(), 467_232);
        assert_eq!(model.reasoning_level().as_deref(), Some("high"));
        assert!(model.set_reasoning_level("low").is_err());
        model.set_reasoning_level("xhigh").unwrap();
        assert_eq!(model.reasoning_level().as_deref(), Some("xhigh"));
        assert_eq!(report.context_window.value, Some(500_000));
        assert_eq!(report.output_reserve.value, Some(32_768));
    }

    #[tokio::test]
    async fn fully_specified_responses_profile_skips_optional_metadata_lookup() {
        use std::net::TcpListener;
        for protocol in ["responses"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let profile = crate::profiles::ProviderProfile {
                base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
                model: "mock".into(),
                api_key: None,
                api_key_env: None,
                deepseek_thinking: None,
                api_protocol: Some(protocol.into()),
                reasoning_effort: Some("high".into()),
                context_window: Some(240_000),
                max_output_tokens: Some(8192),
                thinking_levels: Some(vec!["medium".into(), "high".into()]),
                catalog_model: None,
                native_web_search: None,
                anthropic_cache_ttl: None,
                anthropic_stable_breakpoint: None,
                responses_prompt_cache_key: None,
            };
            let (_, budget, _) = tokio::time::timeout(
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
    async fn unknown_native_model_reserves_the_exact_output_cap_sent_on_wire() -> Result<()> {
        for protocol in ["responses", "anthropic"] {
            let profile = crate::profiles::ProviderProfile {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "unknown".into(),
                api_key: None,
                api_key_env: None,
                deepseek_thinking: None,
                api_protocol: Some(protocol.into()),
                reasoning_effort: Some("auto".into()),
                context_window: Some(8192),
                max_output_tokens: None,
                thinking_levels: None,
                catalog_model: None,
                native_web_search: None,
                anthropic_cache_ttl: None,
                anthropic_stable_breakpoint: None,
                responses_prompt_cache_key: None,
            };
            let (_, budget, report) = model_from_profile(&profile).await?;
            assert_eq!(budget.output_reserve(), 4096, "{protocol}");
            assert_eq!(report.effective_turn_output_tokens.value, Some(4096));
            assert_eq!(report.output_reserve.value, Some(4096));
        }
        Ok(())
    }

    #[tokio::test]
    async fn offline_diagnostics_match_runtime_native_output_reserve() -> Result<()> {
        for protocol in ["responses", "anthropic"] {
            let profile = crate::profiles::ProviderProfile {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "unknown".into(),
                api_key: None,
                api_key_env: None,
                deepseek_thinking: None,
                api_protocol: Some(protocol.into()),
                reasoning_effort: Some("auto".into()),
                context_window: Some(8192),
                max_output_tokens: None,
                thinking_levels: None,
                catalog_model: None,
                native_web_search: None,
                anthropic_cache_ttl: None,
                anthropic_stable_breakpoint: None,
                responses_prompt_cache_key: None,
            };
            let config = Config::from_profile(&profile, Default::default(), None)?;
            let (offline, offline_report) = config.resolved_limits_without_probe()?;
            let (_, runtime, _) = model_from_profile(&profile).await?;
            assert_eq!(
                offline.output_reserve(),
                runtime.output_reserve(),
                "{protocol}: offline config show must not disagree with runtime"
            );
            assert_eq!(
                offline_report.effective_turn_output_tokens.value,
                Some(4096),
                "{protocol}"
            );
        }
        Ok(())
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
            api_key_env: None,
            deepseek_thinking: None,
            api_protocol: Some("chat".into()),
            reasoning_effort: Some("high".into()),
            context_window: Some(50_000),
            max_output_tokens: None,
            thinking_levels: None,
            catalog_model: None,
            native_web_search: None,
            anthropic_cache_ttl: None,
            anthropic_stable_breakpoint: None,
            responses_prompt_cache_key: None,
        };
        let (_, budget, _) = tokio::time::timeout(
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
