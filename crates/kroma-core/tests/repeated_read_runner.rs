use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
use kroma_core::budget::BudgetConfig;
use kroma_core::env::LocalHostEnv;
use kroma_core::events::{EventLogger, EventPayload};
use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage, ToolCall, TranscriptItem};
use kroma_core::provider::{
    MessageSemantics, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};
use kroma_core::session::SessionJournal;
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::types::{TerminalReason, ToolCallId};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
fn results(request: &ProviderRequest) -> Vec<&str> {
    request
        .messages
        .iter()
        .filter_map(|m| match m {
            TranscriptItem::ToolResult(r) if r.tool_name == "read" => Some(r.content.as_ref()),
            _ => None,
        })
        .collect()
}
struct Probe {
    requests: Mutex<Vec<ProviderRequest>>,
    file: std::path::PathBuf,
    changed: String,
}
impl ModelProvider for Probe {
    fn message_semantics(&self) -> MessageSemantics {
        MessageSemantics::all()
    }
    fn complete<'a>(
        &'a self,
        request: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            let bodies = results(&request);
            match turn {
                1 => assert!(bodies[0].contains("original line")),
                2 => {
                    assert!(bodies[0].contains("original line"));
                    assert!(bodies[1].starts_with("[Repeated read:"));
                    assert!(bodies[1].contains("\"read-0\""));
                    std::fs::write(&self.file, &self.changed).unwrap();
                }
                3 => assert!(bodies[2].contains("changed line")),
                4 => assert!(bodies[3].contains("changed line")),
                _ => {}
            }
            if turn > 0 {
                let previous = &requests[turn - 1];
                assert_eq!(
                    &request.messages[..previous.messages.len()],
                    previous.messages.as_slice(),
                    "unchanged history stays byte-stable"
                );
            }
            requests.push(request);
            Ok(ProviderCompletion {
                turn: AssistantTurn {
                    content: (turn == 4).then(|| "done".into()),
                    reasoning_blocks: vec![],
                    tool_calls: if turn < 4 {
                        vec![ToolCall {
                            call_id: ToolCallId::new(format!("read-{turn}")),
                            name: "read".into(),
                            raw_arguments: if turn == 3 {
                                r#"{"path":"a.txt","force":true}"#.into()
                            } else if turn == 2 {
                                // A third identical signature is deliberately blocked by LoopGuard;
                                // explicit defaults preserve the read range without bypassing it.
                                r#"{"path":"a.txt","offset":1,"limit":2000}"#.into()
                            } else {
                                r#"{"path":"a.txt"}"#.into()
                            },
                        }]
                    } else {
                        vec![]
                    },
                    finish_reason: if turn < 4 {
                        FinishReason::ToolCalls
                    } else {
                        FinishReason::Stop
                    },
                    usage: Some(TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Default::default()
                    }),
                },
                request_body_bytes: 123,
                latency_ms: 1,
                ttft_ms: Some(1),
                attempts_used: 1,
                reported_model: None,
            })
        })
    }
}
#[tokio::test]
async fn real_runner_elides_only_outgoing_duplicates_and_preserves_raw_wal() {
    let root = std::env::temp_dir().join(format!("kroma-read-runner-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let initial = "original line\n".repeat(120);
    let changed = "changed line\n".repeat(120);
    std::fs::write(ws.join("a.txt"), &initial).unwrap();
    let provider = Arc::new(Probe {
        requests: Mutex::new(vec![]),
        file: ws.join("a.txt"),
        changed: changed.clone(),
    });
    let journal = SessionJournal::create(root.join("sessions"), &ws).unwrap();
    let wal = journal.path().to_path_buf();
    let budget = BudgetConfig::default();
    let env = Arc::new(LocalHostEnv::new(&ws, root.join("state")).unwrap());
    let runtime = build_default_runtime(
        env,
        PolicyEngine::new(
            PolicyMode::ReadOnly,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        budget.clone(),
        None,
    );
    let config = AgentRunConfig::new("read the file", budget);
    let logger = EventLogger::new(config.run_id, Some(root.join("events")), true).unwrap();
    let (_tx, rx) = watch::channel(false);
    let (summary, logger) = AgentRunner::new(config, provider.clone(), runtime, rx)
        .with_session_journal(journal)
        .run(logger)
        .await;
    assert_eq!(summary.terminal_reason, TerminalReason::ModelStop);
    assert_eq!(provider.requests.lock().unwrap().len(), 5);
    assert!(logger.events().iter().any(|e|matches!(e.event,EventPayload::RepeatedReadElided{saved_utf8_bytes,..} if saved_utf8_bytes>1000)));
    let original: Vec<_> = summary
        .transcript
        .iter()
        .filter_map(|m| match m {
            TranscriptItem::ToolResult(r) if r.tool_name == "read" => Some(r.content.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(original.len(), 4);
    assert!(original[1].contains("original line"));
    assert!(original[2].contains("changed line"));
    assert!(!original[1].starts_with("[Repeated read:"));
    let journal = SessionJournal::open(&wal).unwrap();
    let mut restored = journal.active_transcript();
    assert!(restored
        .iter()
        .any(|m| matches!(m,TranscriptItem::ToolResult(r) if r.content.contains("original line"))));
    let stats = kroma_core::read_dedup::elide_repeated_reads(&mut restored);
    assert_eq!(
        stats.elided_results, 1,
        "resume recomputes against its own visible full anchor"
    );
    std::fs::remove_dir_all(root).unwrap();
}
