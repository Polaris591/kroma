//! Narrow `task_control` capability for the single active controller plugin.
//!
//! The host keeps execution, control state, budgets and cancellation. This
//! adapter only forwards bounded, explicitly shaped requests to the owner's
//! existing [`PluginWorker`] and returns structured replies; it starts no
//! process, thread or model loop and holds no authoritative state. Nothing here
//! is fail-open: every protocol, size, error or timeout outcome is returned as
//! an error so the host can pause the active goal instead of continuing.
use super::{PluginResponse, PluginWorker};
use crate::commands::CommandSession;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

/// Bound for one serialized request and for the bounded decision a finish
/// callback may return.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ControlLimits {
    /// Serialized bytes of the request the adapter builds. The worker adds its
    /// own small envelope (`protocol_version`/`request_id`/`config`/
    /// `data_dir`/`workdir`) on top of this.
    pub request_bytes: usize,
    /// Bytes of the JSON decision text returned by `before_task_finish`.
    pub decision_bytes: usize,
    /// Characters allowed in decision `feedback`.
    pub feedback_chars: usize,
    /// Cancellable call deadline. A timeout makes the host pause the goal.
    pub timeout: Duration,
}

impl ControlLimits {
    pub(crate) const DEFAULT: Self = Self {
        request_bytes: 1024 * 1024,
        decision_bytes: 4 * 1024,
        feedback_chars: 2048,
        timeout: Duration::from_secs(2),
    };
}

/// One bounded machine-readable reason code, never a second feedback channel.
const REASON_CODE_CHARS: usize = 128;

/// Root-level `control` of a command/tool reply. The host validates the
/// operation, identity and revision; the adapter only rejects a non-object.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ControlReply {
    pub content: String,
    pub is_error: bool,
    /// The host must ignore this when `is_error` is true.
    pub control: Option<Value>,
}

/// Advice only: the host may still refuse a `Finish` because of user input,
/// budget or contradictory facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Finish,
    Continue,
    Pause,
}

impl Decision {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Finish => "finish",
            Self::Continue => "continue",
            Self::Pause => "pause",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "finish" => Ok(Self::Finish),
            "continue" => Ok(Self::Continue),
            "pause" => Ok(Self::Pause),
            other => bail!("before_task_finish returned an unknown decision '{other}'"),
        }
    }
}

/// Decoded `before_task_finish` reply. `continue` and `pause` always carry a
/// reason so the host has something displayable to report.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FinishDecision {
    pub decision: Decision,
    pub reason_code: Option<String>,
    pub feedback: String,
}

/// Owner identity plus its one process worker. Only this plugin may produce
/// control effects, and only through its unique slash command or declared
/// tools; the host binds id/scope from this identity, never from a reply.
pub(crate) struct PluginTaskControl {
    owner: String,
    version: String,
    command: String,
    tool_names: Vec<String>,
    worker: Arc<PluginWorker>,
    limits: ControlLimits,
}

impl PluginTaskControl {
    pub(super) fn new(
        owner: impl Into<String>,
        version: impl Into<String>,
        command: impl Into<String>,
        tool_names: Vec<String>,
        worker: Arc<PluginWorker>,
    ) -> Self {
        Self {
            owner: owner.into(),
            version: version.into(),
            command: command.into(),
            tool_names,
            worker,
            limits: ControlLimits::DEFAULT,
        }
    }

    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }

    /// Real slash command name as registered in the manifest, including `/`.
    pub(crate) fn command(&self) -> &str {
        &self.command
    }

    pub(crate) fn tools(&self) -> &[String] {
        &self.tool_names
    }

    /// Command request: the raw user arguments, the real user input bound to
    /// the submitted message, and the read-only control snapshot.
    pub(crate) async fn command_request(
        &self,
        arguments: &str,
        user_input: &str,
        control_context: &Value,
        session: &CommandSession,
    ) -> Result<ControlReply> {
        self.invoke(json!({
            "type": "command",
            "name": self.command,
            "arguments": arguments,
            "user_input": user_input,
            "control_context": control_context,
            "workdir": session.workdir,
            "archive": session.archive,
            "session_id": session.session_id,
        }))
        .await
    }

    /// Normal-finish callback. `facts` carries only what the current plan
    /// needs; the whole transcript, logs and credentials are never sent.
    pub(crate) async fn before_finish(
        &self,
        control_context: &Value,
        facts: &Value,
        candidate_has_content: bool,
        session: &CommandSession,
    ) -> Result<FinishDecision> {
        let reply = self
            .invoke(json!({
                "type": "task_control",
                "name": "before_task_finish",
                "control_context": control_context,
                "arguments": {
                    "facts": facts,
                    "candidate": {"has_content": candidate_has_content}
                },
                "workdir": session.workdir,
                "archive": session.archive,
                "session_id": session.session_id,
            }))
            .await?;
        if reply.is_error {
            bail!("before_task_finish failed: {}", reply.content);
        }
        if reply.control.is_some() {
            bail!("before_task_finish must return a decision, not a control effect");
        }
        parse_decision(&reply.content, self.limits)
    }

    async fn invoke(&self, request: Value) -> Result<ControlReply> {
        check_request_size(&request, self.limits)?;
        let reply: PluginResponse = self
            .worker
            .invoke_async_timeout(request, self.limits.timeout)
            .await
            .context("task_control plugin call failed")?;
        let control = match reply.control {
            Some(value) if !value.is_object() => {
                bail!("task_control reply control must be a JSON object")
            }
            other => other,
        };
        Ok(ControlReply {
            content: reply.content,
            is_error: reply.is_error,
            control,
        })
    }
}

fn check_request_size(request: &Value, limits: ControlLimits) -> Result<()> {
    let bytes = serde_json::to_vec(request)
        .context("serializing task_control request")?
        .len();
    if bytes > limits.request_bytes {
        bail!(
            "task_control request is {bytes} bytes; the limit is {}",
            limits.request_bytes
        );
    }
    Ok(())
}

/// Parse the bounded JSON decision. Malformed, oversized, unknown or
/// reason-less decisions are protocol errors, never an implicit `finish`.
fn parse_decision(content: &str, limits: ControlLimits) -> Result<FinishDecision> {
    if content.len() > limits.decision_bytes {
        bail!(
            "before_task_finish decision is {} bytes; the limit is {}",
            content.len(),
            limits.decision_bytes
        );
    }
    let value: Value = serde_json::from_str(content)
        .context("before_task_finish must return a JSON decision object")?;
    let object = value
        .as_object()
        .context("before_task_finish decision must be a JSON object")?;
    for key in object.keys() {
        match key.as_str() {
            "decision" | "reason_code" | "feedback" => {}
            other => bail!("before_task_finish decision has unknown field '{other}'"),
        }
    }
    let decision = object
        .get("decision")
        .and_then(Value::as_str)
        .context("before_task_finish decision needs a string 'decision'")?;
    let decision = Decision::parse(decision)?;
    let reason_code = match object.get("reason_code") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.trim().to_owned()).filter(|text| !text.is_empty()),
        Some(_) => bail!("before_task_finish reason_code must be a string"),
    };
    if let Some(reason_code) = &reason_code {
        if reason_code.chars().count() > REASON_CODE_CHARS {
            bail!("before_task_finish reason_code exceeds {REASON_CODE_CHARS} characters");
        }
    }
    let feedback = match object.get("feedback") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.trim().to_owned(),
        Some(_) => bail!("before_task_finish feedback must be a string"),
    };
    let feedback_chars = feedback.chars().count();
    if feedback_chars > limits.feedback_chars {
        bail!(
            "before_task_finish feedback is {feedback_chars} characters; the limit is {}",
            limits.feedback_chars
        );
    }
    if decision != Decision::Finish {
        if reason_code.is_none() {
            bail!(
                "before_task_finish '{}' needs a non-empty reason_code",
                decision.as_str()
            );
        }
        if feedback.is_empty() {
            bail!(
                "before_task_finish '{}' needs displayable feedback",
                decision.as_str()
            );
        }
    }
    Ok(FinishDecision {
        decision,
        reason_code,
        feedback,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(content: &str) -> Result<FinishDecision> {
        parse_decision(content, ControlLimits::DEFAULT)
    }

    #[test]
    fn finish_needs_no_reason_and_is_the_only_silent_decision() -> Result<()> {
        let finished = decision("{\"decision\":\"finish\"}")?;
        assert_eq!(finished.decision, Decision::Finish);
        assert_eq!(finished.reason_code, None);
        assert!(finished.feedback.is_empty());
        assert!(decision("{\"decision\":\"finish\",\"reason_code\":\"DONE\"}").is_ok());
        Ok(())
    }

    #[test]
    fn continue_and_pause_require_reason_and_feedback() -> Result<()> {
        assert!(decision("{\"decision\":\"continue\"}").is_err());
        assert!(decision("{\"decision\":\"pause\",\"reason_code\":\"NEED_AUTH\"}").is_err());
        assert!(decision("{\"decision\":\"continue\",\"feedback\":\"run T1\"}").is_err());
        let continued = decision(
            "{\"decision\":\"continue\",\"reason_code\":\"MISSING_CHECK\",\"feedback\":\"run T1\"}",
        )?;
        assert_eq!(continued.decision, Decision::Continue);
        assert_eq!(continued.reason_code.as_deref(), Some("MISSING_CHECK"));
        let paused = decision(
            "{\"decision\":\"pause\",\"reason_code\":\" NEED_TOKEN \",\"feedback\":\" provide a token \"}",
        )?;
        assert_eq!(paused.decision, Decision::Pause);
        assert_eq!(paused.reason_code.as_deref(), Some("NEED_TOKEN"));
        assert_eq!(paused.feedback, "provide a token");
        Ok(())
    }

    #[test]
    fn unknown_and_malformed_decisions_are_errors() {
        assert!(decision("{\"decision\":\"completed\"}").is_err());
        assert!(decision("{\"decision\":\"Finish\"}").is_err());
        assert!(decision("{\"decision\":7}").is_err());
        assert!(decision("{}").is_err());
        assert!(decision("[1]").is_err());
        assert!(decision("not json").is_err());
        assert!(decision("{\"decision\":\"finish\",\"extra\":1}").is_err());
        assert!(decision("{\"decision\":\"finish\",\"feedback\":3}").is_err());
        assert!(decision("{\"decision\":\"finish\",\"reason_code\":3}").is_err());
    }

    #[test]
    fn oversized_decision_and_feedback_are_rejected() {
        let oversized = json!({"decision": "finish", "feedback": "x".repeat(5000)});
        assert!(decision(&oversized.to_string()).is_err());
        let base = json!({"decision": "continue", "reason_code": "MISSING_CHECK"});
        let mut longest = base.clone();
        longest["feedback"] = json!("a".repeat(2048));
        assert!(decision(&longest.to_string()).is_ok());
        let mut too_long = base;
        too_long["feedback"] = json!("a".repeat(2049));
        assert!(decision(&too_long.to_string()).is_err());
    }

    #[test]
    fn request_size_limit_bounds_what_the_host_sends() {
        let small = json!({"type":"task_control","name":"before_task_finish"});
        assert!(check_request_size(&small, ControlLimits::DEFAULT).is_ok());
        let huge =
            json!({"type":"command","user_input":"x".repeat(ControlLimits::DEFAULT.request_bytes)});
        assert!(check_request_size(&huge, ControlLimits::DEFAULT).is_err());
        let tight = ControlLimits {
            request_bytes: 32,
            ..ControlLimits::DEFAULT
        };
        assert!(check_request_size(&small, tight).is_err());
    }

    #[test]
    fn default_limits_match_the_protocol_contract() {
        let limits = ControlLimits::DEFAULT;
        assert_eq!(limits.request_bytes, 1024 * 1024);
        assert_eq!(limits.decision_bytes, 4 * 1024);
        assert_eq!(limits.feedback_chars, 2048);
        assert_eq!(limits.timeout, Duration::from_secs(2));
    }
}
