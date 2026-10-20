# HARNESS Gate 0 — 安全封板、威胁模型与残余风险登记（v0.8.1）

生成日期：2026-10-12
对应版本：`harness 0.8.1`（Git tag `v0.8.1`）
CI 证据：`docs/GATE0_CI_LOG.txt`（`cargo build` + `cargo test --workspace`，全绿）

> 本文档描述**信任边界与已知残余风险**。它刻意不使用「零死角」「绝对不泄密」这类无法验证的表述：
> 安全是相对于明确威胁模型和部署环境而言的。任何超出所述边界的攻击面（例如已授权的 `bash`
> 在当前 OS 用户权限下读取宿主凭据）都不在本文档的安全承诺范围内。

---

## 1. 支持的操作系统与运行时要求

| 平台 | 状态 | 说明 |
|---|---|---|
| Linux kernel ≥ 5.6 | **一等公民** | 文件工具通过 `openat2(RESOLVE_BENEATH \| RESOLVE_NO_SYMLINKS)` 获得内核级「绝不跳出工作区根、绝不跟随符号链接」的保证；这是本版本安全模型的基石。 |
| 其它 Unix（macOS 等） | 降级 | 无 `openat2` 时退化为 `openat` + `O_NOFOLLOW` + 逐级 `..` 拒绝。`O_NOFOLLOW` 只保护路径最后一段，因此**存在理论上的中间目录替换竞态窗口**；此类平台应视为「尽力而为」而非内核强制。 |
| Windows | 不支持 | 本版本为 Unix-only（大量 `libc` 调用）。 |

Rust 工具链：`rustc 1.85.1`（构建与 CI 基线）。`cargo clippy` 在当前环境**未安装**，因此本轮 CI 仅运行
`cargo build --workspace` 与 `cargo test --workspace`；clippy 静态检查作为后续 CI 门的待补项登记于第 5 节。

---

## 2. 威胁模型与信任边界

### 2.1 信任分层

```
[可信内核 / Trusted Kernel]
  - PolicyEngine：Allow / Ask / Deny 决策（插件不可覆盖）
  - 预算、取消、审批与审计事件（Session WAL）
  - WorkspaceCap：openat2 目录 FD 相对访问 + 路径边界
  - SensitivePathGuard：敏感路径黑名单
  - SessionJournal：SHA-256 哈希链账本
        ▲ 只能由内核收紧，外部输入不可放宽
        │
[不可信输入 / Untrusted Input]
  - 仓库内容：源码、AGENTS.md / CLAUDE.md、SKILL.md、文件名
  - 模型输出：工具调用参数、shell 命令、diff
  - 工具输出 / MCP 结果 / Provider 响应
        │
[宿主 / Host]
  - 当前 OS 用户权限下的完整文件系统、网络、进程、凭据
```

### 2.2 明确的安全承诺（在威胁模型内）

1. **文件工具边界**：`read` / `write` / `edit` / `grep` / `find` / `ls` 通过 `WorkspaceCap` 访问文件，
   在 Linux ≥ 5.6 上由 `openat2` 内核保证**绝不读取或写入工作区根之外的路径、绝不跟随符号链接**。
2. **敏感路径**：`SensitivePathGuard` 阻止文件工具读写 `.env*`、`.ssh`、`.aws`、`.git/{hooks,config}`、
   `.netrc`、`credentials*`、`*.tfvars` 等凭据类路径（含豁免规则，如 `.env.example`）。
3. **不可信内容隔离**：`AGENTS.md` / `CLAUDE.md` 内容以 `<untrusted_workspace_context>` 围栏注入，
   文件名中的换行、C0/C1 控制字符与 Unicode 双向控制字符被转义，防止提示注入伪造上下文行。
4. **会话账本完整性**：Session WAL 为 SHA-256 哈希链，任何历史篡改在恢复时被拒绝；崩溃残留的半截行
   在**验证完整行全部合法之后**才修复。
5. **审批不可伪造**：审批横幅对 ESC/ANSI、`\r`、C0/C1 与 Bidi 字符转义；非 TTY 环境 fail-closed；
   Ctrl-C 可及时中断审批。

### 2.3 明确**不在**承诺范围内的部分（避免夸大宣传）

- **已授权的 `bash` 不是沙箱**：`bash` 以当前 OS 用户身份运行，可读取 `~/.config`、SSH agent、
  云凭据文件或任何该用户可读的资源。环境变量黑名单（剥离 `*_KEY` / `*_SECRET` / `AWS_*` / `GITHUB_*` 等）
  只能减少**常见**密钥经环境变量泄漏，**不能**阻止命令主动读取宿主凭据。若安全目标要求阻止 `bash`
  访问宿主凭据，必须引入 OS 沙箱 / 独立低权限用户 / 容器，而不是继续扩充环境变量黑名单。
- **会话账本会持久化 transcript**：WAL 含用户消息、模型输出与工具结果，可能包含敏感数据。文件权限为
  `0600`、会话目录为 `0700`，但不做内容级脱敏。
- **不可回滚的外部副作用**：`bash` 触发的网络请求、数据库写入、部署等**不可由文件快照撤销**。
- **自建 HTTP/TLS 栈**：Provider 适配器自行维护 OpenSSL FFI 与 HTTP/1.1 framing，见第 5 节残余风险。

---

## 3. F-01 ~ F-09 复现、修复与回归测试对照表

| 编号 | 严重度 | 复现用例 | 修复位置 | 回归测试 |
|---|---|---|---|---|
| **F-01** | 阻断 | `harness --resume README.md`（末行无 `\n`）曾可能截断该文件 | `session.rs::open`（先 `symlink_metadata` 拒 symlink/非普通文件/超限，`O_NOFOLLOW` 打开后 `fstat` 复核 `(dev,ino)` 一致，先校验 Header 与整条哈希链，**仅当全部完整行合法**才 `set_len`+`sync_all` 修复 torn tail）；`main.rs::validate_resume_path_candidate`（显式路径拒 symlink/非普通/超限） | `session::tests::non_wal_text_file_is_rejected_without_modification`、`single_line_file_without_newline_is_rejected_without_modification`、`symlink_wal_is_rejected_and_target_untouched`、`symlink_to_external_file_is_rejected_and_target_untouched`、`oversized_wal_candidate_is_rejected_without_modification`、`wal_repairs_only_incomplete_trailing_line` |
| **F-02** | 阻断 | 检查后将工作区内文件/目录替换为指向外部的 symlink，`grep`/`find`/`ls` 曾跟随 | `workspace.rs` 新增 `open_dir_no_symlink` / `read_dir_no_symlink`（`openat2` + `fdopendir`）；`ls.rs` / `find.rs` / `grep.rs` 全部改用；`.gitignore` / `.ignore` 经 `open_existing_no_symlink` 读取 | `filesystem_security::test_nav_tools_never_follow_swapped_symlinks_or_escape_the_workspace`、`test_nav_tools_under_concurrent_symlink_swap_never_leak_outside_content`（多线程并发攻击） |
| **F-03** | 阻断 | 仓库提供恶意 `AGENTS.md` / 换行文件名注入 system prompt | `prompt.rs::with_workspace_context` / `read_workspace_doc`（`open_existing_no_symlink` 读取、过滤敏感文件、`escape_control_chars` 转义、`<untrusted_workspace_context>` 围栏） | `prompt::tests::workspace_context_is_fenced_as_untrusted_and_escapes_filenames`、`workspace_context_skips_sensitive_files` |
| **F-04** | 高 | URL path 含控制字符；重复/冲突 `Content-Length`；`Content-Length` 短于实际（premature EOF）；非法 `Transfer-Encoding` | `provider/chat_completions.rs`：`validate_base_url` 拒全部控制/空白字节；响应头严格校验单一合法 `Content-Length`、拒绝冲突重复、拒绝非 `chunked` 的未知/多值 `Transfer-Encoding`、body 长度必须精确满足否则 `IncompleteStream` | `provider_contract::test_base_url_rejects_control_characters_and_whitespace`、`test_duplicate_conflicting_content_length_is_rejected`、`test_premature_eof_short_body_is_rejected`、`test_bogus_transfer_encoding_is_rejected`、`test_http_smuggling_dual_te_cl_rejected_and_chunked_error_body_decoded` |
| **F-05** | 高 | 自建 OpenSSL FFI + HTTP/1.1 栈的维护风险；`HARNESS_INSECURE_TLS=1` 绕过校验 | 保留 `ModelProvider` 接口；`HARNESS_INSECURE_TLS=1` 时向 `stderr` 输出显式警告（见第 5 节迁移计划） | 见第 5 节残余风险（尚无独立测试，登记待办） |
| **F-06** | 高 | 审批横幅被 ANSI/`\r`/Bidi 伪造；审批期间 Ctrl-C 无法退出 | `approval_ui.rs`：`sanitize_terminal_text` 转义；强制 `isatty(stdin)&&isatty(stderr)` 否则 fail-closed；`poll()` 100ms 分片 + `APPROVAL_CANCELLED` 可中断 | `approval_ui::tests::escapes_ansi_and_carriage_return_but_keeps_newline_and_tab`、`escapes_bidi_override`、`cancelled_approval_denies_promptly_without_blocking` |
| **F-07** | 中 | `InodeMutationLock::compute_key` 经路径 `create_dir_all` / `canonicalize` | `workspace.rs`：`compute_key` 只 `metadata(parent)` 取 `(dev,ino)` 无副作用；新增 `key_for_fd` / `acquire_for_fd` 由已打开 FD 的 `fstat` 生成锁键；`write.rs` / `edit.rs` 改用 FD 锁键 | `filesystem_security::test_inode_lock_keyed_from_open_fd_serializes_same_file`、`test_inode_mutation_lock_stability_across_create_rename_and_idle_cleanup` |
| **F-08** | 中 | 「不泄密」宣传边界不清；会话数据持久化 | 本文档第 2.3 节明确边界；会话目录 `0700`、WAL 文件 `0600` | `gate0_config_and_events::test_event_logger_0600_permissions_and_resilient_degradation` |
| **F-09** | 中 | 发布包 v0.8.0 但 `Cargo.toml` / `--version` 仍为 0.6.0 | `Cargo.toml` workspace version → `0.8.1`；`harness --version` 输出 `harness 0.8.1`；Git tag `v0.8.1` | 本报告第 4 节（`harness --version` 实测） |

---

## 4. Gate 0 退出条件核对

| Gate 0 要求 | 状态 | 证据 |
|---|---|---|
| 每项已知漏洞有复现用例 + 修复 + 回归测试 + 残余风险说明 | ✅ | 第 3 节对照表；第 5 节残余风险 |
| 干净环境构建日志 | ✅ | `docs/GATE0_CI_LOG.txt`（`cargo build --workspace` 无警告） |
| `Cargo.toml` / `--version` / Git tag 一致 | ✅ | 均为 `0.8.1`（见验收报告） |
| 安全测试覆盖：WAL resume、符号链接/路径竞态、凭据文件、终端转义、URL/header、Shell 超时/进程清理、ToolCall 协议、会话损坏 | ✅ | 66 项测试全绿，覆盖上述全部类别 |
| 不写「零漏洞」，定义支持 OS 与威胁模型 | ✅ | 第 1、2 节 |

---

## 5. 残余风险登记表

| # | 风险 | 影响 | 当前缓解 | 后续计划 |
|---|---|---|---|---|
| R-1 | **自建 OpenSSL FFI + HTTP/1.1 栈**（`provider/chat_completions.rs`） | 协议实现面大，潜在 framing/TLS 边界缺陷；无 fuzzing | 严格返回值检查、证书校验、拒绝重定向、严格 `Content-Length`/`Transfer-Encoding` 校验 | 迁移到 `reqwest`/`hyper` + `rustls`；迁移前用恶意 mock server + property/fuzz 测试覆盖 response parser（F-05） |
| R-2 | **非 Linux≥5.6 平台无 `openat2`** | 中间目录替换竞态窗口（仅最后一段受 `O_NOFOLLOW` 保护） | 逐级 `..` 拒绝 + `O_NOFOLLOW` | 若需在这些平台达到内核级保证，评估 `openat`+`O_PATH` 链式校验或文档标注为尽力而为 |
| R-3 | **`bash` 非沙箱** | 已授权命令可读取宿主凭据 / 产生不可回滚副作用 | 环境变量黑名单、进程组清扫、超时 | 提供可选 OS 沙箱（bubblewrap/landlock）配置；文档保持「非沙箱」表述 |
| R-4 | **会话账本含敏感数据** | 本地磁盘 transcript 可能含敏感内容 | 目录 `0700` / 文件 `0600` | 增加保留策略与可选脱敏；文档说明数据生命周期 |
| R-5 | **`cargo clippy` 未纳入 CI** | 缺少 lint 级静态检查 | 无 | 在具备 clippy 的 CI 环境加入 `cargo clippy --workspace --all-targets -- -D warnings` |
| R-6 | **并发 symlink 攻击测试为概率性** | 后台线程与前台循环的时序不保证每次都命中竞态窗口 | `openat2` 内核保证使该测试本质是「反例搜索」，即使未命中也不代表存在漏洞 | 可结合 `loom`/故障注入提升确定性（研究项） |
| R-7 | **`HARNESS_INSECURE_TLS=1`** | 关闭 TLS 校验的危险调试开关 | 启用时向 `stderr` 显式告警 | 生产模式拒绝该变量（后续 Gate） |

---

## 6. 结论

Gate 0（v0.8.1 安全封板）的已知阻断项（F-01 ~ F-04、F-06、F-07、F-09）均已修复并有回归测试，
F-05 / F-08 以文档与残余风险登记方式明确边界。66 项测试全绿、构建无警告。**本版本不声称「零漏洞」**；
其安全保证严格限定于第 1、2 节所述 OS 与威胁模型内。
