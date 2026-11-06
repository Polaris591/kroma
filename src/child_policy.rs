//! Child-only tool and prompt policy. The normal Kroma runtime does not load
//! this file or pay for its checks. These are tool-level restrictions, not OS
//! filesystem isolation: a test process may still execute repository code.

use crate::{
    prompt::PromptComposer,
    protocol::ToolCall,
    tools::{BuiltinShell, Tool, ToolContext, ToolFuture, ToolRegistry, ToolResult},
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{collections::HashSet, fs, path::Path, sync::Arc};

#[derive(Clone, Copy, Debug)]
pub struct ExecutionLimits {
    pub max_turns: usize,
    pub max_tokens: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildPolicy {
    profile: String,
    prompt: String,
    tools: Option<Vec<String>>,
    #[serde(default)]
    disallowed_tools: Vec<String>,
    #[serde(default)]
    host_denied_tools: Vec<String>,
    shell: String,
    max_turns: usize,
    max_tokens: u64,
}

impl ChildPolicy {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os("KROMA_CHILD_POLICY_FILE") else {
            return Ok(None);
        };
        if std::env::var("KROMA_CHILD_DEPTH").ok().as_deref() != Some("1") {
            bail!("child policy requires KROMA_CHILD_DEPTH=1");
        }
        let path = Path::new(&path);
        let metadata = fs::metadata(path).context("subagent policy file unavailable")?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            bail!("subagent policy must be a regular file under 64 KiB");
        }
        let policy: Self =
            serde_json::from_slice(&fs::read(path)?).context("invalid subagent policy JSON")?;
        if policy.profile.trim().is_empty()
            || policy.prompt.trim().is_empty()
            || policy.prompt.len() > 32 * 1024
            || !(1..=100).contains(&policy.max_turns)
            || policy.max_tokens > 1_000_000
            || !matches!(
                policy.shell.as_str(),
                "inherit" | "deny" | "read-only" | "test"
            )
        {
            bail!("invalid subagent profile, prompt or budget");
        }
        Ok(Some(policy))
    }

    pub fn limits(&self) -> ExecutionLimits {
        ExecutionLimits {
            max_turns: self.max_turns,
            max_tokens: self.max_tokens,
        }
    }

    pub fn apply(&self, tools: &mut ToolRegistry, prompt: &mut PromptComposer) -> Result<()> {
        let advertised = tools.definitions();
        let names: Vec<String> = advertised
            .iter()
            .filter_map(|value| value["function"]["name"].as_str().map(str::to_owned))
            .collect();
        let allow: Option<HashSet<&str>> = self
            .tools
            .as_ref()
            .map(|names| names.iter().map(String::as_str).collect());
        let deny: HashSet<&str> = self.disallowed_tools.iter().map(String::as_str).collect();
        let host_deny: HashSet<&str> = self.host_denied_tools.iter().map(String::as_str).collect();
        let mut enabled = 0;
        for name in &names {
            if allow
                .as_ref()
                .is_some_and(|only| !only.contains(name.as_str()))
                || deny.contains(name.as_str())
                || host_deny.contains(name.as_str())
                || (name == "shell" && self.shell == "deny")
            {
                tools.disable(name)?;
            } else {
                enabled += 1;
            }
        }
        if self.tools.as_ref().is_some_and(|items| !items.is_empty()) && enabled == 0 {
            bail!("NO_TOOLS: subagent profile tool allowlist matched no available tools");
        }
        if names.iter().any(|name| name == "shell")
            && enabled > 0
            && matches!(self.shell.as_str(), "read-only" | "test")
            && tools
                .definitions()
                .iter()
                .any(|value| value["function"]["name"] == "shell")
        {
            tools.replace(
                "shell",
                Arc::new(GuardedShell {
                    mode: self.shell.clone(),
                }),
            )?;
        }
        prompt.push_trailing(format!(
            "\nSubagent profile [{}]:\n{}",
            self.profile, self.prompt
        ))?;
        Ok(())
    }
}

struct GuardedShell {
    mode: String,
}

impl Tool for GuardedShell {
    fn name(&self) -> &str {
        "shell"
    }

    fn definition(&self) -> serde_json::Value {
        let mut definition = BuiltinShell.definition();
        definition["function"]["description"] = serde_json::json!(format!(
            "Restricted subagent shell ({}). Use simple commands only; pipelines, redirections and substitutions are not allowed.",
            self.mode
        ));
        definition
    }

    fn execute<'a>(&'a self, call: &'a ToolCall, ctx: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            let command = serde_json::from_str::<serde_json::Value>(&call.function.arguments)
                .ok()
                .and_then(|value| value["command"].as_str().map(str::to_owned));
            match command.as_deref() {
                Some(command) if allowed_command(command, &self.mode) => {
                    BuiltinShell.execute(call, ctx).await
                }
                _ => ToolResult::error(
                    "subagent shell policy refused this command; use an allowed simple command",
                ),
            }
        })
    }
}

fn allowed_command(command: &str, mode: &str) -> bool {
    if command.is_empty() || command.len() > 8192 {
        return false;
    }
    // A single, purely literal command. In particular, reject shell control,
    // expansion, redirection and quoting rather than trying to blacklist
    // dangerous writes after Bash has interpreted them.
    if !command.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || c.is_ascii_whitespace() && c != '\n' && c != '\r'
            || matches!(c, '_' | '-' | '/' | '.' | ':' | '=' | ',' | '%')
    }) {
        return false;
    }
    let parts: Vec<&str> = command.split_ascii_whitespace().collect();
    if parts.len() > 64 {
        return false;
    }
    let Some(first) = parts.first() else {
        return false;
    };
    if matches!(
        *first,
        "pwd" | "ls" | "cat" | "head" | "tail" | "wc" | "stat"
    ) {
        return parts[1..]
            .iter()
            .all(|arg| !arg.starts_with("--output") && *arg != "--files0-from");
    }
    if matches!(*first, "rg" | "grep") {
        return parts[1..].iter().all(|arg| {
            !arg.starts_with('-')
                || matches!(
                    *arg,
                    "-n" | "-i" | "-F" | "-l" | "--files" | "--hidden" | "--no-heading" | "--"
                )
        });
    }
    if *first == "git" {
        let rest = &parts[1..];
        let (verb, args) = if rest.first() == Some(&"--no-pager") {
            (rest.get(1), &rest[2..])
        } else {
            (rest.first(), rest.get(1..).unwrap_or(&[]))
        };
        return verb.is_some_and(|v| {
            matches!(
                *v,
                "status" | "diff" | "log" | "show" | "grep" | "ls-files" | "rev-parse"
            )
        }) && args.iter().all(|arg| {
            !arg.starts_with('-')
                || matches!(
                    *arg,
                    "--short"
                        | "--stat"
                        | "--oneline"
                        | "--name-only"
                        | "--name-status"
                        | "--no-ext-diff"
                        | "--"
                        | "-n"
                )
        });
    }
    if mode == "test" {
        return match *first {
            "cargo" => parts
                .get(1)
                .is_some_and(|verb| matches!(*verb, "test" | "check" | "clippy" | "fmt")),
            "npm" => parts
                .get(1)
                .is_some_and(|verb| matches!(*verb, "test" | "run")),
            "node" => parts.get(1) == Some(&"--test"),
            "pytest" => true,
            "go" => parts.get(1) == Some(&"test"),
            "python" | "python3" => parts.get(1..3) == Some(&["-m", "pytest"][..]),
            _ => false,
        };
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NamedTool(&'static str);

    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn definition(&self) -> serde_json::Value {
            serde_json::json!({"type":"function","function":{"name":self.0,
                "parameters":{"type":"object"}}})
        }
        fn execute<'a>(&'a self, _: &'a ToolCall, _: ToolContext<'a>) -> ToolFuture<'a> {
            Box::pin(async { ToolResult::ok("mock") })
        }
    }

    #[test]
    fn read_only_rejects_shell_control_and_mutation() {
        for allowed in [
            "pwd",
            "ls -la",
            "cat src/lib.rs",
            "rg -n error src",
            "git status --short",
            "git diff --stat",
        ] {
            assert!(allowed_command(allowed, "read-only"), "{allowed}");
        }
        for forbidden in [
            "rm -rf src",
            "echo hello > file",
            "git reset --hard",
            "git diff --output=file",
            "rg --pre sh src",
            "cat file; rm file",
            "cat $(touch file)",
            "cat file | tee new.txt",
            "curl example.com",
        ] {
            assert!(!allowed_command(forbidden, "read-only"), "{forbidden}");
        }
        assert!(allowed_command("cargo test --locked", "test"));
        assert!(!allowed_command("cargo test --locked", "read-only"));
    }

    #[test]
    fn policy_filters_advertised_tools_and_adds_profile_prompt() {
        let policy = ChildPolicy {
            profile: "explore".into(),
            prompt: "Inspect only".into(),
            tools: Some(vec!["shell".into()]),
            disallowed_tools: vec![],
            host_denied_tools: vec![],
            shell: "read-only".into(),
            max_turns: 20,
            max_tokens: 10000,
        };
        let mut tools = ToolRegistry::builtin();
        let mut prompt = PromptComposer::default();
        policy.apply(&mut tools, &mut prompt).unwrap();
        assert_eq!(tools.definitions().len(), 1);
        assert!(tools.definitions()[0]["function"]["description"]
            .as_str()
            .unwrap()
            .contains("Restricted subagent"));
        assert_eq!(policy.limits().max_turns, 20);
    }

    #[test]
    fn permissive_profile_still_forbids_nested_agents_and_memory_writes() {
        let mut tools = ToolRegistry::builtin();
        for name in [
            "subagent",
            "memory_remember",
            "memory_forget",
            "memory_search",
        ] {
            tools.register(Arc::new(NamedTool(name))).unwrap();
        }
        let policy = ChildPolicy {
            profile: "general".into(),
            prompt: "Finish the task".into(),
            tools: None,
            disallowed_tools: vec![],
            host_denied_tools: vec![
                "subagent".into(),
                "memory_remember".into(),
                "memory_forget".into(),
            ],
            shell: "inherit".into(),
            max_turns: 40,
            max_tokens: 0,
        };
        policy
            .apply(&mut tools, &mut PromptComposer::default())
            .unwrap();
        let names: Vec<String> = tools
            .definitions()
            .iter()
            .filter_map(|v| v["function"]["name"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(names, vec!["shell", "memory_search"]);
    }
}
