//! Host customization contracts for `default_runtime_with`: a model swapped by
//! a host must not keep the startup model's report, and host tools/prompt must
//! be the reload base that plugin overlays rebuild from.
use anyhow::Result;
use kroma::{
    app::default_runtime_with,
    config::Config,
    context::ContextConfig,
    model::ModelBackend,
    model_catalog,
    plugins::PluginCatalog,
    prompt::PromptComposer,
    protocol::{Message, ToolCall},
    provider::ModelClient,
    tools::{Tool, ToolContext, ToolFuture, ToolRegistry, ToolResult},
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kroma-runtime-customization-{}-{nanos}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// No report lookup: a configured chat window skips optional metadata.
fn config(workdir: PathBuf) -> Config {
    Config {
        base_url: "http://127.0.0.1:9/v1".into(),
        api_key: None,
        api_key_env: None,
        model: "startup-model".into(),
        workdir,
        deepseek_thinking: None,
        api_protocol: "chat".into(),
        reasoning_effort: "auto".into(),
        max_output_tokens: Some(4_096),
        max_output_tokens_source: None,
        context_window: Some(100_000),
        context_window_source: None,
        thinking_levels: Vec::new(),
        thinking_levels_source: None,
        catalog_model: None,
        catalog_model_source: None,
        native_web_search: false,
        cache_policy: Default::default(),
        selected_profile: None,
    }
}

fn replacement_model(name: &str) -> Result<Arc<dyn ModelBackend>> {
    Ok(Arc::new(ModelClient::new(
        "http://127.0.0.1:9/v1",
        None,
        name.into(),
    )?))
}

struct HostMarkerTool;

impl Tool for HostMarkerTool {
    fn name(&self) -> &str {
        "host_marker"
    }

    fn definition(&self) -> serde_json::Value {
        serde_json::json!({"type":"function","function":{
            "name":"host_marker",
            "description":"Host-registered tool used to verify reload bases.",
            "parameters":{"type":"object","properties":{}}
        }})
    }

    fn execute<'a>(&'a self, _call: &'a ToolCall, _ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async { ToolResult::error("host_marker is not executable in this test") })
    }
}

fn names(registry: &ToolRegistry) -> Vec<String> {
    registry
        .definitions()
        .iter()
        .filter_map(|definition| definition["function"]["name"].as_str().map(str::to_owned))
        .collect()
}

fn rendered_prompt(prompt: &PromptComposer) -> String {
    let mut view = vec![Message::new("system", "stored system prompt")];
    prompt.apply(&mut view);
    view[0].content.clone().unwrap_or_default()
}

fn write_probe_plugin(root: &Path) -> Result<()> {
    let plugin = root.join("reload-probe");
    fs::create_dir_all(&plugin)?;
    fs::write(
        plugin.join("plugin.toml"),
        r#"id = "reload-probe"
name = "Reload Probe"
version = "0.1.0"
description = "Test-only prompt and tool contribution"
command = "node"
args = ["main.js"]
prompt = "PLUGIN-RELOAD-MARKER"

[[tools]]
name = "reload_probe_tool"
description = "Test-only tool."

[tools.parameters]
type = "object"
additionalProperties = false
"#,
    )?;
    fs::write(plugin.join("main.js"), "module.exports = {}\n")?;
    Ok(())
}

#[tokio::test]
async fn item_tool_and_skill_filters_are_independent_and_failed_updates_are_atomic() -> Result<()> {
    use kroma::{app::default_runtime_with_capabilities, capabilities::Capabilities};
    use std::collections::BTreeMap;
    let temp = TestDirectory::new()?;
    for name in ["one", "two"] {
        let directory = temp.path().join(".kroma/skills").join(name);
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: MARKER-{name}\n---\nBODY-{name}\n"),
        )?;
    }
    let mut settings = Capabilities {
        mcp: false,
        plugins: false,
        ..Default::default()
    };
    settings.items.tools.insert("shell".into(), false);
    settings.items.tools.insert("host_marker".into(), false);
    settings.items.skills.insert("one".into(), false);
    settings.items.skills.insert("two".into(), true);
    let mut runtime = default_runtime_with_capabilities(
        config(temp.path().to_owned()),
        settings.clone(),
        |components| {
            components.defer_initial_session = true;
            components.tools.register(Arc::new(HostMarkerTool))?;
            Ok(())
        },
    )
    .await?;
    let history = serde_json::to_value(runtime.agent.messages())?;
    let names = runtime.agent.available_tool_names();
    assert!(!names.contains(&"shell".to_owned()));
    assert!(!names.contains(&"host_marker".to_owned()));
    assert!(names.contains(&"history_read".to_owned()));
    assert!(names.contains(&"load_skill".to_owned()));
    assert!(runtime.skills.load("one").is_err());
    assert!(runtime.skills.load("two")?.to_string().contains("BODY-two"));
    let prompt = rendered_prompt(&runtime.base_prompt);
    assert!(!prompt.contains("MARKER-one") && prompt.contains("MARKER-two"));
    let mut registry = runtime.base_tools.clone();
    registry.retain_enabled(&settings);
    let archive = temp.path().join("unused.json");
    let context = ToolContext {
        model_connection: None,
        user_message: None,
        user_turn: None,
        user_images: None,
        previous_user_images: None,
        workdir: temp.path(),
        archive: &archive,
        on_output: None,
        ask: None,
    };
    let call = ToolCall {
        id: "disabled".into(),
        kind: "function".into(),
        function: kroma::provider::FunctionCall {
            name: "host_marker".into(),
            arguments: "{}".into(),
        },
    };
    assert!(registry
        .execute(&call, context)
        .await
        .content
        .contains("unsupported tool"));
    let mut next = settings.clone();
    next.items.tools.insert("shell".into(), true);
    next.items.tools.insert("host_marker".into(), true);
    next.items.skills.insert("one".into(), true);
    next.items.skills.insert("two".into(), false);
    let writes = std::cell::Cell::new(0);
    let error = runtime
        .update_capabilities(next.clone(), &BTreeMap::new(), || {
            writes.set(writes.get() + 1);
            anyhow::bail!("fixture write failed")
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("fixture write failed"));
    assert_eq!(writes.get(), 1);
    assert_eq!(runtime.capabilities(), settings);
    assert_eq!(runtime.agent.available_tool_names(), names);
    assert_eq!(serde_json::to_value(runtime.agent.messages())?, history);
    assert!(runtime.skills.load("one").is_err());
    runtime
        .update_capabilities(next.clone(), &BTreeMap::new(), || {
            writes.set(writes.get() + 1);
            Ok(())
        })
        .await?;
    assert_eq!(writes.get(), 2);
    assert_eq!(runtime.capabilities(), next);
    assert!(runtime
        .agent
        .available_tool_names()
        .contains(&"host_marker".to_owned()));
    assert!(runtime
        .agent
        .available_tool_names()
        .contains(&"shell".to_owned()));
    assert!(runtime.skills.load("one").is_ok());
    assert!(runtime.skills.load("two").is_err());
    assert_eq!(serde_json::to_value(runtime.agent.messages())?, history);
    // Invalid preparation cannot write settings or replace the active runtime.
    fs::write(temp.path().join(".kroma/mcp.json"), "not JSON")?;
    let mut invalid = next.clone();
    invalid.mcp = true;
    assert!(runtime
        .update_capabilities(invalid, &BTreeMap::new(), || {
            writes.set(99);
            Ok(())
        })
        .await
        .is_err());
    assert_eq!(writes.get(), 2);
    assert_eq!(runtime.capabilities(), next);
    assert_eq!(serde_json::to_value(runtime.agent.messages())?, history);
    Ok(())
}

#[tokio::test]
async fn host_model_replacement_clears_the_startup_report() -> Result<()> {
    let temp = TestDirectory::new()?;
    let runtime = default_runtime_with(config(temp.path().to_path_buf()), |components| {
        assert!(
            components.model_report().is_some(),
            "the startup model report should be resolved before customization"
        );
        let budget = ContextConfig::with_output_reserve(65_536, 4_096)?;
        components.replace_model(replacement_model("host-replacement")?, budget);
        assert!(
            components.model_report().is_none(),
            "replacing an unresolved model must clear the previous report"
        );
        Ok(())
    })
    .await?;
    assert!(
        runtime.agent.model_report().is_none(),
        "the agent must not report the startup model's limits after a replacement"
    );
    assert_eq!(runtime.agent.context_window_tokens(), 65_536);
    assert_eq!(runtime.selected_model_name, None);
    Ok(())
}

#[tokio::test]
async fn explicit_replacement_report_reaches_the_agent() -> Result<()> {
    let temp = TestDirectory::new()?;
    let runtime = default_runtime_with(config(temp.path().to_path_buf()), |components| {
        let budget = ContextConfig::with_output_reserve(32_768, 2_048)?;
        components.replace_model_with_report(
            replacement_model("explicit-host-model")?,
            budget,
            model_catalog::plugin_report("explicit-host-model", 32_768, 2_048, None, &[]),
        );
        Ok(())
    })
    .await?;
    let report = runtime
        .agent
        .model_report()
        .expect("an explicit replacement report must be kept");
    assert_eq!(report.model, "explicit-host-model");
    assert_eq!(
        runtime.selected_model_name.as_deref(),
        Some("explicit-host-model")
    );
    assert_eq!(report.context_window.value, Some(32_768));
    assert_eq!(report.output_reserve.value, Some(2_048));
    Ok(())
}

#[tokio::test]
async fn idle_reload_starts_from_host_tools_and_prompt_without_plugins() -> Result<()> {
    let temp = TestDirectory::new()?;
    let workdir = temp.path().join("project");
    fs::create_dir_all(&workdir)?;
    let runtime = default_runtime_with(config(workdir.clone()), |components| {
        components.tools.register(Arc::new(HostMarkerTool))?;
        components.prompt = components
            .prompt
            .clone()
            .with_trailing("HOST-RELOAD-MARKER")?;
        Ok(())
    })
    .await?;

    // The reload base is host-only: plugin overlays are applied later and
    // must never be baked into it.
    let base_names = names(&runtime.base_tools);
    assert!(base_names.contains(&"host_marker".to_owned()));
    assert!(!base_names.contains(&"reload_probe_tool".to_owned()));
    assert!(rendered_prompt(&runtime.base_prompt).contains("HOST-RELOAD-MARKER"));

    let plugin_root = temp.path().join("plugins");
    write_probe_plugin(&plugin_root)?;
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(plugin_root.clone(), state.clone(), &workdir)?
        .set_enabled("reload-probe", true)?;
    let enabled = PluginCatalog::discover_at(plugin_root, state, &workdir)?;

    let (tools, prompt) = enabled.prepare_idle_reload(&runtime.base_tools, &runtime.base_prompt)?;
    let reloaded = names(&tools);
    for expected in ["host_marker", "reload_probe_tool"] {
        assert_eq!(
            reloaded.iter().filter(|name| *name == expected).count(),
            1,
            "tool '{expected}' must appear exactly once after an idle reload"
        );
    }
    let mut unique = reloaded.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), reloaded.len(), "duplicate tools after reload");

    let text = rendered_prompt(&prompt);
    for marker in ["HOST-RELOAD-MARKER", "PLUGIN-RELOAD-MARKER"] {
        assert_eq!(
            text.matches(marker).count(),
            1,
            "prompt marker '{marker}' must appear exactly once"
        );
    }
    assert!(runtime
        .agent
        .available_tool_names()
        .contains(&"host_marker".to_owned()));
    Ok(())
}

#[tokio::test]
async fn capability_toggles_keep_skills_independent_and_preserve_history() -> Result<()> {
    use kroma::{app::default_runtime_with_capabilities, capabilities::Capabilities};
    let temp = TestDirectory::new()?;
    let skill = temp.path().join(".agents/skills/capability-probe");
    fs::create_dir_all(&skill)?;
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: capability-probe\ndescription: CAPABILITY-SKILL-MARKER\n---\nSkill body.\n",
    )?;
    let disabled = Capabilities {
        tools: false,
        mcp: false,
        skills: false,
        plugins: false,
        ..Default::default()
    };
    let mut runtime = default_runtime_with_capabilities(
        config(temp.path().to_path_buf()),
        disabled.clone(),
        |components| {
            components.defer_initial_session = true;
            components.tools.register(Arc::new(HostMarkerTool))?;
            Ok(())
        },
    )
    .await?;
    let original = serde_json::to_value(runtime.agent.messages())?;
    assert_eq!(runtime.agent.available_tool_names(), vec!["host_marker"]);
    assert!(runtime.skills.is_empty());
    assert!(!rendered_prompt(&runtime.base_prompt).contains("CAPABILITY-SKILL-MARKER"));
    runtime
        .set_capabilities(Capabilities {
            skills: true,
            ..disabled.clone()
        })
        .await?;
    assert!(runtime
        .agent
        .available_tool_names()
        .contains(&"load_skill".to_owned()));
    assert!(!runtime
        .agent
        .available_tool_names()
        .contains(&"shell".to_owned()));
    assert!(rendered_prompt(&runtime.base_prompt).contains("CAPABILITY-SKILL-MARKER"));
    runtime.set_capabilities(disabled.clone()).await?;
    assert_eq!(runtime.agent.available_tool_names(), vec!["host_marker"]);
    assert!(!rendered_prompt(&runtime.base_prompt).contains("CAPABILITY-SKILL-MARKER"));
    assert_eq!(serde_json::to_value(runtime.agent.messages())?, original);
    assert_eq!(runtime.agent.session_id(), "unknown");
    let snapshot =
        kroma::desktop_host::snapshot_with_plugins(&runtime.agent, Some(&runtime.plugins))?;
    assert_eq!(snapshot["capabilities"], serde_json::to_value(disabled)?);
    assert_eq!(snapshot["skills"], serde_json::json!([]));
    Ok(())
}

#[tokio::test]
async fn capability_tools_switch_does_not_remove_lazy_mcp_and_off_skips_config() -> Result<()> {
    use kroma::{app::default_runtime_with_capabilities, capabilities::Capabilities};
    let temp = TestDirectory::new()?;
    fs::create_dir_all(temp.path().join(".kroma"))?;
    let mcp_file = temp.path().join(".kroma/mcp.json");
    fs::write(
        &mcp_file,
        r#"{"mcpServers":{"capability-probe":{"command":"this-program-must-not-be-started","env":{"SECRET":"not-for-ui"}}}}"#,
    )?;
    let capabilities = Capabilities {
        tools: false,
        mcp: true,
        skills: false,
        plugins: false,
        ..Default::default()
    };
    let mut runtime = default_runtime_with_capabilities(
        config(temp.path().to_path_buf()),
        capabilities.clone(),
        |components| {
            components.defer_initial_session = true;
            Ok(())
        },
    )
    .await?;
    assert!(runtime
        .agent
        .available_tool_names()
        .contains(&"mcp_search".to_owned()));
    assert!(runtime
        .agent
        .available_tool_names()
        .contains(&"mcp_call".to_owned()));
    assert!(!runtime
        .agent
        .available_tool_names()
        .contains(&"shell".to_owned()));
    let summaries = serde_json::to_string(&kroma::mcp::configured_server_summaries(temp.path())?)?;
    assert!(summaries.contains("capability-probe"));
    assert!(!summaries.contains("not-for-ui"));
    fs::write(mcp_file, "not json")?;
    runtime
        .set_capabilities(Capabilities {
            tools: true,
            mcp: false,
            ..capabilities
        })
        .await?;
    assert!(runtime
        .agent
        .available_tool_names()
        .contains(&"shell".to_owned()));
    assert!(!runtime
        .agent
        .available_tool_names()
        .contains(&"mcp_search".to_owned()));
    assert!(!runtime
        .agent
        .available_tool_names()
        .contains(&"mcp_call".to_owned()));
    Ok(())
}
