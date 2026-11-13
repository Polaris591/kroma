//! Focused lifecycle regressions; no provider or real session is contacted.
use super::*;
use crate::model::{Completion, FinalProjection, ModelFuture, StreamFuture, TextResult};
use std::sync::{atomic::AtomicUsize, Mutex};

#[derive(Default)]
struct ReferenceModel {
    projection: Mutex<Option<Arc<FinalProjection>>>,
    generation: AtomicUsize,
}
impl ModelBackend for ReferenceModel {
    fn invalidate_ephemeral_context(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        *self.projection.lock().unwrap() = None;
    }
    fn final_projection(&self) -> Option<Arc<FinalProjection>> {
        self.projection.lock().unwrap().clone()
    }
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [serde_json::Value],
        _: &'a mut (dyn FnMut(&str) + Send),
    ) -> StreamFuture<'a, Completion> {
        Box::pin(async move {
            let mut reference = Message::new(
                "user",
                format!(
                    "reference generation {}",
                    self.generation.load(Ordering::Relaxed)
                ),
            );
            reference.provider_items =
                Some(vec![serde_json::json!({"type":"kroma_context_reference"})]);
            let mut sent = messages.to_vec();
            sent.push(reference.clone());
            *self.projection.lock().unwrap() = Some(Arc::new(FinalProjection {
                messages: sent,
                tools: tools.to_vec(),
                rewritten: false,
                reference_bytes: reference.content.as_ref().unwrap().len(),
                appended_references: vec![reference],
            }));
            Ok(Completion {
                message: Message::new("assistant", "done"),
                usage: None,
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
        Box::pin(async { anyhow::bail!("unexpected summary") })
    }
}

fn open(model: Arc<dyn ModelBackend>, dir: &std::path::Path) -> Agent {
    Agent::with_catalog_backends(
        model,
        ToolRegistry::new(),
        Arc::new(BuiltinContextFactory),
        PromptComposer::default(),
        None,
        dir.to_owned(),
        SessionCatalog::from_default(dir.join("session.json")).unwrap(),
        ContextConfig::with_output_reserve(32_768, 4_096).unwrap(),
        false,
        None,
    )
    .unwrap()
}

#[tokio::test]
async fn foundation_reference_inputs_survive_reopen_fork_compaction_and_clear() {
    let dir = std::env::temp_dir().join(format!(
        "kroma-foundation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let model = Arc::new(ReferenceModel::default());
    let mut agent = open(model.clone(), &dir);
    agent.run("continue", &mut |_| {}).await.unwrap();
    let first = serde_json::to_value(agent.messages()).unwrap();
    assert_eq!(
        agent
            .messages()
            .iter()
            .filter(|m| crate::protocol::is_host_reference(m))
            .count(),
        1
    );
    drop(agent);
    let mut agent = open(model.clone(), &dir);
    assert_eq!(serde_json::to_value(agent.messages()).unwrap(), first);
    agent.run("continue", &mut |_| {}).await.unwrap();
    let raw = agent.messages().to_vec();
    assert_eq!(
        raw.iter()
            .filter(|m| crate::protocol::is_host_reference(m))
            .count(),
        2
    );
    let state = crate::context::CompactionState {
        covered_until: 4,
        consumed_until: raw.len(),
        summaries: vec![crate::context::SummarySegment {
            until: 4,
            text: "first task complete".into(),
        }],
        ..Default::default()
    };
    let policy = crate::context::Compression::new(
        state,
        ContextConfig::with_output_reserve(32_768, 4096).unwrap(),
        agent.archive_path(),
    );
    let retained = policy.view(&raw);
    assert!(
        retained.iter().any(crate::protocol::is_host_reference),
        "surviving task retains its durable reference"
    );
    agent.fork(None).unwrap();
    assert_eq!(
        serde_json::to_value(agent.messages()).unwrap(),
        serde_json::to_value(&raw).unwrap()
    );
    agent.reset().unwrap();
    assert_eq!(
        model.generation.load(Ordering::Relaxed),
        1,
        "clear invalidates model-side caches"
    );
    agent.run("continue", &mut |_| {}).await.unwrap();
    let refs: Vec<_> = agent
        .messages()
        .iter()
        .filter(|m| crate::protocol::is_host_reference(m))
        .collect();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].content.as_deref(), Some("reference generation 1"));
    drop(agent);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn foundation_full_tool_evidence_survives_preview_fork_and_source_prune() {
    let dir = std::env::temp_dir().join(format!(
        "kroma-foundation-output-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("source.json");
    let branch = dir.join("branch.json");
    let original = format!(
        "{} UNIQUE_MIDDLE_EVIDENCE {}",
        "a".repeat(40_000),
        "b".repeat(40_000)
    );
    let mut result = Message::tool_with_status("tool1".into(), original.clone(), false);
    crate::tool_archive::archive_message(&mut result, &source, "fixture");
    assert_eq!(
        result.content.as_deref(),
        Some(original.as_str()),
        "WAL/search preserve the full post-hook result"
    );
    let mut messages = vec![Message::new("system", "system"), result];
    crate::tool_archive::clone_references(&source, &branch, &mut messages).unwrap();
    let preview: serde_json::Value =
        serde_json::from_str(crate::tool_archive::model_content(&messages[1])).unwrap();
    let file = std::path::Path::new(preview["archive_file"].as_str().unwrap());
    assert!(file
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("branch.tool-output-"));
    std::fs::write(
        &source,
        br#"{"version":3,"workdir":"workspace","messages":[{"role":"system"}]}"#,
    )
    .unwrap();
    crate::sessions_prune::prune(&dir, Duration::ZERO, std::time::SystemTime::now()).unwrap();
    assert_eq!(std::fs::read_to_string(file).unwrap(), original);
    assert_eq!(messages[1].content.as_deref(), Some(original.as_str()));
    std::fs::remove_dir_all(dir).unwrap();
}
