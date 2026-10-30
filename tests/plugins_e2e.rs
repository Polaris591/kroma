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
    let mut captured = String::new();
    let timeout = tokio::time::timeout(
        Duration::from_millis(450),
        backend.complete(&stalled, &[], &mut |delta| captured.push_str(delta)),
    )
    .await;
    assert!(
        timeout.is_err(),
        "missing final frame must not count as completion"
    );
    assert_eq!(
        captured, "started",
        "partial stream should be delivered before cancel"
    );
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
