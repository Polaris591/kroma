//! Opt-in local microbenchmark, not a substitute for randomized harness trials.
//! Run: cargo test --release --test plugins_performance -- --ignored --nocapture
use anyhow::Result;
use kroma::{plugins::PluginCatalog, prompt::PromptComposer, tools::ToolRegistry};
use std::{
    fs,
    hint::black_box,
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

fn bench(registry: &ToolRegistry, iterations: usize) -> u128 {
    let begin = Instant::now();
    for _ in 0..iterations {
        black_box(registry.definitions());
    }
    begin.elapsed().as_nanos()
}

fn median(values: &mut [u128]) -> u128 {
    values.sort_unstable();
    values[values.len() / 2]
}

#[test]
#[ignore = "run separately in release mode; timing depends on host load"]
fn disabled_plugins_do_not_change_tool_dispatch_hot_path() -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root =
        std::env::temp_dir().join(format!("kroma-disabled-bench-{}-{now}", std::process::id()));
    let plugins = root.join("plugins");
    let data = plugins.join("disabled");
    fs::create_dir_all(&data)?;
    fs::write(
        data.join("plugin.toml"),
        "id='disabled'\nname='Disabled'\nprompt='Invisible'\n",
    )?;
    let baseline = ToolRegistry::builtin();
    let catalog = PluginCatalog::discover_at(plugins, root.join("plugins.toml"), Path::new("."))?;
    let mut disabled = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    assert!(catalog.contribute(&mut disabled, &mut prompt).is_empty());
    assert_eq!(baseline.definitions(), disabled.definitions());
    assert!(disabled.is_builtin_only());

    // Alternating order reduces drift. Median avoids relying on one timer
    // sample, but still measures only schema generation, not end-to-end cost.
    const ITERATIONS: usize = 20_000;
    let mut baseline_times = Vec::new();
    let mut disabled_times = Vec::new();
    for i in 0..10 {
        if i % 2 == 0 {
            baseline_times.push(bench(&baseline, ITERATIONS));
            disabled_times.push(bench(&disabled, ITERATIONS));
        } else {
            disabled_times.push(bench(&disabled, ITERATIONS));
            baseline_times.push(bench(&baseline, ITERATIONS));
        }
    }
    let base = median(&mut baseline_times);
    let plugin_off = median(&mut disabled_times);
    println!("disabled plugin fast-path microbenchmark (median of 10 x {ITERATIONS}):");
    println!("  baseline: {:.3} ms", base as f64 / 1e6);
    println!("  disabled: {:.3} ms", plugin_off as f64 / 1e6);
    println!("  ratio: {:.3}x", plugin_off as f64 / (base.max(1) as f64));
    let _ = fs::remove_dir_all(root);
    Ok(())
}
