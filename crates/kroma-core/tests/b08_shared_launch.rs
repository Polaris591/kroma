//! B08 contract: the one-shot supervisor and the long-lived MCP client launch through the same
//! shared seam (`ProcessLaunch`), and a launch whose process-tree containment cannot be attached
//! fails closed by stopping and reaping the child instead of returning an uncontained process.

use std::path::PathBuf;
use std::time::Duration;

use kroma_core::mcp::McpClient;
use kroma_core::process_group::{ChildProcessGroup, Containment};
use kroma_core::process_launch::{EnvironmentPolicy, ProcessLaunch};
use kroma_core::process_supervisor::{OneShotCommandSpec, ProcessSupervisor};

fn unique_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-b08-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// The tokio containment attach must stop and reap a child it cannot contain, so the caller never
/// keeps an uncontained process. `ForceAttachFailure` reproduces the Windows Job Object failure on
/// every platform.
#[tokio::test]
async fn tokio_containment_failure_stops_and_reaps_child() {
    let mut child = tokio::process::Command::new("sh")
        .args(["-c", "exec sleep 30"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn sh");
    let pid = child.id().expect("child pid") as i32;

    let result =
        ChildProcessGroup::try_from_tokio_child(&mut child, Containment::ForceAttachFailure);
    assert!(
        result.is_err(),
        "a failed containment attach must return Err"
    );
    assert!(
        !ChildProcessGroup::is_process_alive(pid),
        "the uncontained child must be stopped and reaped, not left running (pid {pid})"
    );
}

/// The synchronous counterpart used by the command-hook path.
#[test]
fn std_containment_failure_stops_and_reaps_child() {
    let mut child = std::process::Command::new("sh")
        .args(["-c", "exec sleep 30"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn sh");
    let pid = child.id() as i32;

    let result = ChildProcessGroup::try_from_std_child(&mut child, Containment::ForceAttachFailure);
    assert!(
        result.is_err(),
        "a failed containment attach must return Err"
    );
    assert!(
        !ChildProcessGroup::is_process_alive(pid),
        "the uncontained child must be stopped and reaped, not left running (pid {pid})"
    );
}

/// `ProcessLaunch` propagates the fail-closed error for both the tokio and synchronous spawn paths
/// rather than handing back a child with no containment.
#[tokio::test]
async fn process_launch_fails_closed_when_containment_attach_fails() {
    let argv = || vec!["-c".to_string(), "exec sleep 30".to_string()];
    let tokio_result = ProcessLaunch::new("sh", argv())
        .containment(Containment::ForceAttachFailure)
        .spawn_tokio();
    assert!(
        tokio_result.is_err(),
        "tokio launch must fail closed when containment cannot be attached"
    );
    let std_result = ProcessLaunch::new("sh", argv())
        .containment(Containment::ForceAttachFailure)
        .spawn_std();
    assert!(
        std_result.is_err(),
        "std launch must fail closed when containment cannot be attached"
    );
}

/// A successful launch hands back a real process-group handle, not a hollow one.
#[tokio::test]
async fn process_launch_returns_contained_group_on_success() {
    let (mut child, group) = ProcessLaunch::new("sh", vec!["-c".to_string(), "exit 0".to_string()])
        .spawn_tokio()
        .expect("launch must succeed");
    assert!(
        group.pgid() > 0,
        "launch must return a process-group handle"
    );
    let status = child.wait().await.expect("wait");
    assert_eq!(status.code(), Some(0));
    group.cleanup_after_exit().await;
}

/// The one-shot supervisor launches through the shared seam, so the credential scrub still applies.
#[tokio::test]
async fn one_shot_launch_scrubs_secret_environment() {
    std::env::set_var("KROMA_B08_SECRET", "leak_canary");
    let spec = OneShotCommandSpec::new(
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf %s \"$KROMA_B08_SECRET\"".to_string(),
        ],
        std::env::temp_dir(),
    )
    .with_env_policy(EnvironmentPolicy::SanitizedMinimal)
    .with_timeout(Duration::from_secs(10));

    let result = ProcessSupervisor::execute_one_shot(spec, None)
        .await
        .expect("one-shot run");
    assert!(result.is_success(), "run must succeed: {result:?}");
    assert!(
        !result.stdout.contains("leak_canary"),
        "the shared launch seam must scrub KROMA_* secrets, got {:?}",
        result.stdout
    );
}

/// The long-lived MCP client launches through the same seam: the credential scrub applies, the
/// explicitly authorized server variable is preserved, and the stdio session is left intact.
#[tokio::test]
async fn mcp_spawn_uses_shared_sanitized_launch() {
    std::env::set_var("KROMA_B08_SECRET", "leak_canary");
    let dir = unique_dir("mcp");
    let out = dir.join("env.txt");
    let script = format!(
        "printf '%s|%s' \"$KROMA_B08_SECRET\" \"$AUTHORIZED_MCP_KEY\" > {}; exec sleep 30",
        out.display()
    );

    let client = McpClient::spawn(
        "sh",
        &["-c".to_string(), script],
        &[("AUTHORIZED_MCP_KEY".to_string(), "granted".to_string())],
        Duration::from_secs(5),
    )
    .expect("mcp spawn");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !out.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let written = std::fs::read_to_string(&out).expect("server wrote its env");
    assert_eq!(
        written, "|granted",
        "KROMA_* must be scrubbed and the authorized variable preserved"
    );

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
