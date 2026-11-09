//! M0 host integration for the generic `task_control` seam, driven by the real
//! Goal Node process and a bounded fake model. No network, no paid provider.
use anyhow::{Context, Result};
use kroma::{
    agent::Agent,
    context::{BuiltinContextFactory, ContextConfig},
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, SummaryResult, Usage},
    plugins::{ControlCommand, PluginCatalog},
    prompt::PromptComposer,
    protocol::{FunctionCall, Message, ToolCall},
    tools::ToolRegistry,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kroma-control-{label}-{}-{nanos}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    fn plugin_dir(&self) -> PathBuf {
        self.0.join("plugins")
    }

    fn workdir(&self) -> PathBuf {
        self.0.join("work")
    }

    fn state_path(&self) -> PathBuf {
        self.0.join("plugins.toml")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copy the real Goal plugin into an isolated plugin directory.
fn install_goal(dir: &TestDir) -> Result<()> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/goal");
    let target = dir.plugin_dir().join("goal");
    fs::create_dir_all(&target)?;
    for name in ["main.js", "plugin.toml"] {
        fs::copy(source.join(name), target.join(name))
            .with_context(|| format!("copying {name}"))?;
    }
    fs::create_dir_all(dir.workdir())?;
    // Enable only the Goal plugin. Nothing else is discovered, so an ordinary
    // task cannot gain control IPC from another source.
    fs::write(dir.state_path(), "enabled = [\"goal\"]\n")?;
    Ok(())
}

fn catalog(dir: &TestDir) -> Result<PluginCatalog> {
    PluginCatalog::discover_at(dir.plugin_dir(), dir.state_path(), &dir.workdir())
}

fn control_host(catalog: &PluginCatalog) -> Arc<kroma::plugins::TaskControlHost> {
    match catalog.task_control() {
        Some(host) => host,
        None => panic!("no task control provider; warnings: {:?}", catalog.warnings),
    }
}

/// Build an Agent that contributes the plugin's tools and prompt, exactly like
/// the CLI host does.
fn agent_with(
    dir: &TestDir,
    catalog: &PluginCatalog,
    model: Arc<dyn ModelBackend>,
) -> Result<Agent> {
    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    let warnings = catalog.contribute(&mut tools, &mut prompt);
    assert!(
        warnings.is_empty(),
        "plugin contribution warnings: {warnings:?}"
    );
    let mut agent = Agent::with_runtime_budget(
        model,
        tools,
        Arc::new(BuiltinContextFactory),
        prompt,
        None,
        dir.workdir(),
        ContextConfig::with_output_reserve(16_384, 1024)?,
    )?;
    agent.attach_task_control(control_host(catalog))?;
    Ok(agent)
}

/// Tool arguments may depend on the history position (a real model derives a
/// `source_ref` from the messages it was shown).
type ToolArguments = dyn Fn(usize, &[Message]) -> Value + Send + Sync;

/// One scripted model turn.
enum Step {
    Text(String),
    Metadata(Arc<Mutex<Option<Value>>>),
    /// Simulates a crashed process: the run future is dropped mid-request.
    Crash(Arc<tokio::sync::Notify>),
    Tool {
        name: String,
        arguments: Box<ToolArguments>,
    },
}

struct ScriptedModel {
    steps: Vec<Step>,
    calls: Mutex<usize>,
    /// Messages the host actually projected, for assertions about stripping.
    projections: Mutex<Vec<Vec<Message>>>,
    /// Optional steering delivery at a chosen model call ordinal.
    steer: Mutex<Option<(usize, kroma::steering::TaskInput, String)>>,
}

impl ScriptedModel {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps,
            calls: Mutex::new(0),
            projections: Mutex::new(Vec::new()),
            steer: Mutex::new(None),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

/// The real goal line, not a host control reference. A short session keeps
/// transcript positions aligned with the authoritative history.
fn user_index(messages: &[Message]) -> usize {
    if let Some(index) = messages
        .iter()
        .rev()
        .filter_map(|message| message.content.as_deref())
        .filter(|text| text.contains("宿主投影的目标状态"))
        .filter_map(|text| {
            text.lines()
                .nth(1)
                .and_then(|line| serde_json::from_str::<Value>(line).ok())
        })
        .find_map(|reference| reference["user_sources"][0]["message_index"].as_u64())
    {
        return index as usize;
    }
    messages
        .iter()
        .position(|message| {
            message.role == "user"
                && !message
                    .provider_items
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .any(|item| item["type"] == "kroma_control_reference")
        })
        .expect("a real user message is always projected")
}

fn checkpoint_args(index: usize, claim: &str, satisfied: bool, with_plan: bool) -> Value {
    let assessment = if satisfied { "satisfied" } else { "pending" };
    let mut criterion = json!({
        "id": "C1",
        "text": "登录失败已修复并补上回归测试",
        "source": "user",
        "source_ref": {"message_index": index, "quote": "修复登录失败"},
        "kind": "judgement",
        "assessment": assessment,
    });
    if satisfied {
        criterion["notes"] = json!("回归检查 T1 通过；生产环境未验证。");
    }
    let mut args = json!({
        "revision": 1,
        "summary": if satisfied { "登录修复与回归检查已完成" } else { "先登记验收点与检查计划" },
        "claim": claim,
        "criteria": [criterion],
    });
    if with_plan {
        args["checks"] = json!([{
            "id": "T1",
            "command": "echo check-ok",
            "source": "derived",
            "rationale": "仓库已有的最小回归检查",
        }]);
    }
    if claim == "working" {
        args["next_step"] = json!(if satisfied {
            "提交 done 检查点"
        } else {
            "运行已登记的 T1，再更新检查点"
        });
    }
    args
}

impl ModelBackend for ScriptedModel {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        _: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            let ordinal = {
                let mut calls = self.calls.lock().unwrap();
                let ordinal = *calls;
                *calls += 1;
                ordinal
            };
            self.projections.lock().unwrap().push(messages.to_vec());
            let pending_steer = {
                let mut steer = self.steer.lock().unwrap();
                match steer.as_ref() {
                    Some((at, _, _)) if *at == ordinal => steer.take(),
                    _ => None,
                }
            };
            if let Some((_, handle, text)) = pending_steer {
                handle.steer(text)?;
            }
            let step = self.steps.get(ordinal).with_context(|| {
                let last_tool = messages
                    .iter()
                    .rev()
                    .find(|message| message.role == "tool")
                    .and_then(|message| message.content.clone())
                    .unwrap_or_default();
                let notes = messages
                    .iter()
                    .filter(|message| message.role == "user")
                    .filter_map(|message| message.content.clone())
                    .collect::<Vec<_>>()
                    .join(" | ");
                format!("unexpected model call #{ordinal}; last tool result: {last_tool}; user turns: {notes}")
            })?;
            let message = match step {
                Step::Crash(notify) => {
                    notify.notify_waiters();
                    std::future::pending::<()>().await;
                    unreachable!("a crashed request never returns")
                }
                Step::Metadata(item) => {
                    let mut message = Message::new("assistant", "后端候选回答");
                    message.provider_items = Some(vec![
                        item.lock().unwrap().take().unwrap(),
                        json!({"type":"provider_note","text":"preserve me"}),
                    ]);
                    message
                }
                Step::Text(text) => {
                    on_text(text);
                    Message::new("assistant", text.clone())
                }
                Step::Tool { name, arguments } => Message {
                    role: "assistant".into(),
                    content: Some(String::new()),
                    reasoning_content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: format!("call-{ordinal}"),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: name.clone(),
                            arguments: arguments(user_index(messages), messages).to_string(),
                        },
                    }]),
                    tool_call_id: None,
                    provider_items: None,
                },
            };
            Ok(Completion {
                message,
                usage: Some(Usage {
                    input: Some(100),
                    output: Some(10),
                    cached: Some(0),
                    ..Default::default()
                }),
                elapsed: Duration::ZERO,
                first_token: None,
            })
        })
    }

    fn generate_text<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: usize,
    ) -> ModelFuture<'a, SummaryResult> {
        Box::pin(async { anyhow::bail!("no summarization in a control test") })
    }
}

/// Read the append journal directly: one durable message per line. This proves
/// the control record and its message share the same WAL entry.
fn wal_entries(archive: &Path) -> Result<Vec<(u64, Value)>> {
    let mut path = archive.as_os_str().to_os_string();
    path.push(".wal");
    let text = fs::read_to_string(PathBuf::from(path)).context("reading session journal")?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let Some((payload, _checksum)) = line.rsplit_once('\t') else {
            anyhow::bail!("journal line without checksum");
        };
        let value: Value = serde_json::from_str(payload)?;
        let index = value["index"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("journal entry without index"))?;
        entries.push((index, value["message"].clone()));
    }
    Ok(entries)
}

fn control_records(entries: &[(u64, Value)]) -> Vec<(u64, Value)> {
    entries
        .iter()
        .flat_map(|(index, message)| {
            message["provider_items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "kroma_task_control")
                .map(|item| (*index, item.clone()))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[tokio::test]
async fn goal_m0_runs_one_task_refuses_an_early_finish_and_commits_completion() -> Result<()> {
    let dir = TestDir::new("happy")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let model = Arc::new(ScriptedModel::new(vec![
        // 1. Register one acceptance point and the check plan.
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| checkpoint_args(index, "working", false, true)),
        },
        // 2. Run the registered check exactly as declared.
        Step::Tool {
            name: "shell".into(),
            arguments: Box::new(|_, _| json!({"command": "echo check-ok"})),
        },
        // 3. Premature "done" prose: the controller must refuse it.
        Step::Text("修复完成，测试通过。".into()),
        // 4. A real `done` checkpoint after the check passed.
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| checkpoint_args(index, "done", true, false)),
        },
        // 5. The accepted final answer.
        Step::Text("登录修复与回归检查已完成。".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model.clone())?;
    let archive = agent.archive_path().to_path_buf();
    let scope = agent.control_scope();

    let line = "/goal 修复登录失败，补上回归测试";
    let command = agent
        .task_control_command(line, "修复登录失败，补上回归测试")
        .await?
        .expect("enabled controller dispatches the command");
    let ControlCommand::Prepared(task) = command else {
        anyhow::bail!("expected a prepared task, got {command:?}");
    };
    let body = agent.stage_prepared_task(task)?;
    assert_eq!(body, line, "the raw user line is the committed user turn");

    let answer = agent.run(&body, &mut |_| {}).await?;
    assert_eq!(answer, "登录修复与回归检查已完成。");
    assert_eq!(
        model.calls(),
        5,
        "one normal Agent loop, no extra model call"
    );

    // Host-private metadata is stripped from every provider projection.
    for projection in model.projections.lock().unwrap().iter() {
        for message in projection {
            for item in message.provider_items.as_deref().unwrap_or(&[]) {
                assert!(
                    !matches!(
                        item["type"].as_str(),
                        Some("kroma_task_control" | "kroma_task_execution")
                    ),
                    "host metadata leaked into the provider projection: {item}"
                );
            }
        }
    }
    let entries = wal_entries(&archive)?;
    let records = control_records(&entries);
    assert_eq!(
        records.len(),
        4,
        "start, two checkpoints and completion are durably recorded"
    );
    let (final_index, completed) = records.last().cloned().expect("a control record exists");
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["owner"], "goal");
    assert_eq!(completed["revision"], 1);
    assert!(completed["seq"].as_u64().unwrap() > 1);
    // The accepted answer and the completed snapshot are one WAL message.
    let carried = entries
        .iter()
        .find(|(index, _)| *index == final_index)
        .map(|(_, message)| message)
        .expect("carrier message exists");
    assert_eq!(carried["role"], "assistant");
    assert_eq!(carried["content"], "登录修复与回归检查已完成。");

    // The refused early finish is visible as a host control reference and did
    // not install a completion.
    let notes = entries
        .iter()
        .filter(|(_, message)| {
            message["content"]
                .as_str()
                .is_some_and(|text| text.contains("[任务控制参考]"))
        })
        .count();
    assert_eq!(notes, 1, "exactly one refused finish was explained");

    let start = records.first().expect("start record");
    assert_eq!(start.1["status"], "running");
    assert_eq!(start.1["seq"], 1);
    assert_eq!(start.1["scope"], serde_json::to_value(&scope)?);
    // The plugin saw only the text after `/goal`; the durable user turn is
    // the complete original line.
    assert_eq!(start.1["data"]["summary"], "修复登录失败，补上回归测试");
    let user_turn = entries
        .iter()
        .find(|(index, _)| *index == start.0)
        .map(|(_, message)| message)
        .expect("start carrier");
    assert_eq!(user_turn["role"], "user");
    assert_eq!(user_turn["content"], line);
    drop(agent);
    Ok(())
}

fn blocked_args(index: usize) -> Value {
    let mut args = checkpoint_args(index, "blocked", false, true);
    args["block_reason"] = json!("需要生产环境凭据才能验证登录");
    args
}

#[tokio::test]
async fn blocked_claim_pauses_the_goal_without_pretending_to_finish() -> Result<()> {
    let dir = TestDir::new("blocked")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| checkpoint_args(index, "working", false, true)),
        },
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| blocked_args(index)),
        },
        Step::Text("需要生产环境凭据，无法继续验证。".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model)?;
    let archive = agent.archive_path().to_path_buf();
    let line = "/goal 修复登录失败，补上回归测试";
    let command = agent
        .task_control_command(line, "修复登录失败，补上回归测试")
        .await?
        .context("controller dispatch")?;
    let ControlCommand::Prepared(task) = command else {
        anyhow::bail!("expected a prepared task");
    };
    let body = agent.stage_prepared_task(task)?;
    let answer = agent.run(&body, &mut |_| {}).await?;
    assert_eq!(answer, "需要生产环境凭据，无法继续验证。");

    let records = control_records(&wal_entries(&archive)?);
    let (_, last) = records.last().expect("control record");
    // A blocked claim pauses immediately; missing checks must not hide it.
    assert_eq!(last["status"], "paused");
    assert_eq!(last["reason"], "BLOCKED");
    assert!(records
        .iter()
        .all(|(_, record)| record["status"] != "completed"));
    // The paused record shares the answer's commit, not a later side file.
    assert_eq!(last["data"]["block_reason"], "需要生产环境凭据才能验证登录");
    drop(agent);
    Ok(())
}

/// Raw shell facts the host recorded for a goal (never a result string).
fn shell_execution_count(archive: &Path) -> Result<usize> {
    Ok(wal_entries(archive)?
        .iter()
        .map(|(_, message)| {
            message["provider_items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "kroma_task_execution")
                .filter(|item| item["tool"] == "shell")
                .count()
        })
        .sum())
}

#[tokio::test]
async fn a_crashed_run_recovers_as_paused_without_replaying_anything() -> Result<()> {
    let dir = TestDir::new("crash")?;
    install_goal(&dir)?;
    let crashed = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| checkpoint_args(index, "working", true, true)),
        },
        Step::Tool {
            name: "shell".into(),
            arguments: Box::new(|_, _| json!({"command": "echo check-ok"})),
        },
        Step::Crash(crashed.clone()),
    ]));
    let plugin_dir = dir.plugin_dir();
    let state_path = dir.state_path();
    let workdir = dir.workdir();
    let run_dir = workdir.clone();
    let handle = tokio::spawn(async move {
        let catalog = PluginCatalog::discover_at(plugin_dir, state_path, &run_dir)?;
        let mut tools = ToolRegistry::builtin();
        let mut prompt = PromptComposer::default();
        catalog.contribute(&mut tools, &mut prompt);
        let mut agent = Agent::with_runtime_budget(
            model,
            tools,
            Arc::new(BuiltinContextFactory),
            prompt,
            None,
            run_dir.clone(),
            ContextConfig::with_output_reserve(16_384, 1024)?,
        )?;
        agent.attach_task_control(control_host(&catalog))?;
        let command = agent
            .task_control_command("/goal 修复登录失败", "修复登录失败")
            .await?
            .context("dispatch")?;
        let ControlCommand::Prepared(task) = command else {
            anyhow::bail!("expected a prepared task");
        };
        let body = agent.stage_prepared_task(task)?;
        agent.run(&body, &mut |_| {}).await
    });
    // The second model request never answers: the process "dies" there.
    tokio::time::timeout(Duration::from_secs(20), crashed.notified())
        .await
        .context("the scripted crash point was reached")?;
    handle.abort();
    assert!(handle.await.is_err(), "the run future was dropped");

    // Reopening the same archive must not replay the tool call, and the
    // abandoned running record becomes a durably paused goal.
    let reopened = catalog(&dir)?;
    let session = agent_with(&dir, &reopened, Arc::new(ScriptedModel::new(vec![])))?;
    let archive = session.archive_path().to_path_buf();
    let entries = wal_entries(&archive)?;
    let records = control_records(&entries);
    let (_, last) = records.last().expect("control record after recovery");
    assert_eq!(last["status"], "paused");
    assert_eq!(last["reason"], "interrupted");
    assert_eq!(
        shell_execution_count(&archive)?,
        1,
        "the interrupted shell call is not replayed"
    );
    let note = entries.last().map(|(_, message)| message).context("note")?;
    assert!(note["content"]
        .as_str()
        .is_some_and(|text| text.contains("已记录为暂停")));
    drop(session);
    let _ = workdir;
    Ok(())
}

/// A controller that answers commands/tools but breaks the finish callback:
/// either a non-JSON-decision reply or no reply at all. It exists only to prove
/// the host pauses instead of fail-open.
const CHAOS_JS: &str = r#"
const readline = require("node:readline");
const input = readline.createInterface({input: process.stdin, crlfDelay: Infinity});
const write = frame => process.stdout.write(JSON.stringify(frame) + "\n");
input.on("line", line => {
  let req;
  try { req = JSON.parse(line); } catch { return; }
  const base = {protocol_version: 2, request_id: req.request_id};
  const context = req.control_context || {};
  if (req.type === "command") {
    write({...base, content: "目标已准备", control: {op: "start", base_seq: context.seq || 0,
      data: {summary: "chaos goal", claim: "working", criteria: [], next_step: "登记检查点"}, checks: []}});
  } else if (req.type === "tool") {
    write({...base, content: "accepted", control: {op: "update", base_seq: context.seq,
      id: context.id, revision: context.revision,
      data: {summary: "chaos done", claim: "done", criteria: [
        {id: "C1", text: "完成", source: "derived", kind: "judgement", assessment: "satisfied", notes: "test"}
      ]}}});
  } else if (req.type === "task_control") {
    if ((req.config || {}).chaos === "hang") { return; }
    write({...base, content: "not a decision"});
  } else {
    write({...base, content: "unsupported request", is_error: true});
  }
});
"#;

fn install_chaos(dir: &TestDir, mode: &str) -> Result<()> {
    let target = dir.plugin_dir().join("chaos");
    fs::create_dir_all(&target)?;
    let manifest = format!(
        r#"api_version = 2
id = "chaos"
name = "Chaos"
command = "node"
args = ["main.js"]
capabilities = ["task_control"]

[settings]
chaos = "{mode}"

[[commands]]
name = "/goal"

[[tools]]
name = "goal_checkpoint"
description = "chaos checkpoint"

[tools.parameters]
type = "object"
"#
    );
    fs::write(target.join("plugin.toml"), manifest)?;
    fs::write(target.join("main.js"), CHAOS_JS)?;
    fs::create_dir_all(dir.workdir())?;
    fs::write(dir.state_path(), "enabled = [\"chaos\"]\n")?;
    Ok(())
}

async fn run_chaos_finish(mode: &str) -> Result<()> {
    let dir = TestDir::new(mode)?;
    install_chaos(&dir, mode)?;
    let catalog = catalog(&dir)?;
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|_, _| json!({"revision": 1, "summary": "x", "claim": "working"})),
        },
        Step::Text("全部完成。".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model)?;
    let archive = agent.archive_path().to_path_buf();
    let command = agent
        .task_control_command("/goal 随便做点什么", "随便做点什么")
        .await?
        .context("dispatch")?;
    let ControlCommand::Prepared(task) = command else {
        anyhow::bail!("expected a prepared task");
    };
    let body = agent.stage_prepared_task(task)?;
    let answer = agent.run(&body, &mut |_| {}).await?;
    assert_eq!(answer, "全部完成。");
    let records = control_records(&wal_entries(&archive)?);
    let (_, last) = records.last().expect("control record");
    assert_eq!(last["status"], "paused", "a broken callback never finishes");
    assert_eq!(last["reason"], "plugin_error");
    assert!(records
        .iter()
        .all(|(_, record)| record["status"] != "completed"));
    drop(agent);
    Ok(())
}

#[tokio::test]
async fn a_malformed_finish_reply_pauses_instead_of_failing_open() -> Result<()> {
    run_chaos_finish("garbage").await
}

#[tokio::test]
async fn a_timed_out_finish_reply_pauses_instead_of_failing_open() -> Result<()> {
    run_chaos_finish("hang").await
}

#[tokio::test]
async fn a_disabled_controller_adds_no_control_ipc_to_ordinary_tasks() -> Result<()> {
    let dir = TestDir::new("disabled")?;
    install_goal(&dir)?;
    // Installed but disabled: no controller, no control IPC, no host metadata.
    fs::write(dir.state_path(), "")?;
    let catalog = catalog(&dir)?;
    assert!(catalog.task_control().is_none());
    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    assert!(catalog.contribute(&mut tools, &mut prompt).is_empty());
    let model = Arc::new(ScriptedModel::new(vec![Step::Text("普通回答".into())]));
    let mut agent = Agent::with_runtime_budget(
        model.clone(),
        tools,
        Arc::new(BuiltinContextFactory),
        prompt,
        None,
        dir.workdir(),
        ContextConfig::with_output_reserve(16_384, 1024)?,
    )?;
    assert!(agent.task_control_command_name().is_none());
    let answer = agent.run("普通任务，不需要目标", &mut |_| {}).await?;
    assert_eq!(answer, "普通回答");
    assert_eq!(model.calls(), 1, "one ordinary model request");
    let entries = wal_entries(agent.archive_path())?;
    assert!(control_records(&entries).is_empty());
    assert!(entries.iter().all(|(_, message)| {
        message["provider_items"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|item| item["type"] != "kroma_task_execution")
    }));
    drop(agent);
    Ok(())
}

#[tokio::test]
async fn a_user_stop_wins_and_pauses_without_consulting_the_plugin() -> Result<()> {
    let dir = TestDir::new("stop")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let blocked = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| checkpoint_args(index, "working", true, true)),
        },
        Step::Crash(blocked.clone()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model)?;
    let archive = agent.archive_path().to_path_buf();
    let command = agent
        .task_control_command("/goal 修复登录失败", "修复登录失败")
        .await?
        .context("dispatch")?;
    let ControlCommand::Prepared(task) = command else {
        anyhow::bail!("expected a prepared task");
    };
    let body = agent.stage_prepared_task(task)?;
    let mut sink = |_: &str| {};
    let mut run = Box::pin(agent.run(&body, &mut sink));
    tokio::select! {
        _ = blocked.notified() => {
            // Drop the running future exactly like Ctrl+C, then record the stop.
            drop(run);
            agent.cancel()?;
        }
        result = &mut run => anyhow::bail!("task finished instead of stopping: {result:?}"),
    }
    let records = control_records(&wal_entries(&archive)?);
    let (_, last) = records.last().expect("control record");
    assert_eq!(last["status"], "paused");
    assert_eq!(last["reason"], "user_stop");
    assert!(records
        .iter()
        .all(|(_, record)| record["status"] != "completed"));
    // Cancellation is committed with the durable cancel note, not a new file.
    let entries = wal_entries(&archive)?;
    let note = entries.last().map(|(_, message)| message).context("note")?;
    assert!(note["content"]
        .as_str()
        .is_some_and(|text| text.contains("Task cancelled by user")));
    drop(agent);
    Ok(())
}

#[tokio::test]
async fn two_enabled_controllers_are_reported_instead_of_stacked() -> Result<()> {
    let dir = TestDir::new("conflict")?;
    for id in ["alpha", "beta"] {
        let target = dir.plugin_dir().join(id);
        fs::create_dir_all(&target)?;
        let manifest = format!(
            r#"api_version = 2
id = "{id}"
name = "{id}"
command = "node"
args = ["main.js"]
capabilities = ["task_control"]

[[commands]]
name = "/goal"

[[tools]]
name = "goal_checkpoint"
description = "checkpoint"

[tools.parameters]
type = "object"
"#
        );
        fs::write(target.join("plugin.toml"), manifest)?;
        fs::write(target.join("main.js"), CHAOS_JS)?;
    }
    fs::create_dir_all(dir.workdir())?;
    fs::write(dir.state_path(), "enabled = [\"alpha\", \"beta\"]\n")?;
    let catalog = catalog(&dir)?;
    assert!(
        catalog.task_control().is_none(),
        "no controller is installed"
    );
    assert!(catalog
        .warnings
        .iter()
        .any(|warning| warning.contains("task_control")));
    assert!(
        catalog.ensure_loadable().is_err(),
        "a conflicting controller is an explicit startup error"
    );
    Ok(())
}

#[tokio::test]
async fn repeated_refused_finishes_do_not_force_the_user_to_resume() -> Result<()> {
    let dir = TestDir::new("retry")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let mut steps = vec![Step::Tool {
        name: "goal_checkpoint".into(),
        arguments: Box::new(|index, _| checkpoint_args(index, "working", false, true)),
    }];
    for _ in 0..5 {
        steps.push(Step::Text("尝试提前收尾".into()));
    }
    steps.push(Step::Tool {
        name: "shell".into(),
        arguments: Box::new(|_, _| json!({"command":"echo check-ok"})),
    });
    steps.push(Step::Tool {
        name: "goal_checkpoint".into(),
        arguments: Box::new(|index, _| checkpoint_args(index, "done", true, false)),
    });
    steps.push(Step::Text("完成实际检查后交付".into()));
    let model = Arc::new(ScriptedModel::new(steps));
    let mut agent = agent_with(&dir, &catalog, model.clone())?;
    let body = start_goal(&mut agent).await?;
    assert_eq!(agent.run(&body, &mut |_| {}).await?, "完成实际检查后交付");
    assert_eq!(
        agent.task_control().unwrap().snapshot().unwrap().status,
        kroma::task_control::TaskStatus::Completed
    );
    assert_eq!(model.calls(), 9);
    Ok(())
}

/// Late user input still wins over a controller pause: the instruction must
/// reach the model instead of being dropped with the active-task guard.
#[tokio::test]
async fn a_pause_defers_to_steering_that_arrived_during_the_final_answer() -> Result<()> {
    let dir = TestDir::new("pause-steer")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| blocked_args(index)),
        },
        Step::Text("需要用户补充。".into()),
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| {
                let mut args = blocked_args(index);
                args["revision"] = json!(2);
                args
            }),
        },
        Step::Text("已处理最新指令，仍需凭据。".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model.clone())?;
    *model.steer.lock().unwrap() =
        Some((1, agent.task_input(), "先别暂停，用更简单的方案重做".into()));
    let body = start_goal(&mut agent).await?;
    agent.run(&body, &mut |_| {}).await?;
    assert!(model.projections.lock().unwrap()[2].iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|text| text.contains("先别暂停"))));
    let snapshot = agent.task_control().unwrap().snapshot().unwrap();
    assert_eq!(snapshot.status, kroma::task_control::TaskStatus::Paused);
    assert_eq!(snapshot.revision, 2);
    Ok(())
}

/// A fork, rewind or reset opens a different archive generation. A copied
/// record stays reference-only: the next turn of the new history must neither
/// keep the old controller active nor consult the plugin about it.
#[tokio::test]
async fn a_new_history_generation_cannot_revive_a_running_goal() -> Result<()> {
    let dir = TestDir::new("scope")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let blocked = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| checkpoint_args(index, "working", true, true)),
        },
        Step::Crash(blocked.clone()),
        // The first turn of the cleared history is ordinary work.
        Step::Text("普通回答".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model.clone())?;
    let command = agent
        .task_control_command("/goal 修复登录失败", "修复登录失败")
        .await?
        .context("dispatch")?;
    let ControlCommand::Prepared(task) = command else {
        anyhow::bail!("expected a prepared task");
    };
    let body = agent.stage_prepared_task(task)?;
    let mut sink = |_: &str| {};
    let mut run = Box::pin(agent.run(&body, &mut sink));
    tokio::select! {
        _ = blocked.notified() => {
            drop(run);
            agent.cancel()?;
        }
        result = &mut run => anyhow::bail!("task finished instead of stopping: {result:?}"),
    }
    let host = agent.task_control().cloned().context("controller")?;
    assert_eq!(
        host.snapshot().map(|snapshot| snapshot.status),
        Some(kroma::task_control::TaskStatus::Paused),
        "the user stop paused the goal before the history changed"
    );

    agent.reset()?;
    assert!(
        !host.is_active(),
        "the cleared generation cannot keep the old goal active"
    );
    assert!(
        host.snapshot().is_none(),
        "a copied record is never rebound as the current goal"
    );
    assert_eq!(agent.task_control_command_name().as_deref(), Some("/goal"));

    let answer = agent.run("普通问题", &mut |_| {}).await?;
    assert_eq!(answer, "普通回答");
    assert_eq!(
        model.calls(),
        3,
        "no finish callback or extra model call for the cleared goal"
    );
    assert!(!host.is_active());
    drop(agent);
    Ok(())
}

/// Forking a history that still contains the user's goal line must keep the
/// requirement as a new paused goal: no old id, no check plan, no pass facts.
#[tokio::test]
async fn a_forked_history_keeps_the_requirement_as_a_new_paused_goal() -> Result<()> {
    let dir = TestDir::new("fork")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| checkpoint_args(index, "working", true, true)),
        },
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| blocked_args(index)),
        },
        Step::Text("需要生产环境凭据，无法继续验证。".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model)?;
    let source = agent.archive_path().to_path_buf();
    let line = "/goal 修复登录失败，补上回归测试";
    let command = agent
        .task_control_command(line, "修复登录失败，补上回归测试")
        .await?
        .context("controller dispatch")?;
    let ControlCommand::Prepared(task) = command else {
        anyhow::bail!("expected a prepared task");
    };
    let body = agent.stage_prepared_task(task)?;
    agent.run(&body, &mut |_| {}).await?;
    let before = agent
        .task_control()
        .and_then(|host| host.snapshot())
        .context("paused goal")?;
    assert_eq!(before.status, kroma::task_control::TaskStatus::Paused);

    let fork = agent.fork(None)?;
    let branch = agent.archive_path().to_path_buf();
    assert_ne!(branch, source);
    assert_eq!(agent.session_id(), fork);
    let carried = agent
        .task_control()
        .and_then(|host| host.snapshot())
        .context("carried goal")?;
    assert_ne!(carried.id, before.id, "a branch mints its own goal id");
    assert_eq!(carried.status, kroma::task_control::TaskStatus::Paused);
    assert_eq!(carried.reason.as_deref(), Some("carried_over"));
    assert_eq!(carried.revision, 1);
    assert_eq!(
        carried.checks, before.checks,
        "the plan survives, not its pass facts"
    );
    assert!(
        carried.checkpoint_basis.is_none(),
        "no completion basis is inherited"
    );
    assert!(carried.data.get("claim").is_none());
    assert_eq!(carried.source_indices, before.source_indices);

    let records = control_records(&wal_entries(&branch)?);
    let (_, last) = records.last().context("carried record")?;
    assert_eq!(last["status"], "paused");
    assert_eq!(last["reason"], "carried_over");
    assert_eq!(last["id"], carried.id);

    // The source archive keeps its own record; the branch never rewrote it.
    let source_records = control_records(&wal_entries(&source)?);
    let (_, source_last) = source_records.last().context("source record")?;
    assert_eq!(source_last["id"], before.id);
    assert_eq!(source_last["reason"], "BLOCKED");
    drop(agent);
    Ok(())
}

/// The active goal is rebuilt into every provider projection as a host
/// reference, and an ordinary task gets none. It is never persisted.
#[tokio::test]
async fn the_active_goal_is_projected_as_a_rebuilt_host_reference() -> Result<()> {
    let dir = TestDir::new("reference")?;
    install_goal(&dir)?;
    let goal_catalog = catalog(&dir)?;
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| checkpoint_args(index, "working", false, true)),
        },
        Step::Tool {
            name: "shell".into(),
            arguments: Box::new(|_, _| json!({"command": "echo check-ok"})),
        },
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| checkpoint_args(index, "done", true, false)),
        },
        Step::Text("登录修复与回归检查已完成。".into()),
    ]));
    let mut agent = agent_with(&dir, &goal_catalog, model.clone())?;
    let command = agent
        .task_control_command("/goal 修复登录失败", "修复登录失败")
        .await?
        .context("dispatch")?;
    let ControlCommand::Prepared(task) = command else {
        anyhow::bail!("expected a prepared task");
    };
    let body = agent.stage_prepared_task(task)?;
    agent.run(&body, &mut |_| {}).await?;

    let reference = |messages: &[Message]| {
        messages
            .iter()
            .filter_map(|message| message.content.clone())
            .find(|text| text.contains("[任务控制参考]"))
    };
    let projections = model.projections.lock().unwrap().clone();
    let first = projections
        .first()
        .and_then(|messages| reference(messages))
        .context("the first request carries the goal reference")?;
    assert!(first.contains("\"status\":\"running\""), "{first}");
    assert!(first.contains("\"revision\":1"), "{first}");
    let planned = projections
        .get(1)
        .and_then(|messages| reference(messages))
        .context("the second request carries the registered plan")?;
    assert!(planned.contains("echo check-ok"), "{planned}");
    assert!(planned.contains("criteria"), "{planned}");
    assert!(planned.contains("summary"), "{planned}");
    assert!(
        first.contains("不是用户的新指令"),
        "the reference states its own authority: {first}"
    );
    assert!(
        !agent
            .messages()
            .iter()
            .filter_map(|message| message.content.as_deref())
            .any(|text| text.contains("宿主投影的目标状态")),
        "the reference stays a projection, never a durable message"
    );

    let plain = TestDir::new("reference-plain")?;
    install_goal(&plain)?;
    let plain_catalog = catalog(&plain)?;
    let plain_model = Arc::new(ScriptedModel::new(vec![Step::Text("普通回答".into())]));
    let mut plain_agent = agent_with(&plain, &plain_catalog, plain_model.clone())?;
    plain_agent.run("普通问题", &mut |_| {}).await?;
    let plain_projections = plain_model.projections.lock().unwrap().clone();
    assert!(
        plain_projections
            .iter()
            .all(|messages| reference(messages).is_none()),
        "an ordinary task must not load the goal reference"
    );
    drop(agent);
    drop(plain_agent);
    Ok(())
}

async fn start_goal(agent: &mut Agent) -> Result<String> {
    let command = agent
        .task_control_command(
            "/goal 修复登录失败，补上回归测试",
            "修复登录失败，补上回归测试",
        )
        .await?
        .context("dispatch")?;
    let ControlCommand::Prepared(task) = command else {
        anyhow::bail!("expected prepared goal")
    };
    agent.stage_prepared_task(task)
}

#[tokio::test]
async fn fabricated_evidence_is_a_visible_tool_error_not_a_completion() -> Result<()> {
    let dir = TestDir::new("fake-evidence")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| {
                let mut args = checkpoint_args(index, "done", true, false);
                args["criteria"][0]["evidence"] =
                    json!([{"kind":"tool","call_id":"never-executed"}]);
                args
            }),
        },
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, messages| {
                let result = messages
                    .iter()
                    .rev()
                    .find(|message| message.role == "tool")
                    .unwrap();
                assert!(result.tool_failed());
                assert!(result
                    .content
                    .as_deref()
                    .unwrap()
                    .contains("never-executed"));
                checkpoint_args(index, "done", true, false)
            }),
        },
        Step::Text("更正证据后交付".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model)?;
    let body = start_goal(&mut agent).await?;
    agent.run(&body, &mut |_| {}).await?;
    let snapshot = agent.task_control().unwrap().snapshot().unwrap();
    assert_eq!(snapshot.status, kroma::task_control::TaskStatus::Completed);
    assert!(!snapshot.data.to_string().contains("never-executed"));
    Ok(())
}

#[tokio::test]
async fn checks_can_be_recorded_after_running_and_survive_an_inspection() -> Result<()> {
    let dir = TestDir::new("post-check-inspect")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "shell".into(),
            arguments: Box::new(|_, _| json!({"command":"echo check-ok"})),
        },
        Step::Tool {
            name: "shell".into(),
            arguments: Box::new(|_, _| json!({"command":"echo inspect-only"})),
        },
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| checkpoint_args(index, "done", true, true)),
        },
        Step::Text("检查通过，查看结果后交付".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model.clone())?;
    let body = start_goal(&mut agent).await?;
    agent.run(&body, &mut |_| {}).await?;
    assert_eq!(
        model.calls(),
        4,
        "no forced plan-first step or repeated check"
    );
    assert!(agent.task_control().unwrap().facts()?.checks_satisfied);
    Ok(())
}

#[tokio::test]
async fn old_conversation_length_does_not_block_starting_a_goal() -> Result<()> {
    for messages in [
        (0..64)
            .map(|index| Message::new("user", format!("无关话题 {index}")))
            .collect::<Vec<_>>(),
        (0..9)
            .map(|_| Message::new("user", "x".repeat(8192)))
            .collect::<Vec<_>>(),
    ] {
        let dir = TestDir::new("long-conversation")?;
        install_goal(&dir)?;
        let catalog = catalog(&dir)?;
        let host = control_host(&catalog);
        let session = kroma::commands::CommandSession {
            workdir: dir.workdir(),
            archive: dir.workdir().join("source.json"),
            session_id: "review".into(),
        };
        let scope = kroma::task_control::scope_for(&session.archive, 1);
        let command = host
            .dispatch_command(
                "/goal 修复登录失败",
                "修复登录失败",
                &session,
                &messages,
                &scope,
            )
            .await?;
        let ControlCommand::Prepared(task) = command else {
            anyhow::bail!("not prepared")
        };
        assert_eq!(task.update.snapshot.source_indices, vec![messages.len()]);
    }
    Ok(())
}

#[tokio::test]
async fn a_long_goal_and_all_optional_conditions_remain_in_the_projection() -> Result<()> {
    let dir = TestDir::new("full-goal")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|_, _| {
                json!({
                    "summary":"等待用户条件","claim":"blocked","block_reason":"需要凭据",
                    "criteria":[{"id":"first","text":"A".repeat(512)},{"id":"second","text":"B".repeat(512)},{"id":"third","text":"IMPORTANT_FINAL_REQUIREMENT"}]
                })
            }),
        },
        Step::Text("请补充凭据".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model.clone())?;
    let args = format!(
        "修复登录失败 {} END_OF_USER_REQUIREMENT",
        "x".repeat(17 * 1024)
    );
    let line = format!("/goal {args}");
    let ControlCommand::Prepared(task) = agent.task_control_command(&line, &args).await?.unwrap()
    else {
        anyhow::bail!("not prepared")
    };
    let body = agent.stage_prepared_task(task)?;
    agent.run(&body, &mut |_| {}).await?;
    let views = model.projections.lock().unwrap();
    let reference = views[1]
        .iter()
        .filter_map(|m| m.content.as_deref())
        .find(|text| text.contains("宿主投影的目标状态"))
        .unwrap();
    assert!(reference.contains("END_OF_USER_REQUIREMENT"));
    assert!(reference.contains("IMPORTANT_FINAL_REQUIREMENT"));
    assert!(reference.contains("message_index"));
    Ok(())
}

#[tokio::test]
async fn a_fork_keeps_command_requirements_without_dangling_check_ids() -> Result<()> {
    let dir = TestDir::new("fork-command")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|index, _| {
                let mut args = blocked_args(index);
                args["criteria"][0]["kind"] = json!("command");
                args["criteria"][0]["check_ids"] = json!(["T1"]);
                args
            }),
        },
        Step::Text("等待凭据".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model)?;
    let body = start_goal(&mut agent).await?;
    agent.run(&body, &mut |_| {}).await?;
    agent.fork(None)?;
    let command = agent
        .task_control_command("/goal resume", "resume")
        .await?
        .context("resume")?;
    let ControlCommand::Prepared(task) = command else {
        anyhow::bail!("not prepared")
    };
    assert_eq!(task.update.snapshot.checks[0].id, "T1");
    assert!(task.update.snapshot.checkpoint_basis.is_none());
    assert_eq!(task.update.snapshot.data["claim"], "working");
    Ok(())
}

#[tokio::test]
async fn a_completed_goal_can_reopen_explicitly_but_never_revives_on_fork() -> Result<()> {
    let dir = TestDir::new("fork-finished")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Tool {
            name: "goal_checkpoint".into(),
            arguments: Box::new(|_, _| json!({"summary":"已交付","claim":"done"})),
        },
        Step::Text("已完成".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model)?;
    let body = start_goal(&mut agent).await?;
    agent.run(&body, &mut |_| {}).await?;
    let command = agent
        .task_control_command("/goal resume 补充文档", "resume 补充文档")
        .await?
        .unwrap();
    let ControlCommand::Prepared(task) = command else {
        anyhow::bail!("not prepared")
    };
    assert_eq!(task.update.snapshot.revision, 2);
    assert_eq!(
        agent.task_control().unwrap().snapshot().unwrap().status,
        kroma::task_control::TaskStatus::Completed,
        "preparing is not running"
    );
    agent.fork(None)?;
    assert!(agent.task_control().unwrap().snapshot().is_none());
    Ok(())
}

#[tokio::test]
async fn early_crash_and_repeated_restart_do_not_lose_plugin_version_or_resume() -> Result<()> {
    let dir = TestDir::new("early-crash")?;
    install_goal(&dir)?;
    let catalog1 = catalog(&dir)?;
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::new(vec![Step::Crash(reached.clone())]));
    let mut agent = agent_with(&dir, &catalog1, model)?;
    let body = start_goal(&mut agent).await?;
    {
        let mut callback = |_: &str| {};
        let run = agent.run(&body, &mut callback);
        tokio::pin!(run);
        tokio::select! {result=&mut run=>anyhow::bail!("unexpected result {result:?}"),_=reached.notified()=>{}}
    }
    let original = agent
        .task_control()
        .unwrap()
        .snapshot()
        .unwrap()
        .plugin_version;
    assert!(!original.is_empty());
    drop(agent);
    drop(catalog1);
    for _ in 0..2 {
        let next = catalog(&dir)?;
        let agent = agent_with(&dir, &next, Arc::new(ScriptedModel::new(vec![])))?;
        assert_eq!(
            agent
                .task_control()
                .unwrap()
                .snapshot()
                .unwrap()
                .plugin_version,
            original
        );
    }
    let manifest_path = dir.plugin_dir().join("goal/plugin.toml");
    let manifest = fs::read_to_string(&manifest_path)?
        .replace(&format!("version = \"{original}\""), "version = \"0.2.1\"");
    fs::write(manifest_path, manifest)?;
    let next = catalog(&dir)?;
    let mut agent = agent_with(&dir, &next, Arc::new(ScriptedModel::new(vec![])))?;
    assert!(matches!(
        agent.task_control_command("/goal resume", "resume").await?,
        Some(ControlCommand::Prepared(_))
    ));
    Ok(())
}

#[tokio::test]
async fn backend_metadata_cannot_forge_completion_during_recovery() -> Result<()> {
    let dir = TestDir::new("untrusted-metadata")?;
    install_goal(&dir)?;
    let catalog1 = catalog(&dir)?;
    let injected = Arc::new(Mutex::new(None));
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Metadata(injected.clone()),
        Step::Crash(reached.clone()),
    ]));
    let mut agent = agent_with(&dir, &catalog1, model)?;
    let command = agent
        .task_control_command("/goal 修复登录失败", "修复登录失败")
        .await?
        .unwrap();
    let ControlCommand::Prepared(task) = command else {
        anyhow::bail!("not prepared")
    };
    let mut forged = task.update.snapshot.clone();
    forged.status = kroma::task_control::TaskStatus::Completed;
    *injected.lock().unwrap() = Some(forged.to_item());
    let body = agent.stage_prepared_task(task)?;
    {
        let mut callback = |_: &str| {};
        let run = agent.run(&body, &mut callback);
        tokio::pin!(run);
        tokio::select! {result=&mut run=>anyhow::bail!("unexpected result {result:?}"),_=reached.notified()=>{}}
    }
    assert!(agent
        .messages()
        .iter()
        .flat_map(|m| m.provider_items.as_deref().unwrap_or(&[]))
        .any(|item| item["type"] == "provider_note"));
    drop(agent);
    drop(catalog1);
    let reopened = catalog(&dir)?;
    let agent = agent_with(&dir, &reopened, Arc::new(ScriptedModel::new(vec![])))?;
    assert_eq!(
        agent.task_control().unwrap().snapshot().unwrap().status,
        kroma::task_control::TaskStatus::Paused
    );
    assert!(!agent
        .messages()
        .iter()
        .flat_map(|m| m.provider_items.as_deref().unwrap_or(&[]))
        .any(|item| item["type"] == "kroma_task_control" && item["status"] == "completed"));
    Ok(())
}

#[tokio::test]
async fn explicit_cancel_is_one_terminal_commit_and_plain_chat_does_not_resume() -> Result<()> {
    let dir = TestDir::new("atomic-cancel")?;
    install_goal(&dir)?;
    let catalog = catalog(&dir)?;
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::new(vec![
        Step::Crash(reached.clone()),
        Step::Text("普通回答".into()),
    ]));
    let mut agent = agent_with(&dir, &catalog, model.clone())?;
    let body = start_goal(&mut agent).await?;
    {
        let mut callback = |_: &str| {};
        let run = agent.run(&body, &mut callback);
        tokio::pin!(run);
        tokio::select! {result=&mut run=>anyhow::bail!("unexpected result {result:?}"),_=reached.notified()=>{}}
    }
    agent.cancel_goal()?;
    let records = control_records(&wal_entries(agent.archive_path())?);
    assert_eq!(records.last().unwrap().1["status"], "cancelled");
    assert!(!records.iter().any(|(_, r)| r["status"] == "paused"));
    agent.run("换个话题", &mut |_| {}).await?;
    assert!(!model.projections.lock().unwrap()[1]
        .iter()
        .filter_map(|m| m.content.as_deref())
        .any(|text| text.contains("宿主投影的目标状态")));
    Ok(())
}

/// The manifest is what a provider sees. An evidence object must be able to
/// satisfy its own schema; the first version forbade `kind` and required it.
#[test]
fn the_goal_tool_schema_accepts_the_evidence_it_asks_for() -> Result<()> {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/goal/plugin.toml"),
    )?;
    let manifest: toml::Value = toml::from_str(&text)?;
    let schema = serde_json::to_value(&manifest)?;
    let parameters = &schema["tools"][0]["parameters"];
    assert_eq!(parameters["required"], json!(["summary", "claim"]));
    let evidence =
        &parameters["properties"]["criteria"]["items"]["properties"]["evidence"]["items"];
    assert_eq!(evidence["additionalProperties"], false);
    assert_eq!(
        evidence["properties"]["kind"]["enum"],
        json!(["tool", "final_answer"])
    );
    assert!(
        evidence.get("anyOf").is_none(),
        "no branch may re-require a forbidden field"
    );
    assert_eq!(parameters["properties"]["criteria"].get("maxItems"), None);
    assert_eq!(parameters["properties"]["checks"].get("maxItems"), None);
    Ok(())
}
