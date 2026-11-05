//! Test-only local timing reports. No production instrumentation or thresholds.
use serde_json::{json, Value};
use std::time::Duration;

pub(crate) fn report(name: &str, samples: &[Duration], fixture: Value) {
    assert!(!samples.is_empty());
    let mut ns: Vec<_> = samples.iter().map(Duration::as_nanos).collect();
    ns.sort_unstable();
    let percentile = |p: usize| ns[(ns.len() * p / 100).min(ns.len() - 1)];
    println!(
        "PERF {}",
        json!({"name":name,"samples":ns.len(),"process_id":std::process::id(),"debug_assertions":cfg!(debug_assertions),"platform":std::env::consts::OS,"arch":std::env::consts::ARCH,"p50_ns":percentile(50),
        "p95_ns":percentile(95),"min_ns":ns[0],"max_ns":ns[ns.len()-1],"fixture":fixture})
    );
}
