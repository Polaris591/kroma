use anyhow::{Context, Result};
use kroma::{
    agent::Agent,
    commands::CommandRegistry,
    context::{ContextConfig, ContextFactory},
    context_state::PersistedContext,
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, TextResult},
    plugins::PluginCatalog,
    ports::ContextCommit,
    prompt::PromptComposer,
    protocol::Message,
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
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);
struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kroma-plugin-test-{}-{nanos}-{}",
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

#[test]
fn local_plugin_install_follows_file_and_directory_symlinks_when_supported() -> Result<()> {
    let temp = TestDirectory::new()?;
    let source = temp.path().join("linked");
    let dependency = temp.path().join("dependency");
    fs::create_dir_all(&source)?;
    fs::create_dir_all(&dependency)?;
    fs::write(
        source.join("plugin.toml"),
        "id='linked'\nname='Linked dependencies'\nprompt='Dependencies are part of this plugin'\n",
    )?;
    fs::write(dependency.join("dep.js"), "module.exports = 42;")?;
    fs::write(temp.path().join("local.txt"), "linked content")?;
    #[cfg(unix)]
    let (file_ok, dir_ok) = {
        use std::os::unix::fs::symlink;
        (
            symlink(temp.path().join("local.txt"), source.join("alias.txt")).is_ok(),
            symlink(&dependency, source.join("deps")).is_ok(),
        )
    };
    #[cfg(windows)]
    let (file_ok, dir_ok) = {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        (
            symlink_file(temp.path().join("local.txt"), source.join("alias.txt")).is_ok(),
            symlink_dir(&dependency, source.join("deps")).is_ok(),
        )
    };
    let dest = temp.path().join("plugins");
    let catalog =
        PluginCatalog::discover_at(dest.clone(), temp.path().join("plugins.toml"), temp.path())?;
    catalog.install(&source)?;
    if file_ok {
        assert_eq!(
            fs::read_to_string(dest.join("linked/alias.txt"))?,
            "linked content"
        );
    }
    if dir_ok {
        assert_eq!(
            fs::read_to_string(dest.join("linked/deps/dep.js"))?,
            "module.exports = 42;"
        );
    }
    Ok(())
}

#[tokio::test]
async fn install_enable_configure_execute_disable() -> Result<()> {
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let state_path = temp.path().join("plugins.toml");
    let workspace = temp.path().join("project");
    fs::create_dir_all(&workspace)?;
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/hello");
    let discover = || PluginCatalog::discover_at(user.clone(), state_path.clone(), &workspace);

    let catalog = discover()?;
    assert!(catalog.list()?.contains("No plugins found"));
    fs::write(
        &state_path,
        "enabled = ['hello']\n[settings.hello]\nprefix = 'stale'\n",
    )?;
    assert!(catalog.install(&source)?.contains("Installed 'hello'"));
    let installed = discover()?;
    assert!(installed.list()?.contains("off  hello"));
    assert!(
        installed.show_config("hello")?.contains("Echo: "),
        "reinstall must clear old settings"
    );
    let mut disabled_tools = ToolRegistry::builtin();
    let mut disabled_prompt = PromptComposer::default();
    assert!(installed
        .contribute(&mut disabled_tools, &mut disabled_prompt)
        .is_empty());
    assert!(!disabled_tools
        .definitions()
        .iter()
        .any(|x| x["function"]["name"] == "example_echo"));
    let mut before = disabled_prompt.initial_history(&workspace);
    disabled_prompt.apply(&mut before);
    assert!(!before[0]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("Plugin hello:"));

    installed.set_enabled("hello", true)?;
    installed.set_config("hello", "prefix", Some("Configured: "))?;
    assert!(installed.list()?.contains("restart to apply"));
    assert!(installed.show_config("hello")?.contains("Configured: "));
    assert!(installed.set_enabled("../evil", true).is_err());
    let enabled = discover()?;
    assert!(enabled.list()?.contains("on   hello"));
    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    assert!(enabled.contribute(&mut tools, &mut prompt).is_empty());
    let mut messages = prompt.initial_history(&workspace);
    prompt.apply(&mut messages);
    assert!(messages[0]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("Plugin hello:"));
    let schema = tools
        .definitions()
        .into_iter()
        .find(|x| x["function"]["name"] == "example_echo")
        .expect("plugin tool loaded");
    assert_eq!(
        schema["function"]["parameters"]["properties"]["text"]["type"],
        "string"
    );

    if Command::new("node").arg("--version").output().is_ok() {
        let call = ToolCall {
            id: "tool-1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "example_echo".into(),
                arguments: r#"{"text":"first"}"#.into(),
            },
        };
        let archive = temp.path().join("archive");
        let first = tools
            .execute(
                &call,
                ToolContext {
                    workdir: &workspace,
                    archive: &archive,
                    on_output: None,
                },
            )
            .await;
        assert_eq!(first.content, "Configured: first (call 1)");
        assert!(!first.is_error);
        let second = tools
            .execute(
                &call,
                ToolContext {
                    workdir: &workspace,
                    archive: &archive,
                    on_output: None,
                },
            )
            .await;
        assert_eq!(
            second.content, "Configured: first (call 2)",
            "process must be reused between tool calls"
        );

        let mut commands = CommandRegistry::builtin(vec![])?;
        assert!(enabled.register_commands(&mut commands).is_empty());
        let client = Arc::new(ModelClient::new(
            "http://127.0.0.1:9/v1",
            None,
            "test".into(),
        )?);
        let mut agent = Agent::with_backends(client, ToolRegistry::builtin(), workspace.clone())?;
        let input = agent.task_input();
        let output = commands
            .execute_async("/echo third", &mut agent, &input)
            .await?
            .expect("plugin command registered")
            .output;
        assert_eq!(
            output, "Configured: third (call 3)",
            "commands and tools share one worker"
        );
        let dynamic_panel = enabled.ui_panel_async("hello").await?;
        assert!(
            dynamic_panel.contains("Requests handled: 3"),
            "{dynamic_panel}"
        );
        assert!(dynamic_panel.contains("Configured: "), "{dynamic_panel}");
        assert_eq!(
            enabled.ui_panel("hello")?,
            dynamic_panel,
            "sync CLI and async TUI panels should read the same worker state"
        );
        assert!(commands
            .execute("/plugins list", &mut agent, &input)?
            .unwrap()
            .output
            .contains("hello"));
        assert!(commands
            .execute("/plugins disable hello", &mut agent, &input)?
            .unwrap()
            .output
            .contains("Restart Kroma"));
    } else {
        installed.set_enabled("hello", false)?;
    }

    let disabled = discover()?;
    let mut new_tools = ToolRegistry::builtin();
    let mut new_prompt = PromptComposer::default();
    disabled.contribute(&mut new_tools, &mut new_prompt);
    assert!(!new_tools
        .definitions()
        .iter()
        .any(|x| x["function"]["name"] == "example_echo"));
    let mut after = new_prompt.initial_history(&workspace);
    new_prompt.apply(&mut after);
    assert!(!after[0]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("Plugin hello:"));
    disabled.set_config("hello", "prefix", None)?;
    assert!(disabled.show_config("hello")?.contains("Echo: "));
    Ok(())
}

#[tokio::test]
async fn subscribed_events_are_async_and_unsupported_deltas_are_ignored() -> Result<()> {
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let path = user.join("watch");
    fs::create_dir_all(&path)?;
    fs::write(
        path.join("plugin.toml"),
        "id = 'watch'\nname = 'Watcher'\ncommand = 'node'\nargs = ['main.js']\nevents = ['task_started']\n",
    )?;
    fs::write(
        path.join("main.js"),
        r#"const fs = require("node:fs");
require("node:readline").createInterface({input:process.stdin}).on("line", line => {
  const request = JSON.parse(line);
  fs.appendFileSync("received.log", JSON.stringify(request) + "\n");
  process.stdout.write(JSON.stringify({content:"ok"}) + "\n");
});
"#,
    )?;
    let state = temp.path().join("plugins.toml");
    let current = PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?;
    assert!(
        current.wrap_events(None)?.is_none(),
        "disabled events allocate no sink"
    );
    current.set_enabled("watch", true)?;
    let enabled = PluginCatalog::discover_at(user, state, temp.path())?;
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let sink = enabled.wrap_events(None)?.expect("subscribed event sink");
    sink.on_event(kroma::events::RuntimeEvent::TextDelta("not subscribed"));
    sink.on_event(kroma::events::RuntimeEvent::TaskStarted);
    let log = path.join("received.log");
    let mut received = None;
    for _ in 0..100 {
        if let Ok(text) = fs::read_to_string(&log) {
            received = Some(text);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
    let text = received.expect("event process never received task_started");
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1);
    let event: serde_json::Value = serde_json::from_str(lines[0])?;
    assert_eq!(event["type"], "event");
    assert_eq!(event["name"], "task_started");
    assert!(event["workdir"].as_str().is_some());
    drop(sink);
    Ok(())
}

#[tokio::test]
async fn tool_replacement_prompt_override_and_hooks_execute() -> Result<()> {
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let workdir = temp.path().join("project");
    fs::create_dir_all(&workdir)?;
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/transform");
    let old = PluginCatalog::discover_at(user.clone(), state.clone(), &workdir)?;
    old.install(&source)?;
    PluginCatalog::discover_at(user.clone(), state.clone(), &workdir)?
        .set_enabled("transform", true)?;
    let active = PluginCatalog::discover_at(user.clone(), state.clone(), &workdir)?;
    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    assert!(active.contribute(&mut tools, &mut prompt).is_empty());
    assert_eq!(tools.definitions().len(), 1);
    assert_eq!(tools.definitions()[0]["function"]["name"], "shell");
    let mut system = prompt.initial_history(&workdir);
    prompt.apply(&mut system);
    assert!(system[0]
        .content
        .as_deref()
        .unwrap()
        .contains("custom base prompt"));
    if Command::new("node").arg("--version").output().is_ok() {
        let call = ToolCall {
            id: "replacement".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "shell".into(),
                arguments: r#"{"command":"original"}"#.into(),
            },
        };
        let result = tools
            .execute(
                &call,
                ToolContext {
                    workdir: &workdir,
                    archive: temp.path(),
                    on_output: None,
                },
            )
            .await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "replaced-shell:modified:original:after-hook"
        );
        assert_eq!(
            call.function.arguments, r#"{"command":"original"}"#,
            "hook must not alter durable original call"
        );
    }
    active.set_enabled("transform", false)?;
    let restored = PluginCatalog::discover_at(user, state, &workdir)?;
    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    restored.contribute(&mut tools, &mut prompt);
    assert!(tools.is_builtin_only());
    assert!(!prompt.initial_history(&workdir)[0]
        .content
        .as_deref()
        .unwrap()
        .contains("custom base prompt"));
    Ok(())
}

#[derive(Default)]
struct StateRecorder(Mutex<Vec<PersistedContext>>);
impl ContextCommit for StateRecorder {
    fn commit(&self, state: &PersistedContext) -> Result<()> {
        self.0.lock().unwrap().push(state.clone());
        Ok(())
    }
}

#[tokio::test]
async fn external_context_policy_roundtrip_and_rewind_branch() -> Result<()> {
    let temp = TestDirectory::new()?;
    let global = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let local = temp.path().join("project");
    fs::create_dir_all(&local)?;
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/context-demo");
    PluginCatalog::discover_at(global.clone(), state.clone(), &local)?.install(&source)?;
    PluginCatalog::discover_at(global.clone(), state.clone(), &local)?
        .set_enabled("context-demo", true)?;
    let loaded = PluginCatalog::discover_at(global, state, &local)?;
    let mut factory: Arc<dyn ContextFactory> = Arc::new(kroma::context::BuiltinContextFactory);
    assert!(loaded.replace_context(&mut factory).is_empty());
    let init = factory.initial();
    assert!(matches!(init, PersistedContext::External(_)));
    let budget = ContextConfig::with_output_reserve(64000, 4096)?;
    let mut policy = factory.restore(init, budget, temp.path())?;
    let history = vec![
        Message::new("system", "system"),
        Message::new("user", "hi"),
        Message::new("assistant", "hello"),
    ];
    let model: Arc<dyn ModelBackend> = Arc::new(ModelClient::new(
        "http://127.0.0.1:9/v1",
        None,
        "mock".into(),
    )?);
    let commits = StateRecorder::default();
    policy.new_user_turn();
    policy.mark_consumed(history.len());
    let projected = policy.prepare(&history, &model, &commits, 0).await?;
    assert_eq!(projected.len(), history.len());
    assert_eq!(commits.0.lock().unwrap().len(), 1);
    let saved = policy.state();
    let PersistedContext::External(saved_state) = &saved else {
        panic!("missing external state")
    };
    assert_eq!(saved_state.data["revision"], 1);
    assert_eq!(
        saved_state.data["received"],
        serde_json::json!(["context_new_turn", "context_mark_consumed"])
    );
    policy.observe_with_bytes(&projected, 512, Some(100), Some(80));
    assert!(!policy.recover_overflow(&history, &model, &commits).await?);
    policy.finish_task(&history, &commits).await?;
    let PersistedContext::External(finished) = policy.state() else {
        panic!("external state missing")
    };
    assert_eq!(
        finished.data["finished"],
        serde_json::json!(["context_observe"])
    );
    assert_eq!(policy.take_usage().requests, 0);
    assert_eq!(
        commits.0.lock().unwrap().len(),
        2,
        "full-lifecycle context plugin must commit its final state"
    );
    let _reopened = factory.restore(saved.clone(), budget, temp.path())?;
    let branched =
        factory.branch_state(&saved, &history, &history[..2], temp.path(), temp.path())?;
    let PersistedContext::External(branch) = branched else {
        panic!("missing branch state")
    };
    assert_eq!(branch.data["revision"], 0);
    assert!(factory
        .restore(
            PersistedContext::External(kroma::context_state::ExternalContextState {
                strategy: "other".into(),
                version: 1,
                data: serde_json::Value::Null
            }),
            budget,
            temp.path()
        )
        .is_err());
    Ok(())
}

struct RejectContextCommit;
impl ContextCommit for RejectContextCommit {
    fn commit(&self, _state: &PersistedContext) -> Result<()> {
        anyhow::bail!("simulated persistence failure")
    }
}

#[tokio::test]
async fn context_policy_does_not_advance_state_when_commit_fails() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let directory = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/context-demo");
    PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?.install(&source)?;
    PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?
        .set_enabled("context-demo", true)?;
    let catalog = PluginCatalog::discover_at(directory, state, temp.path())?;
    let mut factory: Arc<dyn ContextFactory> = Arc::new(kroma::context::BuiltinContextFactory);
    assert!(catalog.replace_context(&mut factory).is_empty());
    let mut policy = factory.restore(
        factory.initial(),
        ContextConfig::with_output_reserve(64000, 4096)?,
        temp.path(),
    )?;
    let messages = vec![
        Message::new("system", "system"),
        Message::new("user", "hello"),
    ];
    let model: Arc<dyn ModelBackend> = Arc::new(ModelClient::new(
        "http://127.0.0.1:9/v1",
        None,
        "mock".into(),
    )?);
    let original = serde_json::to_value(policy.state())?;
    let error = policy
        .prepare(&messages, &model, &RejectContextCommit, 0)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("simulated persistence failure"));
    assert_eq!(
        serde_json::to_value(policy.state())?,
        original,
        "in-memory compaction state cannot outrun the durable journal"
    );
    Ok(())
}

#[derive(Default)]
struct CaptureModel(Mutex<Vec<Message>>, Mutex<Vec<serde_json::Value>>);
impl ModelBackend for CaptureModel {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [serde_json::Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            *self.0.lock().unwrap() = messages.to_vec();
            *self.1.lock().unwrap() = tools.to_vec();
            on_text("ok");
            Ok(Completion {
                message: Message::new("assistant", "ok"),
                usage: None,
                elapsed: Duration::ZERO,
                first_token: None,
            })
        })
    }
    fn generate_text<'a>(
        &'a self,
        _system: &'a str,
        _text: &'a str,
        _output_tokens: usize,
    ) -> ModelFuture<'a, TextResult> {
        Box::pin(async move {
            Ok(TextResult {
                text: "summary".into(),
                usage: None,
                elapsed: Duration::ZERO,
                requests: 1,
            })
        })
    }
}

#[tokio::test]
async fn process_model_backend_supports_non_http_protocol_and_correlated_jsonl() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/model-process");
    PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?.install(&source)?;
    PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?
        .set_enabled("model-process", true)?;
    let enabled = PluginCatalog::discover_at(user, state, temp.path())?;
    let (resolved, warnings) = enabled.model_override();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (backend, budget, name) = resolved.expect("process model must override");
    assert_eq!(name, "process-demo");
    assert_eq!(budget.window(), 32768);
    let history = [
        Message::new("system", "system"),
        Message::new("user", "hello"),
    ];
    let mut text = String::new();
    let reply = backend
        .complete(&history, &[], &mut |delta| text.push_str(delta))
        .await?;
    assert_eq!(text, "Model process replied");
    assert_eq!(
        reply.message.content.as_deref(),
        Some("Model process replied")
    );
    assert_eq!(
        reply.usage.as_ref().and_then(|usage| usage.cached),
        Some(30)
    );
    let summary = backend
        .generate_short_text("system", "summary", 128)
        .await?;
    assert_eq!(summary.text, "Summary");
    assert_eq!(summary.usage.and_then(|usage| usage.output), Some(2));
    Ok(())
}

#[test]
fn plugin_ui_shortcut_collisions_are_rejected() -> Result<()> {
    let temp = TestDirectory::new()?;
    let directory = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let hello = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/hello");
    PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?.install(&hello)?;
    let another = directory.join("other");
    fs::create_dir_all(&another)?;
    fs::write(another.join("plugin.toml"),
        "id='other'\nname='Other'\ncommand='node'\nargs=['main.js']\n[[commands]]\nname='/other'\n[[ui.shortcuts]]\nkey='Alt+E'\ncommand='/other'\n")?;
    let current = PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?;
    current.set_enabled("hello", true)?;
    current.set_enabled("other", true)?;
    let updated = PluginCatalog::discover_at(directory, state, temp.path())?;
    assert!(
        updated
            .ui_shortcuts()
            .unwrap_err()
            .to_string()
            .contains("Alt+e"),
        "conflicting shortcuts must not quietly override one another"
    );
    Ok(())
}

#[tokio::test]
async fn idle_reload_rebuilds_tools_prompt_commands_without_changing_history() -> Result<()> {
    let temp = TestDirectory::new()?;
    let workdir = temp.path().join("project");
    fs::create_dir_all(&workdir)?;
    let directory = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let hello = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/hello");
    let status = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/status");
    let model_hook = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/model-hook");
    let discover = || PluginCatalog::discover_at(directory.clone(), state.clone(), &workdir);
    discover()?.install(&hello)?;
    discover()?.install(&status)?;
    discover()?.install(&model_hook)?;
    let before = discover()?;
    let base_tools = ToolRegistry::builtin();
    let base_prompt = PromptComposer::default();
    let model = Arc::new(ModelClient::new(
        "http://127.0.0.1:9/v1",
        None,
        "mock".into(),
    )?);
    let mut agent = Agent::with_backends(model, base_tools.clone(), workdir.clone())?;
    let previous = serde_json::to_value(agent.messages())?;
    let (plain_tools, plain_prompt) = before.prepare_idle_reload(&base_tools, &base_prompt)?;
    agent.reload_extensions(plain_tools, plain_prompt, None);
    assert!(agent
        .available_tool_names()
        .iter()
        .all(|name| name != "example_echo"));

    before.set_enabled("hello", true)?;
    before.set_enabled("status", true)?;
    let mut updated = discover()?;
    let (tools, prompt) =
        before.prepare_idle_reload_from(&mut updated, &base_tools, &base_prompt)?;
    let mut cmds = CommandRegistry::builtin(vec![])?;
    assert!(updated.register_commands(&mut cmds).is_empty());
    agent.reload_extensions(tools, prompt, None);
    assert!(agent
        .available_tool_names()
        .contains(&"example_echo".to_string()));
    assert_eq!(updated.ui_statuses(), vec!["Plugin ready"]);
    assert_eq!(
        updated.ui_shortcuts()?,
        vec![('e', "/echo from shortcut".to_owned())]
    );
    if Command::new("node").arg("--version").output().is_ok() {
        let input = agent.task_input();
        let result = cmds
            .execute_async("/echo hot", &mut agent, &input)
            .await?
            .context("reloaded plugin command should exist")?;
        assert!(result.output.contains("hot"), "{}", result.output);
    }
    assert_eq!(
        serde_json::to_value(agent.messages())?,
        previous,
        "plugin reload must not change the persisted transcript"
    );
    // Reload the same manifests again: the Node process must stay alive,
    // rather than resetting its in-process counter from the first command.
    let mut same = discover()?;
    let (next_tools, next_prompt) =
        updated.prepare_idle_reload_from(&mut same, &base_tools, &base_prompt)?;
    let mut commands_same = CommandRegistry::builtin(vec![])?;
    assert!(same.register_commands(&mut commands_same).is_empty());
    agent.reload_extensions(next_tools, next_prompt, None);
    if Command::new("node").arg("--version").output().is_ok() {
        let input = agent.task_input();
        let result = commands_same
            .execute_async("/echo again", &mut agent, &input)
            .await?
            .context("plugin command should remain active")?;
        assert!(
            result.output.contains("(call 2)"),
            "worker was unexpectedly restarted: {}",
            result.output
        );
    }

    updated.set_enabled("hello", false)?;
    let mut without = discover()?;
    let (tools, prompt) =
        updated.prepare_idle_reload_from(&mut without, &base_tools, &base_prompt)?;
    agent.reload_extensions(tools, prompt, None);
    assert!(!agent
        .available_tool_names()
        .contains(&"example_echo".to_string()));
    assert_eq!(serde_json::to_value(agent.messages())?, previous);

    without.set_enabled("model-hook", true)?;
    let mut changed = discover()?;
    assert!(
        without
            .prepare_idle_reload_from(&mut changed, &base_tools, &base_prompt)
            .is_err(),
        "stateful model hooks require a full restart"
    );
    assert!(
        !agent
            .available_tool_names()
            .contains(&"example_echo".to_string()),
        "a rejected reload must keep the original runtime intact"
    );
    Ok(())
}

#[tokio::test]
async fn idle_reload_restarts_worker_only_when_entrypoint_file_changes() -> Result<()> {
    use std::io::Write;
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let directory = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/hello");
    let discover = || PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path());
    discover()?.install(&root)?;
    discover()?.set_enabled("hello", true)?;
    let mut original = discover()?;
    let base_tools = ToolRegistry::builtin();
    let base_prompt = PromptComposer::default();
    let mut commands = CommandRegistry::builtin(vec![])?;
    assert!(original.register_commands(&mut commands).is_empty());
    let backend = Arc::new(CaptureModel::default());
    let mut agent = Agent::with_backends(backend, base_tools.clone(), temp.path().to_path_buf())?;
    let input = agent.task_input();
    assert!(commands
        .execute_async("/echo one", &mut agent, &input)
        .await?
        .context("initial plugin command")?
        .output
        .contains("(call 1)"));
    let mut unchanged = discover()?;
    original.prepare_idle_reload_from(&mut unchanged, &base_tools, &base_prompt)?;
    original = unchanged;
    let mut commands = CommandRegistry::builtin(vec![])?;
    assert!(original.register_commands(&mut commands).is_empty());
    let input = agent.task_input();
    assert!(commands
        .execute_async("/echo two", &mut agent, &input)
        .await?
        .context("warm plugin command")?
        .output
        .contains("(call 2)"));
    // Source edits should opt out of worker reuse and start a fresh process.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(directory.join("hello").join("main.js"))?;
    file.write_all(b"\n// reload marker to invalidate the entrypoint fingerprint\n")?;
    drop(file);
    let mut changed = discover()?;
    original.prepare_idle_reload_from(&mut changed, &base_tools, &base_prompt)?;
    let mut commands = CommandRegistry::builtin(vec![])?;
    assert!(changed.register_commands(&mut commands).is_empty());
    let input = agent.task_input();
    assert!(
        commands
            .execute_async("/echo three", &mut agent, &input)
            .await?
            .context("reloaded plugin command")?
            .output
            .contains("(call 1)"),
        "an edited entrypoint should start a fresh worker"
    );
    Ok(())
}

#[tokio::test]
async fn idle_reload_ignores_broken_disabled_plugins_but_preserves_active_models() -> Result<()> {
    let temp = TestDirectory::new()?;
    let directory = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let workdir = temp.path().join("project");
    fs::create_dir_all(&workdir)?;
    let discover = || PluginCatalog::discover_at(directory.clone(), state.clone(), &workdir);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins");
    discover()?.install(&root.join("model-hook"))?;
    discover()?.install(&root.join("hello"))?;
    discover()?.set_enabled("model-hook", true)?;
    let initial = discover()?;
    initial.set_enabled("hello", true)?;
    let invalid = directory.join("broken");
    fs::create_dir_all(&invalid)?;
    fs::write(invalid.join("plugin.toml"), "invalid manifest = [")?;
    let mut next = discover()?;
    assert!(
        !next.warnings.is_empty(),
        "discovery should still report malformed plugins"
    );
    let (tools, _) = initial.prepare_idle_reload_from(
        &mut next,
        &ToolRegistry::builtin(),
        &PromptComposer::default(),
    )?;
    assert!(tools
        .definitions()
        .iter()
        .any(|definition| definition["function"]["name"] == "example_echo"));
    // An unchanged model-request hook must not prevent separate tools
    // from reloading. But disabling the active hook still needs restart.
    next.set_enabled("model-hook", false)?;
    let mut removed = discover()?;
    assert!(next
        .prepare_idle_reload_from(
            &mut removed,
            &ToolRegistry::builtin(),
            &PromptComposer::default()
        )
        .is_err());
    Ok(())
}

#[test]
fn idle_reload_preserves_unchanged_provider_and_context_but_blocks_actual_changes() -> Result<()> {
    let temp = TestDirectory::new()?;
    let directory = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins");
    let discover = || PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path());
    for name in ["model-http", "context-demo", "hello"] {
        discover()?.install(&root.join(name))?;
    }
    discover()?.set_enabled("model-http", true)?;
    discover()?.set_enabled("context-demo", true)?;
    let current = discover()?;
    current.set_enabled("hello", true)?;
    let mut next = discover()?;
    current.prepare_idle_reload_from(
        &mut next,
        &ToolRegistry::builtin(),
        &PromptComposer::default(),
    )?;
    // A real provider configuration edit must still require restart.
    let manifest = directory.join("model-http").join("plugin.toml");
    let content = fs::read_to_string(&manifest)?;
    fs::write(&manifest, content.replace("131072", "196608"))?;
    let mut changed = discover()?;
    assert!(next
        .prepare_idle_reload_from(
            &mut changed,
            &ToolRegistry::builtin(),
            &PromptComposer::default()
        )
        .is_err());
    // If the change is reverted, context strategy still remains the same.
    fs::write(&manifest, content)?;
    let mut unchanged = discover()?;
    next.prepare_idle_reload_from(
        &mut unchanged,
        &ToolRegistry::builtin(),
        &PromptComposer::default(),
    )?;
    Ok(())
}

#[test]
fn idle_reload_refuses_invalid_enabled_plugin_without_blocking_other_warnings() -> Result<()> {
    let temp = TestDirectory::new()?;
    let directory = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins");
    let discover = || PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path());
    discover()?.install(&root.join("hello"))?;
    discover()?.set_enabled("hello", true)?;
    let active = discover()?;
    fs::write(directory.join("hello").join("plugin.toml"), "invalid = [")?;
    let mut invalid = discover()?;
    assert!(!invalid.warnings.is_empty());
    let result = active.prepare_idle_reload_from(
        &mut invalid,
        &ToolRegistry::builtin(),
        &PromptComposer::default(),
    );
    assert!(
        result.is_err(),
        "invalid active plugin must not silently disappear"
    );
    Ok(())
}

#[tokio::test]
async fn streaming_model_emits_live_text_reasoning_tools_and_usage() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let directory = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/model-stream");
    PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?.install(&source)?;
    PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?
        .set_enabled("model-stream", true)?;
    let catalog = PluginCatalog::discover_at(directory, state, temp.path())?;
    let (selected, warnings) = catalog.model_override();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (backend, budget, name) = selected.context("streaming model not selected")?;
    assert_eq!(name, "stream-demo");
    assert_eq!(budget.window(), 32_768);
    assert_eq!(backend.reasoning_level().as_deref(), Some("medium"));
    assert_eq!(backend.thinking_levels(), &["low", "medium", "high"]);
    assert!(backend.set_reasoning_level("xhigh").is_err());
    backend.set_reasoning_level("high")?;
    assert_eq!(backend.reasoning_level().as_deref(), Some("high"));
    let history = [
        Message::new("system", "system"),
        Message::new("user", "hello"),
    ];
    let start = std::time::Instant::now();
    let mut texts = Vec::new();
    let mut thoughts = Vec::new();
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        backend.complete_with_reasoning(
            &history,
            &[],
            &mut |delta| texts.push((start.elapsed(), delta.to_string())),
            &mut |delta| thoughts.push((start.elapsed(), delta.to_string())),
        ),
    )
    .await??;
    assert_eq!(
        texts.iter().map(|(_, s)| s.as_str()).collect::<String>(),
        "Hello from stream."
    );
    assert_eq!(
        thoughts.iter().map(|(_, s)| s.as_str()).collect::<String>(),
        "Think first."
    );
    assert_eq!(texts.len(), 2);
    assert_eq!(thoughts.len(), 2);
    assert!(result
        .first_token
        .is_some_and(|first| first < result.elapsed));
    assert!(
        texts[0].0 < result.elapsed,
        "text must arrive before terminal completion"
    );
    assert_eq!(
        result.message.reasoning_content.as_deref(),
        Some("Think first.")
    );
    assert_eq!(
        result.message.content.as_deref(),
        Some("Hello from stream.")
    );
    assert_eq!(result.usage.as_ref().and_then(|u| u.cached), Some(75));
    let call = backend
        .complete(
            &history,
            &[serde_json::json!({
                "type":"function","function":{"name":"shell","parameters":{"type":"object"}}
            })],
            &mut |_| {},
        )
        .await?;
    let tool_calls = call
        .message
        .tool_calls
        .context("missing streamed tool call")?;
    assert_eq!(tool_calls[0].function.name, "shell");
    assert_eq!(tool_calls[0].id, "call-1");
    assert_eq!(
        tool_calls[0].function.arguments,
        "{\"command\":\"echo hi\"}"
    );
    let changed = [
        Message::new("system", "system"),
        Message::new("user", "show-level"),
    ];
    let tier = backend.complete(&changed, &[], &mut |_| {}).await?;
    assert_eq!(
        tier.message.content.as_deref(),
        Some("reasoning_level=high")
    );
    let mismatch = [
        Message::new("system", "system"),
        Message::new("user", "tool-mismatch"),
    ];
    let error = backend
        .complete(
            &mismatch,
            &[serde_json::json!({"type":"function"})],
            &mut |_| {},
        )
        .await
        .err()
        .context("tool stream mismatch must fail")?;
    assert!(
        error.to_string().contains("streamed tool calls differ"),
        "{error:#}"
    );
    assert_eq!(
        backend
            .generate_short_text("system", "summary", 100)
            .await?
            .text,
        "Stream summary"
    );
    Ok(())
}

#[tokio::test]
async fn streaming_model_cancellation_recovers_and_bad_frames_fail() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let directory = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/model-stream");
    PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?.install(&source)?;
    PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?
        .set_enabled("model-stream", true)?;
    let catalog = PluginCatalog::discover_at(directory, state, temp.path())?;
    let (selected, warnings) = catalog.model_override();
    assert!(warnings.is_empty());
    let (backend, _, _) = selected.context("model missing")?;
    let bad = [
        Message::new("system", "system"),
        Message::new("user", "bad-id"),
    ];
    let error = backend
        .complete(&bad, &[], &mut |_| {})
        .await
        .err()
        .context("invalid response should fail")?;
    assert!(
        format!("{error:#}").contains("request_id"),
        "unexpected error: {error:#}"
    );
    let stalled = [
        Message::new("system", "system"),
        Message::new("user", "stall"),
    ];
    let captured = Mutex::new(String::new());
    let (sent, received) = tokio::sync::oneshot::channel::<()>();
    let mut sent = Some(sent);
    let mut callback = |delta: &str| {
        captured.lock().unwrap().push_str(delta);
        if let Some(sender) = sent.take() {
            let _ = sender.send(());
        }
    };
    {
        let mut request = Box::pin(backend.complete(&stalled, &[], &mut callback));
        tokio::select! {
            signalled = received => { signalled.context("missing initial streamed text")?; }
            result = &mut request => { anyhow::bail!("stalled model returned unexpectedly: {:?}", result.err()); }
            _ = tokio::time::sleep(Duration::from_secs(5)) => anyhow::bail!("model did not stream initial text"),
        }
        // Dropping the request while it is waiting for its terminal frame
        // must kill that worker and preserve the next model request.
    }
    assert_eq!(captured.lock().unwrap().as_str(), "started");
    let normal = [
        Message::new("system", "system"),
        Message::new("user", "hello"),
    ];
    let reply = tokio::time::timeout(
        Duration::from_secs(8),
        backend.complete(&normal, &[], &mut |_| {}),
    )
    .await??;
    assert_eq!(
        reply.message.content.as_deref(),
        Some("Hello from stream."),
        "cancelled worker must not poison the next request"
    );
    Ok(())
}

#[tokio::test]
async fn plugin_jsonl_rejects_incorrect_response_correlation() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let dir = temp.path().join("plugins").join("wrong-id");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("plugin.toml"),
        "id='wrong-id'\nname='Wrong ID'\ncommand='node'\nargs=['main.js']\n[[tools]]\nname='wrong_id'\ndescription='test id'\n")?;
    fs::write(
        dir.join("main.js"),
        r#"require("node:readline").createInterface({input:process.stdin}).on("line",line=>{
 const request=JSON.parse(line);
 process.stdout.write(JSON.stringify({request_id:request.request_id+1,protocol_version:1,content:"wrong"})+"\n");
});"#,
    )?;
    let user = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?
        .set_enabled("wrong-id", true)?;
    let enabled = PluginCatalog::discover_at(user, state, temp.path())?;
    let mut tools = ToolRegistry::builtin();
    assert!(enabled
        .contribute(&mut tools, &mut PromptComposer::default())
        .is_empty());
    let call = ToolCall {
        id: "call".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "wrong_id".into(),
            arguments: "{}".into(),
        },
    };
    let result = tools
        .execute(
            &call,
            ToolContext {
                workdir: temp.path(),
                archive: temp.path(),
                on_output: None,
            },
        )
        .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("request_id mismatch"),
        "{}",
        result.content
    );
    Ok(())
}

#[tokio::test]
async fn model_replacement_and_pre_request_hook_keep_budget_and_original_history() -> Result<()> {
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    for plugin in ["model-hook", "model-http"] {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples/plugins")
            .join(plugin);
        PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?.install(&source)?;
        PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?
            .set_enabled(plugin, true)?;
    }
    let catalog = PluginCatalog::discover_at(user.clone(), state, temp.path())?;
    let mut model: Arc<dyn ModelBackend> = Arc::new(CaptureModel::default());
    let original = ContextConfig::with_output_reserve(32000, 1024)?;
    let mut budget = original;
    assert!(catalog.replace_model(&mut model, &mut budget).is_empty());
    assert_eq!(budget.window(), 131072);
    assert_eq!(budget.output_reserve(), 8192);
    // Independently verify interception with a deterministic mock model.
    let backend = Arc::new(CaptureModel::default());
    let intercepted = catalog.model_hooks(backend.clone(), budget);
    let messages = vec![
        Message::new("system", "system"),
        Message::new("user", "original"),
    ];
    if Command::new("node").arg("--version").output().is_ok() {
        let mut received = String::new();
        intercepted
            .complete(&messages, &[], &mut |chunk| received.push_str(chunk))
            .await?;
        assert_eq!(received, "ok");
        assert_eq!(
            backend.0.lock().unwrap()[1].content.as_deref(),
            Some("original [model-hook]")
        );
        assert_eq!(
            messages[1].content.as_deref(),
            Some("original"),
            "WAL source must stay intact"
        );
    }
    Ok(())
}

#[tokio::test]
async fn model_request_hooks_can_change_projection_and_tools_without_touching_wal() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let directory = temp.path().join("plugins").join("projector");
    fs::create_dir_all(&directory)?;
    fs::write(directory.join("plugin.toml"),
        "id='projector'\nname='Request projector'\ncommand='node'\nargs=['main.js']\nhooks=['before_model_request']\n")?;
    fs::write(
        directory.join("main.js"),
        r#"require("node:readline").createInterface({input:process.stdin})
.on("line",line=>{
 const req=JSON.parse(line);
 const messages=req.arguments.messages;
 messages.splice(1,0,{role:"assistant",content:"request-only note"});
 const tools=[{type:"function",function:{name:"custom_tool",description:"hooked",parameters:{type:"object"}}}];
 process.stdout.write(JSON.stringify({content:JSON.stringify({messages,tools})})+"\n");
});"#,
    )?;
    let global = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let discovered = PluginCatalog::discover_at(global.clone(), state.clone(), temp.path())?;
    discovered.set_enabled("projector", true)?;
    let catalog = PluginCatalog::discover_at(global, state, temp.path())?;
    let captured = Arc::new(CaptureModel::default());
    let budget = ContextConfig::with_output_reserve(32768, 1024)?;
    let model = catalog.model_hooks(captured.clone(), budget);
    let history = vec![
        Message::new("system", "original"),
        Message::new("user", "hello"),
    ];
    let definitions = ToolRegistry::builtin().definitions();
    let history_before = serde_json::to_value(&history)?;
    let tools_before = serde_json::to_value(&definitions)?;
    model.complete(&history, &definitions, &mut |_| {}).await?;
    let sent = captured.0.lock().unwrap().clone();
    let sent_tools = captured.1.lock().unwrap().clone();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1].content.as_deref(), Some("request-only note"));
    assert_eq!(sent_tools.len(), 1);
    assert_eq!(sent_tools[0]["function"]["name"], "custom_tool");
    assert_eq!(
        serde_json::to_value(&history)?,
        history_before,
        "provider projection must not modify durable chat history"
    );
    assert_eq!(
        serde_json::to_value(&definitions)?,
        tools_before,
        "provider projection must not modify Kroma's tool registry"
    );
    Ok(())
}

#[tokio::test]
async fn cancelling_stuck_plugin_call_kills_process_and_next_call_recovers() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let dir = temp.path().join("plugins").join("stall");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("plugin.toml"),
        "id='stall'\nname='Stall'\ncommand='node'\nargs=['main.js']\n[[tools]]\nname='stall'\ndescription='Test cancellation'\n")?;
    fs::write(
        dir.join("main.js"),
        r#"require("node:readline").createInterface({input:process.stdin})
.on("line",line=>{let r=JSON.parse(line);if(!r.arguments?.stall){
process.stdout.write(JSON.stringify({content:"recovered"})+"\n");}});"#,
    )?;
    let global = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(global.clone(), state.clone(), temp.path())?
        .set_enabled("stall", true)?;
    let loaded = PluginCatalog::discover_at(global, state, temp.path())?;
    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    loaded.contribute(&mut tools, &mut prompt);
    let mut call = ToolCall {
        id: "c".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "stall".into(),
            arguments: r#"{"stall":true}"#.into(),
        },
    };
    let ctx = ToolContext {
        workdir: temp.path(),
        archive: temp.path(),
        on_output: None,
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(200), tools.execute(&call, ctx))
            .await
            .is_err()
    );
    call.function.arguments = r#"{"stall":false}"#.into();
    let recovered = tokio::time::timeout(Duration::from_secs(5), tools.execute(&call, ctx)).await?;
    assert!(!recovered.is_error);
    assert_eq!(recovered.content, "recovered");
    Ok(())
}

#[tokio::test]
async fn web_tools_roundtrip_through_kroma_to_local_http_server() -> Result<()> {
    // The fixture intentionally listens on loopback; production defaults to
    // rejecting private-network fetches. The plugin child inherits this flag.
    std::env::set_var("KROMA_WEB_ALLOW_PRIVATE", "1");
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let server = TcpListener::bind("127.0.0.1:0")?;
    let address = server.local_addr()?;
    server.set_nonblocking(true)?;
    let server_thread = std::thread::spawn(move || -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(25);
        for _ in 0..2 {
            let (mut stream, _addr) = loop {
                match server.accept() {
                    Ok(accepted) => break accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() > deadline {
                            anyhow::bail!("mock HTTP test server timed out");
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            let mut bytes = [0u8; 4096];
            let mut request_bytes = Vec::new();
            loop {
                let n = stream.read(&mut bytes)?;
                if n == 0 {
                    anyhow::bail!("mock client closed before request headers");
                }
                request_bytes.extend_from_slice(&bytes[..n]);
                if request_bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
                if request_bytes.len() > 32 * 1024 {
                    anyhow::bail!("mock request headers too large");
                }
            }
            let request = String::from_utf8_lossy(&request_bytes);
            let (type_, body) = if request.starts_with("GET /search") {
                (
                    "application/json",
                    r#"{"results":[{"url":"http://example.org/demo","title":"Found","content":"Snippet"}]}"#,
                )
            } else {
                (
                    "text/html",
                    "<title>Mock Page</title><main><h1>Article</h1>Local content.</main>",
                )
            };
            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: {type_}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
            stream.write_all(response.as_bytes())?;
            stream.flush()?;
            // Give Windows TCP time to deliver the last response bytes.
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    });
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/web");
    PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?.install(&source)?;
    let catalog = PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?;
    catalog.set_enabled("web", true)?;
    catalog.set_config("web", "provider", Some("searxng"))?;
    catalog.set_config("web", "searxng_url", Some(&format!("http://{address}")))?;
    let enabled = PluginCatalog::discover_at(user, state, temp.path())?;
    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    assert!(enabled.contribute(&mut tools, &mut prompt).is_empty());
    assert!(tools
        .definitions()
        .iter()
        .any(|schema| schema["function"]["name"] == "web_search"));
    assert!(tools
        .definitions()
        .iter()
        .any(|schema| schema["function"]["name"] == "web_fetch"));
    let ctx = ToolContext {
        workdir: temp.path(),
        archive: temp.path(),
        on_output: None,
    };
    let search = ToolCall {
        id: "search".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "web_search".into(),
            arguments: r#"{"query":"test"}"#.into(),
        },
    };
    let found = tokio::time::timeout(Duration::from_secs(20), tools.execute(&search, ctx)).await?;
    assert!(!found.is_error, "web_search failed: {}", found.content);
    assert!(found.content.contains("Found"));
    let fetch = ToolCall {
        id: "fetch".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "web_fetch".into(),
            arguments: format!(r#"{{"url":"http://{address}/page"}}"#),
        },
    };
    let page = tokio::time::timeout(Duration::from_secs(20), tools.execute(&fetch, ctx)).await?;
    assert!(!page.is_error, "web_fetch failed: {}", page.content);
    assert!(page.content.contains("Article"));
    assert!(page.content.contains("Local content"));
    server_thread.join().unwrap()?;
    Ok(())
}

#[test]
fn declarative_tui_status_panel_only_from_enabled_plugin() -> Result<()> {
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/status");
    PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?.install(&source)?;
    let installed = PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?;
    assert!(installed.ui_statuses().is_empty());
    installed.set_enabled("status", true)?;
    let enabled = PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?;
    assert_eq!(enabled.ui_statuses(), vec!["Plugin ready"]);
    assert!(enabled.ui_panel("status")?.contains("Kroma Plugin Panel"));
    enabled.set_enabled("status", false)?;
    let disabled = PluginCatalog::discover_at(user, state, temp.path())?;
    assert!(disabled.ui_statuses().is_empty());
    Ok(())
}

#[test]
fn declarative_ui_slots_compose_and_detect_conflicting_owners() -> Result<()> {
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    for (id, frames) in [("a-ui", true), ("b-ui", true)] {
        let directory = user.join(id);
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join("plugin.toml"),
            format!(
                "id='{id}'\nname='UI slots'\n[ui]\nstatus='ready'\n{}\n[ui.layout]\nheader_rows=3\ncomposer_max_rows=6\nfull_width_prose=true\n[ui.colors]\naccent='#112233'\n[ui.tool_labels]\nexample_echo='Echo label'\n",
                if frames { "activity_frames=['a','b']" } else { "" }
            ),
        )?;
    }
    let state = temp.path().join("plugins.toml");
    let load = || PluginCatalog::discover_at(user.clone(), state.clone(), temp.path());
    assert!(load()?.ui_activity_frames()?.is_empty());
    assert!(load()?.ui_tool_labels()?.is_empty());
    assert!(load()?.ui_layout()?.is_none());
    assert!(load()?.ui_colors()?.is_empty());
    load()?.set_enabled("a-ui", true)?;
    assert_eq!(load()?.ui_activity_frames()?, vec!["a", "b"]);
    assert_eq!(load()?.ui_layout()?.unwrap().header_rows, Some(3));
    assert_eq!(
        load()?.ui_colors()?.get("accent").map(String::as_str),
        Some("#112233")
    );
    assert_eq!(
        load()?
            .ui_tool_labels()?
            .get("example_echo")
            .map(String::as_str),
        Some("Echo label")
    );
    load()?.set_enabled("b-ui", true)?;
    assert!(
        load()?.ui_activity_frames().is_err(),
        "animation slot must have one owner"
    );
    assert!(
        load()?.ui_layout().is_err(),
        "layout slot must have one owner"
    );
    assert!(
        load()?.ui_colors().is_err(),
        "theme color collision must be rejected"
    );
    assert!(
        load()?.ui_tool_labels().is_err(),
        "duplicate tool labels must be rejected"
    );
    load()?.set_enabled("b-ui", false)?;
    assert_eq!(load()?.ui_activity_frames()?.len(), 2);
    load()?.set_enabled("a-ui", false)?;
    assert!(load()?.ui_activity_frames()?.is_empty());

    let invalid = user.join("bad-frame");
    fs::create_dir_all(&invalid)?;
    fs::write(
        invalid.join("plugin.toml"),
        "id='bad-frame'\nname='Bad UI'\n[ui]\nactivity_frames=['x','this-is-very-wide']\n",
    )?;
    let catalog = load()?;
    assert!(catalog
        .warnings
        .iter()
        .any(|warning| warning.contains("bad-frame")));
    Ok(())
}

#[test]
fn invalid_plugin_does_not_prevent_loading_other_plugins() -> Result<()> {
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let broken = user.join("bad");
    fs::create_dir_all(&broken)?;
    fs::write(
        broken.join("plugin.toml"),
        "id = '../escape'\nname = 'Bad'\nprompt = 'X'\n",
    )?;
    let valid = user.join("good");
    fs::create_dir_all(&valid)?;
    fs::write(
        valid.join("plugin.toml"),
        "id = 'good'\nname = 'Good'\nprompt = 'ok'\n",
    )?;
    let state_path = temp.path().join("plugins.toml");
    let catalog = PluginCatalog::discover_at(user.clone(), state_path.clone(), temp.path())?;
    assert_eq!(catalog.warnings.len(), 1);
    assert!(catalog.list()?.contains("good"));
    assert!(catalog.set_enabled("good", true).is_ok());
    assert!(catalog.set_enabled("../escape", true).is_err());
    fs::write(&state_path, "not valid [toml")?;
    let fallback = PluginCatalog::discover_at(user, state_path, temp.path())?;
    assert_eq!(
        fallback.warnings.len(),
        2,
        "bad plugin and bad settings should warn"
    );
    assert!(
        fallback.list()?.contains("good"),
        "malformed settings cannot prevent startup"
    );
    Ok(())
}

#[test]
fn plugin_tool_name_collision_does_not_change_builtin_tools_or_prompt() -> Result<()> {
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let shadow = user.join("shadow");
    fs::create_dir_all(&shadow)?;
    fs::write(shadow.join("plugin.toml"), "id = 'shadow'\nname = 'Shadow'\ncommand = 'node'\nprompt = 'SHOULD_NOT_APPEAR'\n[[tools]]\nname = 'shell'\ndescription = 'Duplicate builtin'\n")?;
    let state_path = temp.path().join("plugins.toml");
    let old = PluginCatalog::discover_at(user.clone(), state_path.clone(), temp.path())?;
    old.set_enabled("shadow", true)?;
    let active = PluginCatalog::discover_at(user, state_path, temp.path())?;
    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    assert_eq!(active.contribute(&mut tools, &mut prompt).len(), 1);
    assert!(tools.is_builtin_only());
    let mut messages = prompt.initial_history(temp.path());
    prompt.apply(&mut messages);
    assert!(!messages[0]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("SHOULD_NOT_APPEAR"));
    Ok(())
}

#[test]
fn invalid_manifest_can_be_disabled_and_uninstalled() -> Result<()> {
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/hello");
    let catalog = PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?;
    catalog.install(&source)?;
    catalog.set_enabled("hello", true)?;
    catalog.set_config("hello", "prefix", Some("old value"))?;
    fs::write(user.join("hello/plugin.toml"), "not valid [toml")?;
    assert!(catalog.set_enabled("hello", false)?.contains("disabled"));
    assert!(catalog.uninstall("hello")?.contains("Uninstalled"));
    assert!(!user.join("hello").exists());
    assert!(catalog.uninstall("../other").is_err());
    catalog.install(&source)?;
    let fresh = PluginCatalog::discover_at(user, state, temp.path())?;
    assert!(fresh.list()?.contains("off  hello"));
    assert!(!fresh.show_config("hello")?.contains("old value"));
    let alias_dir = temp.path().join("plugins").join("renamed-directory");
    fs::create_dir_all(&alias_dir)?;
    fs::write(
        alias_dir.join("plugin.toml"),
        "id='alias'\nname='Renamed directory'\nprompt='hello'\n",
    )?;
    let with_alias = PluginCatalog::discover_at(
        temp.path().join("plugins"),
        temp.path().join("plugins.toml"),
        temp.path(),
    )?;
    with_alias.uninstall("alias")?;
    assert!(!alias_dir.exists(), "uninstall resolves the manifest ID");
    Ok(())
}

#[test]
fn process_model_declares_vision_without_changing_http_model_contracts() -> Result<()> {
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let path = user.join("vision");
    fs::create_dir_all(&path)?;
    let manifest = "id='vision'\nname='Vision backend'\ncommand='node'\n[model]\nprotocol='plugin'\nmodel='vision-test'\ncontext_window=8192\noutput_reserve=1024\nimage_input=true\n";
    fs::write(path.join("plugin.toml"), manifest)?;
    let state = temp.path().join("plugins.toml");
    let catalog = PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?;
    catalog.set_enabled("vision", true)?;
    let active = PluginCatalog::discover_at(user.clone(), state.clone(), temp.path())?;
    let (selected, warnings) = active.model_override();
    assert!(warnings.is_empty());
    let (backend, _, _) = selected.expect("model replacement");
    assert_eq!(backend.accepts_images(), Some(true));

    fs::write(
        path.join("plugin.toml"),
        manifest.replace("protocol='plugin'", "protocol='chat'"),
    )?;
    let invalid = PluginCatalog::discover_at(user, state, temp.path())?;
    assert!(
        !invalid.warnings.is_empty(),
        "HTTP providers discover vision from upstream"
    );
    Ok(())
}

#[tokio::test]
async fn external_context_can_reset_state_and_requires_explicit_branch_state() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let global = temp.path().join("plugins");
    let path = global.join("context-reset");
    fs::create_dir_all(&path)?;
    fs::write(path.join("plugin.toml"),
        "id='context-reset'\nname='Context replacement fixture'\ncommand='node'\nargs=['main.js']\n[context]\nversion=2\n")?;
    fs::write(
        path.join("main.js"),
        r#"
require('node:readline').createInterface({input:process.stdin}).on('line', line => {
  const req = JSON.parse(line);
  const {messages} = req.arguments;
  let result;
  if (req.type === 'context_prepare') {
    result = {messages: [messages[0], messages[messages.length - 1]],
              state: {original_count: messages.length}};
  } else if (req.type === 'context_finish') {
    result = {state: null,usage:{requests:1,input:120,output:30,cached:80,elapsed_ms:15}};
  } else if (req.type === 'context_branch') {
    result = {};
  } else if (req.type === 'context_overflow') {
    result = {recovered: false};
  } else throw new Error(req.type);
  process.stdout.write(JSON.stringify({request_id: req.request_id,
    protocol_version: req.protocol_version, content: JSON.stringify(result)}) + '\n');
});
"#,
    )?;
    let state = temp.path().join("plugins.toml");
    let catalog = PluginCatalog::discover_at(global.clone(), state.clone(), temp.path())?;
    catalog.set_enabled("context-reset", true)?;
    let enabled = PluginCatalog::discover_at(global, state, temp.path())?;
    let mut factory: Arc<dyn ContextFactory> = Arc::new(kroma::context::BuiltinContextFactory);
    assert!(enabled.replace_context(&mut factory).is_empty());
    let budget = ContextConfig::with_output_reserve(8192, 1024)?;
    let mut policy = factory.restore(factory.initial(), budget, temp.path())?;
    let history = vec![
        Message::new("system", "root"),
        Message::new("user", "u"),
        Message::new("assistant", "a"),
        Message::new("user", "next"),
    ];
    let original = serde_json::to_value(&history)?;
    let model: Arc<dyn ModelBackend> = Arc::new(ModelClient::new(
        "http://127.0.0.1:9/v1",
        None,
        "mock".into(),
    )?);
    let commits = StateRecorder::default();
    let projection = policy.prepare(&history, &model, &commits, 0).await?;
    assert_eq!(
        projection.len(),
        2,
        "external strategy replaces the model view"
    );
    assert_eq!(
        serde_json::to_value(&history)?,
        original,
        "raw history remains intact"
    );
    policy.finish_task(&history, &commits).await?;
    let PersistedContext::External(finished) = policy.state() else {
        panic!("missing context state")
    };
    assert!(
        finished.data.is_null(),
        "explicit state:null must commit a reset"
    );
    assert_eq!(commits.0.lock().unwrap().len(), 2);
    let usage = policy.take_usage();
    assert_eq!(usage.requests, 1);
    assert_eq!(usage.input, Some(120));
    assert_eq!(usage.output, Some(30));
    assert_eq!(usage.cached, Some(80));
    assert_eq!(
        policy.take_usage().requests,
        0,
        "usage must be drained once"
    );
    factory.restore(
        PersistedContext::External(finished.clone()),
        budget,
        temp.path(),
    )?;
    assert!(
        factory
            .branch_state(
                &PersistedContext::External(finished.clone()),
                &history,
                &history[..2],
                temp.path(),
                temp.path()
            )
            .is_err(),
        "partial history needs a plugin-provided branch state"
    );
    let mut incompatible = finished;
    incompatible.version = 3;
    assert!(factory
        .branch_state(
            &PersistedContext::External(incompatible),
            &history,
            &history[..2],
            temp.path(),
            temp.path()
        )
        .is_err());
    Ok(())
}

#[test]
fn disabled_plugin_has_no_model_wrapper_event_worker_or_tool_replacement() -> Result<()> {
    let temp = TestDirectory::new()?;
    let plugins = temp.path().join("plugins");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/hello");
    let catalog = PluginCatalog::discover_at(
        plugins.clone(),
        temp.path().join("plugins.toml"),
        temp.path(),
    )?;
    catalog.install(&source)?; // Remains disabled; no process should start.
    let disabled =
        PluginCatalog::discover_at(plugins, temp.path().join("plugins.toml"), temp.path())?;
    let model: Arc<dyn ModelBackend> = Arc::new(ModelClient::new(
        "http://127.0.0.1:9/v1",
        None,
        "mock".into(),
    )?);
    let budget = ContextConfig::with_output_reserve(8192, 1024)?;
    let wrapped = disabled.model_hooks(model.clone(), budget);
    assert!(Arc::ptr_eq(&wrapped, &model), "no hidden model adapter");
    assert!(disabled.model_override().0.is_none());
    assert!(
        disabled.wrap_events(None)?.is_none(),
        "no event worker thread"
    );
    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    let before = prompt.initial_history(temp.path());
    assert!(disabled.contribute(&mut tools, &mut prompt).is_empty());
    assert!(tools.is_builtin_only());
    assert_eq!(
        serde_json::to_value(prompt.initial_history(temp.path()))?,
        serde_json::to_value(before)?,
        "disabled plugin must not alter the default prompt"
    );
    Ok(())
}

#[tokio::test]
async fn process_model_rejects_invalid_tool_calls_before_executing_them() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let dir = temp.path().join("plugins").join("checked-model");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("plugin.toml"),
        "id='checked-model'\nname='Validated process model'\ncommand='node'\nargs=['main.js']\n[model]\nprotocol='plugin'\nmodel='checked'\ncontext_window=8192\noutput_reserve=1024\n",
    )?;
    fs::write(
        dir.join("main.js"),
        r#"require("node:readline").createInterface({input:process.stdin})
.on("line",line=>{
  const req=JSON.parse(line);
  const prompt=req.arguments.messages.at(-1).content;
  const call=(id,args)=>({id,type:"function",function:{name:"shell",arguments:args}});
  if(prompt==="overflow" || prompt==="plain-error") {
    process.stdout.write(JSON.stringify({request_id:req.request_id,protocol_version:1,
      is_error:true,content:"context overflow",
      ...(prompt==="overflow"?{code:"context_overflow"}:{})})+"\n");
    return;
  }
  let message={role:"assistant",content:"ok"};
  if(prompt==="duplicate") message={role:"assistant",content:null,tool_calls:[call("same","{}"),call("same","{}")]};
  if(prompt==="bad-json") message={role:"assistant",content:null,tool_calls:[call("a","{")]};
  if(prompt==="not-object") message={role:"assistant",content:null,tool_calls:[call("a","[]")]};
  if(prompt==="empty") message={role:"assistant",content:"  "};
  if(prompt==="wrong-role") message={role:"tool",content:"oops"};
  if(prompt==="valid-tool") message={role:"assistant",content:null,tool_calls:[call("a","{}")]};
  process.stdout.write(JSON.stringify({request_id:req.request_id,protocol_version:1,
    content:JSON.stringify({message})})+"\n");
});"#,
    )?;
    let global = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(global.clone(), state.clone(), temp.path())?
        .set_enabled("checked-model", true)?;
    let active = PluginCatalog::discover_at(global, state, temp.path())?;
    let (model, _, _) = active.model_override().0.context("model missing")?;
    for (input, expected) in [
        ("duplicate", "duplicate tool"),
        ("bad-json", "valid JSON"),
        ("not-object", "JSON object"),
        ("empty", "neither text"),
        ("wrong-role", "assistant message"),
    ] {
        let messages = [Message::new("system", "root"), Message::new("user", input)];
        let mut deltas = String::new();
        let error = model
            .complete(&messages, &[], &mut |delta| deltas.push_str(delta))
            .await
            .err()
            .context("invalid model response must fail")?;
        assert!(
            format!("{error:#}").contains(expected),
            "unexpected rejection for {input}: {error:#}"
        );
        assert!(
            deltas.is_empty(),
            "invalid results must not emit final text"
        );
    }
    for (input, should_recover) in [("overflow", true), ("plain-error", false)] {
        let messages = [Message::new("system", "root"), Message::new("user", input)];
        let error = model
            .complete(&messages, &[], &mut |_| {})
            .await
            .err()
            .context("expected process model rejection")?;
        assert_eq!(
            kroma::provider::is_context_overflow(&error),
            should_recover,
            "recovery requires a structured error code"
        );
    }
    let tool_messages = [
        Message::new("system", "root"),
        Message::new("user", "valid-tool"),
    ];
    let tool = model.complete(&tool_messages, &[], &mut |_| {}).await?;
    assert_eq!(tool.message.tool_calls.as_ref().unwrap()[0].id, "a");
    let normal_messages = [
        Message::new("system", "root"),
        Message::new("user", "normal"),
    ];
    assert_eq!(
        model
            .complete(&normal_messages, &[], &mut |_| {})
            .await?
            .message
            .content
            .as_deref(),
        Some("ok"),
        "invalid responses must not poison later model requests"
    );
    Ok(())
}

#[tokio::test]
async fn failed_external_context_task_does_not_replay_old_lifecycle() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let workdir = temp.path().join("project");
    fs::create_dir_all(&workdir)?;
    let directory = temp.path().join("plugins");
    let plugin = directory.join("context-fail-once");
    fs::create_dir_all(&plugin)?;
    fs::write(
        plugin.join("plugin.toml"),
        "id='context-fail-once'\nname='Context lifecycle regression'\ncommand='node'\nargs=['main.js']\n[context]\nversion=2\n",
    )?;
    fs::write(
        plugin.join("main.js"),
        r#"let failed=false;
require("node:readline").createInterface({input:process.stdin}).on("line",line=>{
  const req=JSON.parse(line);
  let body;
  if(req.type==="context_prepare" && !failed) {
    failed=true;
    process.stdout.write(JSON.stringify({request_id:req.request_id,protocol_version:1,
      is_error:true,content:"simulated failed preparation"})+"\n");
    return;
  }
  if(req.type==="context_prepare") {
    const userTurns=(req.arguments.lifecycle||[]).filter(x=>x.type==="context_new_turn");
    const prior=req.arguments.state?.turn_events??0;
    const projected=[{...req.arguments.messages[0],
      content:req.arguments.messages[0].content+"\nprior_turn_events="+prior},
      ...req.arguments.messages.slice(1)];
    body={messages:projected, state:{turn_events:userTurns.length,prior}};
  } else if(req.type==="context_finish") {
    body={state:req.arguments.state};
  } else {
    throw Error(req.type);
  }
  process.stdout.write(JSON.stringify({request_id:req.request_id,protocol_version:1,
    content:JSON.stringify(body)})+"\n");
});"#,
    )?;
    let state_path = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(directory.clone(), state_path.clone(), &workdir)?
        .set_enabled("context-fail-once", true)?;
    let catalog = PluginCatalog::discover_at(directory, state_path, &workdir)?;
    let mut factory: Arc<dyn ContextFactory> = Arc::new(kroma::context::BuiltinContextFactory);
    assert!(catalog.replace_context(&mut factory).is_empty());
    let captured = Arc::new(CaptureModel::default());
    let model: Arc<dyn ModelBackend> = captured.clone();
    let budget = ContextConfig::with_output_reserve(32768, 2048)?;
    let mut agent = Agent::with_runtime_budget(
        model.clone(),
        ToolRegistry::builtin(),
        factory.clone(),
        PromptComposer::default(),
        None,
        workdir.clone(),
        budget,
    )?;
    let err = agent.run("first", &mut |_| {}).await.unwrap_err();
    assert!(format!("{err:#}").contains("simulated failed preparation"));
    assert_eq!(agent.run("second", &mut |_| {}).await?, "ok");
    assert!(agent
        .messages()
        .iter()
        .any(|m| m.content.as_deref() == Some("first")));
    assert!(captured.0.lock().unwrap()[0]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("prior_turn_events=0"));
    drop(agent);
    // Restore the real WAL-backed session and verify the next projection sees
    // exactly one committed user-turn event, not the abandoned first task.
    let mut reopened = Agent::with_runtime_budget(
        model,
        ToolRegistry::builtin(),
        factory,
        PromptComposer::default(),
        None,
        workdir,
        budget,
    )?;
    assert_eq!(reopened.run("third", &mut |_| {}).await?, "ok");
    assert!(captured.0.lock().unwrap()[0]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("prior_turn_events=1"));
    assert_eq!(
        reopened
            .messages()
            .iter()
            .filter(|m| m.role == "user")
            .count(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn external_tools_and_hooks_receive_matching_session_archive_and_call_id() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let dir = temp.path().join("plugins").join("session-metadata");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("plugin.toml"),
        "id='session-metadata'\nname='Session metadata'\ncommand='node'\nargs=['main.js']\nhooks=['before_tool_call','after_tool_result']\n[[tools]]\nname='session_echo'\ndescription='Echo metadata'\n",
    )?;
    fs::write(
        dir.join("main.js"),
        r#"let previous;
require('node:readline').createInterface({input:process.stdin}).on('line',line=>{
  const req=JSON.parse(line);
  let content;
  if(req.type==='hook' && req.name==='before_tool_call') {
    previous={archive:req.archive,call_id:req.call_id};
    content=JSON.stringify({arguments:req.arguments.arguments});
  } else if(req.type==='tool') {
    content=req.archive+'|'+req.call_id+'|'+req.workdir;
  } else if(req.type==='hook' && req.name==='after_tool_result') {
    if(previous.archive!==req.archive || previous.call_id!==req.call_id)
      throw Error('hook received a different session or call');
    content=JSON.stringify({content:req.arguments.content+'|hooks-matched'});
  } else throw Error(req.type);
  process.stdout.write(JSON.stringify({request_id:req.request_id,
    protocol_version:1,content})+'\n');
});"#,
    )?;
    let global = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(global.clone(), state.clone(), temp.path())?
        .set_enabled("session-metadata", true)?;
    let active = PluginCatalog::discover_at(global, state, temp.path())?;
    let mut tools = ToolRegistry::builtin();
    assert!(active
        .contribute(&mut tools, &mut PromptComposer::default())
        .is_empty());
    let call = ToolCall {
        id: "unique-call-id".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "session_echo".into(),
            arguments: "{}".into(),
        },
    };
    let archive = temp.path().join("sessions").join("session.json");
    let outcome = tools
        .execute(
            &call,
            ToolContext {
                workdir: temp.path(),
                archive: &archive,
                on_output: None,
            },
        )
        .await;
    assert!(!outcome.is_error, "{}", outcome.content);
    assert_eq!(
        outcome.content,
        format!(
            "{}|unique-call-id|{}|hooks-matched",
            archive.display(),
            temp.path().display()
        )
    );
    assert!(outcome.executed_arguments.is_none());
    Ok(())
}

#[tokio::test]
async fn external_context_plugin_tracks_fork_rewind_and_resume_archives() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let workdir = temp.path().join("project");
    fs::create_dir_all(&workdir)?;
    let plugins = temp.path().join("plugins");
    let directory = plugins.join("branch-context");
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join("plugin.toml"),
        "id='branch-context'\nname='Branch context contract'\ncommand='node'\nargs=['main.js']\n[context]\nversion=2\n",
    )?;
    fs::write(
        directory.join("main.js"),
        r#"require('node:readline').createInterface({input:process.stdin}).on('line',line=>{
  const req=JSON.parse(line);
  const args=req.arguments;
  let body;
  if(req.type==='context_prepare') {
    const s=args.state??{};
    const projected=[{...args.messages[0],content:args.messages[0].content+
      '\ncurrent_archive='+req.archive+'\nbranch_from='+(s.branchFrom??'')+
      '\nbranch_to='+(s.branchTo??'')},...args.messages.slice(1)];
    body={messages:projected,state:s};
  } else if(req.type==='context_finish') {
    body={state:args.state};
  } else if(req.type==='context_branch') {
    if(!args.source_archive || !args.destination_archive ||
       req.archive!==args.destination_archive || args.retained.length>args.source.length)
      throw Error('invalid branch archive contract');
    body={state:{branchFrom:args.source_archive,branchTo:args.destination_archive}};
  } else throw Error(req.type);
  process.stdout.write(JSON.stringify({request_id:req.request_id,protocol_version:1,
    content:JSON.stringify(body)})+'\n');
});"#,
    )?;
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(plugins.clone(), state.clone(), &workdir)?
        .set_enabled("branch-context", true)?;
    let catalog = PluginCatalog::discover_at(plugins, state, &workdir)?;
    let mut factory: Arc<dyn ContextFactory> = Arc::new(kroma::context::BuiltinContextFactory);
    assert!(catalog.replace_context(&mut factory).is_empty());
    let captured = Arc::new(CaptureModel::default());
    let budget = ContextConfig::with_output_reserve(32768, 2048)?;
    let mut agent = Agent::with_runtime_budget(
        captured.clone(),
        ToolRegistry::builtin(),
        factory,
        PromptComposer::default(),
        None,
        workdir,
        budget,
    )?;
    assert_eq!(agent.run("one", &mut |_| {}).await?, "ok");
    assert_eq!(agent.run("two", &mut |_| {}).await?, "ok");
    let original = agent.archive_path().to_string_lossy().to_string();
    let _branch_id = agent.fork_async(Some(1)).await?;
    let fork_archive = agent.archive_path().to_string_lossy().to_string();
    assert_ne!(original, fork_archive);
    agent.run("forked", &mut |_| {}).await?;
    let prompt = captured.0.lock().unwrap()[0]
        .content
        .clone()
        .unwrap_or_default();
    assert!(
        prompt.contains(&format!("current_archive={fork_archive}")),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("branch_from={original}")),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("branch_to={fork_archive}")),
        "{prompt}"
    );
    let backup_id = agent.rewind_async(1).await?;
    agent.run("after-rewind", &mut |_| {}).await?;
    let prompt = captured.0.lock().unwrap()[0]
        .content
        .clone()
        .unwrap_or_default();
    assert!(
        prompt.contains(&format!("current_archive={fork_archive}")),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("branch_from={fork_archive}")),
        "{prompt}"
    );
    agent.resume_session(&backup_id)?;
    agent.run("backup", &mut |_| {}).await?;
    let backup_archive = agent.archive_path().to_string_lossy().to_string();
    let prompt = captured.0.lock().unwrap()[0]
        .content
        .clone()
        .unwrap_or_default();
    assert!(
        prompt.contains(&format!("current_archive={backup_archive}")),
        "{prompt}"
    );
    assert_ne!(backup_archive, fork_archive);
    Ok(())
}

#[tokio::test]
async fn streaming_process_model_context_overflow_retries_through_external_policy() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let workdir = temp.path().join("project");
    fs::create_dir_all(&workdir)?;
    let global = temp.path().join("plugins");
    let model_dir = global.join("overflow-model");
    let context_dir = global.join("overflow-context");
    fs::create_dir_all(&model_dir)?;
    fs::create_dir_all(&context_dir)?;
    fs::write(
        model_dir.join("plugin.toml"),
        "id='overflow-model'\nname='Overflow model'\ncommand='node'\nargs=['main.js']\n[model]\nmodel='overflow-demo'\nprotocol='plugin'\nstreaming=true\ncontext_window=8192\noutput_reserve=1024\n",
    )?;
    fs::write(
        model_dir.join("main.js"),
        r#"require('node:readline').createInterface({input:process.stdin}).on('line',line=>{
  const req=JSON.parse(line);
  const recovered=req.arguments.messages[0].content.includes('PLUGIN_RECOVERED');
  const base={request_id:req.request_id,protocol_version:1};
  const frame=recovered
    ? {...base,event:'complete',result:{message:{role:'assistant',content:'recovered'}}}
    : {...base,event:'error',code:'context_overflow',message:'upstream context exhausted'};
  process.stdout.write(JSON.stringify(frame)+'\n');
});"#,
    )?;
    fs::write(
        context_dir.join("plugin.toml"),
        "id='overflow-context'\nname='Overflow recovery policy'\ncommand='node'\nargs=['main.js']\n[context]\nversion=2\n",
    )?;
    fs::write(
        context_dir.join("main.js"),
        r#"require('node:readline').createInterface({input:process.stdin}).on('line',line=>{
  const req=JSON.parse(line);
  const state=req.arguments.state;
  let body;
  if(req.type==='context_prepare') {
    const messages=req.arguments.messages.slice();
    if(state?.recovered)
      messages[0]={...messages[0],content:messages[0].content+'\nPLUGIN_RECOVERED'};
    body={messages,state};
  } else if(req.type==='context_overflow') {
    body={recovered:true,state:{recovered:true}};
  } else if(req.type==='context_finish') {
    body={state};
  } else throw Error(req.type);
  process.stdout.write(JSON.stringify({request_id:req.request_id,protocol_version:1,
    content:JSON.stringify(body)})+'\n');
});"#,
    )?;
    let state = temp.path().join("plugins.toml");
    let initial = PluginCatalog::discover_at(global.clone(), state.clone(), &workdir)?;
    initial.set_enabled("overflow-model", true)?;
    initial.set_enabled("overflow-context", true)?;
    let enabled = PluginCatalog::discover_at(global, state, &workdir)?;
    let (selected, warnings) = enabled.model_override();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (model, budget, _) = selected.context("missing external process model")?;
    let mut factory: Arc<dyn ContextFactory> = Arc::new(kroma::context::BuiltinContextFactory);
    assert!(enabled.replace_context(&mut factory).is_empty());
    let mut agent = Agent::with_runtime_budget(
        model,
        ToolRegistry::builtin(),
        factory,
        PromptComposer::default(),
        None,
        workdir,
        budget,
    )?;
    let mut streamed = String::new();
    assert_eq!(
        agent
            .run("hello", &mut |delta| streamed.push_str(delta))
            .await?,
        "recovered"
    );
    assert_eq!(streamed.trim(), "recovered");
    assert!(agent
        .messages()
        .iter()
        .any(|m| m.content.as_deref() == Some("recovered")));
    Ok(())
}

#[tokio::test]
async fn slow_event_plugin_cannot_block_other_event_subscribers() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let root = temp.path().join("plugins");
    for (name, wait_ms) in [("a-slow", 1800), ("z-fast", 0)] {
        let dir = root.join(name);
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("plugin.toml"),
            format!("id='{name}'\nname='Subscriber'\ncommand='node'\nargs=['main.js']\nevents=['task_started']\n"),
        )?;
        fs::write(
            dir.join("main.js"),
            format!(
                r#"const fs=require('node:fs');
require('node:readline').createInterface({{input:process.stdin}}).on('line',line=>{{
  const req=JSON.parse(line);
  setTimeout(()=>{{
    fs.writeFileSync('delivered',String(req.request_id));
    process.stdout.write(JSON.stringify({{request_id:req.request_id,protocol_version:1,content:'ok'}})+'\n');
  }},{wait_ms});
}});
"#
            ),
        )?;
    }
    let state = temp.path().join("plugins.toml");
    let catalog = PluginCatalog::discover_at(root.clone(), state.clone(), temp.path())?;
    catalog.set_enabled("a-slow", true)?;
    catalog.set_enabled("z-fast", true)?;
    let active = PluginCatalog::discover_at(root.clone(), state, temp.path())?;
    let sink = active.wrap_events(None)?.context("missing event sink")?;
    sink.on_event(kroma::events::RuntimeEvent::TaskStarted);
    // The first manifest sorts before the second. A shared event queue would
    // make z-fast wait for a-slow's 1.8 second reply.
    let fast_file = root.join("z-fast/delivered");
    for _ in 0..60 {
        if fast_file.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(fast_file.exists(), "one slow event plugin blocked another");
    drop(sink);
    // Let the fixture exit cleanly before Windows removes its test directory.
    tokio::time::sleep(Duration::from_millis(800)).await;
    Ok(())
}

#[tokio::test]
async fn plugin_private_data_is_lazy_scoped_and_persists_across_restart() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let workdir = temp.path().join("project");
    let root = temp.path().join("plugins");
    let project_root = workdir.join(".kroma/plugins");
    for (directory, id, tool_name) in [
        (root.join("scope-g"), "scope-g", "scope_global"),
        (project_root.join("scope-p"), "scope-p", "scope_project"),
    ] {
        fs::create_dir_all(&directory)?;
        fs::write(directory.join("plugin.toml"), format!(
            "id='{id}'\nname='Scope test'\ncommand='node'\nargs=['main.js']\n[[tools]]\nname='{tool_name}'\ndescription='Store in plugin data directory'\n"
        ))?;
        fs::write(
            directory.join("main.js"),
            r#"const fs=require('node:fs'),path=require('node:path');
require('node:readline').createInterface({input:process.stdin}).on('line',line=>{
  const req=JSON.parse(line);
  const file=path.join(req.data_dir,'count.txt');
  const count=fs.existsSync(file)?Number(fs.readFileSync(file,'utf8'))+1:1;
  fs.writeFileSync(file,String(count));
  process.stdout.write(JSON.stringify({
    protocol_version:1,request_id:req.request_id,
    content:JSON.stringify({data_dir:req.data_dir,count})
  })+'\n');
});"#,
        )?;
    }
    let state_path = temp.path().join("plugins.toml");
    let load = || PluginCatalog::discover_at(root.clone(), state_path.clone(), &workdir);
    let global_dir = temp.path().join("plugin-data/scope-g");
    let project_dir = workdir.join(".kroma/plugin-data/scope-p");
    assert!(!global_dir.exists() && !project_dir.exists());
    load()?.set_enabled("scope-g", true)?;
    load()?.set_enabled("scope-p", true)?;
    assert!(
        !global_dir.exists() && !project_dir.exists(),
        "discovery must not create plugin data"
    );
    for expected_count in [1, 2] {
        let catalog = load()?;
        let mut registry = ToolRegistry::builtin();
        assert!(catalog
            .contribute(&mut registry, &mut PromptComposer::default())
            .is_empty());
        for (tool_name, expected_dir) in [
            ("scope_global", &global_dir),
            ("scope_project", &project_dir),
        ] {
            let call = ToolCall {
                id: "data-call".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: tool_name.into(),
                    arguments: "{}".into(),
                },
            };
            let result = registry
                .execute(
                    &call,
                    ToolContext {
                        workdir: &workdir,
                        archive: temp.path(),
                        on_output: None,
                    },
                )
                .await;
            assert!(!result.is_error, "{}", result.content);
            let payload: serde_json::Value = serde_json::from_str(&result.content)?;
            let reported = payload["data_dir"].as_str().context("missing data_dir")?;
            assert_eq!(
                fs::canonicalize(reported)?,
                fs::canonicalize(expected_dir)?,
                "data_dir is scoped to the correct plugin"
            );
            assert_eq!(payload["count"], expected_count);
        }
    }
    assert_eq!(fs::read_to_string(global_dir.join("count.txt"))?, "2");
    assert_eq!(fs::read_to_string(project_dir.join("count.txt"))?, "2");
    load()?.uninstall("scope-g")?;
    assert!(
        global_dir.exists(),
        "uninstall preserves user-created plugin data"
    );
    Ok(())
}

#[tokio::test]
async fn actual_external_compaction_shortens_model_view_without_dropping_wal_history() -> Result<()>
{
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let workdir = temp.path().join("project");
    fs::create_dir_all(&workdir)?;
    let root = temp.path().join("plugins");
    let dir = root.join("compacting-policy");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("plugin.toml"),
        "id='compacting-policy'\nname='Compacting fixture'\ncommand='node'\nargs=['main.js']\n[context]\nversion=2\n",
    )?;
    fs::write(
        dir.join("main.js"),
        r#"require('node:readline').createInterface({input:process.stdin}).on('line',line=>{
  const req=JSON.parse(line);
  const args=req.arguments;
  let body;
  if(req.type==='context_prepare') {
    const previous=args.state?.compactions??0;
    let messages=args.messages;
    if(messages.length>6) {
      const old=messages.slice(1,-2).map(m=>m.role+':'+(m.content??'')).join(';');
      messages=[messages[0],{role:'system',content:'summary='+old+';previous='+previous},
        ...messages.slice(-2)];
    }
    body={messages,state:{compactions:previous+1}};
  } else if(req.type==='context_finish') {
    body={state:args.state};
  } else throw Error(req.type);
  process.stdout.write(JSON.stringify({request_id:req.request_id,protocol_version:1,
    content:JSON.stringify(body)})+'\n');
});"#,
    )?;
    let state_path = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(root.clone(), state_path.clone(), &workdir)?
        .set_enabled("compacting-policy", true)?;
    let active = PluginCatalog::discover_at(root, state_path, &workdir)?;
    let mut factory: Arc<dyn ContextFactory> = Arc::new(kroma::context::BuiltinContextFactory);
    assert!(active.replace_context(&mut factory).is_empty());
    let capture = Arc::new(CaptureModel::default());
    let budget = ContextConfig::with_output_reserve(32768, 2048)?;
    let mut agent = Agent::with_runtime_budget(
        capture.clone(),
        ToolRegistry::builtin(),
        factory.clone(),
        PromptComposer::default(),
        None,
        workdir.clone(),
        budget,
    )?;
    for i in 0..7 {
        assert_eq!(agent.run(&format!("turn {i}"), &mut |_| {}).await?, "ok");
    }
    assert_eq!(
        agent.messages().len(),
        15,
        "the original conversation is complete"
    );
    let view = capture.0.lock().unwrap().clone();
    assert_eq!(
        view.len(),
        4,
        "plugin replaces long history with a summary view"
    );
    assert!(view[1].content.as_deref().unwrap_or("").contains("turn 0"));
    assert!(view[1]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("previous=6"));
    assert_eq!(view[3].content.as_deref(), Some("turn 6"));
    drop(agent);

    let mut reopened = Agent::with_runtime_budget(
        capture.clone(),
        ToolRegistry::builtin(),
        factory,
        PromptComposer::default(),
        None,
        workdir,
        budget,
    )?;
    assert_eq!(reopened.run("after restart", &mut |_| {}).await?, "ok");
    let view = capture.0.lock().unwrap().clone();
    assert_eq!(view.len(), 4);
    assert!(view[1]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("previous=7"));
    assert_eq!(
        reopened.messages().len(),
        17,
        "restart must recover original WAL"
    );
    Ok(())
}

#[tokio::test]
async fn cancelling_external_context_prepare_restarts_worker_without_losing_session() -> Result<()>
{
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let workdir = temp.path().join("project");
    fs::create_dir_all(&workdir)?;
    let root = temp.path().join("plugins");
    let dir = root.join("context-cancel");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("plugin.toml"),
        "id='context-cancel'\nname='Cancel context'\ncommand='node'\nargs=['main.js']\n[context]\nversion=2\n",
    )?;
    fs::write(
        dir.join("main.js"),
        r#"require('node:readline').createInterface({input:process.stdin}).on('line',line=>{
 const req=JSON.parse(line),args=req.arguments;
 if(req.type==='context_prepare' && args.messages.at(-1)?.content==='hang') return;
 const events=(args.lifecycle??[]).filter(e=>e.type==='context_new_turn').length;
 const body=req.type==='context_prepare'
   ? {messages:[{...args.messages[0],
       content:args.messages[0].content+'\nlifecycle='+events},...args.messages.slice(1)],
       state:{events}}
   : {state:args.state};
 process.stdout.write(JSON.stringify({request_id:req.request_id,protocol_version:1,
   content:JSON.stringify(body)})+'\n');
});"#,
    )?;
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(root.clone(), state.clone(), &workdir)?
        .set_enabled("context-cancel", true)?;
    let catalog = PluginCatalog::discover_at(root, state, &workdir)?;
    let mut factory: Arc<dyn ContextFactory> = Arc::new(kroma::context::BuiltinContextFactory);
    assert!(catalog.replace_context(&mut factory).is_empty());
    let captured = Arc::new(CaptureModel::default());
    let budget = ContextConfig::with_output_reserve(32768, 2048)?;
    let mut agent = Agent::with_runtime_budget(
        captured.clone(),
        ToolRegistry::builtin(),
        factory,
        PromptComposer::default(),
        None,
        workdir,
        budget,
    )?;
    let timeout =
        tokio::time::timeout(Duration::from_millis(500), agent.run("hang", &mut |_| {})).await;
    assert!(timeout.is_err(), "context plugin was expected to stall");
    agent.cancel()?;
    assert_eq!(agent.run("resume", &mut |_| {}).await?, "ok");
    assert!(
        captured.0.lock().unwrap()[0]
            .content
            .as_deref()
            .unwrap_or("")
            .contains("lifecycle=1"),
        "cancelled task events must not be replayed"
    );
    assert!(agent
        .messages()
        .iter()
        .any(|m| m.content.as_deref() == Some("hang")));
    assert!(agent
        .messages()
        .iter()
        .any(|m| m.content.as_deref() == Some("resume")));
    Ok(())
}

#[test]
fn enabled_model_provider_errors_and_broken_manifests_fail_closed() -> Result<()> {
    let temp = TestDirectory::new()?;
    let root = temp.path().join("plugins");
    let bad = root.join("bad-model");
    fs::create_dir_all(&bad)?;
    fs::write(bad.join("plugin.toml"),
        "id='bad-model'\nname='Provider'\n[model]\nprotocol='not-a-protocol'\nbase_url='http://127.0.0.1:9'\nmodel='broken'\ncontext_window=8192\noutput_reserve=1024\n")?;
    let state = temp.path().join("plugins.toml");
    let load = || PluginCatalog::discover_at(root.clone(), state.clone(), temp.path());
    assert!(
        load()?.checked_model_override()?.is_none(),
        "disabled plugin must not affect provider"
    );
    load()?.set_enabled("bad-model", true)?;
    let error = match load()?.checked_model_override() {
        Err(error) => error.to_string(),
        Ok(_) => anyhow::bail!("invalid enabled model must not silently fall back"),
    };
    assert!(
        error.contains("enabled model plugin cannot initialize"),
        "{error}"
    );
    fs::write(bad.join("plugin.toml"), "invalid [")?;
    assert!(
        load()?.ensure_loadable().is_err(),
        "enabled corrupt manifest must not disappear"
    );
    assert!(load()?
        .set_enabled("bad-model", false)?
        .contains("disabled"));
    assert!(
        load()?.ensure_loadable().is_ok(),
        "disable command must remain usable"
    );
    Ok(())
}

#[test]
fn independent_kroma_processes_do_not_lose_plugin_settings() -> Result<()> {
    let temp = TestDirectory::new()?;
    let root = temp.path().join("plugins");
    let plugin = root.join("shared");
    fs::create_dir_all(&plugin)?;
    fs::write(
        plugin.join("plugin.toml"),
        "id='shared'\nname='Shared state'\nprompt='hello'\n",
    )?;
    let mut children = Vec::new();
    for i in 0..12 {
        children.push(
            Command::new(env!("CARGO_BIN_EXE_kroma"))
                .args([
                    "plugins",
                    "set",
                    "shared",
                    &format!("key{i}"),
                    &format!("value{i}"),
                ])
                .env("KROMA_PLUGIN_DIR", &root)
                .env("KROMA_WORKDIR", temp.path())
                .spawn()?,
        );
    }
    for mut process in children {
        assert!(
            process.wait()?.success(),
            "one concurrent plugin settings update failed"
        );
    }
    let config = PluginCatalog::discover_at(root, temp.path().join("plugins.toml"), temp.path())?
        .show_config("shared")?;
    for i in 0..12 {
        assert!(
            config.contains(&format!("key{i} = \"value{i}\"")),
            "lost concurrent update key{i}: {config}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn process_plugin_tool_streams_progress_without_changing_final_result() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let root = temp.path().join("plugins");
    let dir = root.join("progress");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("plugin.toml"),
        "id='progress'\nname='Streaming tool'\ncommand='node'\nargs=['main.js']\n[[tools]]\nname='progress_tool'\ndescription='Streaming test'\nstreaming=true\n")?;
    fs::write(
        dir.join("main.js"),
        r#"require('node:readline').createInterface({input:process.stdin}).on('line',line=>{
  const request=JSON.parse(line);
  const base={request_id:request.request_id,protocol_version:1};
  for(const frame of [
    {event:'output_delta',stream:'stdout',delta:'phase one'},
    {event:'output_delta',stream:'stderr',delta:'phase two'},
    {event:'complete',result:{content:'finished',is_error:false}}
  ]) process.stdout.write(JSON.stringify({...base,...frame})+'\n');
});"#,
    )?;
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(root.clone(), state.clone(), temp.path())?
        .set_enabled("progress", true)?;
    let enabled = PluginCatalog::discover_at(root, state, temp.path())?;
    let mut tools = ToolRegistry::builtin();
    assert!(enabled
        .contribute(&mut tools, &mut PromptComposer::default())
        .is_empty());
    let call = ToolCall {
        id: "stream-1".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "progress_tool".into(),
            arguments: "{}".into(),
        },
    };
    let chunks = Mutex::new(Vec::<(String, String)>::new());
    let callback =
        |channel: &str, delta: &str| chunks.lock().unwrap().push((channel.into(), delta.into()));
    let result = tools
        .execute(
            &call,
            ToolContext {
                workdir: temp.path(),
                archive: temp.path(),
                on_output: Some(&callback),
            },
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "finished");
    assert_eq!(
        *chunks.lock().unwrap(),
        vec![
            ("stdout".into(), "phase one".into()),
            ("stderr".into(), "phase two".into())
        ]
    );
    Ok(())
}

#[tokio::test]
async fn failed_plugin_overflow_recovery_still_accounts_for_compaction_usage() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let root = temp.path().join("plugins");
    let dir = root.join("overflow-usage");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("plugin.toml"),
        "id='overflow-usage'\nname='Compaction usage'\ncommand='node'\nargs=['main.js']\n[context]\nversion=2\n")?;
    fs::write(
        dir.join("main.js"),
        r#"require('node:readline').createInterface({input:process.stdin}).on('line',line=>{
  const request=JSON.parse(line);
  process.stdout.write(JSON.stringify({request_id:request.request_id,protocol_version:1,
    content:JSON.stringify({recovered:false,state:{should_not_persist:true},
      usage:{requests:2,input:125,output:25,cached:30,elapsed_ms:100}})})+'\n');
});"#,
    )?;
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(root.clone(), state.clone(), temp.path())?
        .set_enabled("overflow-usage", true)?;
    let enabled = PluginCatalog::discover_at(root, state, temp.path())?;
    let mut factory: Arc<dyn ContextFactory> = Arc::new(kroma::context::BuiltinContextFactory);
    assert!(enabled.replace_context(&mut factory).is_empty());
    let mut policy = factory.restore(
        factory.initial(),
        ContextConfig::with_output_reserve(8192, 1024)?,
        temp.path(),
    )?;
    let backend: Arc<dyn ModelBackend> = Arc::new(CaptureModel::default());
    let history = [Message::new("system", "root"), Message::new("user", "turn")];
    assert!(
        !policy
            .recover_overflow(&history, &backend, &StateRecorder::default())
            .await?
    );
    let PersistedContext::External(context) = policy.state() else {
        anyhow::bail!("external context state missing");
    };
    assert!(
        context.data.is_null(),
        "failed recovery must not commit state"
    );
    let usage = policy.take_usage();
    assert_eq!(
        (usage.requests, usage.input, usage.output, usage.cached),
        (2, Some(125), Some(25), Some(30))
    );
    assert_eq!(policy.take_usage().requests, 0);
    Ok(())
}

#[tokio::test]
async fn external_context_branch_is_cancellable_and_full_copy_notifies_plugin() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let workdir = temp.path().join("work");
    fs::create_dir_all(&workdir)?;
    let root = temp.path().join("plugins");
    let dir = root.join("branch-wait");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("plugin.toml"),
        "id='branch-wait'\nname='Branch cancellation'\ncommand='node'\nargs=['main.js']\n[context]\nversion=2\n")?;
    fs::write(
        dir.join("main.js"),
        r#"const fs=require('node:fs');
require('node:readline').createInterface({input:process.stdin}).on('line',line=>{
  const req=JSON.parse(line);
  if(req.type==='context_branch') {
    fs.writeFileSync('branch-started',req.arguments.destination_archive);
    if(!fs.existsSync('allow-branch')) return;
    // Full history branches may leave the existing state unchanged.
    process.stdout.write(JSON.stringify({request_id:req.request_id,protocol_version:1,
      content:JSON.stringify({})})+'\n');
  }
});"#,
    )?;
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(root.clone(), state.clone(), &workdir)?
        .set_enabled("branch-wait", true)?;
    let enabled = PluginCatalog::discover_at(root, state, &workdir)?;
    let mut context: Arc<dyn ContextFactory> = Arc::new(kroma::context::BuiltinContextFactory);
    assert!(enabled.replace_context(&mut context).is_empty());
    let mut agent = Agent::with_runtime_budget(
        Arc::new(CaptureModel::default()),
        ToolRegistry::builtin(),
        context,
        PromptComposer::default(),
        None,
        workdir,
        ContextConfig::with_output_reserve(8192, 1024)?,
    )?;
    let original = agent.archive_path().to_path_buf();
    assert!(
        agent.fork(Some(0)).is_err(),
        "sync API must not freeze on external subprocess"
    );
    {
        let mut branch = Box::pin(agent.fork_async(Some(0)));
        let started = dir.join("branch-started");
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result=&mut branch => anyhow::bail!("hung branch returned: {result:?}"),
                _=async {
                    loop {
                        if started.exists(){break;}
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                } => Ok(()),
            }
        })
        .await??;
        // Dropping an awaiting fork future must kill its worker, not the UI.
    }
    assert_eq!(agent.archive_path(), original);
    assert_eq!(agent.messages().len(), 1);
    fs::write(dir.join("allow-branch"), "yes")?;
    let branched =
        tokio::time::timeout(Duration::from_secs(5), agent.fork_async(Some(0))).await??;
    assert!(!branched.is_empty());
    assert_ne!(agent.archive_path(), original);
    let destination = fs::read_to_string(dir.join("branch-started"))?;
    assert_eq!(Path::new(&destination), agent.archive_path());
    Ok(())
}

#[tokio::test]
async fn session_identity_reaches_command_model_hook_and_event_plugins() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let workdir = temp.path().join("project");
    fs::create_dir_all(&workdir)?;
    let root = temp.path().join("plugins");
    let dir = root.join("identity");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("plugin.toml"),
        "id='identity'\nname='Session identity'\ncommand='node'\nargs=['main.js']\nhooks=['before_model_request']\nevents=['task_started']\n[[commands]]\nname='/identity'\n")?;
    fs::write(
        dir.join("main.js"),
        r#"const fs=require('node:fs');
require('node:readline').createInterface({input:process.stdin}).on('line',line=>{
 const req=JSON.parse(line);
 if(req.type==='event'||req.type==='hook')
   fs.writeFileSync(req.type+'.json',JSON.stringify({session_id:req.session_id,archive:req.archive}));
 const content=req.type==='command'?
   JSON.stringify({session_id:req.session_id,archive:req.archive}):
   (req.type==='hook'?'{}':'ok');
 process.stdout.write(JSON.stringify({request_id:req.request_id,protocol_version:1,content})+'\n');
});"#,
    )?;
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(root.clone(), state.clone(), &workdir)?
        .set_enabled("identity", true)?;
    let enabled = PluginCatalog::discover_at(root, state, &workdir)?;
    let backend = Arc::new(CaptureModel::default());
    let budget = ContextConfig::with_output_reserve(8192, 1024)?;
    let model = enabled.model_hooks(backend, budget);
    let mut agent = Agent::with_runtime_budget(
        model,
        ToolRegistry::builtin(),
        Arc::new(kroma::context::BuiltinContextFactory),
        PromptComposer::default(),
        enabled.wrap_events(None)?,
        workdir,
        budget,
    )?;
    let mut commands = CommandRegistry::builtin(vec![])?;
    assert!(enabled.register_commands(&mut commands).is_empty());
    for turn in ["first", "second"] {
        if turn == "second" {
            agent.new_session()?;
        }
        assert_eq!(agent.run(turn, &mut |_| {}).await?, "ok");
        let input_handle = agent.task_input();
        let result = commands
            .execute_async("/identity", &mut agent, &input_handle)
            .await?
            .context("missing plugin response")?;
        let identity: serde_json::Value = serde_json::from_str(&result.output)?;
        let path = agent.archive_path().to_string_lossy().to_string();
        assert_eq!(identity["session_id"], agent.session_id());
        assert_eq!(identity["archive"], path);
        let hook: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("hook.json"))?)?;
        assert_eq!(hook["archive"], path);
        let mut observed = false;
        for _ in 0..100 {
            if let Ok(data) = fs::read_to_string(dir.join("event.json")) {
                let payload: serde_json::Value = serde_json::from_str(&data)?;
                if payload["archive"] == path {
                    observed = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(observed, "session event did not include current archive");
    }
    Ok(())
}

#[tokio::test]
async fn dropping_stalled_event_subscription_terminates_worker() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = TestDirectory::new()?;
    let root = temp.path().join("plugins");
    let dir = root.join("stalled-events");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("plugin.toml"),
        "id='stalled-events'\nname='Event lifecycle'\ncommand='node'\nargs=['main.js']\nevents=['task_started']\n")?;
    fs::write(
        dir.join("main.js"),
        r#"const fs=require('node:fs');
require('node:readline').createInterface({input:process.stdin}).on('line',()=>{
  fs.writeFileSync('worker.pid',String(process.pid));
  // Never complete this event. Unloading its subscription must terminate us.
});"#,
    )?;
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(root.clone(), state.clone(), temp.path())?
        .set_enabled("stalled-events", true)?;
    let catalog = PluginCatalog::discover_at(root, state, temp.path())?;
    let sink = catalog
        .wrap_events(None)?
        .context("event subscription missing")?;
    sink.on_event(kroma::events::RuntimeEvent::TaskStarted);
    let marker = dir.join("worker.pid");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let pid: u32 = fs::read_to_string(marker)?.parse()?;
    let started = std::time::Instant::now();
    drop(sink);
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "unloading an event subscriber must not block the UI"
    );
    let mut exited = false;
    for _ in 0..75 {
        #[cfg(windows)]
        let alive = {
            let output = Command::new("tasklist.exe")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .output()?;
            String::from_utf8_lossy(&output.stdout).contains(&format!(",\"{pid}\","))
        };
        #[cfg(unix)]
        let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
        if !alive {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        exited,
        "stalled event worker {pid} survived subscription unload"
    );
    Ok(())
}
