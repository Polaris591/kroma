//! Offline coverage for the built-in model catalog and its diagnostics.
//! No credentials or public network access are used.
use anyhow::Result;
use kroma::{
    agent::Agent,
    app,
    context::{BuiltinContextFactory, ContextFactory},
    model_catalog::{FieldSource, MetadataState},
    profiles::ProviderProfile,
    prompt::PromptComposer,
    sessions::SessionCatalog,
    tools::ToolRegistry,
};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::PathBuf,
    sync::Arc,
    time::SystemTime,
};

/// A deterministic local metadata endpoint. It never contacts a real provider
/// and exercises the same "metadata unavailable -> field fallback" path.
fn metadata_404_server(requests: usize) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header == "\n" || header.is_empty() {
                    break;
                }
            }
            drop(reader);
            let body = r#"{"error":"metadata unavailable"}"#;
            write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            stream.flush().unwrap();
        }
    });
    (format!("http://{address}/v1"), server)
}

fn profile(
    model: &str,
    catalog_model: Option<&str>,
    context_window: Option<usize>,
    max_output_tokens: Option<usize>,
) -> ProviderProfile {
    ProviderProfile {
        base_url: "http://127.0.0.1:9/v1".into(),
        model: model.into(),
        api_key: None,
        api_key_env: None,
        deepseek_thinking: None,
        api_protocol: Some("chat".into()),
        reasoning_effort: Some("auto".into()),
        context_window,
        max_output_tokens,
        thinking_levels: None,
        catalog_model: catalog_model.map(str::to_owned),
    }
}

#[tokio::test]
async fn explicit_alias_fills_partial_proxy_without_inheriting_by_name() -> Result<()> {
    let (base_url, server) = metadata_404_server(2);
    // A same-named official model on a custom proxy must not inherit limits.
    let mut plain = profile("claude-sonnet-5-5", None, None, None);
    plain.base_url = base_url.clone();
    let (_, budget, report) = app::model_from_profile(&plain).await?;
    assert_eq!(budget.window(), 128_000);
    assert_eq!(report.context_window.source, FieldSource::Default);
    assert_eq!(report.image_input.value, None);

    // The explicit alias is the only way to opt into the known capabilities.
    let mut aliased = profile(
        "renamed-proxy-model",
        Some("anthropic/claude-sonnet-5-5"),
        None,
        None,
    );
    aliased.base_url = base_url;
    let (model, budget, report) = app::model_from_profile(&aliased).await?;
    server.join().unwrap();
    assert_eq!(budget.window(), 1_000_000);
    assert_eq!(budget.output_reserve(), 4096);
    assert_eq!(report.context_window.value, Some(1_000_000));
    assert_eq!(report.context_window.source, FieldSource::Catalog);
    assert_eq!(report.hard_output_limit.value, Some(128_000));
    assert_eq!(report.hard_output_limit.source, FieldSource::Catalog);
    assert_eq!(report.requested_output_tokens.value, None);
    assert_eq!(report.effective_turn_output_tokens.value, Some(4096));
    assert_eq!(report.image_input.value, Some(true));
    assert_eq!(report.image_input.source, FieldSource::Catalog);
    assert_eq!(report.metadata, MetadataState::Unavailable);
    assert!(report
        .catalog
        .as_ref()
        .is_some_and(|catalog| catalog.key == "anthropic/claude-sonnet-5-5"));
    assert_eq!(model.accepts_images(), Some(true));

    // An explicit alias that no longer exists is a startup error, not a
    // silent downgrade to guessed capabilities.
    let broken = profile("renamed", Some("anthropic/not-in-catalog"), None, None);
    assert!(app::model_from_profile(&broken).await.is_err());
    Ok(())
}

#[tokio::test]
async fn model_switch_updates_budget_and_report_together() -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-model-catalog-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    fs::create_dir_all(&dir)?;
    let (base_url, server) = metadata_404_server(1);
    let mut first = profile("proxy-a", Some("anthropic/claude-sonnet-5-5"), None, None);
    first.base_url = base_url;
    let second = profile(
        "proxy-b",
        Some("deepseek/deepseek-flash"),
        Some(200_000),
        Some(64_000),
    );
    let (first_model, first_budget, first_report) = app::model_from_profile(&first).await?;
    server.join().unwrap();
    let (second_model, second_budget, second_report) = app::model_from_profile(&second).await?;
    let mut agent = Agent::with_runtime_budget(
        first_model,
        ToolRegistry::builtin(),
        Arc::new(BuiltinContextFactory) as Arc<dyn ContextFactory>,
        PromptComposer::default(),
        None,
        dir.clone(),
        first_budget,
    )?;
    agent.set_model_report(first_report);
    assert_eq!(agent.context_window_tokens(), 1_000_000);
    assert_eq!(agent.available_context_tokens(), 1_000_000 - 4096);

    agent.change_model(second_model, second_budget)?;
    assert!(
        agent.model_report().is_none(),
        "stale model sources must not survive a switch"
    );
    assert_eq!(agent.context_window_tokens(), 200_000);
    assert_eq!(agent.available_context_tokens(), 200_000 - 32_768);
    agent.set_model_report(second_report);
    let report = agent.model_report().unwrap();
    assert_eq!(report.context_window.value, Some(200_000));
    assert_eq!(report.context_window.source, FieldSource::Profile);
    assert_eq!(report.output_reserve.value, Some(32_768));
    assert_eq!(report.effective_turn_output_tokens.value, Some(32_768));
    assert_eq!(
        report.catalog.as_ref().map(|catalog| catalog.key.as_str()),
        Some("deepseek/deepseek-flash")
    );

    drop(agent);
    cleanup(&dir)?;
    Ok(())
}

fn cleanup(workdir: &PathBuf) -> Result<()> {
    let catalog = SessionCatalog::new(workdir)?;
    let snapshot = catalog.default_path();
    for path in [
        snapshot.to_path_buf(),
        PathBuf::from(format!("{}.wal", snapshot.display())),
        PathBuf::from(format!("{}.lock", snapshot.display())),
    ] {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    fs::remove_dir_all(workdir)?;
    Ok(())
}

#[tokio::test]
async fn flagship_input_cap_and_catalog_effort_reach_the_runtime() -> Result<()> {
    let (base_url, server) = metadata_404_server(1);
    let mut selected = profile("proxy-astra", Some("openai/gpt-6-astra"), None, None);
    selected.base_url = base_url;
    selected.api_protocol = Some("responses".into());
    let config = kroma::config::Config::from_profile(&selected, PathBuf::new(), None)?;
    assert_eq!(
        config.thinking_levels_source, None,
        "absent profile tiers are not an explicit override"
    );
    let (offline_budget, offline_report) = config.resolved_limits_without_probe()?;
    let (model, budget, report) = app::model_from_profile(&selected).await?;
    server.join().unwrap();
    assert_eq!(report.context_window.value, Some(1_050_000));
    assert_eq!(report.hard_output_limit.value, Some(128_000));
    assert_eq!(budget.window(), 1_050_000);
    assert_eq!(budget.available_input(), 922_000);
    assert_eq!(budget.output_reserve(), 4096);
    assert_eq!(offline_budget.available_input(), budget.available_input());
    assert_eq!(
        offline_report.context_window.value,
        report.context_window.value
    );
    assert!(report.status_text().contains("input ceiling 922000"));
    assert_eq!(report.thinking_levels.source, FieldSource::Catalog);
    assert_eq!(
        model.thinking_levels(),
        ["low", "medium", "high", "xhigh", "max"]
    );
    assert!(
        model.set_reasoning_level("none").is_err(),
        "Astra rejects disabled reasoning"
    );
    model.set_reasoning_level("max")?;
    assert_eq!(model.reasoning_level().as_deref(), Some("max"));
    model.set_reasoning_level("auto")?;
    assert_eq!(model.reasoning_level().as_deref(), Some("auto"));
    assert_eq!(model.accepts_images(), Some(true));
    Ok(())
}

#[tokio::test]
async fn explicit_profile_effort_still_overrides_the_catalog() -> Result<()> {
    // Explicit context + chat needs no network metadata; the unused localhost
    // endpoint ensures this is not a real provider integration test.
    let mut selected = profile(
        "proxy-qwen",
        Some("alibaba/qwen3.8-max"),
        Some(1_000_000),
        None,
    );
    let (model, budget, report) = app::model_from_profile(&selected).await?;
    assert_eq!(
        budget.available_input(),
        983_616,
        "thinking-mode input ceiling, not the full shared window"
    );
    assert_eq!(model.thinking_levels(), ["low", "medium", "xhigh"]);
    assert_eq!(report.thinking_levels.source, FieldSource::Catalog);
    selected.thinking_levels = Some(vec!["medium".into()]);
    let (restricted, _, report) = app::model_from_profile(&selected).await?;
    assert_eq!(restricted.thinking_levels(), ["medium"]);
    assert_eq!(report.thinking_levels.source, FieldSource::Profile);
    assert!(restricted.set_reasoning_level("xhigh").is_err());
    restricted.set_reasoning_level("medium")?;
    assert_eq!(restricted.reasoning_level().as_deref(), Some("medium"));
    Ok(())
}

#[tokio::test]
async fn expanded_domestic_and_legacy_catalogs_reach_runtime_without_a_probe() -> Result<()> {
    for (alias, window, input_cap, output_cap) in [
        ("stepfun/step-5-preview", 1_000_000, Some(1_000_000), 64_000),
        ("xiaomi/mimo-v2.6-pro", 1_000_000, None, 128_000),
        ("baidu/ernie-5.1", 128_000, Some(119_000), 65_536),
        ("anthropic/claude-haiku-4-5", 200_000, None, 64_000),
    ] {
        // Explicit limits skip optional network discovery; the explicit alias
        // still supplies capability ceilings for a differently named proxy ID.
        let selected = profile("renamed-by-proxy", Some(alias), Some(600_000), Some(4096));
        let config = kroma::config::Config::from_profile(&selected, PathBuf::new(), None)?;
        let (_, budget, report) = app::model_from_profile(&selected).await?;
        let (offline, offline_report) = config.resolved_limits_without_probe()?;
        assert_eq!(budget.window(), 600_000.min(window), "{alias}");
        assert_eq!(
            budget.available_input(),
            offline.available_input(),
            "{alias}"
        );
        if let Some(cap) = input_cap {
            assert!(budget.available_input() <= cap, "{alias}");
        }
        assert_eq!(report.hard_output_limit.value, Some(output_cap), "{alias}");
        assert_eq!(report.hard_output_limit.source, FieldSource::Catalog);
        assert_eq!(report.image_input.value, offline_report.image_input.value);
        assert_eq!(report.catalog.as_ref().unwrap().key, alias);
    }
    Ok(())
}
