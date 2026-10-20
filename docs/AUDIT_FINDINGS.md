# Harness P0 深度审查与演进报告（v0.3 → v0.4 → v0.5 → v0.6 五路专家终审）

## 第零部分：v0.5 → v0.6 五路专家团终审新增决议（V6-01 至 V6-11）

1. **V6-01（P1 协议）**：`AssistantTurn` 新增 `reasoning_blocks: Vec<ReasoningBlock>`（含 `text/signature/opaque_payload`），在工具循环内原样回传，解决 2026 推理模型多轮调用断链与 HTTP 400 问题。
2. **V6-02（P1 系统死锁）**：`bash` 主进程退出且进程组清理后，对剩余 `stdout/stderr` 管道读取施加 **200ms 硬超时**，超时直接 `drop` 父进程读端 FD，根治逃逸孙进程继承管道写端导致的 Tokio EOF 15 分钟死锁。
3. **V6-03（P1 上下文震荡）**：引入 **Working-Set Pinning**（近 8 轮内被修改过的活跃文件最新 `read` 豁免归档折叠）与 **Outline Stub**（折叠源码时保留顶层符号行号索引），结合字节 + Token 双轨水位线与 4 个显式 `CacheBreakpoint`。
4. **V6-04（P1 契约一致）**：全链路统一 `SHA256` 为 **16 位小写 hex 前缀**（消除 `read` 16 位、存根 8 位与 `edit` 64 位比对的自相矛盾），且 `STALE_FILE` 直接返回最新哈希与目标行原文。
5. **V6-05（P1 安全锁）**：文件并发互斥锁改按 `(st_dev, st_ino)` 加锁，审批与执行复用同一 `cap_std::fs::Dir` 句柄并拒绝覆盖 symlink；子进程环境变量改为**敏感黑名单剥离**，保留完整编译工具链变量。
6. **V6-06（P2 吞吐与 UTF-8）**：连续只读工具（`EffectClass::ReadOnly`）采用 `Semaphore(8)` 并发执行并按原下标顺序入账；双流各自维护 3 字节未完成 UTF-8 尾数缓冲与 2 KiB 行缓冲。
7. **V6-07（下一代架构接口）**：预留终态前 `CompletionGate` trait、工具底层 `ExecutionEnv` trait、每轮闭合后的非阻塞 `steer_rx` 动态纠偏口，以及 AHE 轨迹自演进归因字段（`edit_match_tier`, `diag_kind`, `compaction_reread_count`）。

**最新审查日期：2026-10-12**  
本报告记录两轮架构演进：第一部分汇总 **v0.5 针对 v0.4 的深度实战修正（V5-01 至 V5-08）**——重点解决缓存击穿、测试尾部报错丢失、编辑失配死循环与过度防御问题；第二部分完整保留 **v0.3 → v0.4 的协议与安全底座决议（F-01 至 F-14）**。

---

## 第一部分：v0.4 → v0.5 架构级问题发现与深度优化决议

### V5-01（致命）：v0.4 的 ContextProjector 逐轮改写最旧工具输出，导致 Prompt Cache 每轮全量失效

- **v0.4 现状**：第 9 节规定每次请求前若超限，“先修剪最旧的可修剪 `read/grep/find/ls` 输出，用 placeholder 替换”。
- **实证风险**：根据 arXiv:2601.06007（*Don't Break the Cache*）、arXiv:2606.17016（*TokenPilot*）与 arXiv:2607.06906（*The Harness Effect*），OpenAI / Anthropic / DeepSeek 的前缀缓存均要求历史消息前缀 **字节级 100% 恒等**。一旦在第 $t$ 轮把历史第 3 轮的 `ToolResultMessage` 改写为 placeholder，从第 3 轮往后的全部历史消息 KV Cache 立即失效；下一轮再修剪第 4 轮，缓存再次全破。在 30 轮编码任务中，这会导致本可节省 50%–80% 的输入 Token 账单退化为全额重复计费，首字延迟增加 20%+。
- **v0.5 决策**：
  1. 工具输出在入账时即完成 64 KiB 截断定型；
  2. 改为 **高低水位线步进归档（Watermark Step-Down Compaction）**：在请求体未超过高水位 $B_{\text{high}}$（1.5 MiB）前，历史消息严格保持 **Append-Only 零修改**；仅当跨过 $B_{\text{high}}$ 时，一次性将中间区大块只读输出批量折叠回落至低水位 $B_{\text{low}}$（768 KiB），并固化为新基线，使随后 10–25 轮恢复 100% 缓存命中；
  3. 强制确保 `ToolRegistry` 按工具名 ASCII 确定性排序，System Prompt 零动态时间戳。

### V5-02（严重）：v0.4 的 `bash` 捕获满 64 KiB 后直接丢弃后续输出，会吞掉编译器与测试的最终报错

- **v0.4 现状**：第 7.5 节规定“达到 capture cap (64 KiB) 后继续 drain 而不存储”。
- **实战风险**：在运行 `cargo test`、`pytest`、`npm test` 或大型构建时，前 64 KiB 往往全是正常的编译进度或已通过的测试用例，而真正导致失败的 Assertion 报错栈、Panic 信息与失败汇总全部位于输出流的最末尾（Tail）。如果只存前 64 KiB 而把后面 drain 丢弃，模型只会收到 `exit_code = 101` 和一堆 `test ... ok`，完全看不到报错原因！
- **v0.5 决策**：借鉴本机生产环境 Pi 引擎 `OutputAccumulator` 源码，将 `bash` 捕获器改为 **16 KiB Head + 48 KiB Rolling Tail 环形缓冲（严格按 UTF-8 码点与换行符边界对齐）**，并在超出 64 KiB 时将完整流同步落盘至状态目录临时文件（上限 4 MiB），在中间插入省略提示与日志路径，确保开头上下文与末尾报错栈永远同时保留。

### V5-03（高优）：`edit` 缺少行尾空白容错与失配定位诊断，极易引发 5–7 轮盲猜重试

- **v0.4 现状**：第 7.3 节坚持仅做 `CRLF/CR → LF`，拒绝任何行尾空格归一化，且失配时仅返回笼统的 `MATCH_NOT_FOUND`。
- **实证风险**：SWE-agent（arXiv:2405.15793）指出 Failed Edit Recovery 占全部失败的 23.4%；Pi 官方仓库 Issue #8654 实测显示，模型常因原文件某行末尾多一个尾随空格、缩进差 2 个空格、或不小心把 `read` 输出的行号抄进 `old_text` 而连续失败 5–7 轮。
- **v0.5 决策**：
  1. 引入与 Pi `edit.ts` 一致的 **Tier 2 行尾水平空白宽容匹配**：仅在比较时忽略每行末尾的 `[ 	]+$`，不动行首缩进和代码正文，且替换时未修改行保持原字节不变；
  2. 当仍未命中时，执行有界窗口扫描，直接诊断并提示 **行号前缀污染**、**行首缩进差异（附目标行真实文件片段）** 或 **全文最相近候选行区间（起止行号 + 原文预览）**，让模型在 1 轮内修复。

### V5-04（中优）：人为拆分 `core`(4 工具) 与 `coding`(7 工具) Profile 在默认 `ask` 策略下不可用

- **v0.4 现状**：区分 4 工具 `core` 与 7 工具 `coding`。
- **实战风险**：在默认 `--policy ask` 下，只读导航工具 `ls/find/grep` 自动放行，而 `bash` 每次都要用户手动审批。如果使用 `core` Profile，模型连列目录、找函数定义都只能调 `bash`，用户会被弹窗烦死。同时 arXiv:2608.01347v6 实测 7 个紧凑工具 Schema 总共仅约 1,500 tokens，完全没有精简必要。
- **v0.5 决策**：砍掉日常 CLI 的 `core/coding` Profile 概念，统一默认提供 7 个标准工具；由 `--policy read-only` 控制只读裁剪，仅保留内部 `--tool-filter` 供学术消融基准测试使用。

### V5-05（中优）：默认资源预算过紧，容易诱发“裁剪 → 遗忘 → 重读 → 耗尽预算”雪崩

- **v0.4 现状**：`max_model_turns = 20`、`max_tool_calls = 40`、`max_context_request_bytes = 512 KiB`、`max_total_tool_output_bytes = 512 KiB`。
- **实战风险**：arXiv:2601.06007 与真实 SWE 任务统计表明，正常跨文件修改与测试验证常需 25–45 次工具调用；512 KiB 上下文在读取几个中等源码文件加一次测试输出后就会频繁触发裁剪，导致模型丢失刚读过的行号而被迫重读。
- **v0.5 决策**：将默认预算放宽至实战水位（`max_model_turns = 50`、`max_tool_calls = 80`、上下文高水位 `1.5 MiB` / 低水位 `768 KiB`、内存保留原始工具输出 `4 MiB`）。

### V5-06（中优）：`write` 默认禁止覆盖（`overwrite=false`）徒增无效往返

- **v0.4 现状**：`write` 默认只允许新建，覆盖已有文件必须显式传 `overwrite=true`，且要求底层实现无竞态原子 no-clobber 否则报错。
- **实战风险**：`write` 本身已受 Host Policy（`ask` 弹窗或 `--allow-write`）严格保护。模型在重写小型配置文件或测试快照时若忘传 `overwrite=true` 就会白吃一次报错。
- **v0.5 决策**：在 Policy 放行前提下，`write` 默认走同目录随机临时文件 + 原子替换；同时保留可选 `expected_sha256`（防并发陈旧覆盖）与可选 `if_not_exists=true`（显式要求 `create_new` 原子新建）。

### V5-07（中优）：诊断日志写入失败采取 Fail-Closed 误杀正常任务

- **v0.4 现状**：第 12 节一面声明日志不是安全审计，一面规定写日志失败立即停止执行工具并终止 Run。
- **v0.5 决策**：默认改为 **Resilient Degradation**——日志写入报错时向 `stderr` 告警、置位 `log_degraded=true` 并继续完成用户的编码任务；仅在传入 `--strict-log` 时才启用 Fail-Closed。

### V5-08（中优）：循环检测仅识别连续完全相同调用，漏掉 `read ↔ edit` 乒乓震荡

- **v0.4 现状**：F-13 仅对连续完全相同的调用计数。
- **v0.5 决策**：升级 `LoopGuard`，同时覆盖：①连续 3 次相同 `(tool, canonical_args)`；②周期为 2 的 `(A, B) -> (A, B) -> (A, B)` 乒乓循环；③同一文件路径连续 3 次失败的 `edit`。

---

## 第二部分：保留的 v0.3 → v0.4 底座审查决议（F-01 至 F-14）

以下 v0.4 确立的底层协议与安全不变量在 v0.5 中 100% 完整继承：

- **F-01（只读策略双重门控）**：`read-only` 策略下，`edit/write/bash` 既不出现在发给模型的 Tool Schema 中，也在 Host Dispatcher 层被硬性拒绝。
- **F-02（禁止无界内存收集）**：严禁 `Command::output().await` 或无上限 `fs::read_to_string`；文件与子进程均采用流式有界读取。
- **F-03（日志移出 Workspace）**：JSONL 诊断日志默认存入 OS 用户状态目录（权限 `0600`），避免污染目标 Git 仓库。
- **F-04（同目录随机临时文件原子替换）**：禁止固定 `.tmp` 文件名与 `exists()` 竞态。
- **F-05（Provider 协议与 SSE 严格校验）**：固定 `n=1`；校验 `call_id` 非空唯一；处理 `choices=[]` 的 usage 块与任意网络分块；未完整收尾或 `length` 截断绝不执行工具。
- **F-06（敏感凭据路径默认守卫）**：默认拦截 `.env`、私钥、`.aws/.ssh/.kube` 等常见敏感路径，防止无意发送至远端模型。
- **F-07（区分请求体预算与内存保留预算）**：同时约束序列化后的完整 Provider Request Body 与内存中保留的原始工具输出体积。
- **F-08（全链路输入与正则尺寸上限）**：限制参数 JSON 大小、文件尺寸、目录深度、正则编译大小。
- **F-09（同快照多处原子 Edit）**：支持单文件 1–16 处不重叠替换，All-or-None 验证，从后往前应用。
- **F-10（子进程环境脱敏）**：向 `bash` 子进程传递环境时剥离 `HARNESS_API_KEY` 等密钥。
- **F-11（进程组 Supervisor 清理）**：Unix `process_group(0)` + 独立 Supervisor 执行 `SIGTERM → grace → SIGKILL`。
- **F-12（截断响应拒入历史）**：不完整的流式响应或 `finish_reason=length` 绝不执行副作用。
- **F-13（Canonical JSON 调用签名）**：键排序规范化后计算确定性哈希用于循环检测。
- **F-14（精简 Bounded-Efficiency 提示词）**：摒弃冗长说教，记录 `static_prefix_bytes`、`prompt_hash` 与 `tool_schema_hash`。
