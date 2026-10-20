//! R04 / R05 / R02 at the real CLI door.
//!
//! These tests drive the built `kroma` binary against a local mock provider that never leaves the
//! host. They are the regression for the *wiring* the library tests cannot see:
//!
//! * R04 — a workspace `.kroma.toml` cannot mint a spend envelope. `kroma run` must exit 2 with zero
//!   requests, while an owner-declared ceiling on the same command still dispatches.
//! * R05 — the experiment ceiling is `per-arm × planned arms`, so a second arm is not denied by the
//!   first arm's spend. The same run with a declared ceiling that cannot cover both arms is refused
//!   before the first request.
//! * R02 — an `execution_sandbox` whose backend cannot run is refused before the first request
//!   instead of silently running the benchmark on the host.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Usage the mock reports for every request. Chosen so the first arm consumes almost all of a
/// single arm's budget: with a shared single-arm ledger the second arm's first dispatch is refused.
const MOCK_INPUT_TOKENS: u64 = 19_000;

struct MockProvider {
    base_url: String,
    hits: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
        let port = listener.local_addr().unwrap().port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let hits = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_hits = Arc::clone(&hits);
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        thread_hits.fetch_add(1, Ordering::SeqCst);
                        let _ = respond(socket);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            // `localhost` keeps the broker's private-address gate out of the way: the gate refuses
            // literal loopback addresses, and this fixture is not testing that gate.
            base_url: format!("http://localhost:{port}"),
            hits,
            shutdown,
            thread: Some(thread),
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn respond(mut socket: TcpStream) -> std::io::Result<()> {
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = socket.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buf[..n]);
        assert!(request.len() < 1 << 20, "bounded fixture requests only");
        if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..pos]).to_string();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            if request.len() >= pos + 4 + length {
                break;
            }
        }
    }
    let body = String::from_utf8_lossy(&request).to_string();
    if body.contains("\"stream\":true") {
        let events = format!(
            "data: {{\"model\":\"mock-local\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"done\"}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":{MOCK_INPUT_TOKENS},\"completion_tokens\":1}}}}\n\n\
             data: [DONE]\n\n"
        );
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            events.len()
        );
        socket.write_all(header.as_bytes())?;
        socket.write_all(events.as_bytes())?;
    } else {
        let body = format!(
            "{{\"model\":\"mock-local\",\"choices\":[{{\"index\":0,\"finish_reason\":\"stop\",\"message\":{{\"role\":\"assistant\",\"content\":\"done\"}}}}],\"usage\":{{\"prompt_tokens\":{MOCK_INPUT_TOKENS},\"completion_tokens\":1}}}}"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes())?;
    }
    Ok(())
}

fn unique_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("kroma-cli-t35-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("create scratch root");
    root
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_kroma")
}

fn base_command(root: &Path, mock: &MockProvider) -> Command {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(binary());
    command
        .env("HOME", &home)
        .env("KROMA_HOME", home.join(".kroma"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("KROMA_API_KEY", "local-mock-not-a-secret")
        .env("KROMA_BASE_URL", &mock.base_url)
        .env("KROMA_MODEL", "mock-local")
        .env_remove("KROMA_MAX_TOKENS")
        .env_remove("KROMA_EXECUTION_SANDBOX")
        .env_remove("KROMA_PROFILE")
        .env_remove("KROMA_ALLOWED_MODELS");
    command
}

fn run(command: &mut Command) -> Output {
    command.output().expect("run kroma binary")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn write_workspace(root: &Path) -> PathBuf {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    ws
}

/// R04: a workspace-only `max_tokens` is not an owner authorization. The control (an owner ceiling
/// in the environment) proves the same command reaches the provider when one is declared.
#[test]
fn a_workspace_max_tokens_cannot_authorize_a_paid_run() {
    let root = unique_root("workspace-envelope");
    let mock = MockProvider::start();
    let ws = write_workspace(&root);
    std::fs::write(ws.join(".kroma.toml"), "max_tokens = 999999999\n").unwrap();

    let mut command = base_command(&root, &mock);
    let refused = run(command
        .arg("run")
        .arg("--workspace")
        .arg(&ws)
        .arg("--task")
        .arg("reply with done")
        .arg("--policy")
        .arg("read-only")
        .arg("--no-stream")
        .arg("--quiet")
        .arg("--no-subagents"));
    assert_eq!(
        refused.status.code(),
        Some(2),
        "a workspace-minted envelope must be refused: {}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("no owner spend envelope"),
        "the refusal must name the missing owner envelope: {}",
        stderr(&refused)
    );
    assert_eq!(mock.hits(), 0, "a refused run must not send a request");

    // Control: the same command with an owner ceiling declared in the environment dispatches.
    let mut command = base_command(&root, &mock);
    let allowed = run(command
        .env("KROMA_MAX_TOKENS", "50000")
        .arg("run")
        .arg("--workspace")
        .arg(&ws)
        .arg("--task")
        .arg("reply with done")
        .arg("--policy")
        .arg("read-only")
        .arg("--no-stream")
        .arg("--quiet")
        .arg("--no-subagents"));
    assert_eq!(
        allowed.status.code(),
        Some(0),
        "an owner-declared ceiling must be honoured: {}",
        stderr(&allowed)
    );
    assert_eq!(mock.hits(), 1, "the control must reach the provider once");
    let _ = std::fs::remove_dir_all(&root);
}

struct BenchFixture {
    workspace: PathBuf,
    suite: PathBuf,
    prereg: PathBuf,
    arms: PathBuf,
}

fn bench_fixture(root: &Path) -> BenchFixture {
    let workspace = write_workspace(root);
    let suite = root.join("dev-suite.json");
    let holdout = root.join("holdout-suite.json");
    let task = serde_json::json!([{
        "id": "task-1",
        "prompt": "reply with done",
        "files": {},
        "expect_contains": [],
        "expect_absent": ["forbidden.txt"],
        "regression_guard": false,
        "verify_command": null
    }]);
    std::fs::write(&suite, serde_json::to_string(&task).unwrap()).unwrap();
    // The holdout suite must be a different file; its content is never scored here.
    let mut holdout_task = task.clone();
    holdout_task[0]["id"] = serde_json::json!("holdout-1");
    std::fs::write(&holdout, serde_json::to_string(&holdout_task).unwrap()).unwrap();
    let verifier = root.join("verify.py");
    std::fs::write(&verifier, "print('ok')\n").unwrap();
    let prereg = root.join("prereg.json");
    std::fs::write(
        &prereg,
        serde_json::json!({
            "task_cluster_definition": {
                "dev_suite": suite.display().to_string(),
                "holdout_suite": holdout.display().to_string()
            },
            "model_identity_observability": "local mock only",
            "quality_margin": "fixture",
            "noise_and_power_plan": "fixture",
            "cache_order_design": "fixture",
            "budget": "fixture",
            "stopping_rule": "fixture",
            "holdout_policy": { "verifier_path": verifier.display().to_string() }
        })
        .to_string(),
    )
    .unwrap();
    let arms = root.join("arms.json");
    std::fs::write(
        &arms,
        serde_json::json!([
            { "name": "baseline", "max_subagents": 0 },
            { "name": "candidate", "max_subagents": 0 }
        ])
        .to_string(),
    )
    .unwrap();
    BenchFixture {
        workspace,
        suite,
        prereg,
        arms,
    }
}

fn bench_command(root: &Path, mock: &MockProvider, fixture: &BenchFixture) -> Command {
    let mut command = base_command(root, mock);
    command
        .arg("bench")
        .arg("--workspace")
        .arg(&fixture.workspace)
        .arg("--suite")
        .arg(&fixture.suite)
        .arg("--prereg")
        .arg(&fixture.prereg)
        .arg("--executor")
        .arg("provider")
        .arg("--arms")
        .arg(&fixture.arms)
        .arg("--budget-tokens")
        .arg("20000")
        .arg("--allow-write")
        .arg("--no-verify")
        .arg("--json")
        .arg("--quiet");
    command
}

/// R05: two arms, each granted 20000 tokens, both dispatch. The first arm's 19000-token settlement
/// must not shrink the second arm's allowance.
#[test]
fn each_bench_arm_gets_its_own_allowance_at_the_cli() {
    let root = unique_root("arm-allowance");
    let mock = MockProvider::start();
    let fixture = bench_fixture(&root);
    let output = run(&mut bench_command(&root, &mock, &fixture));
    assert_eq!(
        output.status.code(),
        Some(0),
        "the paired run must complete: {}",
        stderr(&output)
    );
    assert_eq!(
        mock.hits(),
        2,
        "each arm must dispatch its first request from its own allowance: {}",
        stderr(&output)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    let results = report["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "one result per arm");
    for result in results {
        assert_eq!(
            result["request_attempts_used"], 1,
            "every arm must have dispatched: {result}"
        );
        assert_eq!(result["total_tokens"], MOCK_INPUT_TOKENS + 1, "{result}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// R05: an experiment ceiling that cannot cover every planned arm is refused before the first
/// request, instead of denying the last arm mid-experiment.
#[test]
fn an_insufficient_experiment_ceiling_is_refused_at_the_cli() {
    let root = unique_root("experiment-ceiling");
    let mock = MockProvider::start();
    let fixture = bench_fixture(&root);
    let output = run(bench_command(&root, &mock, &fixture)
        .arg("--experiment-budget-tokens")
        .arg("20000"));
    assert_eq!(
        output.status.code(),
        Some(2),
        "the run must be refused: {}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("cannot cover"),
        "the refusal must name the shortfall: {}",
        stderr(&output)
    );
    assert_eq!(mock.hits(), 0, "a refused experiment sends no request");
    let _ = std::fs::remove_dir_all(&root);
}

/// R02: an execution backend that cannot run is refused before the first request. `no-such-runtime`
/// cannot be a container runtime on any host, so the probe is deterministic.
#[test]
fn an_unusable_execution_sandbox_is_refused_before_the_first_request() {
    let root = unique_root("sandbox-preflight");
    let mock = MockProvider::start();
    let fixture = bench_fixture(&root);
    let mut command = bench_command(&root, &mock, &fixture);
    let output = run(command.env(
        "KROMA_EXECUTION_SANDBOX",
        "container:no-such-runtime:kroma-fixture:1",
    ));
    assert_eq!(
        output.status.code(),
        Some(2),
        "an unusable execution backend must be refused: {}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("execution_sandbox cannot run"),
        "the refusal must name the backend: {}",
        stderr(&output)
    );
    assert_eq!(mock.hits(), 0, "a refused benchmark sends no request");
    let _ = std::fs::remove_dir_all(&root);
}
