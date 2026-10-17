# Harness v0.8.1 — GPT 复审报告（F-01 ~ F-09）全量清零与验收报告

日期：2026-10-12  
版本：`v0.8.1`（Git tag: `v0.8.1`，`EVENT_SCHEMA_VERSION = "0.6.0"` 保持事件协议向前兼容）  
测试结果：`cargo test --workspace` → **59 passed / 0 failed / 0 warnings**

---

## 一、F-01 ~ F-09 逐项修复核验清单

### 1. F-01（阻断项）：`--resume` 拒绝非 WAL 文件与软链，先校验 Header 再修复尾部
- **代码位置**：`crates/harness-core/src/session.rs`、`crates/harness-cli/src/main.rs` (`resolve_resume_path`)
- **修复内容**：
  - 新增 `MAX_WAL_FILE_BYTES`（64 MiB）与 `MAX_WAL_LINE_BYTES`（8 MiB）上限；
  - 打开前与加锁后双重校验 `symlink_metadata` / `fstat`（验证 `(st_dev, st_ino)` 一致、拒绝 `S_IFLNK` 软链与非普通文件）；
  - **严格先验证后截断**：先解析第 1 行必须为合法的 `SessionEntryPayload::SessionHeader`（且 `wal_version == SESSION_WAL_VERSION`），并逐行验证所有完整行的 JSON 结构与 SHA-256 哈希链；**唯有第 1 行 Header 与全部完整行 100% 校验通过、仅末尾存在未以 `\n` 结尾的半截行时**，才执行 `set_len(valid_len)` + `sync_all()`。任何非 WAL 文本文件（如 `--resume README.md`）在 Header 校验阶段即原样返回 `Err`，绝不改动原文件一分一毫。
- **回归测试**：
  - `session::tests::non_wal_text_file_is_rejected_without_modification`
  - `session::tests::single_line_file_without_newline_is_rejected_without_modification`
  - `session::tests::symlink_wal_is_rejected_and_target_untouched`
  - `session::tests::wal_repairs_only_incomplete_trailing_line`

### 2. F-02（阻断项）：`ls` / `find` / `grep` 全面接入 `openat2` 目录与文件 FD，消除 TOCTOU
- **代码位置**：`crates/harness-core/src/workspace.rs`、`crates/harness-core/src/tools/{grep,find,ls}.rs`
- **修复内容**：
  - 在 `WorkspaceCap` 新增 `open_dir_no_symlink` 与 `read_dir_no_symlink`，使用 `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` + `O_DIRECTORY | O_NOFOLLOW` 打开目录并基于 `dup(fd)` / `fdopendir` 遍历目录项（含 `d_type`），不再依赖路径预检查后调 `fs::read_dir(abs_path)`；
  - `grep.rs` 彻底移除 `std::fs::read(abs_file)`，所有候选文件与 `.gitignore` / `.ignore` 规则文件统一通过 `workspace.open_existing_no_symlink(&rel_path)` 打开安全 FD 并限长读取。
- **回归测试**：
  - `filesystem_security::test_nav_tools_never_follow_swapped_symlinks_or_escape_the_workspace`

### 3. F-03（阻断项）：`prompt.rs` 工作区文档 `openat2` 读取、文件名转义与不可信数据隔离
- **代码位置**：`crates/harness-core/src/prompt.rs`
- **修复内容**：
  - `read_workspace_doc` 改用 `WorkspaceCap::open_existing_no_symlink` 流式限长读取 `AGENTS.md` / `CLAUDE.md`，消灭 `symlink_metadata` + `File::open` 的 TOCTOU；
  - 顶层目录树遍历改用 `read_dir_no_symlink`，自动跳过 `SensitivePathGuard::is_sensitive` 命中的敏感文件，并对所有文件名执行控制字符、换行与 Unicode 双向控制字符（Bidi override）转义；
  - 显式包裹 `<untrusted_workspace_context>` 标签与安全边界声明，严禁工作区文本覆盖系统安全策略。
- **回归测试**：
  - `prompt::tests::workspace_context_is_fenced_as_untrusted_and_escapes_filenames`
  - `prompt::tests::workspace_context_skips_sensitive_files`

### 4. F-04 & F-05（高优）：URL 控制字符拦截、HTTP `Content-Length` 精确校验与 `HARNESS_INSECURE_TLS` 告警
- **代码位置**：`crates/harness-core/src/provider/chat_completions.rs`
- **修复内容**：
  - `validate_base_url` 拒绝整个 URL（含 path/query）中的任何 ASCII 控制字符（`< 0x20` 或 `0x7f`）及空白字符；
  - 响应头解析严格校验单一合法 `Content-Length` 数值，拒绝重复冲突的 `Content-Length` 与未知 `Transfer-Encoding`，并校验非 chunked 响应体长度精确等于 `Content-Length`（拦截早夭断流 premature EOF 与超长响应）；
  - 当检测到 `HARNESS_INSECURE_TLS=1` 时向 `stderr` 输出显式安全警告。
- **回归测试**：
  - `provider_contract::test_base_url_rejects_control_characters_and_whitespace`
  - `provider_contract::test_http_smuggling_dual_te_cl_rejected_and_chunked_error_body_decoded`

### 5. F-06（高优）：`approval_ui.rs` 终端控制字符转义、TTY 强制校验与 `Ctrl+C` 异步取消
- **代码位置**：`crates/harness-cli/src/approval_ui.rs`、`crates/harness-cli/src/main.rs`
- **修复内容**：
  - 新增 `sanitize_terminal_text`，对审批界面展示的所有路径、命令预览与 diff 预览转义 ESC (`\x1b`)、`\r`、C0/C1 控制字符及 Unicode Bidi 字符；
  - 强制要求 `stdin` 与 `stderr` 均为真实 TTY（`libc::isatty == 1`），否则立即 Fail-Closed 拒绝；
  - 使用 `poll()` 50ms 轮询配合 `CancellationToken`，等待审批期间按 `Ctrl+C` 可在 50ms 内立即中断退出。
- **回归测试**：
  - `approval_ui::tests::escapes_ansi_and_carriage_return_but_keeps_newline_and_tab`
  - `approval_ui::tests::escapes_bidi_override`

### 6. F-07 ~ F-09（中优）：FD 级 `InodeMutationLock`、Session 目录 `0700` 权限、版本 `0.8.1` 与 Git 建仓
- **代码位置**：`crates/harness-core/src/workspace.rs`、`tools/{write,edit}.rs`、`session.rs`、`Cargo.toml`
- **修复内容**：
  - `InodeMutationLock` 新增 `acquire_for_fd(fd, leaf_name)`，直接基于已打开文件或父目录 FD 的 `fstat(st_dev, st_ino)` 生成锁键，`write.rs` 与 `edit.rs` 不再对路径调 `create_dir_all` / `canonicalize`；
  - Session WAL 目录创建时显式设置 Unix `0700` 权限；
  - Workspace 版本统一升级为 `0.8.1`（`harness --version` 输出 `harness 0.8.1`），初始化 `/opt/harness` Git 仓库并打 `v0.8.1` 标签。
- **回归测试**：
  - `filesystem_security::test_inode_lock_keyed_from_open_fd_serializes_same_file`
