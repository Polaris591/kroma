//! One tool-free, bounded model request after the first completed turn.
//! It does not delay the conversation or change the authoritative transcript.
use super::event_queue::EventSender;
use super::runtime_events::UiEvent;
use crate::{model::ModelBackend, sessions::SessionCatalog};
use std::{path::PathBuf, sync::Arc, time::Duration};

fn sanitize_title(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = line
        .trim_start_matches('#')
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '“' | '”' | '「' | '」' | '*' | '`'))
        .trim();
    let title = line
        .chars()
        .filter(|c| !c.is_control())
        .take(48)
        .collect::<String>();
    let title = title.trim().trim_end_matches(['.', ':', '：', '。']).trim();
    (!title.is_empty()).then(|| title.to_owned())
}

fn fallback_title(user_text: &str) -> String {
    sanitize_title(user_text).unwrap_or_else(|| "New conversation".into())
}

pub(super) fn schedule(
    model: Arc<dyn ModelBackend>,
    catalog_default: PathBuf,
    id: String,
    user_text: String,
    assistant_text: String,
    sender: EventSender,
) {
    let Some(permit) = sender.title_permit(&id) else {
        crate::runtime_log!("[session] automatic title skipped: queue unavailable or busy");
        return;
    };
    // Bound task-owned strings before the model request or a reliable send.
    let id = id.into_boxed_str().into_string();
    let user_text = user_text.chars().take(160).collect::<String>();
    let assistant_text = assistant_text.chars().take(120).collect::<String>();
    tokio::spawn(async move {
        let _permit = permit;
        let prompt = format!("User: {user_text}\nAssistant: {assistant_text}");
        let system = "Name this coding assistant conversation in the user's language. Return only a short descriptive title, no explanation or Markdown.";
        let (title, usage, requests) = match tokio::time::timeout(
            Duration::from_secs(12),
            model.generate_short_text(system, &prompt, 128),
        )
        .await
        {
            Ok(Ok(result)) => (sanitize_title(&result.text), result.usage, result.requests),
            // Even an unsuccessful request might be charged by a gateway.
            _ => (None, None, 1),
        };
        // Count title generation as a separate API expense: it must never
        // become the "last coding request" or change Ctx/cache in the footer.
        let _ = sender
            .send_title(UiEvent::TitleUsage {
                session_id: id.clone(),
                requests,
                input: usage.as_ref().and_then(|u| u.input),
                output: usage.as_ref().and_then(|u| u.output),
                cached: usage.as_ref().and_then(|u| u.cached),
                reasoning: usage.as_ref().and_then(|u| u.reasoning),
            })
            .await;
        let title = title.unwrap_or_else(|| fallback_title(&user_text));
        match SessionCatalog::from_default(catalog_default)
            .and_then(|catalog| catalog.name_session_if_unnamed(&id, &title))
        {
            Ok(true) => {
                let _ = sender.send_title(UiEvent::SessionTitleUpdated(id)).await;
            }
            Ok(false) => {}
            Err(error) => crate::runtime_log!("[session] automatic title unavailable: {error:#}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Completion, ModelFuture, StreamFuture, TextResult, Usage};
    use crate::protocol::Message;
    use anyhow::Result;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TitleModel(Arc<AtomicUsize>, Duration);
    impl ModelBackend for TitleModel {
        fn complete<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a [Value],
            _on_text: &'a mut (dyn FnMut(&str) + Send),
        ) -> StreamFuture<'a, Completion> {
            Box::pin(async { anyhow::bail!("title must not invoke the agent loop") })
        }

        fn generate_text<'a>(
            &'a self,
            _system: &'a str,
            _text: &'a str,
            _output_tokens: usize,
        ) -> ModelFuture<'a, TextResult> {
            Box::pin(async { anyhow::bail!("title must use short text generation") })
        }

        fn generate_short_text<'a>(
            &'a self,
            _system: &'a str,
            _text: &'a str,
            output_tokens: usize,
        ) -> ModelFuture<'a, TextResult> {
            self.0.fetch_add(1, Ordering::Relaxed);
            let delay = self.1;
            Box::pin(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                assert_eq!(output_tokens, 128);
                Ok(TextResult {
                    text: "**优化 Kroma 会话**".into(),
                    usage: Some(Usage {
                        input: Some(80),
                        output: Some(12),
                        cached: Some(0),
                        ..Usage::default()
                    }),
                    elapsed: Duration::from_millis(1),
                    requests: 1,
                })
            })
        }
    }

    #[tokio::test]
    #[ignore = "local async/catalog benchmark; mock title requests only"]
    async fn perf_title_schedule_and_completion() -> Result<()> {
        use std::time::{Instant, SystemTime, UNIX_EPOCH};
        let root = std::env::temp_dir().join(format!(
            "kroma-perf-title-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        std::fs::create_dir_all(&root)?;
        let catalog = SessionCatalog::from_default(root.join("default.json"))?;
        let count = Arc::new(AtomicUsize::new(0));
        let model = Arc::new(TitleModel(count.clone(), Duration::from_millis(30)));
        let (sender, mut receiver) = super::super::event_queue::channel();
        let mut scheduled = Vec::new();
        let mut completed = Vec::new();
        for _ in 0..11 {
            let (id, path) = catalog.new_path()?;
            std::fs::write(path, "{}")?;
            let start = Instant::now();
            schedule(
                model.clone(),
                catalog.default_path().to_path_buf(),
                id.clone(),
                "user input".into(),
                "assistant answer".into(),
                sender.clone(),
            );
            scheduled.push(start.elapsed());
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(2), receiver.recv()).await?,
                Some(UiEvent::TitleUsage { requests: 1, .. })
            ));
            assert!(
                matches!(tokio::time::timeout(Duration::from_secs(2), receiver.recv()).await?, Some(UiEvent::SessionTitleUpdated(ref named)) if named == &id)
            );
            completed.push(start.elapsed());
        }
        assert_eq!(count.load(Ordering::Relaxed), 11);
        let fixture = serde_json::json!({"mock_delay_ms":30,"title_requests":11,"output_budget":128,"billable_usage":null});
        crate::performance::report("title.schedule", &scheduled, fixture.clone());
        crate::performance::report("title.completion", &completed, fixture);
        drop(receiver);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn first_turn_title_is_model_generated_and_never_overwrites_manual_name() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-title-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir_all(&root)?;
        let catalog = SessionCatalog::from_default(root.join("default.json"))?;
        let (id, path) = catalog.new_path()?;
        std::fs::write(path, "{}")?;
        let count = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = super::super::event_queue::channel();
        schedule(
            Arc::new(TitleModel(count.clone(), Duration::ZERO)),
            catalog.default_path().to_path_buf(),
            id.clone(),
            "检查会话系统".into(),
            "已实现欢迎页".into(),
            tx.clone(),
        );
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await?;
        assert!(matches!(
            first,
            Some(UiEvent::TitleUsage {
                requests: 1,
                input: Some(80),
                output: Some(12),
                ..
            })
        ));
        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await?;
        assert!(matches!(second, Some(UiEvent::SessionTitleUpdated(ref named)) if named == &id));
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert_eq!(catalog.list()?[0].name.as_deref(), Some("优化 Kroma 会话"));
        catalog.name_session(&id, "手动名称")?;
        schedule(
            Arc::new(TitleModel(count.clone(), Duration::ZERO)),
            catalog.default_path().to_path_buf(),
            id.clone(),
            "later".into(),
            "later".into(),
            tx,
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await?;
        assert_eq!(catalog.list()?[0].name.as_deref(), Some("手动名称"));
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn title_sanitization_handles_markdown_and_multiline_output() {
        assert_eq!(
            sanitize_title("  **Review Kroma**\nExtra notes"),
            Some("Review Kroma".into())
        );
        assert_eq!(
            sanitize_title("  “修复会话系统”\n"),
            Some("修复会话系统".into())
        );
        assert_eq!(sanitize_title("\n"), None);
        assert_eq!(
            fallback_title("检查 Rust 工具输出\n详情"),
            "检查 Rust 工具输出"
        );
    }
}
