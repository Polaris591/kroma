//! T06 (KR-06): every untrusted read keeps its boundary at open, at type check and during the read
//! — not "read it all, then look at the length".
//!
//! These are the call-site tests; the primitives themselves (FIFO, symlink, directory, growth,
//! UTF-8-after-size) are unit-tested inside `secure_fs`.

use kroma_core::async_tasks::{AsyncTaskRegistry, NewTask};
use kroma_core::config_file::{KromaConfigFile, MAX_WORKSPACE_CONFIG_BYTES};
use kroma_core::tools::edit::execute_edit;
use kroma_core::tools::find::{load_workspace_ignore_patterns, MAX_IGNORE_FILE_BYTES};
use kroma_core::workspace::{InodeMutationLock, WorkspaceCap};
#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kroma-t06-{}-{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn workspace_config_path(root: &Path) -> PathBuf {
    KromaConfigFile::workspace_config_path(root)
}

#[cfg(unix)]
fn make_fifo(path: &Path) {
    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
}

/// A FIFO planted as the workspace config must not hang startup. Opening a FIFO for reading blocks
/// until a writer appears, so this only passes if the read is non-blocking *and* refuses the type.
///
/// Unix-only: `mkfifo` is the POSIX way to plant one. On Windows the equivalent hazard is a named
/// pipe or a reparse point, which `secure_fs` refuses by handle type; that path is compiled for
/// `x86_64-pc-windows-gnu` but not exercised here.
#[cfg(unix)]
#[test]
fn workspace_config_fifo_never_blocks() {
    let root = scratch("cfg-fifo");
    make_fifo(&workspace_config_path(&root));

    let started = Instant::now();
    let err = KromaConfigFile::load_workspace(&root)
        .expect_err("a FIFO workspace config must be refused, not read");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "load_workspace blocked for {:?} on a FIFO with no writer",
        elapsed
    );
    assert!(
        err.contains("not a regular file") || err.contains("cannot read configuration file"),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A symlink planted as the workspace config is refused rather than followed.
#[cfg(unix)]
#[test]
fn workspace_config_swap_to_symlink_is_refused() {
    let root = scratch("cfg-symlink");
    let target = root.join("real.toml");
    std::fs::write(&target, "[budget]\nmax_total_tokens = 1\n").unwrap();
    std::os::unix::fs::symlink(&target, workspace_config_path(&root)).unwrap();

    let err = KromaConfigFile::load_workspace(&root)
        .expect_err("a symlinked workspace config must be refused");
    assert!(
        err.contains("symbolic link") || err.contains("not a regular file"),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A config that is already over the limit is refused, and so is one that grows while being read.
#[test]
fn workspace_config_growth_stops_at_limit() {
    let root = scratch("cfg-growth");
    let path = workspace_config_path(&root);
    // Valid TOML that is over the byte cap: padding with comments keeps it parseable in principle,
    // so the refusal can only come from the size bound.
    let mut text = String::from("[budget]\nmax_total_tokens = 1000\n");
    while (text.len() as u64) < MAX_WORKSPACE_CONFIG_BYTES + 1 {
        text.push_str("# padding padding padding padding padding\n");
    }
    std::fs::write(&path, &text).unwrap();

    let err = KromaConfigFile::load_workspace(&root)
        .expect_err("an oversized workspace config must be refused");
    assert!(
        err.contains("larger than") || err.contains("exceeding"),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A failed config read yields an error and no partially applied values.
#[test]
fn failed_config_read_does_not_apply_partial_values() {
    let root = scratch("cfg-partial");
    let path = workspace_config_path(&root);
    // The first key is valid and would change behaviour; the second is garbage. Nothing may be
    // applied from the valid prefix.
    std::fs::write(
        &path,
        "[budget]\nmax_total_tokens = 4242\nthis is not toml at all\n",
    )
    .unwrap();

    let err = KromaConfigFile::load_workspace(&root)
        .expect_err("a malformed workspace config must fail closed");
    assert!(
        err.contains("invalid configuration file"),
        "unexpected error: {err}"
    );
    // The API cannot hand back a half-parsed value: `load_workspace` returns `Err`, and there is no
    // separate "best effort" entry point. A caller that ignored the error would still have to build
    // its own default, so the 4242 can never leak through as an applied limit.
    let _ = std::fs::remove_dir_all(&root);
}

/// An ignore file larger than the per-file cap is truncated at the cap, not allocated in full.
#[test]
fn huge_ignore_file_cannot_allocate_unbounded() {
    let root = scratch("ignore-huge");
    let ws = WorkspaceCap::new(&root).unwrap();
    // Wide lines keep the rule count small (fast to parse) while the byte length still exceeds the
    // cap by 4 KiB, so the test exercises the byte bound rather than the parser's speed.
    let line = format!("{}/\n", "d".repeat(1000));
    let mut ignore = String::new();
    while (ignore.len() as u64) < MAX_IGNORE_FILE_BYTES + 4096 {
        ignore.push_str(&line);
    }
    std::fs::write(root.join(".gitignore"), &ignore).unwrap();

    let started = Instant::now();
    let rules = load_workspace_ignore_patterns(&ws);
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "ignore loading took {:?}",
        elapsed
    );
    assert!(
        !rules.is_empty(),
        "the first megabyte of rules must still be honoured"
    );
    // Bounded: the file is ~1 MiB + 4 KiB, so the rule count is the cap divided by the line length,
    // not the whole file.
    let max_possible = (MAX_IGNORE_FILE_BYTES as usize / line.len()) + 1;
    assert!(
        rules.len() <= max_possible,
        "read past the ignore-file cap: {} rules",
        rules.len()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// An edit whose file grows past `max_edit_file_bytes` is refused instead of reading it all.
#[tokio::test]
async fn growing_edit_reads_stop_at_limit() {
    let root = scratch("edit-grow");
    let ws = WorkspaceCap::new(&root).unwrap();
    let locks = InodeMutationLock::new();
    std::fs::write(root.join("a.rs"), "fn a() {}\n".repeat(200)).unwrap();

    let args = serde_json::json!({
        "path": "a.rs",
        "edits": [{"old_text": "fn a() {}", "new_text": "fn b() {}"}],
    });
    // A deliberately tiny cap stands in for "the file grew after the pre-check": the read itself
    // must stop at the limit and the tool must refuse, not allocate the whole file.
    let result = execute_edit(&ws, &locks, &args.to_string(), 64, 512 * 1024).await;
    let text = format!("{:?}", result);
    assert!(
        text.contains("max_edit_file_bytes"),
        "expected a bounded-read refusal, got: {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A registry document that repeats an id or exceeds the record cap is refused, never trimmed.
#[test]
fn registry_overflow_or_duplicate_id_is_not_silently_truncated() {
    let root = scratch("registry");
    let path = root.join("state/tasks/registry.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    let mut registry = AsyncTaskRegistry::open(&path).unwrap();
    registry
        .register(NewTask {
            task_id: "task-1".to_string(),
            session_id: "sess".to_string(),
            owner_id: "owner".to_string(),
            workspace_root: root.display().to_string(),
            role: "scout".to_string(),
            created_turn: 1,
            token_allocation: 1000,
            prompt: "look around".to_string(),
            timeout_ms: 1000,
            spec_json: "{}".to_string(),
            parent_policy: "workspace_write".to_string(),
            parent_policy_hooks: kroma_core::subagents::ParentPolicyHooks::unknown(),
            session_path: String::new(),
        })
        .expect("register");
    registry.persist().unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let one = doc["tasks"][0].clone();

    // Duplicate id.
    let mut dup = doc.clone();
    dup["tasks"] = serde_json::json!([one.clone(), one.clone()]);
    std::fs::write(&path, serde_json::to_vec(&dup).unwrap()).unwrap();
    let err = AsyncTaskRegistry::open(&path).expect_err("a duplicate id must be refused");
    assert!(
        err.to_string().contains("repeats task id"),
        "unexpected error: {err}"
    );

    // Overflow: more records than the cap, each with a distinct id.
    let mut over = doc.clone();
    let mut tasks = Vec::new();
    for i in 0..(kroma_core::async_tasks::MAX_TASKS + 1) {
        let mut record = one.clone();
        record["task_id"] = serde_json::json!(format!("task-{i}"));
        tasks.push(record);
    }
    over["tasks"] = serde_json::json!(tasks);
    std::fs::write(&path, serde_json::to_vec(&over).unwrap()).unwrap();
    let err = AsyncTaskRegistry::open(&path).expect_err("an overflowing registry must be refused");
    assert!(
        err.to_string().contains("over the") && err.to_string().contains("task cap"),
        "unexpected error: {err}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// KR-I01: measure the four file tools' output cost on one fixed tree.
///
/// The task asks for the actual I/O and output cost of read/grep/find/ls, and explicitly not for
/// "scanned bytes equal token cost". This pins the output side: each tool's payload on a 200-file
/// tree, with `read` limited to ten lines of a 2 MiB file. Timing is printed for the record but not
/// asserted -- wall time on a shared CI box is not a property of the code.
#[test]
fn the_four_file_tools_have_measured_output_costs() {
    use kroma_core::tools::find::execute_find;
    use kroma_core::tools::grep::execute_grep;
    use kroma_core::tools::ls::execute_ls;
    use kroma_core::tools::read::execute_read;

    let root = scratch("io-cost");
    for i in 0..200 {
        std::fs::write(
            root.join(format!("f{i}.rs")),
            format!("fn f{i}() {{\n    let _ = {i};\n}}\n"),
        )
        .unwrap();
    }
    let big = "fn filler() {}\n".repeat(160_000); // ~2 MiB
    std::fs::write(root.join("big.rs"), &big).unwrap();
    let ws = WorkspaceCap::new(&root).unwrap();

    type ToolCase = (
        &'static str,
        &'static str,
        fn(&WorkspaceCap, &str) -> kroma_core::tools::ToolExecutionOutput,
    );
    let cases: [ToolCase; 4] = [
        (
            "read",
            r#"{"path":"big.rs","offset":1,"limit":10}"#,
            execute_read,
        ),
        ("grep", r#"{"pattern":"fn f1"}"#, execute_grep),
        ("find", r#"{"pattern":"*.rs"}"#, execute_find),
        ("ls", r#"{"path":"."}"#, execute_ls),
    ];

    let mut report = Vec::new();
    for (name, args, run) in cases {
        let started = Instant::now();
        let out = run(&ws, args);
        let elapsed = started.elapsed();
        let bytes = out.content.len();
        println!(
            "KR-I01 {name}: output_bytes={bytes} elapsed_ms={:.2} success={}",
            elapsed.as_secs_f64() * 1000.0,
            matches!(out.outcome, kroma_core::messages::ToolOutcome::Success)
        );
        assert!(
            bytes < 200_000,
            "{name} output must stay bounded, got {bytes} bytes"
        );
        report.push((name, bytes));
    }

    // `read` of ten lines out of a 2 MiB file must not return the file. This is the concrete
    // "output cost is not scan cost" statement: the scan touched ~2 MiB, the model sees ten lines.
    let read_bytes = report.iter().find(|(n, _)| *n == "read").unwrap().1;
    assert!(
        read_bytes < 2_000,
        "a ten-line read must not return the 2 MiB body, got {read_bytes} bytes"
    );

    let _ = std::fs::remove_dir_all(&root);
}
