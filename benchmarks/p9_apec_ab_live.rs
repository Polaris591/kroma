//! Explicitly opt-in live A/B benchmark using frozen P7 histories.
//! This test calls Kroma's actual no-tools APEC model client, and does not
//! modify its production compression policy or original P7 files.
use super::*;
use crate::config::Config;
use crate::provider::ModelClient;
use serde_json::{json, Value};
use std::{fs, path::Path, time::Instant};

const RESULTS: &str = "/root/kroma-p9-apec-ab";

struct Case {
    name: &'static str,
    file: &'static str,
    start: usize,
    end: usize,
    anchors: &'static [&'static [&'static str]],
}

const CASES: &[Case] = &[
    Case {
        name: "rename",
        file: "rust-04/B/session.json",
        start: 48,
        end: 73,
        anchors: &[
            &["rename"],
            &["api key", "api_key"],
            &["thinking"],
            &["active"],
            &["no-op", "no op", "same name", "identical"],
            &["cargo test"],
        ],
    },
    Case {
        name: "rename_json",
        file: "rust-01/F/session.json",
        start: 38,
        end: 89,
        anchors: &[
            &["rename"],
            &["api key", "api_key"],
            &["thinking"],
            &["show --json"],
            &["list --json"],
            &["key_configured"],
            &["sorted", "sort"],
        ],
    },
];

const HOLDOUT: Case = Case {
    name: "holdout_rust05",
    file: "rust-05/B/session.json",
    start: 60,
    end: 100,
    anchors: &[
        &["rename"],
        &["api key", "api_key"],
        &["thinking"],
        &["active"],
        &["show --json"],
        &["list --json"],
        &["key_configured"],
        &["cargo test"],
    ],
};

fn load_case(case: &Case) -> Result<(Vec<Message>, String)> {
    let path = Path::new("/root/kroma-p7-abf").join(case.file);
    let snapshot: Value = serde_json::from_slice(&fs::read(path)?)?;
    let all: Vec<Message> = serde_json::from_value(snapshot["messages"].clone())?;
    if case.start >= case.end || case.end > all.len() {
        bail!("invalid source bounds");
    }
    let summaries = snapshot["compression"]["summaries"]
        .as_array()
        .context("missing original memory segments")?;
    let prior = summaries
        .iter()
        .filter(|s| {
            s["until"]
                .as_u64()
                .is_some_and(|n| n as usize <= case.start)
        })
        .filter_map(|s| s["text"].as_str())
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|s| preview_with_size(s, 500))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok((all[case.start..case.end].to_vec(), prior))
}

fn recall(summary: &str, case: &Case) -> Vec<bool> {
    let text = summary.to_lowercase();
    case.anchors
        .iter()
        .map(|alternatives| alternatives.iter().any(|item| text.contains(item)))
        .collect()
}

async fn run_variant(
    model: &ModelClient,
    case: &Case,
    limit: usize,
    round: usize,
) -> Result<Value> {
    let (history, prior) = load_case(case)?;
    let chunks = summary_chunks(&history, limit);
    let output = 1024;
    let started = Instant::now();
    let mut combined: Option<SummaryResult> = None;
    let mut condenses = 0;
    for (i, chunk) in chunks.iter().enumerate() {
        let segment = summarize_history(model, &prior, chunk, output).await?;
        match &mut combined {
            None => combined = Some(segment),
            Some(total) => {
                let joined = format!(
                    "{}\n[additional source fragment {}]\n{}",
                    total.text,
                    i + 1,
                    segment.text
                );
                merge_summary_cost(total, &segment);
                if joined.len() > output * 3 {
                    let condensed = condense_memory(model, &joined, output).await?;
                    merge_summary_cost(total, &condensed);
                    total.text = condensed.text;
                    condenses += 1;
                } else {
                    total.text = joined;
                }
            }
        }
    }
    let total = combined.context("no APEC result")?;
    let flags = recall(&total.text, case);
    let remembered = flags.iter().filter(|&&ok| ok).count();
    let input = total.usage.as_ref().and_then(|u| u.input);
    let cached = total.usage.as_ref().and_then(|u| u.cached);
    let uncached = input.zip(cached).and_then(|(i, c)| i.checked_sub(c));
    let letter = if limit == 100_000 { "B" } else { "A" };
    let summary_path = Path::new(RESULTS).join(format!("{}_{}_r{}.txt", case.name, letter, round));
    fs::write(&summary_path, &total.text)?;
    Ok(json!({
        "case":case.name, "arm":letter, "round":round, "limit_bytes":limit,
        "chunks":chunks.len(), "condenses":condenses,
        "requests":total.requests, "input":input, "cached":cached,
        "uncached":uncached, "output":total.usage.as_ref().and_then(|u| u.output),
        "model_ms":total.elapsed.as_millis(), "wall_ms":started.elapsed().as_millis(),
        "anchors":flags, "remembered":remembered, "possible":case.anchors.len(),
        "summary_bytes":total.text.len()
    }))
}

#[test]
#[ignore = "requires the frozen P7 histories on the experiment server"]
fn p9_apec_ab_offline_counts() -> Result<()> {
    // This optional historical experiment is not part of a general offline
    // validation run on machines that never had the P7 experiment archive.
    if !Path::new("/root/kroma-p7-abf/rust-04/B/session.json").is_file()
        || !Path::new("/root/kroma-p7-abf/rust-01/F/session.json").is_file()
    {
        eprintln!("P9 historical benchmark skipped: P7 source archive unavailable");
        return Ok(());
    }
    let input = ContextConfig::with_output_reserve(128_000, 4096)?.available_input;
    let a = (input * 7 / 10).clamp(800, 100_000);
    let b = 100_000;
    let mut before = 0;
    let mut after = 0;
    for case in CASES {
        let (history, _) = load_case(case)?;
        let x = summary_chunks(&history, a).len();
        let y = summary_chunks(&history, b).len();
        println!("P9 APEC OFFLINE {} A={} B={}", case.name, x, y);
        assert!(y <= x, "larger chunks should not increase chunk count");
        before += x;
        after += y;
    }
    assert!(
        after < before,
        "selected histories must distinguish variants"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "opt-in real-provider APEC model experiment"]
async fn p9_apec_ab_live() -> Result<()> {
    if std::env::var("KROMA_P9_APEC_LIVE").as_deref() != Ok("1") {
        eprintln!("P9 live A/B skipped: set KROMA_P9_APEC_LIVE=1 to opt in");
        return Ok(());
    }
    let dir = Path::new(RESULTS);
    fs::create_dir_all(dir)?;
    let config = Config::from_env()?;
    let model = ModelClient::new(&config.base_url, config.api_key, config.model)?;
    let available = ContextConfig::with_output_reserve(128_000, 4096)?.available_input;
    let baseline = (available * 7 / 10).clamp(800, 100_000);
    let round: usize = std::env::var("KROMA_P9_APEC_ROUND")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .context("invalid benchmark round")?;
    if !(1..=2).contains(&round) {
        bail!("benchmark round must be 1 or 2");
    }
    let report = dir.join("results.json");
    let mut rows: Vec<Value> = if round == 2 {
        serde_json::from_slice(&fs::read(&report).context("first round not found")?)?
    } else {
        Vec::new()
    };
    if round == 2 && rows.len() != CASES.len() * 2 {
        bail!("expected completed first round before reverse-order experiment");
    }
    for (i, case) in CASES.iter().enumerate() {
        let order = if (i + round - 1) % 2 == 0 {
            [baseline, 100_000]
        } else {
            [100_000, baseline]
        };
        for limit in order {
            let arm = if limit == baseline { "A" } else { "B" };
            println!(
                "P9 APEC LIVE START case={} arm={} round={round}",
                case.name, arm
            );
            let row = match run_variant(&model, case, limit, round).await {
                Ok(value) => value,
                Err(error) => json!({
                    "case":case.name, "arm":arm,
                    "error":format!("{error:#}")
                }),
            };
            println!(
                "P9 APEC LIVE DONE {} {} requests={} uncached={} recall={}/{} error={}",
                case.name,
                arm,
                row["requests"],
                row["uncached"],
                row["remembered"],
                row["possible"],
                row.get("error").is_some()
            );
            rows.push(row);
            // Save after each result so interruption cannot erase progress.
            fs::write(&report, serde_json::to_vec_pretty(&rows)?)?;
        }
    }
    assert!(rows.iter().all(|row| row.get("error").is_none()));
    Ok(())
}

#[tokio::test]
#[ignore = "holdout: real-provider APEC model experiment"]
async fn p9_apec_ab_holdout() -> Result<()> {
    if std::env::var("KROMA_P9_APEC_LIVE").as_deref() != Ok("1") {
        eprintln!("P9 live holdout skipped: set KROMA_P9_APEC_LIVE=1 to opt in");
        return Ok(());
    }
    let dir = Path::new(RESULTS);
    fs::create_dir_all(dir)?;
    let config = Config::from_env()?;
    let model = ModelClient::new(&config.base_url, config.api_key, config.model)?;
    let available = ContextConfig::with_output_reserve(128_000, 4096)?.available_input;
    let a = (available * 7 / 10).clamp(800, 100_000);
    let b = 100_000;
    let (history, _) = load_case(&HOLDOUT)?;
    let chunks_a = summary_chunks(&history, a).len();
    let chunks_b = summary_chunks(&history, b).len();
    println!("P9 HOLDOUT OFFLINE chunks A={chunks_a} B={chunks_b}");
    assert!(chunks_b < chunks_a);
    let mut rows = Vec::<Value>::new();
    for (round, order) in [(1, [a, b]), (2, [b, a])] {
        for limit in order {
            let arm = if limit == a { "A" } else { "B" };
            println!("P9 HOLDOUT START round={round} arm={arm}");
            let row = match run_variant(&model, &HOLDOUT, limit, round).await {
                Ok(row) => row,
                Err(error) => json!({
                    "case": HOLDOUT.name, "arm": arm, "round": round,
                    "error": format!("{error:#}")
                }),
            };
            println!(
                "P9 HOLDOUT DONE round={round} arm={arm} calls={} uncached={} facts={}/{} error={}",
                row["requests"],
                row["uncached"],
                row["remembered"],
                row["possible"],
                row.get("error").is_some()
            );
            rows.push(row);
            fs::write(
                dir.join("holdout_results.json"),
                serde_json::to_vec_pretty(&rows)?,
            )?;
        }
    }
    assert!(rows.iter().all(|row| row.get("error").is_none()));
    Ok(())
}

// P10 diagnostic only: reuse one set of source-fragment summaries to compare
// retaining them verbatim versus paying for one additional LLM consolidation.
// It deliberately does not change production APEC's consolidation rule.
#[tokio::test]
#[ignore = "opt-in real-provider consolidation experiment"]
async fn p10_apec_consolidation_probe() -> Result<()> {
    if std::env::var("KROMA_P10_APEC_LIVE").as_deref() != Ok("1") {
        eprintln!("P10 APEC consolidation probe skipped (opt-in required)");
        return Ok(());
    }
    let dir = Path::new(RESULTS);
    fs::create_dir_all(dir)?;
    let config = Config::from_env()?;
    let model = ModelClient::new(&config.base_url, config.api_key, config.model)?;
    let available = ContextConfig::with_output_reserve(128_000, 4096)?.available_input;
    let base = (available * 7 / 10).clamp(800, 100_000);
    let case = &CASES[0];
    let (history, prior) = load_case(case)?;
    let chunks = summary_chunks(&history, base);
    assert_eq!(chunks.len(), 2, "case must reproduce a multi-chunk summary");
    let mut pieces = Vec::new();
    let mut calls = 0;
    let mut input = Some(0u64);
    let mut cached = Some(0u64);
    for chunk in &chunks {
        let piece = summarize_history(&model, &prior, chunk, 1024).await?;
        calls += piece.requests;
        input = input
            .zip(piece.usage.as_ref().and_then(|u| u.input))
            .and_then(|(a, b)| a.checked_add(b));
        cached = cached
            .zip(piece.usage.as_ref().and_then(|u| u.cached))
            .and_then(|(a, b)| a.checked_add(b));
        pieces.push(piece.text);
    }
    let concatenated = format!(
        "{}\n[additional source fragment 2]\n{}",
        pieces[0], pieces[1]
    );
    if concatenated.len() <= 1024 * 3 {
        bail!("frozen case did not trigger the current consolidation threshold");
    }
    let condensed = condense_memory(&model, &concatenated, 1024).await?;
    let result = json!({
        "case": case.name,
        "chunks": chunks.len(),
        "without_consolidation": {
            "model_requests": calls,
            "input_tokens": input,
            "cached_tokens": cached,
            "memory_bytes": concatenated.len(),
            "fact_flags": recall(&concatenated, case)
        },
        "with_consolidation": {
            "model_requests": calls + condensed.requests,
            "extra_requests": condensed.requests,
            "extra_input_tokens": condensed.usage.as_ref().and_then(|u| u.input),
            "extra_cached_tokens": condensed.usage.as_ref().and_then(|u| u.cached),
            "extra_output_tokens": condensed.usage.as_ref().and_then(|u| u.output),
            "memory_bytes": condensed.text.len(),
            "fact_flags": recall(&condensed.text, case)
        },
        "saved_memory_bytes": concatenated.len().saturating_sub(condensed.text.len())
    });
    fs::write(
        dir.join("p10_consolidation.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    // Raw memory texts are stored only in the dedicated experiment area;
    // stdout shows measured usage, sizes and whether facts were retained.
    fs::write(dir.join("p10_unconsolidated.txt"), &concatenated)?;
    fs::write(dir.join("p10_consolidated.txt"), &condensed.text)?;
    println!("P10 APEC CONSOLIDATION {}", result);
    Ok(())
}
