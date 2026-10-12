# Harness v0.6.2 底层系统安全加固报告（FIX_REPORT_v0.6.2）

本文档记录针对 `/opt/harness` 4 项核心底层安全加固（工作区无竞态句柄访问、NormalExit 进程组清扫、TLS/HTTP 严格校验与防走私、非交互权限 Schema 隐藏）及对应攻击/回归测试的落地结果。

---

## 一、工作区访问竞态消除（`workspace.rs`, `read.rs`, `edit.rs`, `write.rs`）

1. **目录能力句柄 `root_dir_fd: Arc<File>` 与 `FD_CLOEXEC`**
   - `crates/harness-core/src/workspace.rs`：将 `WorkspaceCap` 的 `_root_dir_fd` 升级为真正生效的 `root_dir_fd: Arc<File>`，通过 `libc::open(root, O_RDONLY | O_DIRECTORY | O_CLOEXEC)` 打开并显式校验/设置 `FD_CLOEXEC` 标志，防止句柄泄漏至子进程。

2. **内核级无竞态打开 `open_existing_no_symlink(&self, rel: &str) -> Result<(PathBuf, File), WorkspaceError>`**
   - Linux 下优先调用 `libc::syscall(libc::SYS_openat2, root_fd, c_rel.as_ptr(), &how, size_of::<OpenHow>())`，设置 `how.flags = (O_RDONLY | O_CLOEXEC) as u64` 与 `how.resolve = RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`（`0x08 | 0x04`），在内核路径解析层原子拦截任何符号链接组件（`ELOOP` -> `WorkspaceError::SymlinkBlocked`）与越界逃逸（`EXDEV` -> `WorkspaceError::PathEscape`）。
   - 当内核返回 `ENOSYS` 或在非 Linux 平台时，自动回退至 `open_existing_with_openat_steps`：基于 `root_dir_fd` 逐级调用 `libc::openat(cur_fd, part, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)` 步进打开，彻底拒绝任何中间目录软链、叶子软链及 `..` 组件。

3. **`read.rs` / `edit.rs` / `write.rs` 消除路径检查与打开/提交之间的 TOCTOU 窗口**
   - `crates/harness-core/src/tools/read.rs` 与 `edit.rs`：直接调用 `workspace.open_existing_no_symlink(&args.path)` 获取已验证的 `File` 句柄执行 `metadata()` 与内容读取，消除 `resolve_existing_path` 后二次 `File::open(abs_path)` / `fs::read(abs_path)` 的 TOCTOU 窗口。
   - `crates/harness-core/src/workspace.rs` 与 `tools/write.rs`：新增 `open_parent_dir_no_symlink`（基于 `root_dir_fd` 使用 `mkdirat` + `openat(..., O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)` 步进打开父目录句柄）与 `atomic_write_at(parent_dir, file_name, bytes, mode, no_clobber)`：
     - 使用 `libc::openat(parent_fd, tmp_name, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, mode)` 在目标父目录 FD 下创建随机临时文件，并由 RAII `TempFileAtGuard` 在异常退出时自动通过 `libc::unlinkat` 清理；
     - `no_clobber = true`（`if_not_exists = true`）时通过 `libc::linkat` + `libc::unlinkat` 原子提交；
     - 常规覆盖写时先通过 `libc::fstatat(parent_fd, target, AT_SYMLINK_NOFOLLOW)` 拒绝软链目标，再通过 `libc::renameat` 完成同目录原子替换。

---

## 二、正常退出后的进程组残留清扫（`tools/bash.rs`）

- `crates/harness-core/src/tools/bash.rs`：当顶层命令正常退出（`StopTrigger::NormalExit`）且 `stdout_task` / `stderr_task` 完成管道排空后，调用 `cleanup_process_group_after_exit(pgid)`：
  - 若 `libc::kill(-pgid, 0) == 0` 探测到同进程组 `-pgid` 仍有残留后台子进程/孙进程，立即向 `-pgid` 发送 `SIGTERM`；
  - 轮询等待最多 `100ms`，若进程组仍存活则发送 `SIGKILL` 强制收割，确保如 `sh -c "sleep 60 & echo ok"` 等正常退出的脚本绝不残留孤儿后台进程。

---

## 三、TLS/HTTP 通信层安全加固（`provider/chat_completions.rs`）

1. **OpenSSL FFI 返回值严格校验与 `X509_V_OK` 证书断言**
   - `crates/harness-core/src/provider/chat_completions.rs`：在 `OpenSslStream::connect` 中严格检查 `SSL_CTX_set_default_verify_paths(ctx) == 1`、`SSL_CTX_load_verify_locations(...) == 1`、`SSL_ctrl(ssl, SSL_CTRL_SET_TLSEXT_HOSTNAME, ...) == 1`（SNI）以及 `SSL_set1_host(ssl, c_host.as_ptr()) == 1`，任何一步失败立即释放上下文并返回 `io::Error`。
   - `SSL_connect` 握手成功后（非 `HARNESS_INSECURE_TLS=1` 时），调用 `SSL_get_verify_result(ssl)` 并断言返回值等于 `X509_V_OK`（`0`），否则立即返回证书校验失败错误。

2. **HTTP Smuggling 防护与 Chunked 错误响应体解码**
   - 解析 HTTP 响应头时同时追踪 `Transfer-Encoding: chunked` 与 `Content-Length`；若两者同时出现，立即作为 `AttemptFailure::Fatal(ProviderError::Transport(...))` 拒绝响应，防止 HTTP Request/Response Smuggling。
   - 当 HTTP 非 2xx 错误响应（含 429/502/503/504 及其他 4xx/5xx）携带 `Transfer-Encoding: chunked` 时，统一使用 `read_chunked_body` 解码错误体并按 `MAX_ERROR_BODY_BYTES` 截断。

---

## 四、Shell 与非交互权限边界加固（`tool/policy.rs`, `tool/registry.rs`, `agent_loop.rs`）

- `crates/harness-core/src/tool/policy.rs`：新增 `PolicyMode::is_effect_enabled(effect_class, is_interactive)` 与 `PolicyEngine::is_effect_enabled(effect_class)`；在 `PolicyEngine::evaluate` 中当 `!self.is_interactive()` 且工具类别未獲 `--allow-write` / `--allow-shell` 授权时直接返回 `PolicyCheckResult::Deny`。
- `crates/harness-core/src/tool/registry.rs` 与 `agent_loop.rs`：新增 `ToolRegistry::visible_definitions_for_engine(&self, policy_engine: &PolicyEngine)` 并在 `AgentRunner::run` 中调用，使得非交互模式下必定被拒绝的工具（如 `Ask` 下的 `bash/edit/write`、`AllowWrite` 下的 `bash`、`AllowShell` 下的 `edit/write`）从模型可见的 Schema 列表中隐藏，同时在 `ToolRuntime::execute_call` 分发层保留 `PolicyDenied` 拦截。

---

## 五、新增攻击与回归测试验证

1. `crates/harness-core/tests/filesystem_security.rs`
   - 新增 `test_open_existing_no_symlink_and_atomic_write_at_parent_fd`：验证 `root_dir_fd` 的 `FD_CLOEXEC` 标志、`open_existing_no_symlink`（`openat2` 与 `openat(O_NOFOLLOW)` 步进回退）对叶子软链和中间目录软链的拦截，以及 `open_parent_dir_no_symlink` + `atomic_write_at`（`linkat`/`unlinkat` 与 `renameat`）的原子提交。
2. `crates/harness-core/tests/shell_supervisor.rs`
   - 在 `test_bash_process_group_cancellation_flood_cap_env_sanitization_and_zero_spawn_on_deny` 中新增用例 5：执行 `sleep 30 >/dev/null 2>&1 & echo $!; exit 0`，验证主进程 `NormalExit(0)` 且后台子进程关闭标准输出不占管道时，`execute_bash` 返回后后台孙进程 PID 已被 `cleanup_process_group_after_exit` 彻底杀灭（`libc::kill(bg_pid, 0) != 0`）。
3. `crates/harness-core/tests/provider_contract.rs`
   - 新增 `test_http_smuggling_dual_te_cl_rejected_and_chunked_error_body_decoded`：验证同时携带 `Transfer-Encoding: chunked` 与 `Content-Length` 的响应被立即拦截，且 `Transfer-Encoding: chunked` 的 HTTP 400 错误体被正确解码。
4. `crates/harness-core/tests/gate0_config_and_events.rs`
   - 在 `test_tool_registry_ascii_order_and_policy_visibility` 中新增非交互模式 `PolicyEngine` 下 `Ask`、`AllowWrite`、`AllowShell`、`FullAutonomous` 四种策略的 `visible_definitions_for_engine` 断言。

执行 `cargo test --workspace` 全套测试 100% 通过且零警告：
- `harness` (bin unit tests): 1 passed
- `context_cache_stability`: 4 passed
- `edit_diagnostics`: 2 passed
- `filesystem_security`: 4 passed
- `gate0_config_and_events`: 3 passed
- `golden_smoke`: 1 passed (all 16 scenarios)
- `provider_contract`: 5 passed
- `shell_supervisor`: 4 passed
- `sse_contract`: 4 passed
- `tool_contracts`: 1 passed
