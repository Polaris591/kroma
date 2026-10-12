# Harness P0 v0.6（五路专家团终审 & 下一代架构就绪版）

本包是面向下一代轻量级 Rust Coding Harness 的 **P0 v0.6 规范性工程规格（实施定稿）**。在 v0.5 基础上，融合了 5 路独立专家子代理（ACI 编辑、KV 缓存物理层、2026 推理模型协议、Rust/Unix 系统安全、下一代自演进架构）的 11 项深层修正。它在保留 v0.4 协议完备性与系统安全边界的基础上，结合 2025–2026 最新实证论文与主流生产级 Harness（Pi、Codex CLI、Claude Code、SWE-agent/mini-SWE-agent、Hermes）源码剖析，修正了 v0.4 中破坏 KV 缓存、丢弃测试尾部报错、编辑失败盲目重试、预算过紧导致死循环等架构硬伤。

## 文件清单

- `P0_SPEC_v0.6.md`：唯一规范性主规格，覆盖工具 ACI 语义、协议状态机、Cache-Aligned 上下文管理、下一代抽象预留口与发布阻断项。
- `RESEARCH_AND_DECISIONS.md`：最新实证论文（截至 2026-10）、官方协议文档、生产级开源 Harness 源码剖析及工程转化决策。
- `AUDIT_FINDINGS.md`：涵盖 v0.3 → v0.4 → v0.5 → v0.6（五路专家团终审）的三轮完整审查发现与架构决议。
- `P0_IMPLEMENTATION_TASKS_v0.6.md`：Gate 0 – Gate 6 分阶段实施看板与自动化验收标准。

## v0.5 相对 v0.4 的八大核心升级

1. **Cache-Aligned 高低水位线上下文管理（修复 KV Cache 击穿硬伤）**
   - 废除 v0.4“每轮超限即把最旧工具输出改写为 placeholder”的反模式（该做法会破坏消息前缀字节一致性，导致每轮 Prompt Cache 全部失效）。
   - 引入 **Ingestion-Gate 入账定型 + 高低水位线（$B_{\text{high}} \to B_{\text{low}}$）批量步进归档**：平时严格保持 Append-Only 前缀不可变；仅当总请求体跨过 $B_{\text{high}}$（1.5 MiB）时一次性折叠中间区大块只读输出回落至 $B_{\text{low}}$（768 KiB），并在事件与账单统计中显式区分 `cached_input_tokens` 与 `uncached_input_tokens`。
2. **Shell 输出改为「Head + Rolling Tail」双端缓冲与可选溢出落盘（修复测试尾部报错丢失）**
   - v0.4 规定捕获满 64 KiB 后继续 drain 但丢弃后续内容，会导致 `cargo test` / `pytest` 等长输出命令把报错堆栈丢在 64 KiB 之外。
   - v0.5 采用 **16 KiB Head + 48 KiB Rolling Tail（按 UTF-8 码点与换行符对齐）** 环形缓冲，并在超限时将完整流转存至 Run 状态目录临时日志（上限 4 MiB），确保模型永远能同时看到命令开头与最终报错尾部。
3. **Edit 双层确定性匹配 + 可行动失配诊断（ACI 升级，消除 5–7 轮盲猜重试）**
   - 吸收 Pi `edit.ts` 源码与 Issue #8654 实证经验：第一层做 `CRLF/CR → LF` 精确匹配；第二层做**仅忽略行尾水平空白符（保留原缩进与未修改行原样）**的确定性匹配。
   - 当仍报 `MATCH_NOT_FOUND` 时，自动执行有界行窗口扫描：检测是否误抄了 `read` 行号前缀、是否仅缩进层级不同，或返回相似度最高候选行区间（含真实文件片段），让模型在 **1 轮内恢复**。
4. **取消人为割裂的 `core`(4 工具) / `coding`(7 工具) Profile，统一为 7 原语 + Policy 驱动可见性**
   - 论文实测 7 个紧凑工具 Schema + 精简 System Prompt 总固定前缀仅约 1,400–1,800 tokens（仍比 Claude Code 小 10 倍以上）。
   - 取消会让用户在默认 `ask` 模式下连搜代码都被迫调 `bash` 弹窗审批的 4 工具 `core` Profile；默认提供 `read/edit/write/bash/ls/find/grep` 七原语，通过 `--policy read-only` 自动从 Schema 与 Dispatcher 双重移除写/执行工具，仅保留 `--tool-filter` 供基准消融实验使用。
5. **Write 语义回归工程实用主义**
   - 移除 v0.4“默认禁止覆盖，重写已有文件必先报错”的繁琐限制；在已通过 Host Policy（`ask` 或 `--allow-write`）的前提下，默认走同目录 capability 随机临时文件 + 原子替换，同时支持可选 `expected_sha256`（防陈旧覆盖）与 `if_not_exists=true`（基于 `create_new` / `linkat` 的真 no-clobber）。
6. **升级循环检测器：从“连续完全相同调用”扩展到 Ping-Pong 震荡与同文件连续失败检测**
   - 除检测连续 3 次相同 `(tool, canonical_args_hash)` 外，新增检测周期为 2 的 `read ↔ edit` 乒乓死循环，以及针对同一文件路径连续 3 次失败的 `edit`，触发结构化恢复提示而非盲目耗尽预算。
7. **基于真实编码轨迹放宽默认资源预算**
   - 将 `max_model_turns` 从 20 提升至 **50**、`max_tool_calls` 从 40 提升至 **80**、`max_context_request_bytes` 高水位设为 **1.5 MiB**（低水位 768 KiB）、内存保留原始工具输出上限提升至 **4 MiB**，避免中等规模重构在半途因频繁裁剪重读而雪崩暴毙。
8. **诊断日志写入故障降级告警，不再 Fail-Closed 误杀用户任务**
   - 既然明确诊断日志不是防篡改安全审计，磁盘满或权限抖动时不再强行中断已在进行的代码修改，改为向 `stderr` 发出明确告警并置位 `RunSummary.log_degraded=true`（保留 `--strict-log` 供合规 CI 开启）。

## v0.6 五路专家团新增的 11 项关键加固（面向下一代 Harness）

1. **推理模型思维链与加密签名回传（`ReasoningBlock`）**：`AssistantTurn` 新增 `reasoning_blocks`（兼容 Claude `thinking+signature`、Gemini `thoughtSignature`、DeepSeek `reasoning_content`、OpenAI `reasoning`），在工具循环内原样回传，避免多轮推理断裂或 HTTP 400。
2. **只读工具子批次并发 + 确定性顺序入账**：对连续 `EffectClass::ReadOnly` 调用（`read/grep/find/ls`）使用 `Semaphore(8)` 并发执行，再严格按原 `tool_calls` 数组下标归并入账，提速 4–8 倍且不破 KV Cache。
3. **Working-Set Pinning（热文件归档豁免）与 Outline Stub**：近 8 轮内被 `edit/write` 修改过的活跃文件，其最新 `read` 在步进归档时自动豁免折叠；其余源码折叠时提取顶层函数/结构体行号生成 Outline Stub，消除归档重读震荡。
4. **Token + 字节双轨水位线与显式 `CacheBreakpoint`**：水位线结合上轮 `usage.input_tokens`（75% 窗口）与字节数双轨取严；投影层标注最多 4 个 `CacheBreakpoint`（适配 Anthropic `cache_control`）。
5. **全链路统一 16 位短哈希 CAS 与逐项 `edits[i]` 诊断**：统一 `read`、归档存根与 `edit/write` 的 `expected_sha256` 为 16 位 hex 前缀校验；`STALE_FILE` 直接返回最新哈希与目标行原文；多项 `edits` 失败时按索引 `#i` 逐项报告。
6. **孙进程继承管道 FD 的 200ms Drain 超时解绑**：主子进程退出并完成进程组清理后，对剩余 `stdout/stderr` 读取施加 200ms 硬超时，超时立即 `drop` 父进程读端 FD，根治逃逸孙进程持有管道导致的 Tokio EOF 15 分钟死锁。
7. **双流 3 字节未完成 UTF-8 缓冲与行级刷入**：`stdout` 与 `stderr` 各自维护 3 字节 `incomplete_utf8` 尾数缓冲与行缓冲，防止跨 chunk 切碎中文字符及半行交织乱码。
8. **环境变量黑名单剥离与 `(st_dev, st_ino)` 文件锁**：子进程环境改用黑名单剥离（保留 `PATH/PKG_CONFIG_PATH/SSH_AUTH_SOCK/HTTP_PROXY` 等构建变量）；文件互斥锁基于 `(st_dev, st_ino)` 防止硬/符号链接绕过。
9. **终态前 `CompletionGate` 验证门禁与 `ExecutionEnv` 抽象**：在 `ModelStop -> Completed` 之间预留异步 `CompletionGate`；工具层面向 `ExecutionEnv`（`FsBackend + ProcessSpawner`）解耦，无缝衔接 P1 容器沙箱、Git Worktree 子代理与 Codemode。
10. **Mid-Run Steering 动态纠偏通道**：每轮工具闭合后检查非阻塞 `steer_rx`，支持不中断 Run、不丢 KV Cache 的动态指令注入。
11. **AHE 轨迹自演进埋点**：日志新增 `edit_match_tier`、`diag_kind`、`ttft_ms` 与 `compaction_reread_count`（归档后悔率）。
