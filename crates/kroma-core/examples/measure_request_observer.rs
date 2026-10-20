//! Local observation overhead probe; no model, network or credentials.
//! Run: cargo run -p kroma-core --example measure_request_observer --offline
//! Times are observations on this host, not a performance gate or inference-cost estimate.
use kroma_core::messages::{TranscriptItem, UserMessage};
use kroma_core::provider::ProviderRequest;
use kroma_core::request_observation::RequestObserver;
use std::time::Instant;

fn main() {
    for message_count in [64, 384] {
        let request = ProviderRequest {
            system_prompt: "synthetic system".into(),
            messages: (0..message_count)
                .map(|index| {
                    TranscriptItem::User(UserMessage {
                        content: format!("{index}:{}", "x".repeat(4096)),
                    })
                })
                .collect(),
            ..Default::default()
        };
        let mut observer = RequestObserver::default();
        let first = observer.observe(&request);
        let mut samples = Vec::new();
        for _ in 0..30 {
            let start = Instant::now();
            std::hint::black_box(observer.observe(std::hint::black_box(&request)));
            samples.push(start.elapsed().as_micros());
        }
        samples.sort_unstable();
        println!(
            "{}",
            serde_json::json!({
                "probe": "logical-request-observer",
                "build": if cfg!(debug_assertions) { "debug" } else { "release" },
                "messages": message_count, "logical_payload_bytes": first.payload_bytes,
                "samples": samples.len(), "median_us": samples[samples.len()/2],
                "p95_us": samples[(samples.len()*95/100).min(samples.len()-1)],
                "max_us": samples[samples.len()-1],
                "note": "local synthetic observation cost, not model performance or cache evidence"
            })
        );
    }
}
