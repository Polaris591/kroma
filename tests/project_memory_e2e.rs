//! Real-host integration for the zero-maintenance project-memory plugin.
//!
//! The plugin binary is built separately (see examples/plugins/project-memory).
//! Set KROMA_MEMORY_BIN to its path, or build the default debug binary. When no
//! binary is available the test prints an explicit skip instead of failing.
use anyhow::{bail, Context, Result};
use kroma::{
    context::ContextConfig,
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, TextResult},
    plugins::PluginCatalog,
    prompt::PromptComposer,
    protocol::{FunctionCall, Message, ToolCall},
    tools::{ToolContext, ToolRegistry},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);
static NEXT_CALL: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kroma-project-memory-{}-{nanos}-{}",
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

fn memory_binary() -> Result<Option<PathBuf>> {
    let name = if cfg!(windows) {
        "kroma-memory.exe"
    } else {
        "kroma-memory"
    };
    let default = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples/plugins/project-memory/target/debug")
        .join(name);
    let path = match std::env::var_os("KROMA_MEMORY_BIN") {
        Some(value) => {
            let path = PathBuf::from(value);
            if !path.is_file() {
                bail!(
                    "KROMA_MEMORY_BIN is set to '{}', but that is not a regular file",
                    path.display()
                );
            }
            path
        }
        None => default,
    };
    if !path.is_file() {
        eprintln!(
            "skipping project-memory integration: {} is not built (run `cargo build` in examples/plugins/project-memory or set KROMA_MEMORY_BIN)",
            path.display()
        );
        return Ok(None);
    }
    Ok(Some(path.canonicalize().unwrap_or(path)))
}

fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            c if c as u32 == 92 => {
                out.push(char::from_u32(92).unwrap());
                out.push(char::from_u32(92).unwrap());
            }
            '"' => out.push_str("\\\""),
            '\n' => out.push('\n'),
            '\r' => out.push('\r'),
            '\t' => out.push('\t'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Install the shipped manifest after replacing its PATH command with the
/// absolute binary built by this test run.
fn write_plugin(directory: &Path, binary: &Path) -> Result<()> {
    fs::create_dir_all(directory)?;
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples/plugins/project-memory/plugin/plugin.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let marker = "command = \"kroma-memory\"";
    if manifest.matches(marker).count() != 1 {
        bail!(
            "expected exactly one PATH command in {}",
            manifest_path.display()
        );
    }
    let manifest = manifest.replace(
        marker,
        &format!("command = {}", toml_string(&binary.to_string_lossy())),
    );
    fs::write(directory.join("plugin.toml"), manifest)?;
    Ok(())
}

async fn call_tool(
    tools: &ToolRegistry,
    name: &str,
    arguments: Value,
    workdir: &Path,
    archive: &Path,
) -> (String, bool) {
    call_tool_with_user(tools, name, arguments, workdir, archive, None, None).await
}

async fn call_tool_with_user(
    tools: &ToolRegistry,
    name: &str,
    arguments: Value,
    workdir: &Path,
    archive: &Path,
    user: Option<&str>,
    turn: Option<usize>,
) -> (String, bool) {
    let call = ToolCall {
        id: format!("e2e-{}", NEXT_CALL.fetch_add(1, Ordering::Relaxed)),
        kind: "function".into(),
        function: FunctionCall {
            name: name.into(),
            arguments: arguments.to_string(),
        },
    };
    let result = tools
        .execute(
            &call,
            ToolContext {
                model_connection: None,
                user_message: user,
                user_turn: turn,
                workdir,
                archive,
                on_output: None,
            },
        )
        .await;
    (result.content, result.is_error)
}

fn tool_names(tools: &ToolRegistry) -> BTreeSet<String> {
    tools
        .definitions()
        .iter()
        .filter_map(|definition| definition["function"]["name"].as_str().map(str::to_owned))
        .collect()
}

#[derive(Default)]
struct EchoModel(Mutex<Vec<Message>>);

impl ModelBackend for EchoModel {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a [Value],
        on_text: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            *self.0.lock().unwrap() = messages.to_vec();
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
async fn zero_maintenance_flow_is_automatic_persistent_and_isolated() -> Result<()> {
    let Some(binary) = memory_binary()? else {
        return Ok(());
    };
    let temp = TestDirectory::new()?;
    let user = temp.path().join("plugins");
    let state_path = temp.path().join("plugins.toml");
    let workspace_a = temp.path().join("alpha");
    let workspace_b = temp.path().join("beta");
    let archive = temp.path().join("session.json");
    for path in [&workspace_a, &workspace_b] {
        fs::create_dir_all(path)?;
    }
    let source = temp.path().join("source").join("project-memory");
    write_plugin(&source, &binary)?;
    let catalog = PluginCatalog::discover_at(user.clone(), state_path.clone(), &workspace_a)?;
    assert!(catalog
        .install(&source)?
        .contains("Installed 'project-memory'"));
    catalog.set_enabled("project-memory", true)?;
    let enabled = PluginCatalog::discover_at(user.clone(), state_path.clone(), &workspace_a)?;
    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    assert!(enabled.contribute(&mut tools, &mut prompt).is_empty());
    let memory_tools: BTreeSet<String> = tool_names(&tools)
        .into_iter()
        .filter(|name| name.starts_with("memory_"))
        .collect();
    assert_eq!(
        memory_tools,
        BTreeSet::from([
            "memory_forget".to_owned(),
            "memory_read".to_owned(),
            "memory_remember".to_owned(),
            "memory_search".to_owned(),
        ])
    );
    let mut history = prompt.initial_history(&workspace_a);
    prompt.apply(&mut history);
    assert!(history[0]
        .content
        .as_deref()
        .unwrap_or("")
        .contains("Project memory is automatic"));
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(
        user.join("project-memory/plugin.toml"),
    )?)?;
    assert_eq!(manifest["api_version"].as_integer(), Some(2));
    assert_eq!(manifest["capabilities"].as_array().map(Vec::len), Some(2));
    assert!(manifest.get("hooks").is_none());
    assert!(manifest.get("context").is_none());
    assert!(manifest.get("events").is_none());
    assert_eq!(manifest["tools"].as_array().map(Vec::len), Some(4));

    let quote = "以后这个项目用 pnpm 不要用 npm";
    let (content, error) = call_tool(
        &tools,
        "memory_remember",
        json!({"kind":"preference","text":"Use pnpm, not npm","quote":quote}),
        &workspace_a,
        &archive,
    )
    .await;
    assert!(!error, "{content}");
    let remembered: Value = serde_json::from_str(&content)?;
    assert_eq!(remembered["state"], "pending");
    assert_eq!(remembered["searchable"], false);
    let (content, _) = call_tool(
        &tools,
        "memory_search",
        json!({"query":"pnpm"}),
        &workspace_a,
        &archive,
    )
    .await;
    let hits: Value = serde_json::from_str(&content)?;
    assert!(
        hits["results"].as_array().is_some_and(Vec::is_empty),
        "pending rules must not be searchable"
    );

    // New host contract: verified current-user text is supplied out-of-band,
    // not by the model's function arguments, and writes become active NOW.
    let current = "这个项目每次提交前必须运行 clippy";
    let (content, error) = call_tool_with_user(
        &tools,
        "memory_remember",
        json!({"kind":"decision","text":"Run clippy before commits",
            "quote":current,"topic":"commit checks"}),
        &workspace_a,
        &archive,
        Some(current),
        Some(2),
    )
    .await;
    assert!(!error, "{content}");
    let accepted: Value = serde_json::from_str(&content)?;
    assert_eq!(accepted["state"], "accepted");
    let (content, error) = call_tool(
        &tools,
        "memory_search",
        json!({"query":"clippy"}),
        &workspace_a,
        &archive,
    )
    .await;
    assert!(!error, "{content}");
    let hits: Value = serde_json::from_str(&content)?;
    assert_eq!(hits["results"].as_array().unwrap().len(), 1);

    let captured = Arc::new(EchoModel::default());
    let model = enabled.model_hooks(
        captured.clone(),
        ContextConfig::with_output_reserve(32000, 1024)?,
    );
    model.set_session_context("session-a", &archive);
    let first_task = vec![
        Message::new("system", "system"),
        Message::new("user", quote),
    ];
    model.complete(&first_task, &[], &mut |_| {}).await?;
    let (content, _) = call_tool(
        &tools,
        "memory_search",
        json!({"query":"pnpm"}),
        &workspace_a,
        &archive,
    )
    .await;
    let hits: Value = serde_json::from_str(&content)?;
    assert_eq!(hits["results"].as_array().unwrap().len(), 1);

    let second_task = vec![
        Message::new("system", "system"),
        Message::new("user", "pnpm 的命令是什么"),
    ];
    model.complete(&second_task, &[], &mut |_| {}).await?;
    let sent = captured.0.lock().unwrap().clone();
    let injected = sent
        .last()
        .and_then(|message| message.content.as_deref())
        .unwrap_or("");
    assert!(injected.contains("<project-memory trust=\"unverified-reference\">"));
    assert!(injected.contains("Use pnpm, not npm"));

    drop(model);
    drop(tools);
    drop(enabled);
    drop(catalog);
    let restarted = PluginCatalog::discover_at(user.clone(), state_path.clone(), &workspace_a)?;
    let mut restarted_tools = ToolRegistry::builtin();
    let mut restarted_prompt = PromptComposer::default();
    assert!(restarted
        .contribute(&mut restarted_tools, &mut restarted_prompt)
        .is_empty());
    let (content, _) = call_tool(
        &restarted_tools,
        "memory_search",
        json!({"query":"pnpm"}),
        &workspace_a,
        &archive,
    )
    .await;
    let hits: Value = serde_json::from_str(&content)?;
    assert_eq!(hits["results"].as_array().unwrap().len(), 1);

    let other = PluginCatalog::discover_at(user.clone(), state_path.clone(), &workspace_b)?;
    let mut other_tools = ToolRegistry::builtin();
    let mut other_prompt = PromptComposer::default();
    assert!(other
        .contribute(&mut other_tools, &mut other_prompt)
        .is_empty());
    let (content, _) = call_tool(
        &other_tools,
        "memory_search",
        json!({"query":"pnpm"}),
        &workspace_b,
        &archive,
    )
    .await;
    let hits: Value = serde_json::from_str(&content)?;
    assert!(hits["results"].as_array().is_some_and(Vec::is_empty));

    other.set_enabled("project-memory", false)?;
    let disabled = PluginCatalog::discover_at(user, state_path, &workspace_b)?;
    let mut disabled_tools = ToolRegistry::builtin();
    let mut disabled_prompt = PromptComposer::default();
    assert!(disabled
        .contribute(&mut disabled_tools, &mut disabled_prompt)
        .is_empty());
    assert!(!tool_names(&disabled_tools).contains("memory_search"));
    Ok(())
}
