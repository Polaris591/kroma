//! Operator-run small provider experiment. Reads credentials from environment, never stores them.
//! One process = one arm/sample; external cgroup bounds the entire run. No automatic repeats.
use kroma_core::bench::{
    BenchExecutor, BenchStrategyConfig, BenchTask, ProviderBenchExecutor, ProviderBenchSettings,
};
use kroma_core::provider::{build_provider, ProviderProtocol, ProviderSettings};
use kroma_core::tool::policy::PolicyMode;
use std::path::PathBuf;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 7 {
        return Err("usage: paired_smoke SUITE INDEX single|adaptive REP WORKSPACE RESULT".into());
    }
    let suite: Vec<BenchTask> = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let task = suite.get(args[2].parse::<usize>()?).ok_or("task index")?;
    let strategy = match args[3].as_str() {
        "single" => BenchStrategyConfig::single_agent(),
        "adaptive" => BenchStrategyConfig::adaptive_tournament(2),
        _ => return Err("arm".into()),
    };
    let settings = ProviderSettings {
        base_url: std::env::var("KROMA_BASE_URL")?,
        api_key: std::env::var("KROMA_API_KEY")?,
        model: std::env::var("KROMA_MODEL")?,
        max_retries: 1,
        reasoning_effort: None,
        protocol: ProviderProtocol::ChatCompletions,
        max_tokens: 0,
    };
    let (protocol, provider) = build_provider(&settings)?;
    let config = ProviderBenchSettings::new(settings.model, protocol.as_str())
        .with_policy_mode(PolicyMode::FullAutonomous)
        .with_max_subagents(2)
        .with_max_model_turns(16)
        .with_max_run_duration(Duration::from_secs(65))
        .with_verify_timeout(Duration::from_secs(5));
    let executor = ProviderBenchExecutor::new(provider, config);
    let mut result = executor
        .execute(task, &strategy, 300_000, &PathBuf::from(&args[5]))
        .await;
    result.repetition = args[4].parse()?;
    std::fs::write(&args[6], serde_json::to_vec_pretty(&result)?)?;
    println!(
        "task={} arm={} resolved={} tokens={} children={} terminal={:?} activity={:?}",
        result.task_id,
        result.strategy,
        result.resolved,
        result.total_tokens,
        result.children_tokens,
        result.terminal_reason,
        result.mechanism
    );
    Ok(())
}
