//! Opt-in per-request cache diagnostics. Never write prompts, tool arguments,
//! message text, or API credentials. Disabled unless KROMA_CACHE_TRACE=1.
use crate::{context_state::PersistedContext, model::Usage, protocol::Message};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
struct MessageStamp {
    hash: u64,
    bytes: usize,
    role: String,
}

pub(crate) struct CacheSnapshot {
    messages: Vec<MessageStamp>,
    system_hash: u64,
    tools_hash: u64,
    reasoning_level: Option<String>,
    revision: Option<u64>,
    covered_until: Option<usize>,
    summaries: Option<usize>,
    cleared_tools: Option<usize>,
}

pub(crate) struct CacheTrace {
    previous: Option<CacheSnapshot>,
    session: Option<PathBuf>,
    request: u64,
}

fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

impl CacheSnapshot {
    /// This is the provider's budget projection, not a claim about the
    /// gateway's exact serialized wire bytes or its internal cache keys.
    pub(crate) fn capture(
        messages: &[Message],
        tools: &[Value],
        context: &PersistedContext,
        reasoning_level: Option<String>,
    ) -> Option<Self> {
        let messages = messages
            .iter()
            .map(|message| {
                let bytes = serde_json::to_vec(message).ok()?;
                Some(MessageStamp {
                    hash: fingerprint(&bytes),
                    bytes: bytes.len(),
                    role: message.role.clone(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let tools_hash = fingerprint(&serde_json::to_vec(tools).ok()?);
        let (revision, covered_until, summaries, cleared_tools) = match context {
            PersistedContext::Builtin(state) => (
                Some(state.revision),
                Some(state.covered_until),
                Some(state.summaries.len()),
                Some(state.cleared_tools.len()),
            ),
            PersistedContext::External(_) => (None, None, None, None),
        };
        Some(Self {
            system_hash: messages.first().map_or(0, |m| m.hash),
            messages,
            tools_hash,
            reasoning_level,
            revision,
            covered_until,
            summaries,
            cleared_tools,
        })
    }

    fn change_from(&self, previous: Option<&Self>) -> (Value, Value, Value, &'static str) {
        let Some(previous) = previous else {
            return (Value::Null, Value::Null, Value::Null, "baseline");
        };
        let prefix = self
            .messages
            .iter()
            .zip(&previous.messages)
            .take_while(|(a, b)| a.hash == b.hash)
            .count();
        let prefix_bytes: usize = self.messages[..prefix]
            .iter()
            .map(|item| item.bytes + 1)
            .sum();
        let old_len = previous.messages.len();
        let new_len = self.messages.len();
        let role = if prefix < new_len {
            json!(self.messages[prefix].role)
        } else {
            Value::Null
        };
        let change = if self.system_hash != previous.system_hash {
            "system_changed"
        } else if self.tools_hash != previous.tools_hash {
            "tools_changed"
        } else if self.reasoning_level != previous.reasoning_level {
            "reasoning_setting_changed"
        } else if self.covered_until != previous.covered_until
            || self.summaries != previous.summaries
        {
            "apec_projection_changed"
        } else if self.cleared_tools != previous.cleared_tools {
            "trc_projection_changed"
        } else if prefix < old_len.min(new_len) {
            "history_rewritten"
        } else if new_len < old_len {
            "history_shortened"
        } else if new_len > old_len {
            "append_only"
        } else {
            "unchanged"
        };
        (json!(prefix), json!(prefix_bytes), role, change)
    }
}

impl CacheTrace {
    pub(crate) fn from_env() -> Option<Self> {
        (std::env::var("KROMA_CACHE_TRACE").ok().as_deref() == Some("1")).then_some(Self {
            previous: None,
            session: None,
            request: 0,
        })
    }

    fn sidecar(session: &Path) -> PathBuf {
        let mut name = OsString::from(session.as_os_str());
        name.push(".cache-trace.jsonl");
        PathBuf::from(name)
    }

    pub(crate) fn record(
        &mut self,
        session: &Path,
        snapshot: CacheSnapshot,
        usage: Option<&Usage>,
        success: bool,
    ) {
        if self.session.as_deref() != Some(session) {
            self.session = Some(session.to_owned());
            self.previous = None;
            self.request = 0;
        }
        self.request += 1;
        let (same_prefix_messages, same_prefix_json_bytes, first_changed_role, change) =
            snapshot.change_from(self.previous.as_ref());
        let time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let record = json!({
            "time_ms": time_ms,
            "pid": std::process::id(),
            "request": self.request,
            "status": if success {"ok"} else {"error"},
            "projection": "provider_budget",
            "message_count": snapshot.messages.len(),
            "system_hash": format!("{:016x}", snapshot.system_hash),
            "tools_hash": format!("{:016x}", snapshot.tools_hash),
            "reasoning_level": snapshot.reasoning_level.as_deref(),
            "context_revision": snapshot.revision,
            "covered_until": snapshot.covered_until,
            "summaries": snapshot.summaries,
            "cleared_tools": snapshot.cleared_tools,
            "change": change,
            "common_prefix_messages": same_prefix_messages,
            "common_prefix_json_bytes": same_prefix_json_bytes,
            "first_changed_role": first_changed_role,
            "input_tokens": usage.and_then(|u| u.input),
            "cached_tokens": usage.and_then(|u| u.cached),
        });
        if let Err(error) = append_record(&Self::sidecar(session), &record) {
            crate::runtime_log!("[cache-trace] failed to write request diagnostic: {error}");
        }
        self.previous = Some(snapshot);
    }
}

fn append_record(path: &Path, record: &Value) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_state::CompactionState;

    fn capture(messages: &[Message], context: &CompactionState) -> CacheSnapshot {
        CacheSnapshot::capture(
            messages,
            &[],
            &PersistedContext::Builtin(context.clone()),
            None,
        )
        .unwrap()
    }

    #[test]
    fn append_and_rewrite_have_distinct_fingerprints() {
        let mut messages = vec![
            Message::new("system", "PRIVATE SYSTEM INSTRUCTIONS"),
            Message::new("user", "PRIVATE TASK CONTENT"),
            Message::tool("id".into(), "PRIVATE TOOL OUTPUT".into()),
        ];
        let state = CompactionState::default();
        let original = capture(&messages, &state);
        messages.push(Message::new("assistant", "done"));
        let appended = capture(&messages, &state);
        let (prefix, _, _, change) = appended.change_from(Some(&original));
        assert_eq!(prefix, json!(3));
        assert_eq!(change, "append_only");

        messages[2].content = Some("preview".into());
        let mut trc = state.clone();
        trc.cleared_tools.insert(2);
        trc.revision = 1;
        let pruned = capture(&messages, &trc);
        let (prefix, _, role, change) = pruned.change_from(Some(&appended));
        assert_eq!(prefix, json!(2));
        assert_eq!(role, json!("tool"));
        assert_eq!(change, "trc_projection_changed");

        messages[1].content = Some("summary".into());
        let mut apec = trc.clone();
        apec.covered_until = 2;
        apec.summaries.push(crate::context_state::SummarySegment {
            until: 2,
            text: "summary".into(),
        });
        let summarized = capture(&messages, &apec);
        let (_, _, _, change) = summarized.change_from(Some(&pruned));
        assert_eq!(change, "apec_projection_changed");
    }

    #[test]
    fn same_messages_distinguish_prompt_tools_and_effort_changes() {
        let messages = vec![
            Message::new("system", "stable"),
            Message::new("user", "ask"),
        ];
        let context = PersistedContext::Builtin(CompactionState::default());
        let before =
            CacheSnapshot::capture(&messages, &[], &context, Some("medium".into())).unwrap();
        let higher =
            CacheSnapshot::capture(&messages, &[], &context, Some("xhigh".into())).unwrap();
        assert_eq!(
            higher.change_from(Some(&before)).3,
            "reasoning_setting_changed"
        );
        let new_tools = CacheSnapshot::capture(
            &messages,
            &[json!({"type":"function","name":"new"})],
            &context,
            Some("xhigh".into()),
        )
        .unwrap();
        assert_eq!(new_tools.change_from(Some(&higher)).3, "tools_changed");
        let new_prompt = CacheSnapshot::capture(
            &[Message::new("system", "different"), messages[1].clone()],
            &[json!({"type":"function","name":"new"})],
            &context,
            Some("xhigh".into()),
        )
        .unwrap();
        assert_eq!(new_prompt.change_from(Some(&new_tools)).3, "system_changed");
    }

    #[test]
    fn trace_contains_only_counts_hashes_and_roles() {
        let messages = vec![
            Message::new("system", "DO NOT LOG THIS PROMPT"),
            Message::new("user", "DO NOT LOG THIS USER TEXT"),
        ];
        let root = std::env::temp_dir().join(format!(
            "kroma-trace-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let session = root.join("sample.json");
        let mut trace = CacheTrace {
            previous: None,
            session: None,
            request: 0,
        };
        trace.record(
            &session,
            capture(&messages, &CompactionState::default()),
            None,
            true,
        );
        let jsonl = std::fs::read_to_string(CacheTrace::sidecar(&session)).unwrap();
        assert!(!jsonl.contains("DO NOT LOG"));
        let record: Value = serde_json::from_str(jsonl.trim()).unwrap();
        assert_eq!(record["change"], "baseline");
        assert_eq!(record["message_count"], 2);
        assert!(record["cached_tokens"].is_null());
        std::fs::remove_dir_all(root).unwrap();
    }
}
