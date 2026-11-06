//! Regressions for the runtime/context budget and accounting repairs.
use anyhow::{bail, Result};
use kroma::{
    agent::Agent,
    context::{
        BuiltinContextFactory, CompactionState, Compression, ContextConfig, ContextFactory,
        ContextPolicy,
    },
    context_state::PersistedContext,
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, TextResult, Usage},
    plugins::PluginCatalog,
    ports::ContextCommit,
    prompt::PromptComposer,
    protocol::Message,
    provider::ModelClient,
    tools::ToolRegistry,
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const GOOD: &str = "1. OBJECTIVE\nFinish the parser repair and preserve the public API.\n\
2. CONSTRAINTS\nKeep exact error text PARSER_E17 and do not skip required tests.\n\
3. PROGRESS\nThe focused test is complete; the integration test is still pending.\n\
4. CODE STATE\nParser handling changed; the trailing-input path is not verified yet.\n\
5. UNRESOLVED ISSUES\nMalformed trailing input still needs investigation before success.\n\
6. KEY DECISIONS\nKeep the tokenizer unchanged so the real defect is not hidden.\n\
7. NEXT ACTION\nInspect the trailing-input path and run the regression test.";

struct Temp(PathBuf);
impl Temp {
    fn new(label: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "kroma-budget-{label}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct Captured {
    source: Vec<Message>,
    instruction: String,
}
#[derive(Default)]
struct Capture {
    calls: AtomicUsize,
    seen: Mutex<Vec<Captured>>,
    hang: bool,
    started: tokio::sync::Notify,
}
impl ModelBackend for Capture {
    fn complete<'a>(
        &'a self,
        _: &'a [Message],
        _: &'a [Value],
        _: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Completion {
                message: Message::new("assistant", "ok"),
                usage: Some(Usage::default()),
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
    ) -> ModelFuture<'a, TextResult> {
        Box::pin(async { bail!("unexpected generic generation") })
    }
    fn generate_checkpoint<'a>(
        &'a self,
        m: &'a [Message],
        _: &'a [Value],
        i: &'a str,
        _: usize,
    ) -> ModelFuture<'a, TextResult> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(Captured {
                source: m.to_vec(),
                instruction: i.to_owned(),
            });
            self.started.notify_one();
            if self.hang {
                return std::future::pending().await;
            }
            Ok(TextResult {
                text: GOOD.into(),
                usage: Some(Usage::default()),
                elapsed: Duration::ZERO,
                requests: 1,
            })
        })
    }
}
#[derive(Default)]
struct Store(Mutex<Vec<PersistedContext>>);
impl ContextCommit for Store {
    fn commit(&self, state: &PersistedContext) -> Result<()> {
        self.0.lock().unwrap().push(state.clone());
        Ok(())
    }
}
struct Reject;
impl ContextCommit for Reject {
    fn commit(&self, _: &PersistedContext) -> Result<()> {
        bail!("simulated durable commit failure")
    }
}

fn estimate(messages: &[Message]) -> usize {
    serde_json::to_vec(messages).unwrap().len().div_ceil(3) + 256
}
fn turns() -> Vec<Message> {
    let mut history = vec![Message::new("system", "system")];
    for _ in 0..6 {
        history.push(Message::new("user", "task ".repeat(500)));
        history.push(Message::new("assistant", "answer ".repeat(500)));
    }
    history
}
fn compression(history: &[Message], budget: ContextConfig, archive: &Path) -> Compression {
    let mut state = CompactionState::default();
    state.consumed_until = history.len();
    state.covered_until = 1;
    Compression::new(state, budget, archive)
}

#[tokio::test]
async fn replacement_prompt_is_budgeted_by_its_actual_projection() -> Result<()> {
    let temp = Temp::new("short-prompt")?;
    let model = Arc::new(Capture::default());
    let budget = ContextConfig::with_output_reserve(8_192, 1_024)?;
    let mut agent = Agent::with_runtime_budget(
        model.clone(),
        ToolRegistry::new(),
        Arc::new(BuiltinContextFactory),
        PromptComposer::default().with_system_prompt("L".repeat(30_000))?,
        None,
        temp.path().to_path_buf(),
        budget,
    )?;
    // The stored system message stays large; the request projection is short.
    let short = PromptComposer::default().with_system_prompt("short replacement")?;
    let mut actual = agent.messages().to_vec();
    short.apply(&mut actual);
    actual.push(Message::new("user", "hello"));
    assert!(budget.validate_final_request(&actual, &[]).is_ok());

    agent.reload_extensions(ToolRegistry::new(), short, None);
    agent.run("hello", &mut |_| {}).await?;
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn escaped_prompt_growth_reaches_compaction_before_the_final_guard() -> Result<()> {
    let temp = Temp::new("escaped-prompt")?;
    let model = Arc::new(Capture::default());
    let budget = ContextConfig::with_output_reserve(8_192, 1_024)?;
    let mut agent = Agent::with_runtime_budget(
        model.clone(),
        ToolRegistry::new(),
        Arc::new(BuiltinContextFactory),
        PromptComposer::default(),
        None,
        temp.path().to_path_buf(),
        budget,
    )?;
    for _ in 0..4 {
        agent.run(&"x".repeat(2_100), &mut |_| {}).await?;
    }
    // Text length grows by 4000 chars, JSON escaping grows by ~8000 bytes.
    let prompt = PromptComposer::default().with_extra("\"".repeat(4_000))?;
    let mut projected = agent.messages().to_vec();
    projected.push(Message::new("user", "continue"));
    let planned = estimate(&projected) + prompt.extra_estimated_tokens(&projected[0]) + 1;
    prompt.apply(&mut projected);
    let escaped = estimate(&projected) + 1;
    eprintln!(
        "planned={planned} soft={} escaped={escaped} hard={}",
        budget.soft_limit(),
        budget.hard_limit()
    );
    assert!(
        planned < budget.soft_limit(),
        "fixture must not trigger on the text estimate alone"
    );
    assert!(
        escaped > budget.hard_limit(),
        "fixture must exceed the escaped hard limit"
    );
    assert!(
        escaped < budget.available_input(),
        "fixture must still be representable"
    );
    agent.reload_extensions(ToolRegistry::new(), prompt, None);
    agent.run("continue", &mut |_| {}).await?;
    assert_eq!(model.calls.load(Ordering::SeqCst), 5);
    assert!(
        !model.seen.lock().unwrap().is_empty(),
        "the escaped projection must trigger an automatic checkpoint"
    );
    Ok(())
}

/// Applies the real provider budget projection while keeping summary
/// generation local and deterministic.
struct Projecting {
    inner: Capture,
    client: ModelClient,
}
impl ModelBackend for Projecting {
    fn project_for_budget(&self, messages: &mut [Message]) {
        self.client.project_for_budget(messages);
    }
    fn complete<'a>(
        &'a self,
        m: &'a [Message],
        t: &'a [Value],
        on: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        self.inner.complete(m, t, on)
    }
    fn generate_text<'a>(
        &'a self,
        s: &'a str,
        t: &'a str,
        o: usize,
    ) -> ModelFuture<'a, TextResult> {
        self.inner.generate_text(s, t, o)
    }
    fn generate_checkpoint<'a>(
        &'a self,
        m: &'a [Message],
        t: &'a [Value],
        i: &'a str,
        o: usize,
    ) -> ModelFuture<'a, TextResult> {
        self.inner.generate_checkpoint(m, t, i, o)
    }
}

#[tokio::test]
async fn checkpoint_input_cap_covers_the_appended_instruction() -> Result<()> {
    let temp = Temp::new("instruction")?;
    let mut history = vec![Message::new("system", "system")];
    history.push(Message::new("user", "e".repeat(1_500)));
    history.push(Message::new("assistant", "ok"));
    for _ in 0..4 {
        history.push(Message::new("user", "turn question"));
        history.push(Message::new("assistant", "turn answer"));
    }
    history.push(Message::new("user", "u".repeat(4_700)));
    history.push(Message::new("assistant", "a".repeat(3_000)));
    let cap = 2_048;
    let budget = ContextConfig::with_output_reserve(16_384, 1_024)?.with_input_limit(cap)?;
    let model = Arc::new(Capture::default());
    let backend: Arc<dyn ModelBackend> = model.clone();
    let mut context = compression(&history, budget, &temp.path().join("session.json"));
    context.set_request_context(&backend, &[]);
    context
        .compact(&history, &backend, &Store::default(), 0)
        .await?;
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "fixture must request one checkpoint");
    let mut full = seen[0].source.clone();
    assert!(!seen[0].instruction.is_empty());
    full.push(Message::new("user", &seen[0].instruction));
    let complete = estimate(&full);
    assert!(
        complete <= cap,
        "checkpoint source plus its instruction must respect the input cap: {complete} > {cap}"
    );
    Ok(())
}

#[tokio::test]
async fn provider_filtered_reasoning_does_not_block_a_fitting_checkpoint() -> Result<()> {
    let temp = Temp::new("reasoning")?;
    let history = vec![
        Message::new("system", "system"),
        Message::new("user", "previous task"),
        Message {
            reasoning_content: Some("stale thinking ".repeat(8_000)),
            ..Message::new("assistant", "a".repeat(2_000))
        },
        Message::new("user", "u".repeat(8_000)),
        Message::new("assistant", "a".repeat(8_000)),
    ];
    let mut state = CompactionState::default();
    state.consumed_until = history.len();
    state.covered_until = 2;
    state.revision = 1;
    state.summaries = vec![kroma::context_state::SummarySegment {
        until: 2,
        text: "prior task checkpoint ".repeat(10),
    }];
    state.validate(&history)?;
    let budget = ContextConfig::with_output_reserve(8_192, 1_024)?;
    let mut context = Compression::new(state, budget, &temp.path().join("session.json"));
    let client = ModelClient::new("http://127.0.0.1:9/v1", None, "plain-chat".into())?;
    let model = Arc::new(Projecting {
        inner: Capture::default(),
        client: client.clone(),
    });
    let mut projected = context.view(&history);
    projected.pop();
    client.project_for_budget(&mut projected);
    assert!(
        budget.validate_final_request(&projected, &[]).is_ok(),
        "the provider-filtered candidate fits"
    );
    let backend: Arc<dyn ModelBackend> = model.clone();
    context.set_request_context(&backend, &[]);
    let report = context
        .compact(&history, &backend, &Store::default(), 0)
        .await?;
    assert!(
        report.compacted,
        "history the provider will not send must not make every boundary look oversized"
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_checkpoint_reports_unknown_usage() -> Result<()> {
    let temp = Temp::new("cancelled")?;
    let history = turns();
    let model = Arc::new(Capture {
        hang: true,
        ..Default::default()
    });
    let backend: Arc<dyn ModelBackend> = model.clone();
    let mut context = compression(
        &history,
        ContextConfig::with_output_reserve(32_768, 4_096)?,
        &temp.path().join("session.json"),
    );
    context.set_request_context(&backend, &[]);
    let store = Store::default();
    let mut compact = Box::pin(context.compact(&history, &backend, &store, 0));
    tokio::select! {
        ready = tokio::time::timeout(Duration::from_secs(2), model.started.notified()) => ready?,
        result = &mut compact => panic!("unexpected completion: {result:?}"),
    }
    drop(compact);
    assert_eq!(model.seen.lock().unwrap().len(), 1);
    assert!(store.0.lock().unwrap().is_empty());
    let usage = context.take_usage();
    assert_eq!(usage.requests, 1, "a started checkpoint is not free");
    assert!(usage.input.is_none(), "cancelled usage stays unknown");
    Ok(())
}

fn capture_provider() -> Result<(String, std::sync::mpsc::Receiver<Value>)> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        // A connection may close before sending an HTTP body. Do not let that
        // consume the only capture slot and make subsequent retries hit
        // ConnectionRefused on a now-closed test listener (seen on macOS CI).
        while started.elapsed() < Duration::from_secs(10) {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(_) => return,
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let parsed = (|| -> Result<Value> {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                if reader.read_line(&mut line)? == 0 {
                    bail!("empty HTTP request");
                }
                let mut length = None;
                loop {
                    line.clear();
                    if reader.read_line(&mut line)? == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = Some(value.trim().parse::<usize>()?);
                    }
                }
                let length = length.ok_or_else(|| anyhow::anyhow!("missing content length"))?;
                if length > 65_536 {
                    bail!("unexpectedly large mock model payload");
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body)?;
                Ok(serde_json::from_slice(&body)?)
            })();
            let text = "audit stop";
            let _ = write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                text.len(),
                text
            );
            let _ = stream.flush();
            if let Ok(request) = parsed {
                let _ = sender.send(request);
                return;
            }
        }
    });
    Ok((url, receiver))
}

fn install_http_model(temp: &Temp, url: &str, protocol: &str) -> Result<PluginCatalog> {
    let source = temp.path().join("http-source");
    fs::create_dir_all(&source)?;
    fs::write(
        source.join("plugin.toml"),
        format!(
            "id='http-probe'\nname='HTTP Probe'\n\n[model]\nmodel='audit-native'\nbase_url='{url}'\nprotocol='{protocol}'\ncontext_window=8192\noutput_reserve=1024\n"
        ),
    )?;
    fs::write(source.join("main.js"), "// no process model\n")?;
    let directory = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?.install(&source)?;
    PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?
        .set_enabled("http-probe", true)?;
    PluginCatalog::discover_at(directory, state, temp.path())
}

#[tokio::test]
async fn http_model_plugin_wire_output_cap_matches_its_budget() -> Result<()> {
    for protocol in ["responses", "anthropic"] {
        let temp = Temp::new(protocol)?;
        let (url, requests) = capture_provider()?;
        let catalog = install_http_model(&temp, &url, protocol)?;
        let (backend, budget, _) = catalog
            .checked_model_override()?
            .expect("enabled HTTP model plugin");
        assert_eq!(budget.output_reserve(), 1_024, "{protocol}");
        let _ = backend
            .complete(&[Message::new("user", "hi")], &[], &mut |_| {})
            .await;
        let request = requests
            .recv_timeout(Duration::from_secs(15))
            .map_err(|error| anyhow::anyhow!("{protocol}: no request captured: {error}"))?;
        let field = if protocol == "responses" {
            "max_output_tokens"
        } else {
            "max_tokens"
        };
        assert_eq!(
            request[field].as_u64(),
            Some(1_024),
            "{protocol} must not send a larger output cap than the budget reserved"
        );
    }
    Ok(())
}

#[tokio::test]
async fn external_context_usage_survives_a_rejected_state_commit() -> Result<()> {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        return Ok(());
    }
    let temp = Temp::new("plugin-usage")?;
    let source = temp.path().join("plugin-source");
    fs::create_dir_all(&source)?;
    fs::write(
        source.join("plugin.toml"),
        "id='audit-probe'\nname='Audit Probe'\ncommand='node'\nargs=['main.js']\n[context]\nversion=2\n",
    )?;
    fs::write(
        source.join("main.js"),
        r#"require('node:readline').createInterface({input:process.stdin}).on('line', line => {
  const req = JSON.parse(line);
  const patch = {messages:req.arguments.messages, state:{revision:1}, recovered:true,
    usage:{requests:1,input:40,output:6,cached:3}};
  process.stdout.write(JSON.stringify({content:JSON.stringify(patch)})+String.fromCharCode(10));
});
"#,
    )?;
    let directory = temp.path().join("plugins");
    let state = temp.path().join("plugins.toml");
    PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?.install(&source)?;
    PluginCatalog::discover_at(directory.clone(), state.clone(), temp.path())?
        .set_enabled("audit-probe", true)?;
    let catalog = PluginCatalog::discover_at(directory, state, temp.path())?;
    let mut factory: Arc<dyn ContextFactory> = Arc::new(BuiltinContextFactory);
    assert!(catalog.replace_context(&mut factory).is_empty());
    let mut policy = factory.restore(
        factory.initial(),
        ContextConfig::with_output_reserve(32_768, 4_096)?,
        &temp.path().join("session.json"),
    )?;
    let model: Arc<dyn ModelBackend> = Arc::new(Capture::default());
    let history = vec![
        Message::new("system", "system"),
        Message::new("user", "hello"),
    ];
    let original = serde_json::to_value(policy.state())?;
    let failure = policy
        .prepare(&history, &model, &Reject, 0)
        .await
        .unwrap_err();
    eprintln!("PLUGIN ERROR: {failure:#}");
    assert_eq!(
        serde_json::to_value(policy.state())?,
        original,
        "derived state must stay transactional"
    );
    let usage = policy.take_usage();
    assert_eq!(usage.requests, 1, "usage is an observation, not a commit");
    assert_eq!(usage.input, Some(40));
    assert_eq!(usage.cached, Some(3));
    Ok(())
}
