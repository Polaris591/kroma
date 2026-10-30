//! Idle CLI commands, registered by name so future extensions can add their
//! own commands without modifying Kroma's input loop. Running-task steering
//! is intentionally handled separately; no nested agent task is started.
use crate::{agent::Agent, skills::Skill, steering::TaskInput};
use anyhow::{bail, Result};
use std::{collections::BTreeMap, future::Future, path::PathBuf, pin::Pin};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAction {
    Continue,
    Exit,
    RunFollowups,
    PauseFollowups,
}

pub struct CommandReply {
    pub output: String,
    pub action: CommandAction,
}

impl CommandReply {
    pub fn text(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            action: CommandAction::Continue,
        }
    }
    pub fn action(action: CommandAction) -> Self {
        Self {
            output: String::new(),
            action,
        }
    }
}

type SyncHandler = Box<dyn Fn(&mut Agent, &TaskInput, &str) -> Result<CommandReply> + Send + Sync>;
type ExternalFuture = Pin<Box<dyn Future<Output = Result<CommandReply>> + Send>>;
type AgentFuture<'a> = Pin<Box<dyn Future<Output = Result<CommandReply>> + Send + 'a>>;
type AgentAsyncHandler =
    Box<dyn for<'a> Fn(&'a mut Agent, &'a TaskInput, &'a str) -> AgentFuture<'a> + Send + Sync>;
#[derive(Clone)]
pub struct CommandSession {
    pub workdir: PathBuf,
    pub archive: PathBuf,
    pub session_id: String,
}
type ExternalHandler = Box<dyn Fn(String, CommandSession) -> ExternalFuture + Send + Sync>;

enum Handler {
    Sync(SyncHandler),
    AgentAsync(AgentAsyncHandler),
    External(ExternalHandler),
}

#[derive(Default)]
pub struct CommandRegistry {
    commands: BTreeMap<String, Handler>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        name: &str,
        handler: impl Fn(&mut Agent, &TaskInput, &str) -> Result<CommandReply> + Send + Sync + 'static,
    ) -> Result<()> {
        if !name.starts_with('/') || name.len() < 2 || name.chars().any(char::is_whitespace) {
            bail!("command must start with / and contain no spaces");
        }
        if self.commands.contains_key(name) {
            bail!("duplicate command '{name}'");
        }
        self.commands
            .insert(name.into(), Handler::Sync(Box::new(handler)));
        Ok(())
    }

    pub fn register_agent_async(
        &mut self,
        name: &str,
        handler: impl for<'a> Fn(&'a mut Agent, &'a TaskInput, &'a str) -> AgentFuture<'a>
            + Send
            + Sync
            + 'static,
    ) -> Result<()> {
        if !name.starts_with('/') || name.len() < 2 || name.chars().any(char::is_whitespace) {
            bail!("command must start with / and contain no spaces");
        }
        if self.commands.contains_key(name) {
            bail!("duplicate command '{name}'");
        }
        self.commands
            .insert(name.into(), Handler::AgentAsync(Box::new(handler)));
        Ok(())
    }

    /// External commands are asynchronous so a stalled plugin cannot freeze
    /// the terminal UI. The callback receives only owned arguments/workdir.
    pub fn register_async<F, Fut>(&mut self, name: &str, handler: F) -> Result<()>
    where
        F: Fn(String, PathBuf) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CommandReply>> + Send + 'static,
    {
        if !name.starts_with('/') || name.len() < 2 || name.chars().any(char::is_whitespace) {
            bail!("command must start with / and contain no spaces");
        }
        if self.commands.contains_key(name) {
            bail!("duplicate command '{name}'");
        }
        self.register_async_context(name, move |args, context| handler(args, context.workdir))
    }

    pub fn register_async_context<F, Fut>(&mut self, name: &str, handler: F) -> Result<()>
    where
        F: Fn(String, CommandSession) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CommandReply>> + Send + 'static,
    {
        if !name.starts_with('/') || name.len() < 2 || name.chars().any(char::is_whitespace) {
            bail!("command must start with / and contain no spaces");
        }
        if self.commands.contains_key(name) {
            bail!("duplicate command '{name}'");
        }
        self.commands.insert(
            name.into(),
            Handler::External(Box::new(move |args, ctx| Box::pin(handler(args, ctx)))),
        );
        Ok(())
    }

    pub async fn execute_async(
        &self,
        line: &str,
        agent: &mut Agent,
        input: &TaskInput,
    ) -> Result<Option<CommandReply>> {
        let (name, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match self.commands.get(name) {
            Some(Handler::Sync(handler)) => handler(agent, input, rest.trim()).map(Some),
            Some(Handler::AgentAsync(handler)) => {
                handler(agent, input, rest.trim()).await.map(Some)
            }
            Some(Handler::External(handler)) => handler(
                rest.trim().into(),
                CommandSession {
                    workdir: agent.workdir().to_path_buf(),
                    archive: agent.archive_path().to_path_buf(),
                    session_id: agent.session_id().to_owned(),
                },
            )
            .await
            .map(Some),
            None => Ok(None),
        }
    }

    pub fn execute(
        &self,
        line: &str,
        agent: &mut Agent,
        input: &TaskInput,
    ) -> Result<Option<CommandReply>> {
        let (name, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match self.commands.get(name) {
            Some(Handler::Sync(handler)) => handler(agent, input, rest.trim()).map(Some),
            Some(Handler::AgentAsync(_) | Handler::External(_)) => {
                bail!("command '{name}' requires execute_async")
            }
            None => Ok(None),
        }
    }

    pub fn builtin(skills: Vec<Skill>) -> Result<Self> {
        Self::builtin_with_warnings(skills, Vec::new())
    }

    pub fn builtin_with_warnings(skills: Vec<Skill>, warnings: Vec<String>) -> Result<Self> {
        let mut registry = Self::new();
        for cmd in ["/exit", "/quit"] {
            registry.register(cmd, |_, _, args| {
                require_no_args(args, "/exit")?;
                Ok(CommandReply::action(CommandAction::Exit))
            })?;
        }
        registry.register("/new", |agent, _, args| {
            require_no_args(args, "/new")?;
            Ok(CommandReply::text(format!(
                "New session: {}",
                agent.new_session()?
            )))
        })?;
        registry.register("/clear", |agent, _, args| {
            require_no_args(args, "/clear")?;
            agent.reset()?;
            Ok(CommandReply::text(format!(
                "Current session '{}' cleared.",
                agent.session_id()
            )))
        })?;
        registry.register("/sessions", |agent, _, args| {
            require_no_args(args, "/sessions")?;
            let mut lines = Vec::new();
            for entry in agent.list_sessions()? {
                let mark = if entry.id == agent.session_id() {
                    '*'
                } else {
                    ' '
                };
                lines.push(format!(
                    "{mark} {}{}",
                    entry.id,
                    entry.name.map(|n| format!(" · {n}")).unwrap_or_default()
                ));
            }
            Ok(CommandReply::text(lines.join("\n")))
        })?;
        registry.register("/resume", |agent, _, args| {
            if args.is_empty() {
                bail!("Usage: /resume <session-id-or-name>");
            }
            let id = agent.resolve_session_name(args)?;
            agent.resume_session(&id)?;
            Ok(CommandReply::text(format!(
                "Resumed session: {}",
                agent.session_id()
            )))
        })?;
        registry.register("/continue", |agent, _, args| {
            require_no_args(args, "/continue")?;
            let catalog = crate::sessions::SessionCatalog::new(agent.workdir())?;
            let recent = catalog
                .recent()?
                .ok_or_else(|| anyhow::anyhow!("no saved sessions"))?;
            if recent == agent.session_id() {
                return Ok(CommandReply::text("Already in the most recent session."));
            }
            agent.resume_session(&recent)?;
            Ok(CommandReply::text(format!(
                "Resumed recent session: {recent}"
            )))
        })?;
        registry.register("/rename", |agent, _, args| {
            agent.rename_session(args)?;
            Ok(CommandReply::text(format!("Session named: {args}")))
        })?;
        registry.register("/tools", move |agent, _, args| {
            require_no_args(args, "/tools")?;
            let mut tools = agent.available_tool_names().join("\n");
            if !warnings.is_empty() {
                tools.push_str("\n\nUnavailable MCP servers:\n");
                tools.push_str(&warnings.join("\n"));
            }
            Ok(CommandReply::text(tools))
        })?;
        registry.register("/usage", |agent, _, args| {
            require_no_args(args, "/usage")?;
            Ok(CommandReply::text(agent.usage().report()))
        })?;
        registry.register("/diff", |agent, _, args| {
            require_no_args(args, "/diff")?;
            Ok(CommandReply::text(crate::convenience::git_diff(
                agent.workdir(),
            )?))
        })?;
        registry.register("/export", |agent, _, args| {
            let name = if args.is_empty() {
                "kroma-conversation.md"
            } else {
                args
            };
            let path = crate::convenience::export_session(agent.messages(), agent.workdir(), name)?;
            Ok(CommandReply::text(format!("Exported {}", path.display())))
        })?;
        registry.register("/skills", move |_, _, args| {
            require_no_args(args, "/skills")?;
            if skills.is_empty() {
                return Ok(CommandReply::text("No skills discovered."));
            }
            Ok(CommandReply::text(
                skills
                    .iter()
                    .map(|skill| format!("{} — {}", skill.name, skill.description))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        })?;
        registry.register("/history", |agent, _, args| {
            let count = if args.is_empty() { 20 } else { parse_number(args, "/history [count]")? };
            let history = agent.history();
            let mut lines = vec![
                format!("Session '{}': {} completed checkpoint(s)", agent.session_id(), history.len()-1),
                "  #0  Start of conversation".to_owned(),
            ];
            let tail = history.len().saturating_sub(1).saturating_sub(count);
            for point in history.iter().skip(tail+1) {
                lines.push(format!("  #{}  {} -> {}", point.number, excerpt(&point.prompt), excerpt(&point.response)));
            }
            lines.push("Use /fork <number> or /rewind <number>. These change conversation state, not files.".into());
            Ok(CommandReply::text(lines.join("\n")))
        })?;
        registry.register_agent_async("/fork", |agent, _, args| {
            Box::pin(async move {
                let checkpoint = if args.is_empty() {
                    None
                } else {
                    Some(parse_number(args, "/fork [checkpoint]")?)
                };
                let id = agent.fork_async(checkpoint).await?;
                Ok(CommandReply::text(format!(
                    "Forked to session '{id}'. Use /resume to return to the source."
                )))
            })
        })?;
        registry.register_agent_async("/rewind", |agent, _, args| {
            Box::pin(async move {
                let checkpoint = parse_number(args, "/rewind <checkpoint>")?;
                let saved = agent.rewind_async(checkpoint).await?;
                Ok(CommandReply::text(format!(
                "Rewound to #{checkpoint}. Previous future saved as '{saved}' (/resume {saved})."
            )))
            })
        })?;
        registry.register_agent_async("/undo", |agent, _, args| {
            Box::pin(async move {
                require_no_args(args, "/undo")?;
                let (checkpoint, saved) = agent.undo_async().await?;
                Ok(CommandReply::text(format!(
                "Rewound to #{checkpoint}. Previous future saved as '{saved}' (/resume {saved})."
            )))
            })
        })?;
        registry.register("/pending", |_, input, args| {
            require_no_args(args, "/pending")?;
            Ok(CommandReply::text(pending_text(input)))
        })?;
        registry.register("/queue", |_, input, args| match args {
            "" => bail!("Usage: /queue <task> (or list|clear|run)"),
            "list" => Ok(CommandReply::text(pending_text(input))),
            "clear" => {
                let cleared = input.clear_followups();
                Ok(CommandReply {
                    output: format!("[queue] Cleared {cleared} pending task(s)."),
                    action: CommandAction::PauseFollowups,
                })
            }
            "run" => Ok(CommandReply::action(CommandAction::RunFollowups)),
            task => {
                let position = input.queue_followup(task)?;
                Ok(CommandReply {
                    output: format!("[queue] Added task #{position}."),
                    action: CommandAction::RunFollowups,
                })
            }
        })?;
        registry.register("/steer", |_, _, _| {
            bail!("/steer <instruction> is only available during an active task")
        })?;
        Ok(registry)
    }
}

fn require_no_args(args: &str, usage: &str) -> Result<()> {
    if !args.is_empty() {
        bail!("Usage: {usage}");
    }
    Ok(())
}

fn parse_number(args: &str, usage: &str) -> Result<usize> {
    args.parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Usage: {usage}"))
}

fn excerpt(text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flattened.chars();
    let short = chars.by_ref().take(64).collect::<String>();
    if chars.next().is_some() {
        format!("{short}...")
    } else {
        short
    }
}

pub fn pending_text(input: &TaskInput) -> String {
    let queued = input.queued();
    let mut lines = vec![format!(
        "[queue] {} task(s) waiting; {} steering instruction(s) waiting for a model boundary.",
        queued.len(),
        input.pending_steering()
    )];
    for (index, text) in queued.iter().enumerate() {
        lines.push(format!("  {}. {}", index + 1, text));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_must_be_unique_and_slash_prefixed() {
        let mut commands = CommandRegistry::new();
        assert!(commands
            .register("/example", |_, _, _| Ok(CommandReply::text("hello")))
            .is_ok());
        assert!(commands
            .register("/example", |_, _, _| Ok(CommandReply::text("dup")))
            .is_err());
        assert!(commands
            .register("example", |_, _, _| Ok(CommandReply::text("bad")))
            .is_err());
        assert!(commands
            .register("/bad name", |_, _, _| Ok(CommandReply::text("bad")))
            .is_err());
    }
}
