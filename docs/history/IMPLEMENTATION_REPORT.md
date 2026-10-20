# Next-Gen Coding Harness P0 v0.6 — 实现架构与测试验收报告

**工程路径**：`/opt/harness`  
**工具链版本**：`rustc 1.85.1` / `cargo 1.85.1`  
**规范基准**：`docs/P0_SPEC_v0.6.md` & `docs/P0_IMPLEMENTATION_TASKS_v0.6.md`（Gate 0 – Gate 6 & V6-01 – V6-11）  
**测试状态**：`cargo test` 全量 9 大集成测试集 + CLI 单元测试 **100% 绿灯通过（25 passed, 0 failed, 0 warnings）**

---

## 1. Workspace 工程结构与模块映射

工程采用精简、高内聚的双 Crate Workspace 结构，依赖收敛于 `tokio`、`serde`、`serde_json`、`sha2`、`regex`、`uuid`、`thiserror`、`libc`：

```text
/opt/harness/
├── Cargo.toml
├── Cargo.lock
├── crates/
│   ├── harness-core/
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── types.rs               # RunId, ToolCallId, EventSeq, RunStatus, TerminalReason, ErrorCode, EffectClass, AHE 枚举
│   │   │   ├── messages.rs            # Provider-neutral 消息、ReasoningBlock、TokenUsage、CacheBreakpoint、Arc<str> 共享输出
│   │   │   ├── budget.rs              # v0.5/v0.6 实战校准默认值（50 turns / 80 calls / 1.5MiB-768KiB 水位）与零值/溢出校验
│   │   │   ├── prompt.rs              # 零动态时间戳确定性 PromptBuilder (< 8 KiB) 与 16 位 SHA-256 指纹
│   │   │   ├── events.rs              # OS 状态目录 0600 JSONL 诊断日志、写故障弹性降级 (log_degraded=true) 与 --strict-log
│   │   │   ├── sensitive_paths.rs     # SensitivePathGuard (.env*, 私钥, 云凭据拦截与 .env.example/.sample/.template/.test 豁免)
│   │   │   ├── workspace.rs           # WorkspaceCap 目录能力边界与基于 (st_dev, st_ino) 的 InodeMutationLock
│   │   │   ├── env.rs                 # ExecutionEnv trait (LocalHostEnv 默认实现) 与终态前 CompletionGate trait
│   │   │   ├── loop_guard.rs          # LoopGuard：连续 3 次同参调用、周期 2 Ping-Pong 震荡 3 次、同文件连续 3 次失败 edit 熔断
│   │   │   ├── context_projector.rs   # Cache-Aligned ContextProjector：双轨高低水位、Working-Set Pinning、Outline Stub、4 断点
│   │   │   ├── agent_loop.rs          # ReAct 主循环：Semaphore(8) 只读并发保序入账、副作用串行 Barrier、steer_rx、合成闭合
│   │   │   ├── provider/
│   │   │   │   ├── mod.rs             # ModelProvider trait、Pre-Dispatch 批次预检、FakeProvider
│   │   │   │   ├── chat_completions.rs# OpenAI-compatible 异步 HTTP/1.1 + SSE 客户端、HTTPS/URL 校验、禁重定向、429 Retry-After
│   │   │   │   └── sse.rs             # SseStreamAssembler：任意字节分块、注释行、choices=[] usage 块、[DONE]、断流零副作用
│   │   │   ├── tool/
│   │   │   │   ├── definition.rs      # 模型可见 ToolDefinition 与宿主 ToolMetadata 物理分离
│   │   │   │   ├── policy.rs          # PolicyMode (ReadOnly/Ask/AllowWrite/AllowShell)、非 TTY 自动拒绝、审批 TOCTOU 复核
│   │   │   │   ├── registry.rs        # ToolRegistry：7 工具 ASCII 严格升序排列、Policy 与 --tool-filter 双重过滤
│   │   │   │   ├── runtime.rs         # 统一验证、敏感路径、Policy、TOCTOU、执行与结果封装管线
│   │   │   │   └── output_buffer.rs   # HeadTailBuffer (16KiB Head + 48KiB Rolling Tail + 4MiB 落盘) + 3 字节 UTF-8 尾数缓冲
│   │   │   └── tools/
│   │   │       ├── read.rs            # 流式有界读取、16 位 hex SHA256、2KiB 单行截断、二进制拒绝、翻页提示
│   │   │       ├── edit.rs            # 1-16 项同快照原子替换、Tier 1/2 匹配、三类失配自愈诊断、STALE_FILE 附原文片段
│   │   │       ├── write.rs           # 同目录随机临时文件原子替换、expected_sha256 CAS、if_not_exists=true 原子新建
│   │   │       ├── bash.rs            # Process Group Supervisor (-pgid SIGTERM->grace->SIGKILL)、16MiB 洪泛强杀、200ms Drain 超时
│   │   │       ├── ls.rs              # 确定性只读列目录（不跟随 symlink、跳过敏感路径与忽略目录）
│   │   │       ├── find.rs            # 确定性只读文件搜索（遵守工作区 .gitignore/.ignore、跳过敏感文件）
│   │   │       └── grep.rs            # 2MiB 正则上限防 ReDoS、单文件 2MiB / 总计 32MiB 扫描上限、300 字符行截断
│   │   └── tests/                     # Gate 0 – Gate 6 全套自动化测试集
│   └── harness-cli/
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs                # CLI 参数解析、环境变量凭据读取、Ctrl-C 全局取消信号绑定
│           └── approval_ui.rs         # TTY 交互式 Diff/命令审批框与非 TTY 自动拒绝 Fail-Safe
├── tests -> crates/harness-core/tests
└── fixtures/sample_repo/
```

---

## 2. Gate 0 – Gate 6 核心特性与 V6-01 – V6-11 实现核验

### 2.1 协议闭合与 2026 推理模型支持（Gate 0, Gate 1, Gate 5 / §3, §4）
- **`ReasoningBlock` 原样回传（V6-01）**：`AssistantTurn` 显式维护 `reasoning_blocks: Vec<ReasoningBlock>`（含 `text`、`signature`、`opaque_payload`），兼容 Claude `thinking+signature`、Gemini `thought_signature`、DeepSeek `reasoning_content` 与 OpenAI `reasoning`，在多轮工具循环序列化中完整回传。
- **缓存指标采集**：`TokenUsage` 解析 `prompt_tokens_details.cached_tokens` / `cache_read_input_tokens`，计算 `uncached_input_tokens` 与 `prefix_cache_hit_ratio`。
- **确定性前缀与物理分离**：`ToolRegistry` 将 7 个内置工具按 ASCII 名称严格升序排列（`["bash", "edit", "find", "grep", "ls", "read", "write"]`），模型可见 `ToolDefinition` 与宿主执行元数据 `ToolMetadata` 物理分离；`serde_json` 默认 `BTreeMap` 键序确保跨轮次 `static_prefix_bytes` 零抖动。
- **SSE 任意分块与截断熔断**：`SseStreamAssembler` 支持从 1 字节到任意切分的 chunk 流、`: keepalive` 注释、`choices=[]` 尾随 usage 块与 `data: [DONE]`。当流未收到 `[DONE]` 或 `finish_reason == FinishReason::Length / ContentFilter / Unknown` 时，哪怕已收到的工具参数碰巧是合法 JSON，也在前置批次校验阶段 100% 拒绝执行任何工具。

### 2.2 Agent Loop、只读并发、动态纠偏与验证门禁（Gate 2 / §3.2, §4.1, §8）
- **只读子批次并发与保序入账（V6-06）**：`AgentRunner::execute_tool_batch` 自动识别连续的 `EffectClass::ReadOnly` 工具子批次，先按原下标顺序执行预算扣减与 `LoopGuard` 签名校验，再通过 `Semaphore(8)` 并发执行底层文件 I/O，最后**严格按照原始 `tool_calls` 数组下标顺序**写入会话历史与事件日志；遇到 `edit/write/bash` 时自动作为 Barrier 串行化。
- **1:1 闭合与合成结果**：当批次中途遭遇用户取消、Run 超时或工具调用预算耗尽时，立即中止后续调用，并为剩余未执行的 `call_id` 合成 `NOT_EXECUTED_CANCELLED` 或 `NOT_EXECUTED_BUDGET_EXHAUSTED`，保证 `verify_tool_call_pairing` 永远成立。
- **`LoopGuard` 三重护栏（§8）**：
  1. 连续 3 次相同 `(tool_name, canonical_json_args)` 触发 `REPEATED_CALL_BLOCKED`；
  2. 周期为 2 的 `(sig_A, sig_B) x 3` 乒乓震荡触发 `PING_PONG_CYCLE_BLOCKED`；
  3. 同一文件路径连续 3 次 `edit` 失败触发熔断警告，并在调用 `read` 前拦截第 4 次盲猜 `edit`（`EDIT_FAILURE_STREAK_BLOCKED`）。
- **`CompletionGate`、`ExecutionEnv` 与 `steer_rx`（V6-07）**：
  - 当本轮 Run 修改过文件且模型返回 `stop` 时，先经过异步 `CompletionGate::check`；若被拒绝则追加反馈继续下一轮修复；
  - 每轮 `ToolResultMessage` 闭合后非阻塞检查 `steer_rx.try_recv()` 动态注入纠偏指令；
  - 工具层通过 `ExecutionEnv`（默认 `LocalHostEnv`）解耦。

### 2.3 Cache-Aligned `ContextProjector`（Gate 2 / §7）
- **高低水位线步进归档（V5-01, V6-03）**：默认字节高水位 $B_{\text{high}} = 1.5\text{ MiB}$、低水位 $B_{\text{low}} = 768\text{ KiB}$，结合可选 Token 水位线（$0.75 W_{\text{ctx}} \to 0.45 W_{\text{ctx}}$）。未跨过水位线时 100% Append-Only 透传（连续 15 轮前缀字节级 100% 恒等）。
- **Working-Set Pinning（热文件豁免）**：近 8 轮内被 `edit` 或 `write` 成功修改过的活跃文件，其最新一次有效 `read` 结果与对应 `edit` Diff 在步进归档时自动豁免折叠。
- **Outline Stub 与 4 断点**：中间区其余 > 1 KiB 的旧只读输出折叠为含 16 位 `sha256` 与前 12 个顶层符号行号索引（`fn/struct/enum/impl/trait/class/def`）的 `Outline Stub`；投影输出标注 `ToolsEnd`、`SystemEnd`、`PinnedBaselineEnd`、`TailWindowStart` 四个 `CacheBreakpoint`；当已折叠文件被再次 `read` 时自动累加 `compaction_reread_count`。

### 2.4 七大内置工具、安全边界与 ACI 自愈（Gate 3, Gate 4 / §5, §6）
- **安全边界与 `(st_dev, st_ino)` 锁（V6-05）**：`WorkspaceCap` 拒绝绝对路径、`..` 越界与指向工作区外的符号链接；`InodeMutationLock` 基于 `(st_dev, st_ino)` 加锁，防止硬链接绕过路径锁；`SensitivePathGuard` 默认拦截 `.env`、私钥与云凭据，豁免 `.env.example/.sample/.template/.test`。
- **`edit` 双层匹配与三类失配诊断（V5-03, V6-04）**：
  - 支持 1–16 项同快照 All-or-None 验证，统一支持 16 位短哈希 `expected_sha256` CAS 校验（`STALE_FILE` 直接附带当前最新 16 位哈希与目标区域文件片段）；拦截 `[line truncated]` 字面量；
  - Tier 1 执行 `CRLF/CR -> LF` 精确唯一匹配；Tier 2 执行行尾空白（`[ \t]+$`）宽容匹配，100% 保留未修改行原始字节、UTF-8 BOM 与 CRLF 行尾风格；
  - 失配时按 `edits[i]` 下标逐项输出状态，并自动识别：① `read` 行号前缀污染；② 行首缩进失配（附带目标行附近 10 行真实原文）；③ 全文最相近候选行区间（相似度 ≥ 50% 附上下文预览）或 `MATCH_NOT_UNIQUE` 起始行号列表。
- **`bash` 进程组 Supervisor、双端缓冲与 200ms 管道超时（V5-02, V6-02, V6-06）**：
  - 采用敏感环境变量黑名单剥离策略（剔除 `HARNESS_*`、`OPENAI_*`、`ANTHROPIC_*`、`*_API_KEY`、`*_SECRET`、`*_TOKEN`、`*_PASSWORD`、`*_CREDENTIAL*`，保留完整编译构建环境）；
  - `StreamLineCollector` 维护双流独立 3 字节未完成 UTF-8 尾数缓冲与 2 KiB 行缓冲，汇入 **16 KiB Head + 48 KiB Rolling Tail** 环形缓冲，超出 64 KiB 自动转存至状态目录 `0600` 权限 `tool-<call_id>.log`（上限 4 MiB），超出 16 MiB 触发 `OutputFloodCap` 强杀进程组；
  - Unix `process_group(0)` 配合 `-pgid` 的 `SIGTERM -> grace -> SIGKILL` 清理；主子进程退出后对 `stdout/stderr` 读取任务施加 **200ms 硬超时**，超时立即 `abort()` 读任务并释放管道读端 FD，彻底消除逃逸孙进程继承管道导致的死锁。

---

## 3. 自动化测试集与验收结果

在 `/opt/harness` 下运行 `cargo test`，全部 9 个集成测试套件与 CLI 单元测试均一次通过：

| 测试套件文件 | 覆盖 Gate 与核心不变量 | 测试数 | 结果 |
|---|---|---:|---|
| `tests/gate0_config_and_events.rs` | Gate 0：预算零值/溢出/水位校验、ASCII 升序与 Policy 物理隐藏、`0600` 日志与 `log_degraded` 弹性降级、`--strict-log` Fail-Closed | 3 | PASS |
| `tests/sse_contract.rs` | Gate 5：1 字节分块与 100 次随机切分一致性、`: keepalive`、`choices=[]` 尾随 usage、断流/`length` 零工具执行、重复/空 `call_id` 拒绝 | 3 | PASS |
| `tests/provider_contract.rs` | Gate 1：URL 安全规则（禁 userinfo / 强制远端 HTTPS）、Mock HTTP 429 `Retry-After` 重试、302 重定向拦截、SSE `length` 截断零副作用 | 3 | PASS |
| `tests/context_cache_stability.rs` | Gate 2：连续 15 轮前缀字节 100% 恒等、高低水位一步归档、Working-Set Pinning 热文件豁免、Outline Stub、4 断点、`Semaphore(8)` 并发只读保序、`CompletionGate` 与 `steer_rx`、`LoopGuard` 三类拦截 | 4 | PASS |
| `tests/filesystem_security.rs` | Gate 3：绝对路径/`..`/外部 symlink 拦截、`.env`/`id_rsa` 拦截与 `.env.example` 豁免、硬链接共享 `(st_dev, st_ino)` 互斥锁 | 3 | PASS |
| `tests/tool_contracts.rs` | Gate 3：`read` 16 位哈希/2KiB 长行截断/翻页/二进制拒绝、`write` 原子替换/`expected_sha256`/`if_not_exists=true`、`ls/find/grep` 跳过 symlink 与敏感文件 | 1 | PASS |
| `tests/edit_diagnostics.rs` | Gate 3：Tier 1 & Tier 2 行尾空白匹配（保留未改行、BOM 与 CRLF）、三类失配诊断（行号前缀污染、2 vs 4 空格缩进失配附原文、最相近候选窗口）、All-or-None 回滚、`STALE_FILE` 附片段 | 2 | PASS |
| `tests/shell_supervisor.rs` | Gate 4：跨 chunk 3 字节中文 UTF-8 还原、300KiB 输出 16KiB Head + 48KiB Rolling Tail 保尾与 4MiB 落盘、孙进程占管道 **200ms Drain 超时**不死锁、16MiB 洪泛强杀、进程组清理、环境变量黑名单剥离、未授权零 spawn | 4 | PASS |
| `tests/golden_smoke.rs` | Gate 6：16 个端到端确定性 Golden Smoke 回归场景（只读导航、Edit 缩进失配 1 轮自愈、多处原子替换、中途取消/超预算合成 `NOT_EXECUTED_*` 闭合等） | 1 | PASS |
| `crates/harness-cli/src/main.rs` | CLI 参数契约、Policy 组合推导、拒绝 CLI 明文 `--api-key`、非 TTY 自动 `Deny` Fail-Safe | 1 | PASS |
| **合计** | **全量自动化测试** | **25** | **100% PASS** |
