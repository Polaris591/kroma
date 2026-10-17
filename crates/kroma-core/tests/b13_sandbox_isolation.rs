//! B13: OS-level sandbox isolation contract.
//!
//! Two kinds of checks:
//!
//! * **argv translation** — pure assertions on what the sandbox would launch (whitelist only, one
//!   writable path, network unshared, container caps). These need no privileges and run anywhere.
//! * **real isolation** — the argv is actually launched through the shared `ProcessSupervisor`, and
//!   the sandbox's own behaviour is asserted: host secrets are absent, symlink escapes dead-end,
//!   network egress fails, the system view is read-only, the workspace is writable, and the tmpfs
//!   cap holds. These require `bwrap` (present on the verification host); a missing `bwrap` fails
//!   the test loudly instead of silently skipping, because the isolation claim would be unproven.
//!
//! Fail-closed is asserted separately: an unusable backend must execute *nothing* on the host.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kroma_core::bench::{BenchTask, VerifyCommandLimits};
use kroma_core::process_launch::ResourceLimits;
use kroma_core::sandbox::{execute_sandboxed, SandboxBackend, SandboxError, SandboxSpec};

/// A private scratch directory under `TMPDIR`.
fn unique_root(tag: &str) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let dir = std::env::temp_dir().join(format!("kroma-b13-{}-{}", tag, uuid::Uuid::new_v4()));
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(&dir).expect("create private temp dir");
    dir
}

fn workspace_of(root: &std::path::Path) -> PathBuf {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("create workspace");
    ws
}

/// The isolation tests need a real sandbox backend; a host without one cannot prove the claim.
fn require_bwrap() {
    if SandboxSpec::bwrap("/tmp").probe().is_err() {
        panic!("bwrap is required to prove B13 isolation and is not usable on this host");
    }
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// ---------------------------------------------------------------------------------------------
// argv translation (pure)
// ---------------------------------------------------------------------------------------------

#[test]
fn bwrap_argv_is_whitelisted_and_has_exactly_one_writable_path() {
    let spec = SandboxSpec::bwrap("/tmp/kroma-b13-argv-ws");
    let argv = spec
        .wrap_argv("python3", &args(&["-c", "print(1)"]))
        .expect("wrap");
    let joined = argv.join(" ");
    assert_eq!(argv[0], "bwrap");
    assert!(joined.contains("--unshare-all"), "all namespaces: {joined}");
    assert!(
        !joined.contains("--share-net"),
        "network must stay unshared by default: {joined}"
    );
    assert!(joined.contains("--die-with-parent"), "{joined}");
    // Exactly one `--bind` (the writable workspace); everything else is `--ro-bind-try`.
    let binds = argv.iter().filter(|a| *a == "--bind").count();
    assert_eq!(binds, 1, "exactly one writable bind: {joined}");
    assert!(
        joined.contains("--bind /tmp/kroma-b13-argv-ws /workspace"),
        "{joined}"
    );
    // Host secret/state paths must never be bound.
    for forbidden in [
        "/root",
        "/home",
        "/var",
        "/etc/shadow",
        "/etc/ssh",
        "/proc/self",
    ] {
        assert!(
            !argv.iter().any(|a| a == forbidden),
            "forbidden path {forbidden} appears in argv: {joined}"
        );
    }
    assert!(joined.contains("--size"), "tmpfs size cap: {joined}");
    assert!(joined.contains("--new-session"), "{joined}");
    assert!(joined.ends_with("-- python3 -c print(1)"), "{joined}");
}

#[test]
fn bwrap_argv_opts_into_network_only_when_explicit() {
    let spec = SandboxSpec::bwrap("/tmp/kroma-b13-argv-net").with_network(true);
    let argv = spec.wrap_argv("sh", &[]).expect("wrap");
    let joined = argv.join(" ");
    assert!(joined.contains("--unshare-all --share-net"), "{joined}");
}

#[test]
fn container_argv_disables_network_and_carries_the_caps() {
    let spec = SandboxSpec::container("docker", "kroma-sandbox:pinned", "/tmp/kroma-b13-argv-ctr")
        .with_tmpfs_bytes(64 * 1024 * 1024);
    let argv = spec
        .wrap_argv("python3", &args(&["-m", "unittest"]))
        .expect("wrap");
    let joined = argv.join(" ");
    assert_eq!(argv[0], "docker");
    assert!(joined.contains("run --rm"), "{joined}");
    assert!(
        joined.contains("--network none"),
        "network must be off: {joined}"
    );
    assert!(joined.contains("--read-only"), "{joined}");
    assert!(
        joined.contains("-v /tmp/kroma-b13-argv-ctr:/workspace:rw"),
        "{joined}"
    );
    assert!(joined.contains("--memory 536870912"), "{joined}");
    assert!(joined.contains("--pids-limit 128"), "{joined}");
    assert!(joined.contains("--cpus 1"), "{joined}");
    assert!(
        joined.contains("--tmpfs /tmp:rw,size=67108864,mode=1777"),
        "{joined}"
    );
    assert!(
        joined.ends_with("kroma-sandbox:pinned python3 -m unittest"),
        "{joined}"
    );
}

#[test]
fn invalid_specs_are_refused_before_anything_runs() {
    let empty_image = SandboxSpec::container("docker", "  ", "/tmp/kroma-b13-spec");
    assert!(matches!(
        empty_image.wrap_argv("sh", &[]),
        Err(SandboxError::InvalidSpec(_))
    ));
    let empty_runtime = SandboxSpec::container("", "img:1", "/tmp/kroma-b13-spec");
    assert!(matches!(
        empty_runtime.wrap_argv("sh", &[]),
        Err(SandboxError::InvalidSpec(_))
    ));
}

// ---------------------------------------------------------------------------------------------
// Real isolation through the supervisor
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn host_secrets_and_home_do_not_exist_inside_the_sandbox() {
    require_bwrap();
    let root = unique_root("secrets");
    let ws = workspace_of(&root);
    let spec = SandboxSpec::bwrap(&ws);
    // Every probe must fail: the paths are not mounted at all.
    let script = "test -e /etc/shadow && echo SHADOW_READ || echo shadow-absent; \
                  test -e /root && echo ROOT_READ || echo root-absent; \
                  test -e /home && echo HOME_READ || echo home-absent; \
                  test -e /etc/ssh && echo SSH_READ || echo ssh-absent; \
                  exit 0";
    let out = execute_sandboxed(&spec, "sh", &args(&["-c", script]), Duration::from_secs(30))
        .await
        .expect("sandboxed run");
    let text = out.combined_output();
    assert!(out.is_success(), "probe script must run: {text}");
    assert!(text.contains("shadow-absent"), "{text}");
    assert!(text.contains("root-absent"), "{text}");
    assert!(text.contains("home-absent"), "{text}");
    assert!(text.contains("ssh-absent"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn symlink_escape_from_the_workspace_does_not_reach_host_files() {
    require_bwrap();
    let root = unique_root("symlink");
    let ws = workspace_of(&root);
    // A symlink inside the workspace pointing at a host secret.
    std::os::unix::fs::symlink("/etc/shadow", ws.join("escape")).expect("create symlink");
    let spec = SandboxSpec::bwrap(&ws);
    let out = execute_sandboxed(
        &spec,
        "sh",
        &args(&["-c", "cat /workspace/escape || echo escape-blocked"]),
        Duration::from_secs(30),
    )
    .await
    .expect("sandboxed run");
    let text = out.combined_output();
    assert!(
        text.contains("escape-blocked"),
        "symlink escape must fail: {text}"
    );
    assert!(
        !text.contains("root:"),
        "no shadow content may leak: {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn network_egress_is_blocked_by_default() {
    require_bwrap();
    let root = unique_root("network");
    let ws = workspace_of(&root);
    let spec = SandboxSpec::bwrap(&ws);
    // The primary assertion is structural: inside the unshared network namespace the only
    // interface is loopback. The socket probe is depth, not the evidence — the verification host's
    // own egress is firewalled, so an unreachable target proves nothing by itself.
    let probe = r#"with open('/proc/net/dev') as fh:
    ifaces = sorted(
        line.split(':')[0].strip()
        for line in fh
        if ':' in line and not line.lstrip().startswith('Inter-')
    )
print('IFACES', ifaces)
import socket
try:
    socket.create_connection(('1.1.1.1', 443), timeout=3)
    print('NET_OK')
except Exception as exc:
    print('NET_BLOCKED', type(exc).__name__)
"#;
    let out = execute_sandboxed(
        &spec,
        "python3",
        &args(&["-c", probe]),
        Duration::from_secs(30),
    )
    .await
    .expect("sandboxed run");
    let text = out.combined_output();
    assert!(
        text.contains("IFACES ['lo']"),
        "the sandbox must have loopback only: {text}"
    );
    assert!(text.contains("NET_BLOCKED"), "egress must fail: {text}");
    assert!(!text.contains("NET_OK"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn host_toolchain_is_read_only_inside_the_sandbox() {
    require_bwrap();
    let root = unique_root("ro");
    let ws = workspace_of(&root);
    let spec = SandboxSpec::bwrap(&ws);
    let out = execute_sandboxed(
        &spec,
        "sh",
        &args(&[
            "-c",
            "touch /usr/kroma-should-not-exist && echo WROTE || echo read-only",
        ]),
        Duration::from_secs(30),
    )
    .await
    .expect("sandboxed run");
    let text = out.combined_output();
    assert!(text.contains("read-only"), "{text}");
    assert!(!std::path::Path::new("/usr/kroma-should-not-exist").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_normal_task_runs_and_writes_only_inside_the_workspace() {
    require_bwrap();
    let root = unique_root("normal");
    let ws = workspace_of(&root);
    std::fs::write(ws.join("calc.py"), "def add(a, b):\n    return a + b\n").expect("seed module");
    let spec = SandboxSpec::bwrap(&ws);
    let verify = r#"import sys; sys.path.insert(0, '/workspace')
from calc import add
assert add(2, 3) == 5
with open('/workspace/result.txt', 'w') as fh:
    fh.write('ok')
print('verify-ok')
"#;
    let out = execute_sandboxed(
        &spec,
        "python3",
        &args(&["-c", verify]),
        Duration::from_secs(30),
    )
    .await
    .expect("sandboxed run");
    let text = out.combined_output();
    assert!(out.is_success(), "normal task must succeed: {text}");
    assert!(text.contains("verify-ok"), "{text}");
    // The write landed in the host workspace...
    let written = std::fs::read_to_string(ws.join("result.txt")).expect("workspace write lands");
    assert_eq!(written, "ok");
    // ...and the host root was not touched.
    assert!(!std::path::Path::new("/workspace/result.txt").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn tmpfs_cap_stops_a_runaway_writer() {
    require_bwrap();
    let root = unique_root("tmpfs");
    let ws = workspace_of(&root);
    let spec = SandboxSpec::bwrap(&ws).with_tmpfs_bytes(1024 * 1024);
    let out = execute_sandboxed(
        &spec,
        "sh",
        &args(&[
            "-c",
            "dd if=/dev/zero of=/tmp/fill bs=1M count=4 2>/dev/null; echo dd-exit=$?",
        ]),
        Duration::from_secs(30),
    )
    .await
    .expect("sandboxed run");
    let text = out.combined_output();
    assert!(
        text.contains("dd-exit="),
        "the dd script must actually run inside the sandbox: {text}"
    );
    assert!(
        !text.contains("dd-exit=0"),
        "a 4MiB write into a 1MiB tmpfs must fail: {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Fail-closed
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn unusable_container_backend_executes_nothing_on_the_host() {
    let root = unique_root("failclosed");
    let ws = workspace_of(&root);
    let marker = root.join("must-not-exist.txt");
    let spec = SandboxSpec::container("kroma-not-a-runtime-xyz", "kroma-nonexistent:zzz", &ws);
    let err = execute_sandboxed(
        &spec,
        "sh",
        &args(&["-c", &format!("touch {}", marker.display())]),
        Duration::from_secs(5),
    )
    .await
    .expect_err("an unusable backend must refuse");
    assert!(
        err.contains("unavailable") || err.contains("refusing"),
        "refusal must say why: {err}"
    );
    assert!(
        !marker.exists(),
        "nothing may run on the host when the sandbox is unavailable"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn container_probe_rejects_a_missing_image_without_running() {
    // Uses the real runtime when present (docker on the verification host); an absent runtime is
    // also a fail-closed refusal, which is exactly what is asserted.
    let spec = SandboxSpec::container("docker", "kroma-image-that-does-not-exist:zzz", "/tmp");
    match spec.probe() {
        Err(SandboxError::ContainerUnavailable(msg)) => {
            assert!(
                msg.contains("image") || msg.contains("runtime"),
                "probe must explain the refusal: {msg}"
            );
        }
        other => panic!("a missing image must be refused, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Bench verify wiring
// ---------------------------------------------------------------------------------------------

/// A held-out `verify_command` runs inside the sandbox when one is given, and a sandbox pointed at
/// a different directory is refused rather than scoring the wrong bytes.
#[tokio::test]
async fn bench_verify_command_runs_inside_the_sandbox_when_one_is_given() {
    require_bwrap();
    let root = unique_root("bench-verify");
    let ws = workspace_of(&root);
    std::fs::write(ws.join("calc.py"), "def add(a, b):\n    return a + b\n").expect("seed");
    let task = BenchTask {
        id: "sandboxed-verify".to_string(),
        prompt: "unused in this contract".to_string(),
        files: Default::default(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: Some(args(&[
            "python3",
            "-c",
            "import sys; sys.path.insert(0, '/workspace'); from calc import add; assert add(2, 3) == 5; print('sandboxed-verify-ok')",
        ])),
    };
    let limits = VerifyCommandLimits::new(Duration::from_secs(30), 1024 * 1024);
    let spec = SandboxSpec::bwrap(&ws);

    let sandboxed = task.evaluate_with_sandbox(&ws, &limits, Some(&spec)).await;
    assert!(sandboxed.resolved, "failures: {:?}", sandboxed.failures);
    assert!(
        sandboxed
            .checks
            .iter()
            .any(|c| c.contains("verify_command exit 0")),
        "checks: {:?}",
        sandboxed.checks
    );

    // A sandbox over a different directory must be refused: the verdict would otherwise score
    // bytes nobody asked about.
    let other = unique_root("bench-verify-other");
    let bogus = SandboxSpec::bwrap(&other);
    let mismatched = task.evaluate_with_sandbox(&ws, &limits, Some(&bogus)).await;
    assert!(!mismatched.resolved);
    assert!(
        mismatched
            .failures
            .iter()
            .any(|f| f.contains("is not the scored workspace")),
        "failures: {:?}",
        mismatched.failures
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&other);
}

/// Control experiment: the same `verify_command` (`cat /etc/shadow`) succeeds unsandboxed on this
/// host and fails inside the sandbox, so the isolation — not a broken command — is what hides the
/// host secret.
#[tokio::test]
async fn sandboxed_verify_cannot_read_host_secrets_while_the_host_run_can() {
    require_bwrap();
    let root = unique_root("bench-secret");
    let ws = workspace_of(&root);
    assert!(
        std::fs::read_to_string("/etc/shadow").is_ok(),
        "the control requires a host-readable /etc/shadow"
    );
    let task = BenchTask {
        id: "secret-probe".to_string(),
        prompt: "unused in this contract".to_string(),
        files: Default::default(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        verify_command: Some(args(&["cat", "/etc/shadow"])),
    };
    let limits = VerifyCommandLimits::new(Duration::from_secs(30), 1024 * 1024);

    // Control: the host run reads the secret (so the command itself is not trivially failing).
    let unsandboxed = task.evaluate_with_limits(&ws, &limits).await;
    assert!(
        unsandboxed.resolved,
        "control run must succeed: {:?}",
        unsandboxed.failures
    );

    // Sandboxed: the same command cannot see the path at all.
    let spec = SandboxSpec::bwrap(&ws);
    let sandboxed = task.evaluate_with_sandbox(&ws, &limits, Some(&spec)).await;
    assert!(
        !sandboxed.resolved,
        "the sandbox must hide /etc/shadow, checks: {:?}",
        sandboxed.checks
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Resource limits (bwrap backend; the container backend uses runtime flags instead)
// ---------------------------------------------------------------------------------------------

/// An address-space limit must bite on an oversized allocation while a normal task still runs:
/// without it the "RAM" part of the isolation claim is just documentation.
#[tokio::test]
async fn address_space_limit_stops_an_oversized_allocation() {
    require_bwrap();
    let root = unique_root("rlimit-as");
    let ws = workspace_of(&root);
    let spec = SandboxSpec::bwrap(&ws).with_rlimits(ResourceLimits {
        address_space_bytes: Some(256 * 1024 * 1024),
        ..ResourceLimits::default()
    });

    // The interpreter itself still starts and does small work.
    let small = execute_sandboxed(
        &spec,
        "python3",
        &args(&["-c", "print('small-ok', sum(range(1000)))"]),
        Duration::from_secs(30),
    )
    .await
    .expect("sandboxed run");
    assert!(
        small.is_success(),
        "a small task must still run: {}",
        small.combined_output()
    );

    let big = execute_sandboxed(
        &spec,
        "python3",
        &args(&[
            "-c",
            "ba = bytearray(400 * 1024 * 1024)\nprint('alloc-ok')\n",
        ]),
        Duration::from_secs(30),
    )
    .await
    .expect("sandboxed run");
    let text = big.combined_output();
    assert!(
        !big.is_success(),
        "a 400MiB allocation must fail under a 256MiB limit: {text}"
    );
    assert!(!text.contains("alloc-ok"), "{text}");
    assert!(text.contains("MemoryError"), "expected MemoryError: {text}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A per-file size limit stops a scorer from filling the host disk through the workspace, which the
/// tmpfs cap alone does not cover (the workspace is a host bind mount, not tmpfs).
#[tokio::test]
async fn file_size_limit_stops_an_oversized_write() {
    require_bwrap();
    let root = unique_root("rlimit-fsize");
    let ws = workspace_of(&root);
    let spec = SandboxSpec::bwrap(&ws).with_rlimits(ResourceLimits {
        file_bytes: Some(1024 * 1024),
        ..ResourceLimits::default()
    });
    let out = execute_sandboxed(
        &spec,
        "python3",
        &args(&[
            "-c",
            "open('/workspace/big.bin','wb').write(b'x' * (4 * 1024 * 1024))\nprint('write-ok')\n",
        ]),
        Duration::from_secs(30),
    )
    .await
    .expect("sandboxed run");
    let text = out.combined_output();
    assert!(
        !out.is_success(),
        "a 4MiB write must fail under a 1MiB file limit: {text}"
    );
    assert!(!text.contains("write-ok"), "{text}");
    // The partial write may exist on the host, but it must be bounded by the limit.
    let written = std::fs::metadata(ws.join("big.bin"))
        .map(|m| m.len())
        .unwrap_or(0);
    assert!(
        written <= 4 * 1024 * 1024,
        "unexpected large file: {written} bytes"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A CPU-time limit terminates a runaway scorer instead of letting it burn the host forever; the
/// supervisor's wall deadline is a second, independent bound.
#[tokio::test]
async fn cpu_time_limit_terminates_a_spin_loop() {
    require_bwrap();
    let root = unique_root("rlimit-cpu");
    let ws = workspace_of(&root);
    let spec = SandboxSpec::bwrap(&ws).with_rlimits(ResourceLimits {
        cpu_seconds: Some(1),
        ..ResourceLimits::default()
    });
    let started = std::time::Instant::now();
    let out = execute_sandboxed(
        &spec,
        "python3",
        &args(&["-c", "while True:\n    pass\n"]),
        Duration::from_secs(30),
    )
    .await
    .expect("sandboxed run");
    let elapsed = started.elapsed();
    assert!(
        !out.is_success(),
        "a spin loop must not be reported as success: {}",
        out.combined_output()
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the CPU limit must end the run well before the wall deadline, took {elapsed:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The container backend deliberately ignores `rlimits`: its runtime already enforces memory/CPU
/// with cgroup flags, and stacking two different ceilings would be confusing. This pins the choice
/// so it cannot silently change.
#[test]
fn container_argv_is_unaffected_by_rlimits() {
    let plain = SandboxSpec::container("docker", "img:1", "/tmp/kroma-b13-rl");
    let limited = plain.clone().with_rlimits(ResourceLimits {
        address_space_bytes: Some(64 * 1024 * 1024),
        ..ResourceLimits::default()
    });
    assert_eq!(
        plain.wrap_argv("sh", &[]).unwrap(),
        limited.wrap_argv("sh", &[]).unwrap(),
        "rlimits must not alter the container argv"
    );
}

// ---------------------------------------------------------------------------------------------
// Scored path: the benchmark harness must actually use the sandbox for its held-out verifier
// ---------------------------------------------------------------------------------------------

/// Control experiment through the real benchmark executor: the same task `cat /etc/shadow`
/// resolves unsandboxed and must not resolve once the executor is configured with a sandbox. That
/// is the difference between "a sandbox exists" and "the scored path is sandboxed".
#[tokio::test]
async fn scored_benchmark_arm_runs_its_verifier_inside_the_sandbox() {
    use kroma_core::bench::{
        BenchExecutor, BenchStrategyConfig, BenchTask, ProviderBenchExecutor, ProviderBenchSettings,
    };
    use kroma_core::messages::{AssistantTurn, FinishReason, TokenUsage};
    use kroma_core::provider::FakeProvider;
    use kroma_core::sandbox::SandboxTemplate;
    use kroma_core::tool::policy::PolicyMode;

    require_bwrap();
    assert!(
        std::fs::read_to_string("/etc/shadow").is_ok(),
        "the control requires a host-readable /etc/shadow"
    );
    let task = BenchTask {
        id: "shadow-probe".to_string(),
        prompt: "unused in this contract".to_string(),
        files: Default::default(),
        expect_contains: Vec::new(),
        expect_absent: Vec::new(),
        regression_guard: false,
        // The held-out verifier reads a host secret; only the sandbox can stop it.
        verify_command: Some(args(&["cat", "/etc/shadow"])),
    };
    let stop = AssistantTurn {
        content: Some("done".to_string()),
        reasoning_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(TokenUsage {
            input_tokens: 10,
            cached_input_tokens: None,
            output_tokens: 2,
            reasoning_tokens: None,
            cache_creation_input_tokens: None,
        }),
    };
    let strategy = BenchStrategyConfig::single_agent();
    let run_arm = |sandbox: Option<SandboxTemplate>, label: &str| {
        let task = task.clone();
        let strategy = strategy.clone();
        let root = unique_root(label);
        let stop = stop.clone();
        let settings = ProviderBenchSettings::new("fake-model", "chat-completions")
            .with_policy_mode(PolicyMode::AllowWrite)
            .with_max_model_turns(1)
            .with_max_run_duration(Duration::from_secs(30))
            .with_sandbox(sandbox);
        async move {
            let executor =
                ProviderBenchExecutor::new(Arc::new(FakeProvider::new(vec![stop])), settings);
            let result = executor.execute(&task, &strategy, 100_000, &root).await;
            let _ = std::fs::remove_dir_all(&root);
            result
        }
    };

    // Control: with no sandbox configured the verifier reads the host secret, so the arm resolves.
    let unsandboxed = run_arm(None, "bench-arm-plain").await;
    assert!(
        unsandboxed.resolved,
        "control arm must resolve: {unsandboxed:?}"
    );

    // Sandboxed: the executor must route the verifier through the sandbox, so it cannot read it.
    let sandboxed = run_arm(Some(SandboxTemplate::bwrap()), "bench-arm-sandbox").await;
    assert!(
        !sandboxed.resolved,
        "the scored arm must not resolve once its verifier is sandboxed: {sandboxed:?}"
    );
}

#[test]
fn backend_choice_is_explicit_and_never_falls_back() {
    // The spec carries its backend; there is no auto-detection helper that could pick the host.
    let spec = SandboxSpec::bwrap("/tmp/ws");
    assert_eq!(spec.backend, SandboxBackend::Bwrap);
    let container = SandboxSpec::container("docker", "img:1", "/tmp/ws");
    assert_eq!(
        container.backend,
        SandboxBackend::Container {
            runtime: "docker".to_string(),
            image: "img:1".to_string()
        }
    );
    assert!(!container.network_allowed());
}

// ---------------------------------------------------------------------------------------------
// Container backend: a real run
// ---------------------------------------------------------------------------------------------

/// The image used by the real container run. ~8 MiB, pulled explicitly; production pins by digest.
const CONTAINER_TEST_IMAGE: &str = "alpine:3.20";

fn container_test_image() -> String {
    std::env::var("KROMA_B13_CONTAINER_IMAGE").unwrap_or_else(|_| CONTAINER_TEST_IMAGE.to_string())
}

/// `true` when a container runtime is present *and* the image is already local. The run below is
/// skipped otherwise, so a host without a container runtime still passes the suite.
fn container_ready() -> bool {
    std::process::Command::new("docker")
        .args(["image", "inspect", &container_test_image()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A real container run: the pinned image executes the command, the runtime's caps land in the
/// container's own cgroup, the network is unshared, the root filesystem is read-only, and the
/// workspace bind mount is the only writable path — visible from the host afterwards.
#[tokio::test]
async fn container_backend_really_isolates_and_carries_the_caps() {
    if !container_ready() {
        eprintln!("skipping: no container runtime / test image on this host");
        return;
    }
    const MEMORY: u64 = 64 * 1024 * 1024;
    const PIDS: u64 = 32;
    let root = unique_root("container-real");
    let ws = workspace_of(&root);
    let spec = SandboxSpec::container("docker", container_test_image(), &ws).with_container_limits(
        Some(MEMORY),
        Some(PIDS),
        Some(1.0),
    );
    let script = concat!(
        "echo marker=$(cat /etc/alpine-release 2>/dev/null || echo none)\n",
        "echo mem=$(cat /sys/fs/cgroup/memory.max 2>/dev/null || echo none)\n",
        "echo pids=$(cat /sys/fs/cgroup/pids.max 2>/dev/null || echo none)\n",
        "echo cpus=$(cat /sys/fs/cgroup/cpu.max 2>/dev/null || echo none)\n",
        "echo net=$(ls /sys/class/net 2>/dev/null | tr '\\n' ',')\n",
        "touch /definitely-not-writable 2>/dev/null; echo ro-exit=$?\n",
        "echo workspace-proof > /workspace/container-proof.txt\n",
    );
    let out = execute_sandboxed(
        &spec,
        "sh",
        &args(&["-c", script]),
        Duration::from_secs(180),
    )
    .await
    .expect("a real container run");
    let text = out.combined_output();
    assert!(
        text.contains("marker=3.20"),
        "the pinned image must be what actually ran: {text}"
    );
    assert!(
        text.contains(&format!("mem={MEMORY}")),
        "the memory cap must be installed in the container's cgroup: {text}"
    );
    assert!(
        text.contains(&format!("pids={PIDS}")),
        "the pid cap must be installed in the container's cgroup: {text}"
    );
    assert!(
        text.contains("cpus=100000 100000"),
        "the cpu cap must be installed in the container's cgroup: {text}"
    );
    assert!(
        text.contains("net=lo,"),
        "the network must be unshared inside the container: {text}"
    );
    assert!(
        !text.contains("ro-exit=0"),
        "the container root filesystem must be read-only: {text}"
    );
    let proof = ws.join("container-proof.txt");
    assert_eq!(
        std::fs::read_to_string(&proof)
            .expect("the bind mount must be visible on the host")
            .trim(),
        "workspace-proof",
        "the workspace bind mount must be the only writable path"
    );
    let _ = std::fs::remove_dir_all(&root);
}
