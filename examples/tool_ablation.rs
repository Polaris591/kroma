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

/// Benchmark-only unified-diff tool. Production ToolRegistry remains unchanged.
/// The parser confines every patch target to the benchmark workdir before git
/// checks and applies the patch. No shell evaluates model-supplied text.
struct ApplyPatch;

fn apply_patch_text(workdir: &Path, patch: &str) -> Result<Value> {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    if patch.trim().is_empty() || patch.len() > 128 * 1024 {
        bail!("patch must contain a bounded unified diff");
    }
    let root = workdir.canonicalize()?;
    let mut headers = 0;
    for line in patch.lines() {
        let raw = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "));
        let Some(raw) = raw else {
            continue;
        };
        if raw == "/dev/null" {
            continue;
        }
        let rel = raw
            .strip_prefix("a/")
            .or_else(|| raw.strip_prefix("b/"))
            .context("patch paths must start with a/ or b/")?;
        let target = Path::new(rel);
        if !target
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            bail!("patch path escapes project directory");
        }
        let mut parent = root.join(target);
        while !parent.exists() {
            parent = parent
                .parent()
                .context("invalid patch target")?
                .to_path_buf();
        }
        if !parent.canonicalize()?.starts_with(&root) {
            bail!("patch target resolves outside project directory");
        }
        headers += 1;
    }
    if headers == 0 || !patch.lines().any(|line| line.starts_with("@@ ")) {
        bail!("expected a unified diff containing file headers and hunks");
    }
    for check in [true, false] {
        let mut cmd = Command::new("git");
        cmd.arg("apply").arg("--whitespace=nowarn");
        if check {
            cmd.arg("--check");
        }
        let mut child = cmd
            .arg("-")
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start git apply")?;
        child
            .stdin
            .take()
            .context("git apply stdin")?
            .write_all(patch.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr);
            bail!(
                "git apply {} failed: {}",
                if check { "--check" } else { "apply" },
                message.trim()
            );
        }
    }
    Ok(json!({"applied":true,"checked":true}))
}

impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{
            "name":"apply_patch",
            "description":"Apply a unified Git diff to project files. Use headers --- a/path and +++ b/path, followed by @@ -start,count +start,count @@ and lines prefixed space, minus or plus. Use /dev/null for file creation or deletion. Paths must stay inside this project. Prefer this tool for related multi-line edits; do not use shell heredocs or sed to edit code.",
            "parameters":{"type":"object","properties":{
                "patch":{"type":"string","description":"Complete unified diff, with --- and +++ file headers and @@ hunks"}
            },"required":["patch"],"additionalProperties":false}
        }})
    }
    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            let result: Result<Value> = (|| {
                let args: Value = serde_json::from_str(&call.function.arguments)?;
                let patch = args["patch"].as_str().context("missing patch string")?;
                apply_patch_text(ctx.workdir, patch)
            })();
            match result {
                Ok(value) => ToolResult::ok(value.to_string()),
                Err(error) => ToolResult::error(json!({"error":format!("{error:#}")}).to_string()),
            }
        })
    }
}

#[cfg(test)]
mod native_patch_tests {
    use super::*;
    #[test]
    fn patch_changes_only_a_project_file() {
        let root =
            std::env::temp_dir().join(format!("kroma-patch-ablation-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("sample.txt"), "before\n").unwrap();
        let patch = "--- a/sample.txt\n+++ b/sample.txt\n@@ -1 +1 @@\n-before\n+after\n";
        apply_patch_text(&root, patch).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("sample.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "after\n"
        );
        assert!(
            apply_patch_text(&root, "--- a/../bad\n+++ b/../bad\n@@ -0,0 +1 @@\n+wrong\n").is_err()
        );
        fs::remove_dir_all(root).unwrap();
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
        "native_edit",
        "native_patch",
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
        "aligned" | "scoped" | "batch" | "cache_prefix" | "native_edit" | "native_patch"
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
        if arm == "native_patch" {
            components.tools.disable("edit_file")?;
            components.tools.register(Arc::new(ApplyPatch))?;
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
