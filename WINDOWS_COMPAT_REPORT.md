# Harness v1.1.0 — Cross-Platform & Native Windows Compatibility Report

## 1. Summary
Harness `v1.1.0` delivers full three-OS native support (**Linux**, **macOS**, and **Windows `x86_64` / `aarch64`**) with zero external runtime DLL dependencies and unified security invariants across all platforms. In addition, invoking `harness` with zero arguments in any project directory now automatically launches the interactive REPL/TUI in the current working directory (`.`), matching Claude Code / Codex / OpenCode terminal ergonomics.

## 2. Modern Native Windows Primitives

1. **Kernel Job Object Process-Tree Containment (`crates/harness-core/src/process_group.rs`)**:
   - Child processes on Windows are spawned with `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` and attached to an anonymous kernel Job Object (`CreateJobObjectW` + `SetInformationJobObject` with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK`).
   - `from_tokio_child` and `from_std_child` propagate `AssignProcessToJobObject` errors so unattached job handles are never retained.
   - `terminate_gracefully` awaits the grace period before invoking `TerminateJobObject` once, guaranteeing zero grandchild leaks or inherited pipe hangs.
   - When `cmd.exe` is selected by deterministic shell resolution (`HARNESS_SHELL` -> Git Bash `bash.exe` -> `pwsh.exe` / `powershell.exe` -> `ComSpec` / `cmd.exe`), `apply_shell_args_tokio` passes `/D /S /C "<raw_command>"` via `CommandExt::raw_arg` so inner double quotes are never mangled by MSVCRT `\"` escaping.

2. **Reparse-Point-Free Workspace Capability & Atomic File Replacement (`crates/harness-core/src/workspace.rs`, `tools/write.rs`, `revision.rs`)**:
   - Rejects Windows drive prefixes (`C:\`), UNC paths (`\\server\share`), reserved DOS device names (`CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `COM1`..`COM9`, `LPT1`..`LPT9` with extensions/trailing dots), and NTFS Alternate Data Streams (`file.txt:stream`, `:$DATA`).
   - Step-by-step directory traversal (`open_existing_no_symlink`, `open_dir_no_symlink`, `open_parent_dir_no_symlink`) opens directories with `FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS` without `FILE_SHARE_DELETE`, keeps all ancestor directory handles alive until the target handle is opened, and verifies via `GetFinalPathNameByHandleW` (`verify_windows_handle_within_root` with case-insensitive `\\?\` verbatim prefix normalization) that the opened handle resides strictly inside `self.root`.
   - `atomic_write_at` resolves the parent path from the open parent handle via a dynamically resizing `GetFinalPathNameByHandleW` loop, writes a sibling temporary file with `FILE_FLAG_OPEN_REPARSE_POINT` + `sync_all()`, commits via `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`, and synchronizes the Windows `readonly` attribute with `(mode & 0o222) == 0`.
   - `InodeMutationLock` derives unique 64-bit file keys on Windows from `(dwVolumeSerialNumber, (nFileIndexHigh << 32) | nFileIndexLow)` via `GetFileInformationByHandle`.

3. **Non-Blocking Win32 Sentinel Byte-Range WAL Locking (`crates/harness-core/src/session.rs`)**:
   - `SessionJournal` acquires an exclusive non-blocking lock via `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY)` on sentinel byte `0xFFFF_FFFF_FFFF_FFFE` (length 1) and unlocks the exact range in `Drop` via `UnlockFileEx`, preventing concurrent writers while allowing `peek_workspace_root` to read the WAL header.

4. **Zero-OpenSSL Native Windows TLS (`crates/harness-core/src/provider/chat_completions.rs`)**:
   - Uses Windows OS-native **Schannel** (`tokio-native-tls` / `native-tls`) on `#[cfg(windows)]`, with full multi-certificate PEM bundle parsing for `HARNESS_CA_BUNDLE`.

5. **Non-Blocking Console Input Polling (`crates/harness-cli/src/approval_ui.rs`)**:
   - Validates `STD_INPUT_HANDLE` against both `NULL` and `INVALID_HANDLE_VALUE`, uses `GetConsoleMode` + `GetNumberOfConsoleInputEvents` + `ReadConsoleInputW` to process `KEY_EVENT` (`bKeyDown != 0`) records without blocking inside cooked-mode `ReadFile` on key-up or window-resize events, and falls back to `ReadFile` only on redirected non-console pipes.

6. **Zero-Arg Interactive REPL/TUI & One-Line Installers (`crates/harness-cli/src/main.rs`, `install.sh`, `install.ps1`)**:
   - Running `harness` with no arguments (or `harness --allow-write`) defaults `--workspace` to the current working directory (`.`) and launches the interactive REPL/TUI (`CliSurface::Repl`).
   - Added `install.sh` (Linux/macOS) and `install.ps1` (Windows PowerShell) to install `harness` into the user's `PATH`.
