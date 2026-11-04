//! Closing the downstream end of a CLI pipe must not turn ordinary menu or
//! help output into a Rust panic.
use std::process::{Command, Stdio};

#[test]
fn help_survives_a_closed_stdout_pipe() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kroma"))
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kroma");
    drop(child.stdout.take());
    let output = child.wait_with_output().expect("wait for kroma");
    assert!(
        output.status.success(),
        "help panicked on a closed pipe: {:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
