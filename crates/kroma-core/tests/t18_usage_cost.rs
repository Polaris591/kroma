//! T18 (KR-15): request-level cost, priced per attempt, with incompleteness made explicit.
//!
//! The defects pinned here:
//! * cache *writes* were priced as if they were uncached input (the rate table had no write rate at
//!   all), so every warm-up turn was under-charged;
//! * a whole run was priced with one model name, so a retry that landed on a different checkpoint, or
//!   a child running a different model, was priced at the last model's rates;
//! * an estimate or a missing usage could quietly become a number, which reads as measured spend.

use kroma_core::messages::TokenUsage;
use kroma_core::usage_cost::{CostLedger, PriceTable, TokenRates, UsageBasis};
use std::collections::BTreeMap;

fn table_with_creation(creation: Option<u64>) -> PriceTable {
    let mut models = BTreeMap::new();
    models.insert(
        "model-a".to_string(),
        TokenRates {
            uncached_input_nano_usd: 100,
            cached_input_nano_usd: 10,
            cache_creation_input_nano_usd: creation,
            output_nano_usd: 400,
        },
    );
    models.insert(
        "model-b".to_string(),
        TokenRates {
            uncached_input_nano_usd: 200,
            cached_input_nano_usd: 20,
            cache_creation_input_nano_usd: Some(250),
            output_nano_usd: 800,
        },
    );
    PriceTable {
        version: "prices-v1".to_string(),
        source: "operator table".to_string(),
        models,
    }
}

fn usage(input: u64, cached: u64, creation: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        cached_input_tokens: Some(cached),
        cache_creation_input_tokens: Some(creation),
        output_tokens: output,
        reasoning_tokens: None,
    }
}

/// Writes and reads are different line items with different rates.
#[test]
fn cache_creation_and_read_have_distinct_rates() {
    let table = table_with_creation(Some(125));
    // 1000 input = 100 uncached + 600 read + 300 written; 100 output.
    // 100×100 + 600×10 + 300×125 + 100×400 = 10000 + 6000 + 37500 + 40000 = 93500
    let estimate = table.estimate(Some("model-a"), Some(usage(1000, 600, 300, 100)));
    assert_eq!(
        estimate.estimated_nano_usd,
        Some(93_500),
        "reason: {:?}",
        estimate.unknown_reason
    );

    // Charging the write at the read rate would be 10000 + 6000 + 3000 + 40000 = 59000 — a 37%
    // understatement, which is exactly the old behaviour.
    assert_ne!(estimate.estimated_nano_usd, Some(59_000));

    // A table without a write rate cannot price a turn that wrote to the cache: it is unknown, not
    // free and not "same as read".
    let no_write_rate = table_with_creation(None);
    let unknown = no_write_rate.estimate(Some("model-a"), Some(usage(1000, 600, 300, 100)));
    assert_eq!(unknown.estimated_nano_usd, None);
    assert!(
        unknown
            .unknown_reason
            .as_deref()
            .unwrap_or("")
            .contains("cache-creation"),
        "unexpected reason: {:?}",
        unknown.unknown_reason
    );

    // With no cache write at all the same table is fine.
    let clean = no_write_rate.estimate(Some("model-a"), Some(usage(1000, 600, 0, 100)));
    // 400 uncached × 100 + 600 cached × 10 + 100 output × 400 = 86_000
    assert_eq!(clean.estimated_nano_usd, Some(86_000));
}

/// Each attempt is priced at the model *it* reported, not at the run's last model.
#[test]
fn mixed_returned_models_are_priced_per_attempt() {
    let table = table_with_creation(Some(125));
    let mut ledger = CostLedger::new();

    // Attempt 0: the requested alias resolved to model-a.
    ledger.record_attempt(
        Some("alias"),
        Some("model-a"),
        Some(usage(1000, 0, 0, 100)),
        UsageBasis::Measured,
        None,
        &table,
    );
    // Attempt 1 is a retry that landed on model-b (a different checkpoint, twice the rates).
    ledger.record_attempt(
        Some("alias"),
        Some("model-b"),
        Some(usage(1000, 0, 0, 100)),
        UsageBasis::Measured,
        Some(0),
        &table,
    );

    let summary = ledger.summary();
    assert_eq!(summary.attempts, 2);
    assert_eq!(summary.measured_attempts, 2);
    assert!(
        !summary.incomplete,
        "both attempts were measured and priced"
    );
    // model-a: 1000×100 + 100×400 = 140000; model-b: 1000×200 + 100×800 = 280000.
    assert_eq!(summary.by_model_nano_usd["model-a"], 140_000);
    assert_eq!(summary.by_model_nano_usd["model-b"], 280_000);
    assert_eq!(summary.measured_nano_usd, 420_000);
    assert_eq!(summary.total_nano_usd(), Some(420_000));

    // Pricing the whole 2000 input tokens at the *last* model would have been 2000×200 + 200×800 =
    // 560000, or at the first 280000 — either way not the real 420000.
    assert_ne!(summary.measured_nano_usd, 560_000);
    assert_ne!(summary.measured_nano_usd, 280_000);
    assert_eq!(ledger.attempts()[1].retry_of, Some(0));
}

/// An estimate is not a measurement.
#[test]
fn byte_estimate_cannot_claim_provider_measured_cost() {
    let table = table_with_creation(Some(125));
    let mut ledger = CostLedger::new();
    // No provider usage at all: the host estimated the tokens.
    ledger.record_attempt(
        Some("model-a"),
        Some("model-a"),
        Some(usage(500, 0, 0, 50)),
        UsageBasis::Estimated,
        None,
        &table,
    );
    let summary = ledger.summary();
    assert_eq!(summary.measured_attempts, 0);
    assert_eq!(
        summary.measured_nano_usd, 0,
        "an estimate must not land in measured spend"
    );
    assert_eq!(summary.estimated_attempts, 1);
    assert!(summary.estimated_nano_usd > 0);
    assert!(
        summary.incomplete,
        "the total is not a complete measured cost"
    );
    assert_eq!(
        summary.total_nano_usd(),
        None,
        "a report must not be handed a number that looks measured"
    );
    assert!(!summary.unknown_reasons.is_empty());
}

/// A retry whose usage never arrived makes the total incomplete instead of silently smaller.
#[test]
fn unknown_retry_usage_makes_total_cost_incomplete() {
    let table = table_with_creation(Some(125));
    let mut ledger = CostLedger::new();
    ledger.record_attempt(
        Some("model-a"),
        Some("model-a"),
        Some(usage(1000, 600, 0, 100)),
        UsageBasis::Measured,
        None,
        &table,
    );
    // The retry failed after dispatch; no usage is known.
    ledger.record_attempt(
        Some("model-a"),
        None,
        None,
        UsageBasis::Unknown,
        Some(0),
        &table,
    );

    let summary = ledger.summary();
    assert_eq!(summary.attempts, 2);
    assert_eq!(summary.measured_attempts, 1);
    assert_eq!(summary.unknown_attempts, 1);
    assert!(
        summary.incomplete,
        "a missing retry usage makes the total incomplete"
    );
    assert_eq!(summary.total_nano_usd(), None);
    // The known part is still reported, so an operator can see the floor.
    assert_eq!(summary.measured_nano_usd, 86_000);
    assert!(
        summary
            .unknown_reasons
            .iter()
            .any(|r| r.contains("attempt 1")),
        "the reason must name the attempt: {:?}",
        summary.unknown_reasons
    );

    // An unknown *price* (model not in the table) is also incomplete, not zero.
    let mut unpriced = CostLedger::new();
    unpriced.record_attempt(
        Some("model-z"),
        Some("model-z"),
        Some(usage(10, 0, 0, 1)),
        UsageBasis::Measured,
        None,
        &table,
    );
    let summary = unpriced.summary();
    assert_eq!(summary.measured_nano_usd, 0);
    assert!(summary.incomplete);
    assert!(summary
        .unknown_reasons
        .iter()
        .any(|r| r.contains("no exact rate")));
}

/// The basis labels are distinct and survive into the record.
#[test]
fn usage_basis_labels_are_distinct() {
    assert_ne!(UsageBasis::Measured, UsageBasis::Estimated);
    assert_ne!(UsageBasis::Estimated, UsageBasis::Unknown);
    assert_eq!(UsageBasis::Measured.as_str(), "measured");
    assert_eq!(UsageBasis::Estimated.as_str(), "estimated");
    assert_eq!(UsageBasis::Unknown.as_str(), "unknown");

    let table = table_with_creation(Some(125));
    let mut ledger = CostLedger::new();
    ledger.record_attempt(
        Some("m"),
        Some("model-a"),
        Some(usage(10, 0, 0, 1)),
        UsageBasis::Measured,
        None,
        &table,
    );
    assert_eq!(ledger.attempts()[0].basis, UsageBasis::Measured);
    assert_eq!(ledger.attempts()[0].requested_model.as_deref(), Some("m"));
    assert_eq!(
        ledger.attempts()[0].reported_model.as_deref(),
        Some("model-a")
    );
}
