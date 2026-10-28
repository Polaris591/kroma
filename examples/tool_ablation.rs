//! Benchmark-only host: Kroma runtime with either shell alone or shell +
//! focused edit/write. No production code or Agent loop is changed.
use anyhow::{bail, Context, Result};
use kroma::{
    app,
    config::Config,
    protocol::ToolCall,
    tools::{Tool, ToolContext, ToolFuture, ToolResult},
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

// Keep each experimental arm as a change to ONE factor. Existing Shell and
// Edit/Write arms remain byte-identical to the earlier tool ablation.
const SCOPED_SEARCH: &str = "\nBenchmark coding-inspection guidance: Start with targeted filename discovery, then scoped `rg -n`/`grep -n` on the smallest relevant files, and bounded `sed -n` ranges. Avoid full-file `cat`, broad `grep -R` over an entire repository, repeated unchanged inspections or large logs unless targeted evidence is insufficient. This is a search-order hint, not permission to skip necessary evidence or verification.";
const MULTI_CALL: &str = "\nBenchmark independent-call guidance: When you have two or more INDEPENDENT read-only inspections to perform, issue 2-3 separate shell tool calls in the SAME assistant response. Their results will be returned together and in the specified order. Do not batch state-changing operations, edits, tests that depend on earlier output, or redundant checks. Only batch when genuinely independent; do not create extra calls just to batch.";

struct Edit;
struct Write;

fn confined_existing(root: &Path, path: &str) -> Result<PathBuf> {
    let candidate = root.join(path);
    let actual = candidate
        .canonicalize()
        .with_context(|| format!("file not found: {path}"))?;
    if !actual.starts_with(root) || !actual.is_file() {
        bail!("edit path must name a file in the benchmark workdir");
    }
    Ok(actual)
}

fn confined_new(root: &Path, path: &str) -> Result<PathBuf> {
    let candidate = root.join(path);
    let parent = candidate
        .parent()
        .context("write requires a parent directory")?;
    fs::create_dir_all(parent)?;
    let actual_parent = parent.canonicalize()?;
    if !actual_parent.starts_with(root) {
        bail!("write path must stay in benchmark workdir");
    }
    let name = candidate
        .file_name()
        .context("write path needs a file name")?;
    let output = actual_parent.join(name);
    if output.exists() && !output.canonicalize()?.starts_with(root) {
        bail!("write target must stay in benchmark workdir");
    }
    Ok(output)
}

impl Tool for Edit {
    fn name(&self) -> &str {
        "edit"
    }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{
            "name":"edit",
            "description":"Edit a file by replacing an exact occurrence of oldText with newText. Use read or shell to inspect the exact original text first. Reports an error if oldText does not occur or is ambiguous. Use this instead of ad-hoc sed/Python text replacement when possible.",
            "parameters":{"type":"object","properties":{
                "path":{"type":"string","description":"Path to an existing file"},
                "oldText":{"type":"string","description":"Exact old text"},
                "newText":{"type":"string","description":"Replacement text"}
            },"required":["path","oldText","newText"],"additionalProperties":false}
        }})
    }
    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            let result: Result<Value> = (|| {
                let args: Value = serde_json::from_str(&call.function.arguments)?;
                let path = args["path"].as_str().context("missing path")?;
                let before = args["oldText"].as_str().context("missing oldText")?;
                let after = args["newText"].as_str().context("missing newText")?;
                if before.is_empty() {
                    bail!("oldText cannot be empty");
                }
                let actual = confined_existing(ctx.workdir, path)?;
                let old = fs::read_to_string(&actual)?;
                let count = old.matches(before).count();
                if count != 1 {
                    bail!("edit matched {count} occurrences; expected exactly 1");
                }
                fs::write(&actual, old.replacen(before, after, 1))?;
                Ok(json!({"success":true,"path":path,"replacements":1}))
            })();
            match result {
                Ok(value) => ToolResult::ok(value.to_string()),
                Err(err) => ToolResult::error(json!({"error":err.to_string()}).to_string()),
            }
        })
    }
}

impl Tool for Write {
    fn name(&self) -> &str {
        "write"
    }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{
            "name":"write",
            "description":"Create a file or overwrite its entire contents with the supplied text. Use edit for localized changes to existing files. Parent directories are created when needed.",
            "parameters":{"type":"object","properties":{
                "path":{"type":"string","description":"Destination path"},
                "content":{"type":"string","description":"Complete text to write"}
            },"required":["path","content"],"additionalProperties":false}
        }})
    }
    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            let result: Result<Value> = (|| {
                let args: Value = serde_json::from_str(&call.function.arguments)?;
                let path = args["path"].as_str().context("missing path")?;
                let content = args["content"].as_str().context("missing content")?;
                let actual = confined_new(ctx.workdir, path)?;
                fs::write(actual, content)?;
                Ok(json!({"success":true,"path":path,"bytes":content.len()}))
            })();
            match result {
                Ok(value) => ToolResult::ok(value.to_string()),
                Err(err) => ToolResult::error(json!({"error":err.to_string()}).to_string()),
            }
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let arm = args.next().context(
        "usage: tool_ablation shell|editwrite|aligned|scoped|batch|cache_prefix <prompt>",
    )?;
    if ![
        "shell",
        "editwrite",
        "aligned",
        "scoped",
        "batch",
        "cache_prefix",
    ]
    .contains(&arm.as_str())
    {
        bail!("unknown arm {arm}");
    }
    let prompt = args.collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        bail!("empty prompt");
    }
    let mut config = Config::from_env()?;
    // Pi's WorkBuddy openai-completions compatibility path supplies
    // reasoning_effort=high, without an additional thinking.type field.
    // The stock Shell arm retains its original Kroma payload for comparison.
    if matches!(
        arm.as_str(),
        "aligned" | "scoped" | "batch" | "cache_prefix"
    ) {
        config.reasoning_effort = "high".into();
        config.deepseek_thinking = None;
    }
    let workdir = config.workdir.clone();
    let runtime = app::default_runtime_with(config, |components| {
        if arm == "editwrite" {
            components.tools.register(Arc::new(Edit))?;
            components.tools.register(Arc::new(Write))?;
        }
        if arm == "scoped" {
            components.prompt = std::mem::take(&mut components.prompt).with_extra(SCOPED_SEARCH)?;
        } else if arm == "batch" {
            components.prompt = std::mem::take(&mut components.prompt).with_extra(MULTI_CALL)?;
        } else if arm == "cache_prefix" {
            use kroma::prompt::EXECUTION_GUIDANCE;
            let standard = components.prompt.initial_history(&workdir)[0]
                .content
                .clone()
                .context("missing default prompt")?;
            let dynamic = format!("The starting working directory is {}. ", workdir.display());
            if !standard.contains(&dynamic) {
                bail!("dynamic system path not found");
            }
            let stable = standard.replacen(&dynamic, "", 1);
            // The default static guidance is also included before the path,
            // making the first prefix maximally shareable across workdirs.
            let candidate = format!(
                "{stable}{EXECUTION_GUIDANCE}\nStarting working directory: {}.",
                workdir.display()
            );
            components.prompt =
                std::mem::take(&mut components.prompt).with_system_prompt(candidate)?;
        }
        Ok(())
    })
    .await?;
    let mut agent = runtime.agent;
    // Session path is resolved at startup. Do not pass Kroma's session-override
    // environment variable to commands under test, especially cargo tests.
    std::env::remove_var("KROMA_SESSION_FILE");
    let answer = agent.run(&prompt, &mut |_| {}).await?;
    println!("{answer}");
    Ok(())
}
