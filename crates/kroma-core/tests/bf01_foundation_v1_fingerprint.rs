//! Foundation v1 cross-language contract: Node freezes a BenchTask JSON suite and
//! Rust must derive the same scoring-oracle fingerprint before model execution.
//! This must run with cargo; do not treat the Node copy of the hash as Rust proof.

use kroma_core::bench::{load_suite, suite_fingerprint};
use kroma_core::experiment::ExperimentManifest;
use std::path::Path;

#[test]
fn bf01_sample_suite_fingerprint_matches_offline_node_freeze() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../experiments/foundation-v1");
    let suite = load_suite(&root.join("suite.json")).expect("suite schema and oracles");
    assert_eq!(suite.len(), 6);
    // Generated from JSON.stringify of BenchTask's serde field order; this is the
    // required cross-language equivalence test (not yet run in this container).
    assert_eq!(suite_fingerprint(&suite), "8b51d4aae03f6c7c");
    let manifest_text = std::fs::read_to_string(root.join("manifest.json")).unwrap();
    let resolved = ExperimentManifest::from_json(&manifest_text).expect("frozen manifest");
    assert_eq!(resolved.manifest.id, "FOUNDATION-01-SYNTHETIC-SMOKE");
    assert_eq!(resolved.jobs.len(), 24);
}
