//! The shipped Todo plugin is an ordinary plugin, so it gets its own test
//! binary: `ui.status` lines are process-global and must not race the other
//! plugin tests in `plugins_e2e.rs`.

use anyhow::{Context, Result};
use kroma::{
    agent::Agent,
    commands::{CommandAction, CommandRegistry},
    context::{BuiltinContextFactory, ContextConfig},
    plugins::{PluginCatalog, WidgetPlacement},
    prompt::PromptComposer,
    protocol::{FunctionCall, ToolCall},
    provider::ModelClient,
    tools::{ToolContext, ToolRegistry},
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
            "kroma-todo-plugin-test-{}-{nanos}-{}",
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

/// The host knows nothing about todos: this drives the shipped plugin through
/// the registered command, the registered tool, the dynamic panel, the live
/// widget/status bus and the generic turn-end gate.
#[tokio::test]
async fn example_todo_plugin_works_through_generic_seams_alone() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/todo");
    let temp = TestDirectory::new()?;
    let dir = temp.path().join("plugins").join("todo");
    fs::create_dir_all(&dir)?;
    for name in ["plugin.toml", "main.js"] {
        fs::copy(source.join(name), dir.join(name))?;
    }
    let global = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(global.clone(), state.clone(), temp.path())?
        .set_enabled("todo", true)?;
    let catalog = PluginCatalog::discover_at(global, state, temp.path())?;

    let mut registry = CommandRegistry::new();
    assert!(catalog.register_commands(&mut registry).is_empty());
    assert!(registry.knows("/todo"), "the plugin supplies /todo itself");
    assert!(
        catalog.turn_end_gate().is_some(),
        "the plugin supplies the turn-end gate; the core just calls it"
    );

    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    assert!(catalog.contribute(&mut tools, &mut prompt).is_empty());
    let mut messages = prompt.initial_history(temp.path());
    prompt.apply(&mut messages);
    assert!(messages[0]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("todo_write"));
    let schema = tools
        .definitions()
        .into_iter()
        .find(|x| x["function"]["name"] == "todo_write")
        .expect("plugin tool loaded");
    assert_eq!(
        schema["function"]["parameters"]["properties"]["items"]["items"]["properties"]["status"]
            ["enum"][1],
        "doing"
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

    let empty = registry
        .execute_async("/todo", &mut agent, &input)
        .await?
        .context("the todo command must answer")?;
    assert!(empty.output.contains("还没有任务清单"), "{}", empty.output);

    let added = registry
        .execute_async("/todo add 写 README", &mut agent, &input)
        .await?
        .context("add must answer")?;
    assert!(
        added.output.contains("已加入：写 README"),
        "{}",
        added.output
    );
    assert!(
        catalog
            .ui_statuses()
            .iter()
            .any(|line| line == "📋 清单 0/1"),
        "the plugin pushes its own status line: {:?}",
        catalog.ui_statuses()
    );

    let planned = registry
        .execute_async("/todo plan 把插件装上", &mut agent, &input)
        .await?
        .context("plan must answer")?;
    let CommandAction::Task(body) = &planned.action else {
        panic!(
            "/todo plan must hand the host one task: {:?}",
            planned.action
        );
    };
    assert!(
        body.contains("todo_write") && body.contains("把插件装上"),
        "{body}"
    );

    // The model-facing tool replaces the whole list; the host owns no todo state.
    let archive = temp.path().join("archive");
    let call = ToolCall {
        id: "todo-1".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "todo_write".into(),
            arguments: serde_json::json!({
                "items": [
                    {"text": "读 src/agent.rs", "status": "done"},
                    {"text": "改 todo 插件", "status": "doing"}
                ]
            })
            .to_string(),
        },
    };
    let written = tools
        .execute(
            &call,
            ToolContext {
                model_connection: None,
                user_message: None,
                user_turn: None,
                user_images: None,
                previous_user_images: None,
                workdir: temp.path(),
                archive: &archive,
                on_output: None,
                ask: None,
            },
        )
        .await;
    assert!(!written.is_error, "{}", written.content);
    assert!(
        written.content.contains("清单已更新（1/2 完成）"),
        "{}",
        written.content
    );
    assert!(
        catalog
            .ui_statuses()
            .iter()
            .any(|line| line == "📋 清单 1/2 · 进行中：改 todo 插件"),
        "the live line follows the current item: {:?}",
        catalog.ui_statuses()
    );
    let widget = catalog
        .ui_widgets()
        .into_iter()
        .find(|widget| widget.id == "checklist")
        .expect("the plugin pushes its checklist card");
    assert_eq!(widget.placement, WidgetPlacement::AboveComposer);
    assert_eq!(widget.title, "任务清单");
    assert!(widget.text.contains("[>] 改 todo 插件"), "{}", widget.text);
    assert_eq!(widget.actions.len(), 2);

    let panel = catalog.ui_panel("todo")?;
    assert!(panel.contains("任务清单 1/2"), "{panel}");
    assert!(panel.contains("[x] 读 src/agent.rs"), "{panel}");
    assert_eq!(panel, catalog.ui_panel_async("todo").await?);

    // The gate stays out of the way unless `finish_guard` is enabled.
    let advice = catalog
        .turn_end_gate()
        .expect("gate")
        .gate(&kroma::plugins::TurnEndFacts {
            session_id: agent.session_id().to_owned(),
            archive: agent.archive_path().display().to_string(),
            workdir: temp.path().display().to_string(),
            final_text: "all done".into(),
            tools: serde_json::json!([{"name": "todo_write"}]),
        })
        .await;
    assert!(
        advice.is_none(),
        "the default must not hold the turn: {advice:?}"
    );

    let done = registry
        .execute_async("/todo done 2", &mut agent, &input)
        .await?
        .context("done must answer")?;
    assert!(done.output.contains("已把 2 标为 done"), "{}", done.output);
    assert!(
        catalog
            .ui_statuses()
            .iter()
            .any(|line| line == "✅ 清单完成 2/2"),
        "a finished checklist shows completion: {:?}",
        catalog.ui_statuses()
    );

    let cleared = registry
        .execute_async("/todo clear", &mut agent, &input)
        .await?
        .context("clear must answer")?;
    assert!(cleared.output.contains("清单已清空"), "{}", cleared.output);
    assert!(
        catalog
            .ui_statuses()
            .iter()
            .all(|line| !line.contains("清单")),
        "an empty checklist clears the live line: {:?}",
        catalog.ui_statuses()
    );
    Ok(())
}
