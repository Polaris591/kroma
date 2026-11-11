//! The shipped Goal plugin is an ordinary plugin, so it gets its own test
//! binary: `ui.status` lines are process-global and must not race other
//! plugin tests in `plugins_e2e.rs`.

use anyhow::{Context, Result};
use kroma::{
    agent::Agent,
    commands::{CommandAction, CommandRegistry},
    context::{BuiltinContextFactory, ContextConfig},
    plugins::PluginCatalog,
    prompt::PromptComposer,
    provider::ModelClient,
    tools::ToolRegistry,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
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
            "kroma-goal-plugin-test-{}-{nanos}-{}",
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

/// Declares one command, one `before_turn_end` hook and one live status line,
/// and the host knows nothing about goals. This drives it through those
/// generic seams only.
#[tokio::test]
async fn example_goal_plugin_works_through_generic_seams_alone() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/goal");
    let temp = TestDirectory::new()?;
    let dir = temp.path().join("plugins").join("goal");
    fs::create_dir_all(&dir)?;
    for name in ["plugin.toml", "main.js"] {
        fs::copy(source.join(name), dir.join(name))?;
    }
    let global = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(global.clone(), state.clone(), temp.path())?
        .set_enabled("goal", true)?;
    let catalog = PluginCatalog::discover_at(global, state, temp.path())?;

    let mut registry = CommandRegistry::new();
    assert!(catalog.register_commands(&mut registry).is_empty());
    assert!(registry.knows("/goal"), "the plugin supplies /goal itself");
    assert!(
        catalog.turn_end_gate().is_some(),
        "the plugin supplies the turn-end gate; the core just calls it"
    );

    let model = ModelClient::new("http://127.0.0.1:9/v1", None, "mock".into())?;
    let mut agent = Agent::with_runtime_budget(
        Arc::new(model),
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory),
        PromptComposer::default(),
        None,
        temp.path().to_path_buf(),
        ContextConfig::with_output_reserve(32768, 1024)?,
    )?;
    let input = agent.task_input();
    let started = registry
        .execute_async("/goal 修复登录失败", &mut agent, &input)
        .await?
        .context("the goal command must answer")?;
    let CommandAction::Task(body) = &started.action else {
        panic!(
            "a goal command must hand the host one task: {:?}",
            started.action
        );
    };
    assert!(body.contains("修复登录失败"), "{body}");
    let status = registry
        .execute_async("/goal", &mut agent, &input)
        .await?
        .context("the status command must answer")?;
    assert!(status.output.contains("修复登录失败"), "{}", status.output);
    assert!(
        catalog
            .ui_statuses()
            .iter()
            .any(|line| line.contains("修复登录失败")),
        "the plugin pushes its own status line: {:?}",
        catalog.ui_statuses()
    );

    let advice = catalog
        .turn_end_gate()
        .expect("gate")
        .gate(&kroma::plugins::TurnEndFacts {
            session_id: agent.session_id().to_owned(),
            archive: agent.archive_path().display().to_string(),
            workdir: temp.path().display().to_string(),
            final_text: "all done".into(),
            tools: serde_json::json!([]),
        })
        .await;
    assert!(
        advice.is_some(),
        "an unfinished goal keeps the turn open through the generic gate"
    );

    let cancelled = registry
        .execute_async("/goal cancel", &mut agent, &input)
        .await?
        .context("cancel must answer")?;
    assert!(cancelled.output.contains("取消"), "{}", cancelled.output);
    assert!(
        catalog
            .ui_statuses()
            .iter()
            .all(|line| !line.contains("修复登录失败")),
        "a cancelled goal clears the live line: {:?}",
        catalog.ui_statuses()
    );
    Ok(())
}
