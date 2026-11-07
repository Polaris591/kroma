//! Child-only tool and prompt policy. The normal Kroma runtime does not load
//! this file or pay for its checks. The policy can remove tools and cap turns
//! and tokens; it deliberately does not parse shell commands, because a command
//! grammar is not an OS sandbox and blocks ordinary read-only work.

use crate::{prompt::PromptComposer, tools::ToolRegistry};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{collections::HashSet, fs, path::Path};

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
            || policy.max_turns > 1_000_000_000
            || !matches!(policy.shell.as_str(), "allow" | "deny")
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
        prompt.push_trailing(format!(
            "\nSubagent profile [{}]:\n{}",
            self.profile, self.prompt
        ))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::ToolCall,
        tools::{Tool, ToolContext, ToolFuture, ToolResult},
    };
    use std::sync::Arc;

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
    fn policy_filters_advertised_tools_and_adds_profile_prompt() {
        let policy = ChildPolicy {
            profile: "explore".into(),
            prompt: "Inspect only".into(),
            tools: Some(vec!["shell".into()]),
            disallowed_tools: vec![],
            host_denied_tools: vec![],
            shell: "allow".into(),
            max_turns: 20,
            max_tokens: 10000,
        };
        let mut tools = ToolRegistry::builtin();
        let mut prompt = PromptComposer::default();
        policy.apply(&mut tools, &mut prompt).unwrap();
        assert_eq!(tools.definitions().len(), 1);
        assert_eq!(tools.definitions()[0]["function"]["name"], "shell");
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
            shell: "allow".into(),
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
