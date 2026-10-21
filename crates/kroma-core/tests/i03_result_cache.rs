//! KR-I03: the read-result cache must never change what the model sees. A randomised sequence of
//! file rewrites is replayed with the cache on and off; every pair of reads has to agree, and each
//! rewrite has to produce fresh content (so invalidation is proved, not assumed).

use kroma_core::workspace::WorkspaceCap;

fn read_with(ws: &WorkspaceCap, cache: bool, args: &str) -> (String, bool) {
    // The env var is process-global, so the parity run sets it per call and restores it after.
    let prev = std::env::var("KROMA_RESULT_CACHE").ok();
    if cache {
        std::env::set_var("KROMA_RESULT_CACHE", "1");
    } else {
        std::env::set_var("KROMA_RESULT_CACHE", "0");
    }
    let out = kroma_core::tools::read::execute_read(ws, args);
    match prev {
        Some(v) => std::env::set_var("KROMA_RESULT_CACHE", v),
        None => std::env::remove_var("KROMA_RESULT_CACHE"),
    }
    (out.content, matches!(out.outcome, kroma_core::messages::ToolOutcome::Success))
}

#[test]
fn cached_and_uncached_reads_agree_across_randomised_rewrites() {
    let root = std::env::temp_dir().join(format!("kroma-i03-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let ws = WorkspaceCap::new(&root).unwrap();
    let mut seed: u64 = 0x5eed_1234;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };

    for round in 0..10 {
        let lines: Vec<String> = (0..8).map(|i| format!("round{} line{} payload-{}", round, i, next() % 1000)).collect();
        std::fs::write(root.join("f.txt"), lines.join("\n")).unwrap();
        let offset = 1 + next() % 3;
        let limit = 1 + next() % 5;
        let args = format!(r#"{{"path":"f.txt","offset":{offset},"limit":{limit}}}"#);

        let (cached, ok_cached) = read_with(&ws, true, &args);
        let (uncached, ok_uncached) = read_with(&ws, false, &args);
        assert_eq!(ok_cached, ok_uncached, "round {round}: success flags diverged");
        assert_eq!(cached, uncached, "round {round}: cached read differed from uncached");
        assert!(cached.contains(&format!("round{}", round)), "round {round}: read is stale: {cached}");
        // A second cached read must be identical (this is the hit path).
        let (again, _) = read_with(&ws, true, &args);
        assert_eq!(again, cached, "round {round}: cache hit changed the payload");
    }
    let _ = std::fs::remove_dir_all(&root);
}
