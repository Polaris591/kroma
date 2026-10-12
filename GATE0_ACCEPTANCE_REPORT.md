# HARNESS Gate 0 验收报告 — v0.8.1 安全封板

- **版本**：`harness 0.8.1`
- **Git**：commit `18d4843`（tag `v0.8.1`，注释 "Harness v0.8.1 Gate 0 Verified"）
- **日期**：2026-10-12
- **CI 证据**：`docs/GATE0_CI_LOG.txt`
- **安全与残余风险**：`docs/GATE0_SECURITY_AND_RESIDUAL_RISKS.md`
- **构建环境**：`rustc 1.85.1`，Linux（`uname -srm` 见 CI 日志）
- **结论**：Gate 0 退出条件全部满足，**66 项测试全绿、构建零警告**。

---

## 1. 本轮新增/修复的残余缺口（在上一轮 F-01~F-09 基础上的深度核查）

上一轮已实现 F-01~F-09 的主体修复。本轮逐项精查后又补齐了以下**尚未覆盖的死角**：

| # | 缺口 | 修复 |
|---|---|---|
| G-1 | 显式 `--resume <path>` 只拒 symlink/非普通文件，**未拒超大文件** | `main.rs::validate_resume_path_candidate` 增加 `MAX_WAL_FILE_BYTES`（64 MiB）上限校验；`session::MAX_WAL_FILE_BYTES` 改为 `pub` 供 CLI 复用 |
| G-2 | `SessionJournal::open` 未比较 `symlink_metadata` 快照与打开句柄的 **`(st_dev, st_ino)`** | 在 `O_NOFOLLOW` 打开并 `fstat` 后，若 `(dev,ino)` 与预检快照不一致则拒绝——彻底封死「预检后路径被换」的窗口 |
| G-3 | F-02 只有**单线程**一次 swap 测试，缺少并发压力测试 | 新增 `test_nav_tools_under_concurrent_symlink_swap_never_leak_outside_content`：后台线程高频 `普通文件 ↔ 外部 symlink` 互换，前台 400 轮循环跑 `grep`/`find`/`ls`/安全读，断言外部秘密**永不**泄漏 |
| G-4 | F-04 缺少专门的 framing 负面测试 | 新增 `test_duplicate_conflicting_content_length_is_rejected`、`test_premature_eof_short_body_is_rejected`、`test_bogus_transfer_encoding_is_rejected` |
| G-5 | F-06 缺少审批取消的非阻塞回归测试 | 新增 `cancelled_approval_denies_promptly_without_blocking`：`APPROVAL_CANCELLED` 置位后 `request_approval` 立即 `Deny` 且 <500ms |
| G-6 | 根目录散落 8 份历史报告 | 归档至 `docs/history/`，设计文档归入 `docs/`，根目录仅保留 `Cargo.toml`/`crates`/`docs`/`fixtures`/`tests` |
| G-7 | 缺 Gate 0 规范文档与 CI 证据 | 新增 `docs/GATE0_SECURITY_AND_RESIDUAL_RISKS.md`、`docs/GATE0_CI_LOG.txt` |

---

## 2. F-01 ~ F-09 逐项复核结论

| 编号 | 复核结论 | 关键证据 |
|---|---|---|
| F-01 | ✅ 已闭环（含 G-1/G-2 补强） | `session.rs::open` 先 `symlink_metadata` 拒 symlink/非普通/超限 → `O_NOFOLLOW` 打开 → `flock` → `fstat` 复核 `(dev,ino)` 一致 → 校验 Header 与整条 SHA-256 链 → **仅当全部完整行合法**才 `set_len`+`sync_all`；6 项回归测试 |
| F-02 | ✅ 已闭环（含 G-3 补强） | `open_dir_no_symlink`/`read_dir_no_symlink`（`openat2`+`fdopendir`）；`ls`/`find`/`grep` 与 `.gitignore` 读取全部改用；全仓 `grep` 确认 `tools/` 下**已无**裸 `fs::read`/`File::open`；单线程 + 并发双测试 |
| F-03 | ✅ 已闭环 | `prompt.rs` 经 `open_existing_no_symlink` 读 `AGENTS.md`/`CLAUDE.md`、过滤敏感文件、`escape_control_chars`（换行/C0/C1/DEL/Bidi）、`<untrusted_workspace_context>` 围栏；2 项测试 |
| F-04 | ✅ 已闭环（含 G-4 补强） | `validate_base_url` 拒全部控制/空白字节；响应头严格 `Content-Length`（拒绝非数字、冲突重复）、拒绝非 `chunked` 的未知/多值 `Transfer-Encoding`、拒绝 TE+CL 并存、body 长度不满足即 `IncompleteStream`；5 项测试 |
| F-05 | ⚠️ 已缓解，登记残余风险 | `HARNESS_INSECURE_TLS=1` 启用时向 stderr 显式告警（`Once` 仅一次）；迁移 `reqwest`+`rustls` 列入 R-1 |
| F-06 | ✅ 已闭环（含 G-5 补强） | `sanitize_terminal_text` 转义；强制 `isatty(stdin)&&isatty(stderr)`；`poll()` 100ms 分片 + `APPROVAL_CANCELLED`；3 项测试 |
| F-07 | ✅ 已闭环 | `compute_key` 无 `create_dir_all`/`canonicalize` 副作用；新增 `key_for_fd`/`acquire_for_fd`（FD `fstat` 锁键）；`write.rs`/`edit.rs` 改用；2 项测试 |
| F-08 | ✅ 已文档化 | 第 2.3 节明确「`bash` 非沙箱 / 账本持久化 / 外部副作用不可回滚」边界；会话目录 `0700`、WAL `0600` |
| F-09 | ✅ 已闭环 | `Cargo.toml` = `0.8.1`；`harness --version` → `harness 0.8.1`；tag `v0.8.1` |

---

## 3. 验证命令与结果

| 命令 | 结果 |
|---|---|
| `cargo build --workspace` | ✅ 成功，零警告 |
| `cargo test --workspace` | ✅ **66 passed / 0 failed**（6 CLI + 25 core-lib + 4 + 2 + 7 + 3 + 1 + 9 + 4 + 4 + 1） |
| `cargo clippy --version` | ⚠️ 未安装（登记为残余风险 R-5） |
| `install -m 755 target/debug/harness /usr/local/bin/harness` | ✅ |
| `/usr/local/bin/harness --version` | ✅ `harness 0.8.1` |
| `git tag -f -a v0.8.1` | ✅ 指向 `18d4843` |

原始日志见 `docs/GATE0_CI_LOG.txt`。

---

## 4. Gate 0 退出条件核对

| 要求 | 状态 |
|---|---|
| 每项已知漏洞有复现用例 + 修复 + 回归测试 + 残余风险说明 | ✅ 第 2 节 + `docs/GATE0_SECURITY_AND_RESIDUAL_RISKS.md` 第 3、5 节 |
| 干净环境构建日志 | ✅ `docs/GATE0_CI_LOG.txt` |
| `Cargo.toml` / `--version` / Git tag 一致 | ✅ 三者均为 `0.8.1` |
| 安全测试覆盖（WAL resume、符号链接/路径竞态、凭据文件、终端转义、URL/header、Shell 超时/进程清理、ToolCall 协议、会话损坏） | ✅ 66 项测试覆盖全部类别 |
| 不写「零漏洞」，定义支持 OS 与威胁模型 | ✅ `docs/GATE0_SECURITY_AND_RESIDUAL_RISKS.md` 第 1、2 节 |

---

## 5. 残余风险（摘要，详见 `docs/GATE0_SECURITY_AND_RESIDUAL_RISKS.md` 第 5 节）

- **R-1**：自建 OpenSSL FFI + HTTP/1.1 栈 → 计划迁移 `reqwest`+`rustls` 并加 fuzzing。
- **R-2**：非 Linux≥5.6 平台无 `openat2`，仅 `O_NOFOLLOW` 降级。
- **R-3**：`bash` 非沙箱，可读宿主凭据、产生不可回滚副作用。
- **R-4**：会话账本含敏感数据（0600/0700，无内容级脱敏）。
- **R-5**：`cargo clippy` 未纳入 CI。
- **R-6**：并发 symlink 攻击测试为概率性（`openat2` 内核保证使其本质为反例搜索）。
- **R-7**：`HARNESS_INSECURE_TLS=1` 危险调试开关。

---

## 6. 下一步

Gate 0 已封板。下一阶段按 `docs/v1_blueprint/IMPLEMENTATION_GATES_AND_EVAL.md` 进入
**Gate 1 — Workspace Revision 与最小 Undo**（基于 Git/shadow repository 的 `SnapshotStore`、
`restore` 三态结果 `Applied`/`Conflict`/`RecoveryRequired`、修改事务状态机与故障注入测试）。
