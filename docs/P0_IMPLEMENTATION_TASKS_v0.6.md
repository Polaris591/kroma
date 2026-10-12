# P0 v0.6 实施看板（Gate-by-Gate Implementation Kanban）

目标：按严格依赖顺序构建可测试、高缓存命中、自愈能力强的 Rust Coding Harness。每个 Gate 必须通过对应的离线确定性测试后方可进入下一阶段。

---

## Gate 0 — 核心类型、配置校验与边界冻结

- [ ] 创建 Rust Workspace：`crates/harness-core` 与 `crates/harness-cli`，固定 `Cargo.lock`。
- [ ] 定义强类型 `RunId`、`ToolCallId`、`EventSeq`、`RunStatus`、`TerminalReason`、`ErrorCode`。
- [ ] 定义 Provider-neutral 消息结构（含 `ReasoningBlock { text, signature, opaque_payload }`、`Arc<str>` 共享输出与 `CacheBreakpoint`）及含 `cached_input_tokens` 的 `TokenUsage`。
- [ ] 定义 `ExecutionEnv` trait（解耦本机文件系统与进程执行）与终态前 `CompletionGate` trait。
- [ ] 实现 `Policy`（`ReadOnly`, `Ask`, `AllowWrite`, `AllowShell`）与可选消融开关 `--tool-filter`。
- [ ] 实现 `BudgetConfig`（采用 v0.5 校准默认值：50 turns / 80 tool calls / 1.5 MiB 高水位 / 768 KiB 低水位 / 4 MiB 内存保留），校验所有参数 `> 0` 且无溢出。
- [ ] 实现 OS 状态目录解析与 `0600` 权限 JSONL 事件写入器（默认写故障降级告警 + `--strict-log` 可选 Fail-Closed）。

**Gate 0 验收**：配置单元测试覆盖 `0`、极大值、非法组合；日志降级测试在模拟只读状态目录时告警但不 panic。

---

## Gate 1 — 非流式 Provider、确定性序列化与协议闭合

- [ ] 实现 OpenAI-compatible Chat Completions 非流式客户端（禁用自动 redirect、强制远端 HTTPS、拒绝 userinfo URL、敏感 Header 掩码）。
- [ ] 确保发出的 `tools` 列表按名称 ASCII 严格升序排列，Schema JSON 键顺序完全确定，确保跨轮次 `static_prefix_bytes` 零抖动。
- [ ] 实现工具批次前置校验（`n=1`、`finish_reason=tool_calls`、`length` 截断熔断、`call_id` 非空且唯一、单批 ≤ 16 个调用、参数 ≤ 1 MiB）。
- [ ] 实现 HTTP 429 / 502 / 503 / 504 有界重试（尊重 `Retry-After`，计入 `request_attempts`）。
- [ ] 解析并提取 Provider 返回的 `prompt_tokens_details.cached_tokens` / `cache_read_input_tokens`。

**Gate 1 验收**：本地 Mock HTTP Server 离线跑通正常文本、多工具调用、`length` 截断拒绝、重复 `call_id` 整批拒绝、429 重试与缓存指标解析。

---

## Gate 2 — Agent Loop、循环/乒乓守卫与 Cache-Aligned ContextProjector

- [ ] 实现 ReAct 主循环：对连续 `EffectClass::ReadOnly` 工具子批次使用 `Semaphore(8)` 并发执行并严格按原下标顺序入账；遇有副作用工具时串行化；每轮闭合后支持非阻塞 `steer_rx` 动态指令注入与 `CompletionGate` 终态校验。
- [ ] 实现 `LoopGuard`：
  - 连续 3 次相同 `(tool_name, canonical_args_sha256)` 拦截；
  - 周期为 2 的 `(sig_A, sig_B) x 3` 乒乓震荡拦截；
  - 同文件路径连续 3 次失败 `edit` 熔断提示。
- [ ] 实现 **Cache-Aligned `ContextProjector`**：
  - 未超 $B_{\text{high}}$（1.5 MiB）时 100% Append-Only 透传；
  - 跨过字节/Token 双轨高水位时执行一次性 Step-Down Compaction，实施 **Working-Set Pinning**（近 8 轮修改过的活跃文件最新 `read` 豁免折叠），将中间区其余大体积只读输出折叠为含 16 位哈希与顶层符号行号的 **Outline Stub**，一步降至低水位以下并标注 `CacheBreakpoint`；
  - 维护 Canonical Transcript 原始输出 4 MiB 内存上限。
- [ ] 实现精简 `PromptBuilder`（静态前缀 < 8 KiB，零动态时间戳，记录 `prompt_hash` 与 `tool_schema_hash`）。

**Gate 2 验收**：连续 15 轮未超水位请求的前缀字节 100% 恒等测试通过；超水位一步归档后恢复 Append-Only 测试通过；乒乓死循环拦截测试通过。

---

## Gate 3 — 基于 `cap-std` 的文件与搜索工具（含自愈 ACI 诊断）

- [ ] 封装 `WorkspaceCap`（基于 `cap_std::fs::Dir`）与 `PerPathMutationLock`，杜绝绝对路径、`..` 与外部 symlink 越界。
- [ ] 实现 `SensitivePathGuard`：默认拦截 `.env`（豁免 example/sample/template）、私钥、云凭据等路径。
- [ ] 实现 `read`：流式有界读取（64 KiB / 2000 行，单行 2 KiB 截断，≤ 4 MiB 文件返回 `content_sha256`，二进制拒绝，清晰行号格式与翻页提示）。
- [ ] 实现 `edit`（1–16 处同快照原子替换）：
  - Tier 1 精确匹配（CRLF/CR 归一化后唯一匹配）；
  - Tier 2 行尾水平空白宽容匹配（保留未修改行原始字节、BOM 与行尾风格）；
  - **失配自愈诊断器**：自动检测 `read` 行号前缀污染、行首缩进失配（附目标行原文）与全文最相近候选行区间推荐；
  - 同目录随机临时文件原子替换 + `expected_sha256` 校验。
- [ ] 实现 `write`：同目录随机临时文件 + 原子替换，支持可选 `expected_sha256` 与 `if_not_exists=true`（原子 `create_new`）。
- [ ] 实现 `ls`、`find`、`grep`：基于 `ignore::WalkBuilder`（不跟随 symlink、禁用宿主全局 gitignore、跳过敏感文件、确定性排序、限制正则与扫描体积）。

**Gate 3 验收**：路径越界与外部 symlink 拒绝；Tier 2 行尾空格匹配保留未改行；失配诊断准确识别行号污染与 2 vs 4 空格缩进差；并发/过期 `expected_sha256` 返回 `STALE_FILE`。

---

## Gate 4 — Shell Supervisor 与「16 KiB Head + 48 KiB Rolling Tail」缓冲

- [ ] 实现 `HeadTailBuffer` 与双流 3 字节未完成 UTF-8 尾数缓冲 / 2 KiB 行缓冲：前 16 KiB 固定保留 + 后 48 KiB 环形滚动，超出 64 KiB 自动转存至状态目录 `tool-<id>.log`（上限 4 MiB）。
- [ ] 实现子进程退出后的 **200ms 管道 Drain 硬超时**（超时主动 `drop` 读端 FD，防逃逸孙进程继承管道导致 Tokio EOF 死锁）与敏感环境变量黑名单剥离。
- [ ] 实现 `BashSupervisor`：
  - 未授权时 `spawn` 次数为 0；
  - 剥离 API Key / Secret 环境变量，固定工作区 CWD，`stdin = Null`；
  - Unix `process_group(0)`，并发流式读取 `stdout/stderr`；
  - 输出达 16 MiB 硬上限、超时或收到取消信号时，对 `-pgid` 执行 `SIGTERM → 2s grace → SIGKILL` 并回收子进程。

**Gate 4 验收**：模拟产生 300 KiB 输出且最后一行打印 `FAILED: assertion X == Y` 的测试脚本，验证返回结果完整包含开头上下文、中间省略提示、临时日志路径以及最后一行报错；测试 `sleep` 子孙进程树在取消后无残留。

---

## Gate 5 — SSE 流式引擎与 CLI 交互体验

- [ ] 实现 SSE 解析器：处理任意字节切分、注释行、`[DONE]`、`choices=[]` 尾随 usage 块、按 `index` 增量拼装多个 `tool_calls`。
- [ ] 确保流式中断或 `finish_reason=length` 时零工具执行。
- [ ] 实现 TTY 交互审批 UI（展示彩色/对齐 Unified Diff 与待执行命令）与非 TTY 自动拒绝逻辑。
- [ ] 绑定 `Ctrl-C` 信号至全局 `CancellationToken`，干净中断在途 HTTP/SSE 请求或正在运行的 Shell 进程组，并写出唯一 `Cancelled` 终态事件。

**Gate 5 验收**：对同一 SSE 样本做 100 次随机分块 fuzz 测试，输出事件流完全一致；断流样本零副作用。

---

## Gate 6 — 端到端回归集与发布门禁（Release Gate）

- [ ] 编写 15–20 个确定性 Golden Smoke 任务（涵盖只读导航、多处代码修改、Edit 缩进失配后一轮自愈、测试失败根据尾部日志修复、超水位上下文归档、权限拒绝、用户中途取消）。
- [ ] 运行 `cargo fmt --check`、`cargo clippy -- -D warnings`、全量契约与安全测试集。
- [ ] 核对 `P0_SPEC_v0.5.md` 第 12 节全部发布阻断项清零。
