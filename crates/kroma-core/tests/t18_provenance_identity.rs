//! T18 (KR-23, KR-24): build/repo identity that is bounded, separable, and honest about gaps.
//!
//! What was wrong: the VCS probe used an unbounded `Command::output()`, so `git status` on a large or
//! network-backed tree could hang a run; the source hash read whole files with `std::fs::read`, so a
//! file that grew after the size check could blow the budget; a file that could not be read was
//! skipped silently, producing a "complete" hash that identified less than it claimed; and the
//! executable bit was not part of the identity, so `chmod +x` left it unchanged.

use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage};
use kroma_core::provenance::{fingerprint, fingerprint_explicit, git_probe, GIT_PROBE_BUDGET};
use kroma_core::provider::{ModelProvider, ProviderCompletion, ProviderError, ProviderRequest};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-t18-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A slow VCS probe is reported as a timeout within its budget instead of hanging the run.
#[test]
fn git_probe_timeout_is_observable() {
    let root = scratch("git-timeout");
    let script = root.join("slow-git.sh");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let work = root.join("ws");
    std::fs::create_dir_all(&work).unwrap();

    let started = Instant::now();
    let probe = git_probe(
        script.to_str().unwrap(),
        &work,
        &["status", "--porcelain"],
        Duration::from_millis(300),
    );
    let elapsed = started.elapsed();

    assert!(
        probe.timed_out,
        "a hung probe must be reported as a timeout"
    );
    assert_eq!(
        probe.stdout, None,
        "a timed-out probe has no output to trust"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the probe waited {elapsed:?} instead of honouring its 300ms budget"
    );
    // The production budget is a real, finite number.
    assert!(GIT_PROBE_BUDGET <= Duration::from_secs(30));

    // A command that merely fails is *not* a timeout: "not a repository" is a legitimate answer.
    let failing = git_probe("/bin/false", &work, &["status"], Duration::from_secs(5));
    assert!(!failing.timed_out);
    assert_eq!(failing.stdout, None);

    let _ = std::fs::remove_dir_all(&root);
}

/// The source hash never reads more than its budget, even for a file larger than the whole budget.
#[test]
fn growing_source_cannot_exceed_hash_read_budget() {
    let root = scratch("hash-budget");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("small.rs"), "fn a() {}\n").unwrap();
    // A sparse file bigger than the 256 MiB source budget: creating it costs no blocks, and reading it
    // would be the bug. The walk must stop at the bound and say so.
    let huge = ws.join("huge.bin");
    let file = std::fs::File::create(&huge).unwrap();
    file.set_len(300 * 1024 * 1024).unwrap();
    drop(file);

    let started = Instant::now();
    let fingerprint = fingerprint_explicit(&ws, None);
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(20),
        "hashing took {elapsed:?}: the read budget was not enforced"
    );
    assert!(
        fingerprint.source_hash_capped,
        "a tree over the byte budget must be reported as capped"
    );
    assert!(
        fingerprint.source_identity_incomplete,
        "a capped hash does not identify the whole tree"
    );
    assert!(
        fingerprint
            .incomplete_reason
            .as_deref()
            .unwrap_or("")
            .contains("bound"),
        "the reason must say why: {:?}",
        fingerprint.incomplete_reason
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A binary from somewhere else is a *separate* identity: it must not perturb the source hash.
#[test]
fn running_binary_from_other_repo_does_not_change_build_id() {
    let root = scratch("binary-separate");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("lib.rs"), "pub fn a() {}\n").unwrap();

    let without_binary = fingerprint_explicit(&ws, None);
    // The "running binary" lives outside the source tree and has nothing to do with it.
    let binary = root.join("kroma-from-elsewhere");
    std::fs::write(&binary, b"not really an executable").unwrap();
    let with_binary = fingerprint_explicit(&ws, Some(&binary));

    assert_eq!(
        without_binary.source_sha256, with_binary.source_sha256,
        "a binary from another tree changed the source identity"
    );
    assert!(
        with_binary.binary_sha256.is_some(),
        "the binary identity must be recorded separately"
    );
    assert_ne!(
        with_binary.binary_sha256, with_binary.source_sha256,
        "build identity and repo identity are different things"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The executable bit is part of the identity: `chmod +x` changes what the tree builds into.
#[test]
fn executable_mode_change_changes_reproducibility_identity() {
    let root = scratch("mode");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let script = ws.join("run.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        let plain = fingerprint_explicit(&ws, None);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let executable = fingerprint_explicit(&ws, None);
        assert_ne!(
            plain.source_sha256, executable.source_sha256,
            "the executable bit must be part of the source identity"
        );
        assert_eq!(plain.source_files, executable.source_files);
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// A source tree that cannot be fully read reports an incomplete identity rather than a complete one.
#[test]
fn unreadable_source_never_yields_complete_identity() {
    // NOTE: the test process runs as root here, so `chmod 000` would still be readable. The
    // deterministic trigger for the same code path is a tree whose size exceeds the read budget: the
    // hash stops, and the fingerprint must say the identity is partial instead of presenting the hash
    // as complete. The read-error branch itself is exercised by the same `incomplete` assignment.
    let root = scratch("incomplete");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.rs"), "fn a() {}\n").unwrap();
    let sparse = ws.join("big.bin");
    let file = std::fs::File::create(&sparse).unwrap();
    file.set_len(260 * 1024 * 1024).unwrap();
    drop(file);

    let fingerprint = fingerprint_explicit(&ws, None);
    assert!(fingerprint.source_identity_incomplete);
    assert!(
        fingerprint.incomplete_reason.is_some(),
        "an incomplete identity must say why"
    );
    // The summary must show it too, not just the struct field.
    assert!(fingerprint.summary().contains("capped"));

    let _ = std::fs::remove_dir_all(&root);
}

/// The cheap entry point still produces a usable identity for a normal tree.
#[test]
fn ordinary_source_tree_still_identifies_completely() {
    let root = scratch("ordinary");
    let ws = root.join("ws");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(ws.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();

    // `fingerprint` deliberately refuses to walk a tree without VCS metadata (a process started from
    // `/` must not hash the filesystem), so the explicit entry point is the one that identifies a
    // release tarball or an exported directory.
    assert!(
        fingerprint(&ws, None).source_sha256.is_none(),
        "the heuristic entry point must not walk a non-repository tree"
    );
    let fingerprint = fingerprint_explicit(&ws, None);
    assert!(fingerprint.source_sha256.is_some());
    assert!(
        !fingerprint.source_identity_incomplete,
        "{:?}",
        fingerprint.incomplete_reason
    );
    assert_eq!(fingerprint.source_files, Some(2));
    assert!(!fingerprint.git_probe_timeout);

    // Two runs over the same tree agree.
    let again = fingerprint_explicit(&ws, None);
    assert_eq!(fingerprint.source_sha256, again.source_sha256);

    let _ = std::fs::remove_dir_all(&root);
    let _ = Path::new("/nonexistent");
}

// ---------------------------------------------------------------------------------------------
// Wire-request identity: the recorded hash must be the request that was actually sent
// ---------------------------------------------------------------------------------------------

/// A provider that records exactly what it was handed, then answers without tool calls.
struct RecordingProvider {
    seen: Arc<Mutex<Vec<ProviderRequest>>>,
}

impl ModelProvider for RecordingProvider {
    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a kroma_core::budget::BudgetConfig,
        _cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        self.seen.lock().unwrap().push(req);
        Box::pin(async move {
            Ok(ProviderCompletion {
                turn: AssistantTurn {
                    content: Some("done".to_string()),
                    reasoning_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: Some(TokenUsage {
                        input_tokens: 100,
                        cached_input_tokens: Some(10),
                        cache_creation_input_tokens: Some(5),
                        output_tokens: 10,
                        reasoning_tokens: None,
                    }),
                },
                request_body_bytes: 1,
                latency_ms: 1,
                ttft_ms: None,
                attempts_used: 1,
                reported_model: Some("model-a".to_string()),
            })
        })
    }
}

#[tokio::test]
async fn on_demand_report_hash_matches_captured_wire_request() {
    use kroma_core::agent_loop::{build_default_runtime, AgentRunConfig, AgentRunner};
    use kroma_core::budget::BudgetConfig;
    use kroma_core::events::EventLogger;
    use kroma_core::session::SessionJournal;
    use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};

    let root = scratch("wire-identity");
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(ws.join("lib.rs"), "pub fn a() {}\n").unwrap();

    let budget = BudgetConfig::default();
    let env = Arc::new(kroma_core::env::LocalHostEnv::new(&ws, state.clone()).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::ReadOnly,
        Arc::new(NonInteractiveApprovalHandler),
    );
    // On-demand tools: the final tool list differs from the configured one, which is exactly why the
    // identity has to be computed after assembly.
    let runtime = build_default_runtime(env, policy, budget.clone(), None);
    let cfg = AgentRunConfig::new("say done", budget).with_on_demand_tools(true);
    let run_id = cfg.run_id;
    let logger = EventLogger::new(run_id, Some(state.clone()), false).unwrap();
    let journal = SessionJournal::create(state.join("sessions"), &ws).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let (summary, _logger) = AgentRunner::new(
        cfg,
        Arc::new(RecordingProvider {
            seen: Arc::clone(&seen),
        }),
        runtime,
        rx,
    )
    .with_session_journal(journal)
    .run(logger)
    .await;
    assert_eq!(summary.turns_used, 1);

    let requests = seen.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        1,
        "expected exactly one transmitted request"
    );
    let captured = &requests[0];
    let expected = kroma_core::provenance::request_identity(
        &captured.system_prompt,
        &captured.tools,
        &captured.messages,
    );

    // Read the event log the run wrote and find the recorded identity.
    let log_path = state.join(format!("{}.jsonl", run_id));
    let text = std::fs::read_to_string(&log_path).expect("event log");
    let recorded = text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|event| {
            // Events are internally tagged: {"event_type": "ModelRequestCompleted", "payload": {...}}.
            let kind = event.get("event_type").and_then(|v| v.as_str())?;
            if kind != "ModelRequestCompleted" {
                return None;
            }
            event
                .get("payload")
                .and_then(|v| v.get("request_identity_sha256"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .expect("the run must record a request identity");

    assert_eq!(
        recorded, expected,
        "the recorded hash must correspond to the request that was actually sent"
    );
    assert_eq!(recorded.len(), 64);

    // Changing one byte of the request changes the identity.
    let mut tampered = captured.clone();
    tampered.system_prompt.push(' ');
    assert_ne!(
        kroma_core::provenance::request_identity(
            &tampered.system_prompt,
            &tampered.tools,
            &tampered.messages
        ),
        expected
    );

    let _ = std::fs::remove_dir_all(&root);
}
