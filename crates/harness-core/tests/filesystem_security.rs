use harness_core::agent_loop::build_default_runtime;
use harness_core::budget::BudgetConfig;
use harness_core::env::LocalHostEnv;
use harness_core::messages::{ToolCall, ToolOutcome};
use harness_core::sensitive_paths::SensitivePathGuard;
use harness_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use harness_core::types::{ErrorCode, ToolCallId};
use harness_core::workspace::{
    is_windows_reserved_device_name, InodeMutationLock, WorkspaceCap, WorkspaceError,
};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

fn symlink(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dst)
    }
    #[cfg(windows)]
    {
        if src.as_ref().is_dir() {
            std::os::windows::fs::symlink_dir(src, dst)
        } else {
            std::os::windows::fs::symlink_file(src, dst)
        }
    }
}

#[test]
fn test_sensitive_path_guard_rules_and_exemptions() {
    // Blocked sensitive paths
    assert!(SensitivePathGuard::is_sensitive(Path::new(".env")));
    assert!(SensitivePathGuard::is_sensitive(Path::new("config/.env.production")));
    assert!(SensitivePathGuard::is_sensitive(Path::new(".env.local")));
    assert!(SensitivePathGuard::is_sensitive(Path::new("certs/server.pem")));
    assert!(SensitivePathGuard::is_sensitive(Path::new("certs/private.key")));
    assert!(SensitivePathGuard::is_sensitive(Path::new("id_rsa")));
    assert!(SensitivePathGuard::is_sensitive(Path::new("keys/id_ed25519")));
    assert!(SensitivePathGuard::is_sensitive(Path::new(".aws/credentials")));
    assert!(SensitivePathGuard::is_sensitive(Path::new(".ssh/config")));
    assert!(SensitivePathGuard::is_sensitive(Path::new(".kube/config")));
    assert!(SensitivePathGuard::is_sensitive(Path::new(".npmrc")));
    assert!(SensitivePathGuard::is_sensitive(Path::new("gcp-credentials-prod.json")));
    assert!(SensitivePathGuard::is_sensitive(Path::new("terraform.tfstate.backup")));

    // Explicitly exempted templates
    assert!(!SensitivePathGuard::is_sensitive(Path::new(".env.example")));
    assert!(!SensitivePathGuard::is_sensitive(Path::new(".env.sample")));
    assert!(!SensitivePathGuard::is_sensitive(Path::new(".env.template")));
    assert!(!SensitivePathGuard::is_sensitive(Path::new(".env.test")));
    assert!(!SensitivePathGuard::is_sensitive(Path::new("src/main.rs")));
}

#[tokio::test]
async fn test_workspace_boundary_blocks_parent_traversal_absolute_and_external_symlinks() {
    let tmp_root = std::env::temp_dir().join(format!("harness-sec-{}", Uuid::new_v4()));
    let ws_dir = tmp_root.join("workspace");
    let outside_dir = tmp_root.join("outside");
    let state_dir = tmp_root.join("state");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::create_dir_all(&outside_dir).unwrap();

    let outside_secret = outside_dir.join("secret.txt");
    fs::write(&outside_secret, "TOP_SECRET_OUTSIDE\n").unwrap();
    fs::write(ws_dir.join(".env"), "SECRET_KEY=123\n").unwrap();
    fs::write(ws_dir.join(".env.example"), "SECRET_KEY=example\n").unwrap();

    // Create symlink inside workspace pointing outside
    symlink(&outside_secret, ws_dir.join("escape_link.txt")).unwrap();
    // Create symlink inside workspace pointing to sensitive `.env` inside workspace
    symlink(ws_dir.join(".env"), ws_dir.join("readme_link.txt")).unwrap();

    let budget = BudgetConfig::default();
    let env = Arc::new(LocalHostEnv::new(&ws_dir, &state_dir).unwrap());
    let policy = PolicyEngine::new(
        PolicyMode::AllowWrite,
        Arc::new(NonInteractiveApprovalHandler),
    );
    let runtime = build_default_runtime(env, policy, budget, None);
    let (_tx, rx) = watch::channel(false);

    // 1. Absolute path blocked
    let (res_abs, _, _) = runtime
        .execute_call(
            &ToolCall {
                call_id: ToolCallId::new("c1"),
                name: "read".into(),
                raw_arguments: format!("{{\"path\":\"{}\"}}", outside_secret.display()),
            },
            0,
            rx.clone(),
        )
        .await;
    assert_eq!(
        res_abs.outcome,
        ToolOutcome::Error {
            code: ErrorCode::PathEscapeBlocked
        }
    );

    // 2. `..` traversal blocked
    let (res_dots, _, _) = runtime
        .execute_call(
            &ToolCall {
                call_id: ToolCallId::new("c2"),
                name: "read".into(),
                raw_arguments: r#"{"path":"../outside/secret.txt"}"#.into(),
            },
            0,
            rx.clone(),
        )
        .await;
    assert_eq!(
        res_dots.outcome,
        ToolOutcome::Error {
            code: ErrorCode::PathEscapeBlocked
        }
    );

    // 3. Symlink pointing outside workspace blocked on `read`, `edit`, and `write`
    for tool in ["read", "write"] {
        let args = if tool == "read" {
            r#"{"path":"escape_link.txt"}"#
        } else {
            r#"{"path":"escape_link.txt","content":"overwrite"}"#
        };
        let (res_sym, _, _) = runtime
            .execute_call(
                &ToolCall {
                    call_id: ToolCallId::new("c3"),
                    name: tool.into(),
                    raw_arguments: args.into(),
                },
                0,
                rx.clone(),
            )
            .await;
        assert!(matches!(
            res_sym.outcome,
            ToolOutcome::Error {
                code: ErrorCode::PathEscapeBlocked | ErrorCode::SymlinkBlocked
            }
        ));
    }

    // 4. Sensitive `.env` blocked in non-interactive mode, while `.env.example` is allowed
    let (res_env, _, _) = runtime
        .execute_call(
            &ToolCall {
                call_id: ToolCallId::new("c4"),
                name: "read".into(),
                raw_arguments: r#"{"path":".env"}"#.into(),
            },
            0,
            rx.clone(),
        )
        .await;
    assert_eq!(
        res_env.outcome,
        ToolOutcome::Error {
            code: ErrorCode::SensitivePathBlocked
        }
    );

    let (res_example, _, _) = runtime
        .execute_call(
            &ToolCall {
                call_id: ToolCallId::new("c5"),
                name: "read".into(),
                raw_arguments: r#"{"path":".env.example"}"#.into(),
            },
            0,
            rx.clone(),
        )
        .await;
    assert_eq!(res_example.outcome, ToolOutcome::Success);
    assert!(res_example.content.contains("SECRET_KEY=example"));

    // 5. Intra-workspace symlink pointing to `.env` is blocked with SensitivePathBlocked
    let (res_sym_env, _, _) = runtime
        .execute_call(
            &ToolCall {
                call_id: ToolCallId::new("c6"),
                name: "read".into(),
                raw_arguments: r#"{"path":"readme_link.txt"}"#.into(),
            },
            0,
            rx,
        )
        .await;
    assert_eq!(
        res_sym_env.outcome,
        ToolOutcome::Error {
            code: ErrorCode::SensitivePathBlocked
        }
    );

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test]
async fn test_inode_mutation_lock_stability_across_create_rename_and_idle_cleanup() {
    use harness_core::tools::write::atomic_write_file;

    let tmp_dir = std::env::temp_dir().join(format!("harness-inode-{}", Uuid::new_v4()));
    let sub_dir = tmp_dir.join("sub");
    fs::create_dir_all(&sub_dir).unwrap();
    let file_new = tmp_dir.join("a.txt");
    let file_rel_alias = sub_dir.join("..").join("a.txt");

    // Key before file exists matches key after file is created and after atomic rename
    let key_before = InodeMutationLock::compute_key(&file_new).unwrap();
    atomic_write_file(&file_new, b"v1", 0o644, true).unwrap();
    let key_after_create = InodeMutationLock::compute_key(&file_new).unwrap();
    atomic_write_file(&file_new, b"v2", 0o644, false).unwrap();
    let key_after_rename = InodeMutationLock::compute_key(&file_rel_alias).unwrap();

    assert_eq!(key_before, key_after_create);
    assert_eq!(key_after_create, key_after_rename);

    let locks = InodeMutationLock::new();
    let (_k, guard_a) = locks.acquire(&file_new).await.unwrap();
    assert_eq!(locks.active_lock_count(), 1);

    // Acquiring via alias path must block while guard_a is held
    let acquire_b = tokio::time::timeout(
        std::time::Duration::from_millis(30),
        locks.acquire(&file_rel_alias),
    )
    .await;
    assert!(acquire_b.is_err(), "Lock on same canonical parent+name should wait for guard_a");
    drop(guard_a);

    // Idle lock must be reclaimed from HashMap when last guard drops
    assert_eq!(locks.active_lock_count(), 0);

    let acquire_b_after = tokio::time::timeout(
        std::time::Duration::from_millis(30),
        locks.acquire(&file_rel_alias),
    )
    .await;
    assert!(acquire_b_after.is_ok());
    drop(acquire_b_after);
    assert_eq!(locks.active_lock_count(), 0);

    let _ = WorkspaceCap::new(&tmp_dir).unwrap();
    let _ = fs::remove_dir_all(&tmp_dir);
}

#[test]
fn test_open_existing_no_symlink_and_atomic_write_at_parent_fd() {
    use harness_core::tools::write::atomic_write_at;
    use std::ffi::OsStr;
    use std::io::Read;

    let tmp_root = std::env::temp_dir().join(format!("harness-openat2-{}", Uuid::new_v4()));
    let ws_dir = tmp_root.join("ws");
    let outside_dir = tmp_root.join("outside");
    fs::create_dir_all(ws_dir.join("sub")).unwrap();
    fs::create_dir_all(&outside_dir).unwrap();

    fs::write(ws_dir.join("sub/real.txt"), "hello_openat2\n").unwrap();
    fs::write(outside_dir.join("ext.txt"), "external\n").unwrap();

    // Create leaf symlink and intermediate directory symlink
    symlink(ws_dir.join("sub/real.txt"), ws_dir.join("sub/leaf_sym.txt")).unwrap();
    symlink(&outside_dir, ws_dir.join("dir_sym")).unwrap();

    let cap = WorkspaceCap::new(&ws_dir).unwrap();

    // 1. Verify root_dir_fd has FD_CLOEXEC set on Unix
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let flags = unsafe { libc::fcntl(cap.root_dir_fd().as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && (flags & libc::FD_CLOEXEC) != 0);
    }

    // 2. Normal file opens cleanly via open_existing_no_symlink and step-by-step O_NOFOLLOW fallback
    let (norm, mut f) = cap.open_existing_no_symlink("sub/real.txt").unwrap();
    assert_eq!(norm, Path::new("sub/real.txt"));
    let mut content = String::new();
    f.read_to_string(&mut content).unwrap();
    assert_eq!(content, "hello_openat2\n");

    let (_, mut f_step) = cap
        .open_existing_with_openat_steps("sub/real.txt", norm)
        .unwrap();
    let mut step_content = String::new();
    f_step.read_to_string(&mut step_content).unwrap();
    assert_eq!(step_content, "hello_openat2\n");

    // 3. Leaf symlink and intermediate directory symlink are strictly rejected with SymlinkBlocked
    assert!(matches!(
        cap.open_existing_no_symlink("sub/leaf_sym.txt"),
        Err(WorkspaceError::SymlinkBlocked { .. })
    ));
    assert!(matches!(
        cap.open_existing_with_openat_steps(
            "sub/leaf_sym.txt",
            Path::new("sub/leaf_sym.txt").to_path_buf()
        ),
        Err(WorkspaceError::SymlinkBlocked { .. })
    ));
    assert!(matches!(
        cap.open_existing_no_symlink("dir_sym/ext.txt"),
        Err(WorkspaceError::SymlinkBlocked { .. })
    ));
    assert!(matches!(
        cap.open_existing_with_openat_steps(
            "dir_sym/ext.txt",
            Path::new("dir_sym/ext.txt").to_path_buf()
        ),
        Err(WorkspaceError::SymlinkBlocked { .. })
    ));

    // 4. Parent dir open_parent_dir_no_symlink rejects symlink parent and commits atomically via linkat/renameat
    assert!(matches!(
        cap.open_parent_dir_no_symlink(Path::new("dir_sym/new.txt"), false),
        Err(WorkspaceError::SymlinkBlocked { .. })
    ));

    let (parent_fd_file, fname) = cap
        .open_parent_dir_no_symlink(Path::new("sub/created_via_at.txt"), false)
        .unwrap();
    assert_eq!(fname, OsStr::new("created_via_at.txt"));

    // No-clobber commit via linkat + unlinkat succeeds on new file, fails with FileAlreadyExists on second call
    atomic_write_at(&parent_fd_file, &fname, b"v1_linkat\n", 0o644, true).unwrap();
    assert_eq!(
        fs::read_to_string(ws_dir.join("sub/created_via_at.txt")).unwrap(),
        "v1_linkat\n"
    );
    let dup_err = atomic_write_at(&parent_fd_file, &fname, b"v2\n", 0o644, true).unwrap_err();
    assert_eq!(dup_err.0, ErrorCode::FileAlreadyExists);

    // Overwrite commit via renameat updates atomically
    atomic_write_at(&parent_fd_file, &fname, b"v2_renameat\n", 0o644, false).unwrap();
    assert_eq!(
        fs::read_to_string(ws_dir.join("sub/created_via_at.txt")).unwrap(),
        "v2_renameat\n"
    );

    // Overwriting a leaf symlink via atomic_write_at is blocked with SymlinkBlocked
    let sym_err = atomic_write_at(
        &parent_fd_file,
        OsStr::new("leaf_sym.txt"),
        b"overwrite_sym",
        0o644,
        false,
    )
    .unwrap_err();
    assert_eq!(sym_err.0, ErrorCode::SymlinkBlocked);

    let _ = fs::remove_dir_all(&tmp_root);
}

/// F-02: the navigation tools (`grep`, `find`, `ls`) must never follow a symlink that was swapped
/// in after a path check, and must never surface content from outside the workspace.
#[tokio::test]
async fn test_nav_tools_never_follow_swapped_symlinks_or_escape_the_workspace() {
    use harness_core::tools::find::execute_find;
    use harness_core::tools::grep::execute_grep;
    use harness_core::tools::ls::execute_ls;
    use harness_core::workspace::WorkspaceError;

    let tmp_root = std::env::temp_dir().join(format!("harness-f02-{}", Uuid::new_v4()));
    let ws_dir = tmp_root.join("ws");
    let outside_dir = tmp_root.join("outside");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::create_dir_all(&outside_dir).unwrap();
    fs::write(outside_dir.join("secret.txt"), "OUTSIDE_SECRET_TOKEN\n").unwrap();
    fs::write(ws_dir.join("data.txt"), "inside_only_marker\n").unwrap();

    let cap = WorkspaceCap::new(&ws_dir).unwrap();

    // Baseline: the benign in-workspace file is found and the outside secret is not.
    let baseline = execute_grep(&cap, r#"{"pattern":"inside_only_marker"}"#);
    assert!(baseline.content.contains("inside_only_marker"));
    assert!(!baseline.content.contains("OUTSIDE_SECRET"));

    // Attacker swaps `data.txt` for a symlink that points outside the workspace.
    fs::remove_file(ws_dir.join("data.txt")).unwrap();
    symlink(outside_dir.join("secret.txt"), ws_dir.join("data.txt")).unwrap();

    let after_swap = execute_grep(&cap, r#"{"pattern":"OUTSIDE_SECRET_TOKEN"}"#);
    assert!(
        !after_swap.content.contains("OUTSIDE_SECRET_TOKEN"),
        "grep followed a swapped symlink outside the workspace: {}",
        after_swap.content
    );

    // A symlinked directory must not be traversed by `find` or listed by `ls`.
    symlink(&outside_dir, ws_dir.join("escape_dir")).unwrap();

    let find_out = execute_find(&cap, r#"{"pattern":"*.txt"}"#);
    assert!(
        !find_out.content.contains("secret.txt"),
        "find traversed a symlinked directory: {}",
        find_out.content
    );

    let ls_out = execute_ls(&cap, r#"{"path":"."}"#);
    assert!(
        !ls_out.content.contains("escape_dir"),
        "ls listed a symlinked directory: {}",
        ls_out.content
    );

    // `read_dir_no_symlink` reports the symlink entry without following it.
    let (_, entries) = cap.read_dir_no_symlink(".").unwrap();
    let escape = entries
        .iter()
        .find(|e| e.name == "escape_dir")
        .expect("escape_dir entry must be listed");
    assert!(escape.is_symlink, "symlink entry must be classified as a symlink");
    assert!(!escape.is_dir, "a symlink must never be classified as a directory");

    // `open_dir_no_symlink` refuses to traverse it.
    assert!(matches!(
        cap.open_dir_no_symlink("escape_dir"),
        Err(WorkspaceError::SymlinkBlocked { .. })
    ));

    let _ = fs::remove_dir_all(&tmp_root);
}

/// F-07: the mutation lock keyed off an already-open FD / handle must serialize two writers of the
/// same file without ever re-deriving the key from a path.
#[tokio::test]
async fn test_inode_lock_keyed_from_open_fd_serializes_same_file() {
    use harness_core::tools::write::atomic_write_file;

    let tmp_root = std::env::temp_dir().join(format!("harness-f07-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_root).unwrap();
    let target = tmp_root.join("x.txt");
    atomic_write_file(&target, b"v1", 0o644, true).unwrap();

    let locks = InodeMutationLock::new();
    let cap = WorkspaceCap::new(&tmp_root).unwrap();
    let (parent_dir1, leaf1) = cap.open_parent_dir_no_symlink(Path::new("x.txt"), false).unwrap();
    let (_k, guard) = locks
        .acquire_for_file(&parent_dir1, Some(leaf1.as_os_str()))
        .await
        .unwrap();
    assert_eq!(locks.active_lock_count(), 1);

    let (parent_dir2, leaf2) = cap.open_parent_dir_no_symlink(Path::new("x.txt"), false).unwrap();
    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(30),
        locks.acquire_for_file(&parent_dir2, Some(leaf2.as_os_str())),
    )
    .await;
    assert!(blocked.is_err(), "parent-handle + leaf-name lock must serialize concurrent write/edit");
    drop(guard);
    assert_eq!(locks.active_lock_count(), 0);

    let _ = fs::remove_dir_all(&tmp_root);
}

/// F-02: concurrent attack. A background thread hammers the workspace, repeatedly swapping a
/// regular file for a symlink that points at an outside secret (and back), while the foreground
/// thread loops `grep` / `find` / `ls` / symlink-safe reads. The invariant: the outside secret
/// must NEVER be surfaced, and no tool may crash or block forever.
#[test]
fn test_nav_tools_under_concurrent_symlink_swap_never_leak_outside_content() {
    use harness_core::tools::find::execute_find;
    use harness_core::tools::grep::execute_grep;
    use harness_core::tools::ls::execute_ls;
    use std::sync::atomic::{AtomicBool, Ordering};

    let tmp_root = std::env::temp_dir().join(format!("harness-f02-race-{}", Uuid::new_v4()));
    let ws_dir = tmp_root.join("ws");
    let outside_dir = tmp_root.join("outside");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::create_dir_all(&outside_dir).unwrap();
    const SECRET: &str = "OUTSIDE_SECRET_RACE_TOKEN";
    fs::write(outside_dir.join("secret.txt"), format!("{}\n", SECRET)).unwrap();
    fs::write(ws_dir.join("data.txt"), "benign_inside_content\n").unwrap();

    let cap = Arc::new(WorkspaceCap::new(&ws_dir).unwrap());
    let stop = Arc::new(AtomicBool::new(false));

    let attacker = {
        let ws_dir = ws_dir.clone();
        let outside = outside_dir.join("secret.txt");
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let target = ws_dir.join("data.txt");
            let mut flip = false;
            while !stop.load(Ordering::Relaxed) {
                let _ = fs::remove_file(&target);
                if flip {
                    let _ = fs::write(&target, "benign_inside_content\n");
                } else {
                    let _ = symlink(&outside, &target);
                }
                flip = !flip;
            }
            // Leave a benign file behind so cleanup is deterministic.
            let _ = fs::remove_file(&target);
            let _ = fs::write(&target, "benign_inside_content\n");
        })
    };

    let mut iterations = 0usize;
    for _ in 0..400 {
        let grep_out = execute_grep(&cap, r#"{"pattern":"OUTSIDE_SECRET_RACE_TOKEN"}"#);
        assert!(
            !grep_out.content.contains(SECRET),
            "grep leaked outside content under a concurrent swap: {}",
            grep_out.content
        );

        let find_out = execute_find(&cap, r#"{"pattern":"*.txt"}"#);
        assert!(
            !find_out.content.contains(SECRET),
            "find leaked outside content under a concurrent swap: {}",
            find_out.content
        );

        let ls_out = execute_ls(&cap, r#"{"path":"."}"#);
        assert!(
            !ls_out.content.contains(SECRET),
            "ls leaked outside content under a concurrent swap: {}",
            ls_out.content
        );

        // A symlink-safe read must either fail (SymlinkBlocked) or return inside content only.
        if let Ok((_, mut file)) = cap.open_existing_no_symlink("data.txt") {
            let mut buf = String::new();
            use std::io::Read as _;
            if file.read_to_string(&mut buf).is_ok() {
                assert!(
                    !buf.contains(SECRET),
                    "symlink-safe read leaked outside content: {}",
                    buf
                );
            }
        }
        iterations += 1;
    }
    assert_eq!(iterations, 400);

    stop.store(true, Ordering::Relaxed);
    attacker.join().unwrap();
    let _ = fs::remove_dir_all(&tmp_root);
}

#[test]
fn test_windows_path_normalization_dos_devices_ntfs_ads_and_shell_resolution() {
    use harness_core::process_group::resolve_windows_shell_with_env;
    use harness_core::tools::bash::is_blacklisted_env_var;
    use std::ffi::OsStr;

    // 1. Backslash relative paths normalize cleanly to forward slashes
    let norm = WorkspaceCap::validate_relative_components(r".\crates\harness-core\src\lib.rs")
        .expect("backslash relative path must normalize");
    assert_eq!(norm.to_string_lossy(), "crates/harness-core/src/lib.rs");

    // 2. Windows drive letters and UNC paths are rejected as PathEscape
    for abs in [
        r"C:\Windows\System32\drivers\etc\hosts",
        "c:/Users/Admin/.ssh/id_rsa",
        r"\\server\share\secret.txt",
        r"src\..\..\outside.txt",
    ] {
        assert!(
            matches!(
                WorkspaceCap::validate_relative_components(abs),
                Err(WorkspaceError::PathEscape { .. })
            ),
            "expected PathEscape for '{}'",
            abs
        );
    }

    // 3. Reserved Windows DOS device names (with or without extensions/trailing dots) are rejected
    for dev in [
        "CON",
        "con.txt",
        "src/NUL",
        "sub/prn.log",
        "aux.rs",
        "com1",
        "COM9.json",
        "lpt1.txt",
        "LPT9.",
    ] {
        assert!(
            is_windows_reserved_device_name(dev.rsplit('/').next().unwrap()),
            "'{}' should be recognized as reserved DOS device",
            dev
        );
        assert!(
            matches!(
                WorkspaceCap::validate_relative_components(dev),
                Err(WorkspaceError::InvalidPath { .. })
            ),
            "expected InvalidPath for reserved DOS device '{}'",
            dev
        );
    }

    // 4. NTFS Alternate Data Streams (`:`) in relative components are rejected
    for ads in ["file.txt:secret", "src/main.rs:$DATA", "dir:stream/a.txt"] {
        assert!(
            matches!(
                WorkspaceCap::validate_relative_components(ads),
                Err(WorkspaceError::InvalidPath { .. })
            ),
            "expected InvalidPath for NTFS ADS '{}'",
            ads
        );
    }

    // 5. Case-insensitive credential environment variable stripping
    for secret_var in [
        "harness_api_key",
        "Harness_Api_Key",
        "openai_api_key",
        "Anthropic_Api_Key",
        "github_token",
        "my_custom_secret",
        "db_password",
    ] {
        assert!(
            is_blacklisted_env_var(secret_var),
            "mixed-case secret env var '{}' must be stripped",
            secret_var
        );
    }
    assert!(!is_blacklisted_env_var("SystemRoot"));
    assert!(!is_blacklisted_env_var("PATH"));
    assert!(!is_blacklisted_env_var("COMSPEC"));

    // 6. Windows shell resolution order: HARNESS_SHELL -> Git Bash -> pwsh -> powershell -> cmd.exe
    let (sh1, args1) = resolve_windows_shell_with_env(
        "cargo test",
        Some(r"C:\Program Files\PowerShell\7\pwsh.exe"),
        None,
        None,
        |_| false,
    );
    assert_eq!(sh1, r"C:\Program Files\PowerShell\7\pwsh.exe");
    assert_eq!(
        args1,
        vec![
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "cargo test"
        ]
    );

    let (sh2, args2) = resolve_windows_shell_with_env(
        "echo hi",
        None,
        Some(OsStr::new(r"C:\Tools;C:\Git\bin")),
        None,
        |p| p == Path::new(r"C:\Git\bin\bash.exe"),
    );
    assert!(sh2.ends_with("bash.exe"));
    assert_eq!(args2, vec!["-c", "echo hi"]);

    let (sh3, args3) = resolve_windows_shell_with_env(
        "dir",
        None,
        Some(OsStr::new(r"C:\Windows\System32")),
        Some(r"C:\Windows\System32\cmd.exe"),
        |_| false,
    );
    assert_eq!(sh3, r"C:\Windows\System32\cmd.exe");
    assert_eq!(args3, vec!["/D", "/S", "/C", "dir"]);
    assert!(harness_core::process_group::is_cmd_exe(&sh3));
    assert!(!harness_core::process_group::is_cmd_exe(&sh1));

    // 7. Extended-length verbatim prefix normalization (`\\?\` and `\\?\UNC\`)
    assert_eq!(
        harness_core::workspace::strip_windows_verbatim_prefix(Path::new(r"\\?\C:\Users\dev\repo")),
        Path::new(r"C:\Users\dev\repo")
    );
    assert_eq!(
        harness_core::workspace::strip_windows_verbatim_prefix(Path::new(
            r"\\?\UNC\server\share\repo"
        )),
        Path::new(r"\\server\share\repo")
    );
}

