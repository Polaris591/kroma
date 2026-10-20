# Rust Harness P0 — 工程规格 v0.6（五路专家终审 & 下一代架构就绪版）

**审查基准日：2026-10-12**  
**状态：规范性实现规格（Normative Engineering Specification）**  
**范围：单机、本地、单 Agent 生产级 Coding Harness；支持 Linux / macOS。**

---

## 0. 核心定位与第一性原则

给定一项编码或仓库分析任务，Harness 以最小静态前缀开销驱动大模型完成「观察 → 修改 → 验证」闭环，按确定性宿主策略执行工具，维护高缓存命中率的上下文视图，并在单一终态下输出可诊断、可复现的轨迹日志。

P0 v0.5 的核心设计铁律：

1. **协议绝对闭合**：任何情况下不产生悬空 `tool_call_id`，任何未完整结束的流或截断输出（`finish_reason=length`）绝不触发副作用。
2. **缓存形态纪律（Cache-Shape Discipline）**：静态前缀字节级稳定（工具列表确定性排序、零动态时间戳）；消息历史默认 Append-Only，仅在跨越上下文高水位线时执行步进式批量归档，严禁逐轮篡改旧消息击穿 KV Cache。
3. **ACI 自愈导向（Self-Healing ACI）**：工具输出与错误提示必须帮模型在 **1 轮内定位并恢复错误**（如 `bash` 保留头部+滚动尾部报错栈、`edit` 失配返回最相近行区间与缩进诊断），消灭盲猜重试循环。
4. **有界流式资源控制**：文件与子进程输出在入账大门处即做流式有界控制，绝不因 `Command::output()` 或 `read_to_string()` 引发 OOM。
5. **诚实的安全边界**：Host Policy（审批与只读门控）+ `cap-std` 目录能力 + 默认敏感路径拦截，明确区分「权限审批」与「OS 沙箱」——P0 的 `bash` 是受监管的本机命令执行器，不冒充容器沙箱。

---

## 1. 范围定义（Scope & Non-Goals）

### 1.1 P0 必做交付物
- Rust Workspace：`harness-core`（核心库）与 `harness-cli`（命令行入口）。
- Provider-neutral 内部消息模型 + OpenAI-compatible Chat Completions 适配器（支持非流式与 SSE 流式，提供 `--no-stream` 回退，完整采集 Prompt Cache 命中指标）。
- 顺序单 Agent ReAct Loop：支持单轮多工具调用批次、单调用失败恢复、循环/乒乓震荡检测、全局取消与终态合成结果。
- 7 个标准内置工具：`read`、`edit`、`write`、`bash`、`ls`、`find`、`grep`。
- 统一 `ToolRegistry`（模型 Schema 与宿主执行元数据物理分离、确定性排序）、`ToolRuntime`、Policy 引擎（`read-only` / `ask` / `allow-write` / `allow-shell`）。
- 基于 `cap-std` 的工作区目录能力封装、默认敏感凭据路径守卫、per-path 异步修改锁。
- 高低水位线 Cache-Aligned `ContextProjector` 与双层资源预算器。
- OS 状态目录下的脱敏 JSONL 诊断事件日志（写故障降级告警，不误杀正常任务）。
- 离线确定性契约测试（Fake Provider + Mock HTTP/SSE Server）、文件系统安全与进程组取消测试、Golden Smoke 回归集。

### 1.2 P0 明确不做
- 多 Agent 编排、树搜索（MCTS）、并行有副作用工具执行。
- MCP 客户端、WASM 插件宿主、动态热加载扩展。
- LLM 驱动的异步摘要生成、向量数据库（Embedding 检索）、跨会话长期记忆。
- Linux Landlock/seccomp/namespaces 或 macOS Seatbelt 级 OS 沙箱、Windows 支持。
- 对故意调用 `setsid()` 脱离进程组的对抗性守护进程做绝对清理保证。

---

## 2. CLI 契约与权限策略（Policy-Driven Visibility）

### 2.1 废除 `core/coding` 双 Profile 割裂，统一由 Policy 驱动工具面
v0.4 将工具拆为 `core`（4 工具）与 `coding`（7 工具），导致默认 `ask` 策略下 `core` 模式连找文件都必须调用需人工审批的 `bash`。v0.5 统一默认启用 7 个基础工具，由 **Policy** 决定模型可见集合与宿主分发权限：

```bash
# 1. 只读分析模式：Schema 与 Dispatcher 仅暴露 read/ls/find/grep，物理隐藏并拒绝 edit/write/bash
harness run --workspace ./repo --task "梳理模块依赖并找出测试入口" --policy read-only

# 2. 交互式默认模式（默认 --policy ask）：只读工具自动放行（敏感文件除外），edit/write/bash 每次弹窗审批
harness run --workspace ./repo --task "修复单元测试失败"

# 3. 授权文件修改模式：read/ls/find/grep/edit/write 自动放行，仅 bash 每次询问
harness run --workspace ./repo --task "重构配置解析模块" --allow-write

# 4. 完全自主编码模式（高风险，仅限受信仓库）：文件读写与本机 Shell 均自动放行
harness run --workspace ./repo --task "修复构建并跑通全部测试" --allow-write --allow-shell
```

- **实验消融开关（仅供基准测试）**：提供隐藏或评测专用参数 `--tool-filter <csv>`（如 `--tool-filter read,edit,write,bash`），用于复现 4 vs 7 工具学术消融实验，不在日常 CLI 中制造概念混淆。
- **非交互终端 Fail-Safe**：当 `stdin` 不是 TTY 且未显式传入对应 `--allow-*` 标志时，任何触发 `Ask` 的调用立即判定为 `Deny`（不挂起等待，不 spawn 子进程）。
- **交互审批粒度**：提供 `[y] Allow once`、`[a] Allow this tool category for current run`、`[n] Deny`。审批界面对 `edit/write` 展示统一 diff 预览与目标路径；对 `bash` 展示完整命令字符串与工作目录。执行前二次校验目标文件 SHA-256，防止审批等待期间发生外部篡改（TOCTOU）。
- **凭据读取**：仅从环境变量读取 `HARNESS_API_KEY`、`HARNESS_BASE_URL`、`HARNESS_MODEL`，禁止通过 CLI 明文参数传 Key，且在启动 `bash` 子进程时从环境变量中剥离这些密钥。

---

## 3. 内部消息模型与单 Run 状态机

### 3.1 Provider-Neutral 会话模型
内部 Canonical Transcript 不直接耦合 OpenAI JSON 结构：
- `UserMessage { content: String }`
- `AssistantTurn { content: Option<String>, reasoning_blocks: Vec<ReasoningBlock>, tool_calls: Vec<ToolCall>, finish_reason: FinishReason, usage: Option<TokenUsage> }`
- `ReasoningBlock { text: Option<String>, signature: Option<String>, opaque_payload: Option<String> }`（兼容 2026 推理模型：Claude `thinking+signature`、Gemini `thoughtSignature`、DeepSeek `reasoning_content`、OpenAI `reasoning`，在当前多轮工具循环内原样回传，避免 HTTP 400 或思维链断裂）
- `ToolCall { call_id: ToolCallId, name: String, raw_arguments: String }`
- `ToolResultMessage { call_id: ToolCallId, tool_name: String, outcome: ToolOutcome, content: Arc<str>, meta: ToolResultMeta }`

`TokenUsage` 显式包含：
- `input_tokens: u64`（总输入 Token）
- `cached_input_tokens: Option<u64>`（命中前缀缓存的 Token，映射自 OpenAI `prompt_tokens_details.cached_tokens` 或兼容字段）
- `output_tokens: u64`
- `reasoning_tokens: Option<u64>`

### 3.2 状态机、验证门禁（CompletionGate）与动态纠偏（Steering）
```text
Created -> Running (含 WaitingForApproval / CompletionGateCheck) -> Completed | Failed | Cancelled | BudgetExhausted
```
- **终态验证门禁（`CompletionGate` trait）**：当模型返回 `finish_reason=stop` 且本轮 Run 存在文件修改时，状态机先经过异步 `CompletionGate::check(&RunContext)`（P0 默认放行，可挂载语法/编译/测试门禁）；若门禁返回 `GateOutcome::Rejected(feedback)`，则不进入 `Completed`，而是追加合成验证反馈消息继续下一轮修复。
- **运行中动态纠偏（Mid-Run Steering）**：在每轮全部 `ToolResultMessage` 1:1 闭合入账后、发起下一轮 Provider 请求前，主循环非阻塞检查 `steer_rx.try_recv()`；若有外部注入的纠偏指令，作为新的 `UserMessage` 追加至尾部，无需中断当前 Run 或丢弃已热好的 KV Cache。
- **执行环境解耦（`ExecutionEnv` trait）**：7 个内置工具通过统一的 `ExecutionEnv`（聚合 `FsBackend` 与 `ProcessSpawner`）访问文件系统与子进程；P0 默认提供基于 `cap-std` 与本机进程组的 `LocalHostEnv`，使得 P1/P2 接入容器沙箱、Git Worktree 并发子代理或 Codemode 脚本引擎时无需重写工具逻辑。
- 每次 Run 具有强类型 `RunId(Uuid)`、单调递增 `EventSeq(u64)`。
- 终态严格互斥且只记录一次，附带明确 `TerminalReason`（如 `ModelStop`, `MaxTurns`, `MaxToolCalls`, `ContextBudgetExceeded`, `RunDeadlineExceeded`, `ProviderProtocolError`, `UserCancelled`）。
- 工具命令返回非零退出码或参数校验不通过属于正常的 `ToolOutcome::Error`（交还模型自我修正），绝不等同于 Harness 自身 `Failed`。

---

## 4. Provider 协议、SSE 流式拼装与网络韧性

### 4.1 协议完整性校验（Pre-Dispatch Batch Validation）
1. 请求固定 `n = 1`；`tools` 数组按工具名称 ASCII 严格升序排列，JSON Schema 键顺序固定，确保跨轮次字节级完全一致。
2. 仅当响应完整结束且 `finish_reason == FinishReason::ToolCalls`（或兼容端点在完整 `stop` 下附带完整 `tool_calls`，需显式配置开关，默认严格要求 `tool_calls`）时，才进入工具批次验证。
3. **截断熔断**：若 `finish_reason == Length`、`ContentFilter`、未知值或流中途断开，**哪怕已收到的 `raw_arguments` 碰巧能解析为合法 JSON，也严禁执行任何工具**，直接记为 `ProviderProtocolError(TruncatedToolCall)`。
4. **Batch Envelope 预检**：单轮最多接受 `max_tool_calls_per_response`（默认 16）个调用；检查所有 `call_id` 非空且在当前批次内唯一、工具名非空、`raw_arguments` 字节数不超过 `max_tool_argument_bytes`（1 MiB）。若 ID 缺失或重复，在产生任何副作用前整批拒绝。
5. **1:1 配对闭合不变量与只读并发执行（Read-Only Parallel Batching）**：
   - 已接受的 `AssistantTurn` 中每个 `ToolCallId`，在发起下一轮 Provider 请求前必须有且仅有一个对应的 `ToolResultMessage`。
   - **只读子批次并发**：当单轮 `tool_calls` 包含连续多个 `EffectClass::ReadOnly` 工具（`read/grep/find/ls`）且均无需交互审批时，`ToolRuntime` 使用 `Semaphore(8)` 并发执行这些只读调用，随后**严格按照原始 `tool_calls` 数组下标顺序**将结果写入会话与事件日志，兼顾高吞吐 I/O 与 100% 字节级缓存确定性；遇到 `edit/write/bash`（`EffectClass::Mutating /SideEffect`）时自动执行 Barrier 串行化。
   - 若批次执行中途发生全局取消、超时或预算耗尽，立即停止后续工具执行，并为剩余未执行的 `call_id` 自动合成 `NOT_EXECUTED_CANCELLED` 或 `NOT_EXECUTED_BUDGET_EXHAUSTED` 结果，保证消息历史永远合法闭合。

### 4.2 SSE 流式解析器契约
- 支持任意字节边界分块（chunk segmentation）、SSE 注释心跳行（`: keepalive`）、多行 `data:` 拼接、`data: [DONE]` 终止符，以及 OpenAI `stream_options.include_usage=true` 下的尾随 `choices=[]` 统计块。
- 按 `tool_calls[].index` 增量聚合 `id`、`function.name` 与 `function.arguments`，单事件上限 256 KiB，整包响应体上限 2 MiB。
- 提供 `--no-stream` CLI 标志回退至普通 JSON 请求，共享同一套后置 Batch 验证逻辑。

### 4.3 HTTP 安全与有界重试
- 禁用 Reqwest 自动重定向（`redirect::Policy::none()`），防止带 `Authorization` 头的请求被 301/302 诱导至第三方地址。
- 远端地址默认强制 HTTPS；仅 `127.0.0.1`、`::1`、`localhost` 允许 HTTP。拒绝含 URL userinfo（`user:pass@host`）的 `base_url`。
- 针对 HTTP 429 / 502 / 503 / 504 及连接建立超时，在总 Run Deadline 内最多执行 2 次指数退避重试（尊重 `Retry-After` 响应头）；一旦 SSE 已吐出任何业务 chunk 或工具已开始执行，连接中断不再自动静默重试。

---

## 5. 工具 ACI 规范（面向高成功率与一轮自愈）

所有工具统一经过流水线：
`JSON 解析 → 参数 Schema 与路径语义校验 → 敏感路径与 Policy 门控 → 循环/震荡检测 → Budget 扣减 → 受控执行 → 有界格式化 → 诊断事件落盘`

### 5.1 `read` — 流式有界读取与清晰行号锚定
- **参数**：`path: String`（工作区相对路径）、`offset: Option<usize>`（1-indexed 起始行，默认 1）、`limit: Option<usize>`（最大读取行数，默认 2000，上限 2000）。
- **安全与边界**：基于 `cap_std::fs::Dir` 打开文件；拒绝绝对路径、`..` 越界、目录或设备文件。流式逐块读取，单次调用最多输出 **64 KiB** 或 **2000 行**（先到为准），单行超过 2 KiB 时截断行尾并标注 `[line truncated]`，最大扫描字节上限 **8 MiB**。非有效 UTF-8 或含 NUL 字节的二进制文件直接返回 `BINARY_OR_INVALID_UTF8` 错误。
- **ACI 输出格式**：
  - 头部元数据：`[File: src/lib.rs | Lines: 1-120 of 340 | SHA256: <hex_prefix_16>]`（当文件大小 ≤ 4 MiB 时计算 SHA-256 并取前 16 位小写 hex 作为全链路统一版本指纹；> 4 MiB 时标注 `SHA256: skipped_large_file`）。
  - 正文每行采用 `{line_num:>5}	{line_content}` 格式。
  - 若发生截断，尾部给出明确翻页指令：`[Truncated at line 200. Call read with offset=201 to continue.]`。

### 5.2 `edit` — 双层确定性批量替换与可行动失配诊断
- **参数**：`path: String`、`edits: Vec<{ old_text: String, new_text: String }>`（1–16 项）、`expected_sha256: Option<String>`。
- **并发与尺寸约束**：同进程内基于目标文件的 `(st_dev, st_ino)`（新建文件基于父目录 `(st_dev, st_ino) + file_name`）维护异步互斥锁（`InodeMutationLock`），防止硬链接/符号链接绕过路径字符串锁；审批与执行复用同一已打开的父目录 `cap_std::fs::Dir` 句柄并校验 `symlink_metadata`（拒绝覆盖符号链接，消除审批 TOCTOU）。目标文件大小上限 **2 MiB**，单次调用总输入上限 **512 KiB**。
- **统一 16 位短哈希 CAS 校验**：`expected_sha256` 支持传入 16 位 hex 前缀（与 `read` 头部及归档存根完全一致）或 64 位完整哈希；若与当前文件哈希前缀不匹配，返回 `STALE_FILE` 并直接在错误消息中附带当前最新 16 位 `SHA256` 及目标区域最新文件片段，省去模型额外 `read` 一轮。
- **截断标记防呆预检**：若检测到任意 `edits[i].old_text` 或 `new_text` 包含 `[line truncated]` 字面量，立即拦截并提示模型缩小单行修改范围。
- **双层确定性匹配（All-or-None 快照验证）**：
  所有 `edits` 均针对**同一份原始文件快照**进行验证，任意一项失配、重复或区间重叠，整次调用不写入任何字节：
  1. **Tier 1（精确匹配）**：仅将文件与 `old_text / new_text` 的 `CRLF / CR` 统一归一化为 `LF` 后做精确子串匹配；要求在全文中唯一出现（`occurrences == 1`）。
  2. **Tier 2（行尾水平空白宽容匹配）**：若 Tier 1 未命中，将文件各行与 `old_text` 各行的**行尾空格/Tab（`[ 	]+$`）**剥离后进行整行序列匹配（**绝不改变行首缩进、行内空白或任何代码字符**）。若唯一命中，仅替换命中的行区间，文件中所有未修改行 100% 保持原始字节（含原有行尾空格、CRLF 风格与 UTF-8 BOM）。
- **可行动失配诊断（消灭盲猜重试的核心 ACI）**：
  当多项 `edits` 验证失败时，按数组下标 `edits[0..k]` 逐项输出结构化状态（如 `edits[0]: OK (lines 12-15); edits[1]: MATCH_NOT_FOUND`）。对未命中的项，基于行哈希倒排索引在 ≤ 256 KiB 文件上执行快速候选窗口扫描（< 5ms），在错误消息中直接附带三类诊断之一：
  1. **行号前缀污染检测**：若检测到 `old_text` 每行开头带有类似 `   42	` 或 `42: ` 的行号前缀，明确提示：`[Hint: old_text appears to include line number prefixes from read output. Strip the leading line numbers and tab before retrying.]`
  2. **行首缩进失配定位**：若将行首空白全部忽略后能在文件中找到唯一匹配区间（如第 45–52 行），直接返回：`[Indentation mismatch at lines 45-52: file uses 4-space indent (or tabs), whereas old_text used 2-space indent. Exact file lines 44-53 are shown below:]` 并附上这 10 行真实原文。
  3. **最相近候选行区间推荐**：按行级相似度找出全文相似度最高（且相似度 ≥ 50%）的 1 个窗口，返回其起止行号与前后 3 行真实原文预览；当报 `MATCH_NOT_UNIQUE`（出现 $N > 1$ 次）时，明确列出所有匹配所在的起始行号列表（如 `Matched at lines [24, 108, 215]. Include 2-3 surrounding unique lines in old_text to disambiguate.`）。
- **原子提交与返回**：全部校验通过后按匹配偏移从后往前应用，恢复原文件 BOM 与 CRLF/LF 行尾风格，在同目录 capability 下通过随机命名临时文件写入、同步权限并原子替换目标文件。返回精简 Unified Diff（上下各 3 行上下文）、修改起止行号及最新 `content_sha256`。

### 5.3 `write` — 受控原子写入
- **参数**：`path: String`、`content: String`（上限 1 MiB）、`expected_sha256: Option<String>`、`if_not_exists: Option<bool>`（默认 `false`）。
- **工程语义**：
  - 在已通过 Host Policy（`ask` 确认或 `--allow-write`）的前提下，默认允许创建新文件或原子覆盖已有普通文件（拒绝目录、FIFO、设备文件及指向工作区外的符号链接）。
  - 若调用方传入 `expected_sha256`，覆盖前必须核对当前文件哈希一致，否则拒绝并返回 `STALE_FILE`。
  - 若传入 `if_not_exists = true`，则强制走原子 `create_new(true)` / `linkat` no-clobber 路径，目标已存在时返回 `FILE_ALREADY_EXISTS`。
  - 所有写入均通过目标父目录 capability 下的随机临时文件完成，保留原文件基础读写执行权限位（mode）。

### 5.4 `bash` — 进程组 Supervisor 与「Head + Rolling Tail」双端捕获
- **参数**：`command: String`、`timeout_sec: Option<u64>`（默认 120s，受总 Run Deadline 约束）。
- **授权与环境隔离**：默认 `Ask`；未获授权时 `spawn` 次数严格为 0。固定 CWD 为工作区根目录，`stdin = Stdio::null()`，使用非 login shell（`/bin/sh -c` 或 `/bin/bash -c`）。子进程环境变量采用**敏感黑名单剥离策略**：剔除 `HARNESS_*`、`OPENAI_*`、`ANTHROPIC_*`、`*_API_KEY`、`*_SECRET`、`*_TOKEN`、`*_PASSWORD`、`*_CREDENTIAL*`，完整保留宿主机的 `PATH, HOME, USER, PKG_CONFIG_PATH, LD_LIBRARY_PATH, SSH_AUTH_SOCK, HTTP_PROXY, HTTPS_PROXY, VIRTUAL_ENV, CARGO_*, RUSTUP_*` 等编译构建环境。
- **双端有界流式捕获（Head + Rolling Tail Ring Buffer）**：
  - 严禁使用 `Command::output().await`。子进程启动后，由独立异步任务并发读取 `stdout` 与 `stderr`（合并为按时间交织的统一输出流或分别维护再合并）。
  - 内存内联捕获上限为 **64 KiB**（最大 2000 行），分为两段：
    - **Head Buffer（前 16 KiB）**：固定保留命令最开头的输出（如环境信息、编译起始命令、首屏报错）；
    - **Rolling Tail Buffer（后 48 KiB 环形窗口）**：持续滚动保留最新输出，切分边界严格对齐 UTF-8 字符起始字节与换行符 `
`。
  - **溢出转存与硬熔断**：当总输出超过 64 KiB 时，自动在当前 Run 的 OS 状态目录下打开有界溢出日志文件 `tool-<call_id>.log`（权限 `0600`，磁盘写入上限 **4 MiB**），并将返回给模型的文本格式化为：
    ```text
    <16 KiB Head 内容>
    
    [... Omitted 1,420 lines (312 KiB). Full log up to 4 MiB saved at <state_dir>/tool-<id>.log ...]
    
    <48 KiB Rolling Tail 内容（含最终编译器/测试失败摘要）>
    ```
  - 若子进程持续吐出垃圾日志超过 `max_process_output_observed_bytes`（默认 **16 MiB**），Supervisor 立即主动终止该进程组并标记 `terminated_by = OutputFloodCap`。
- **流式 UTF-8 尾数保护与行缓冲**：`stdout` 与 `stderr` 读取协程各自维护 3 字节 `incomplete_utf8` 尾数缓冲（防止多字节字符跨 read chunk 被切碎）与 2 KiB 行缓冲（遇 `\n` 或满 2 KiB 刷入合并 HeadTailBuffer），杜绝双流并发半行交织乱码。
- **进程组生命周期、管道死锁防护（200ms Drain Timeout）与取消清理**：
  - Unix 下通过 `Command::process_group(0)` 将子进程放入独立进程组（PGID = child PID）。
  - 超时、输出超 16 MiB 或收到全局 `CancellationToken` 时，Supervisor 先向 `-pgid` 发送 `SIGTERM`，等待 `grace_period`（默认 2 秒），若未退出再向 `-pgid` 发送 `SIGKILL` 并 `wait().await` 回收直接子进程。
  - **防孙进程继承管道 FD 死锁**：一旦直接子进程退出（或被 `SIGKILL` 回收），Supervisor 给 `stdout/stderr` 读取任务施加 **200ms 硬超时（`tokio::time::timeout`）**；若 200ms 后管道仍未 EOF（说明有脱离进程组的后台孙进程仍持有管道写端 FD），立即主动 `drop` 父进程管道读端句柄并正常返回已捕获输出，绝不死等至 15 分钟 Run Deadline。

### 5.5 `ls`、`find`、`grep` — 确定性只读代码导航
- 在所有策略下均对模型可见且免弹窗自动执行（敏感路径自动过滤除外）。
- 基于 `ignore::WalkBuilder` 构建遍历器：**默认不跟随符号链接（`follow_links(false)`）**，禁用宿主机器全局 gitignore（`git_global(false)`、`git_exclude(false)`）以保证跨机器测试 100% 确定性；仅遵守工作区内的 `.gitignore` / `.ignore`，并内置跳过 `.git/`、`target/`、`node_modules/`、`dist/`、`.venv/` 及默认敏感路径。
- 结果按字典序稳定排序，默认上限 200 条（或 32 KiB），`grep` 单行展示上限 300 字符、单文件最大扫描 2 MiB、总扫描上限 32 MiB、正则编译大小上限 2 MiB（防 ReDoS）。

---

## 6. 敏感路径守卫与 Prompt Injection 边界

### 6.1 默认敏感文件拦截（Sensitive Path Guard）
即便位于工作区内，以下路径模式默认标记为 `Sensitive`：
- `.env` 及 `.env.*`（显式豁免 `.env.example`、`.env.sample`、`.env.template`、`.env.test`）
- 私钥与证书：`*.pem`、`*.key`、`*.p12`、`*.pfx`、`id_rsa*`、`id_ed25519*`、`id_ecdsa*`
- 凭据与状态文件：`.aws/**`、`.ssh/**`、`.kube/**`、`.git-credentials`、`.npmrc`、`.pypirc`、`*credentials*.json`、`terraform.tfstate*`

行为规则：
- `ls/find/grep` 遍历时默认静默跳过敏感文件，并在结果摘要中附注 `[Skipped N sensitive file(s)]`。
- 模型直接对敏感路径调用 `read/edit/write` 时：在非交互或 `read-only` 下直接拒绝（返回 `SENSITIVE_PATH_BLOCKED`）；在交互式模式下必须单独弹出高危确认框。

### 6.2 提示注入防御与诚实声明
- 工具输出、代码注释、外部文档均作为不可信数据封装在 `ToolResultMessage` 中，绝不拼接到 System Prompt，也绝不允许通过文本内容修改 Host Policy 或预算配置。
- 文档与 CLI 帮助中明确声明：敏感路径规则仅防御文件工具的常见误传；一旦用户对不可信仓库授予 `--allow-shell`，子进程仍可读取当前 OS 用户有权访问的任何文件。

---

## 7. Cache-Aligned ContextProjector 与内存管理

### 7.1 核心设计：维护前缀字节级不变性（Prefix Stability）
为最大化利用 OpenAI / Anthropic / DeepSeek 的自动前缀缓存（节省 40%–80% 输入成本与 15%–30% 首字延迟），P0 v0.5 严格遵守三条缓存纪律：
1. **静态前缀零抖动**：System Prompt 与 Tool Schemas 在整个 Run 期间字节完全固定（禁止包含当前时间戳、随机 UUID、动态剩余预算数字或非确定性 HashMap 键顺序）。
2. **入账即定型（Ingestion-Gate Bounding）**：工具输出在进入 Canonical Transcript 时已限制在 ≤ 64 KiB；未触发高水位线前，`ContextProjector` 直接原样映射历史消息，保证第 $t$ 轮请求的前 $N$ 条消息与第 $t+1$ 轮请求的前 $N$ 条消息 **100% 字节完全相同**。
3. **双轨高低水位线步进归档（Token + Byte Watermark Step-Down Compaction）**：
   - 设字节高水位 $B_{\text{high}} = 1.5\text{ MiB}$、低水位 $B_{\text{low}} = 768\text{ KiB}$；若配置了模型上下文窗口 $W_{\text{ctx}}$（且上一轮返回了 `usage.input_tokens`），同时启用 Token 高水位 $T_{\text{high}} = 0.75 \times W_{\text{ctx}}$、低水位 $T_{\text{low}} = 0.45 \times W_{\text{ctx}}$，两者取严触发。尾部保护窗口 $W_{\text{tail}} = 256\text{ KiB}$（且至少保留最近 4 个完整回合）。
   - **Working-Set Pinning（活跃热文件豁免）**：凡在最近 8 个回合内被 `edit` 或 `write` 成功修改过的文件路径，其最新一次有效 `read` 结果与对应的 `edit` Diff 在步进归档时**自动豁免折叠**，防止模型在多步修改同一文件时因上下文被归档而陷入“重读震荡”。
   - **Outline Stub（带符号索引的结构化存根）**：当跨过水位线触发一次性 **Step-Down Compaction Pass** 时，中间区域（Middle Region）内未被豁免且大于 1 KiB 的旧只读输出被批量替换为包含 16 位哈希与顶层符号行号索引（轻量正则提取前 12 个 `fn/struct/enum/impl/class/def` 行号，≤ 400B）的 Outline Stub：
     `[Compacted: read(src/parser.rs, lines 1-200, 14.2 KiB, sha256=a1b2c3d4e5f60718, seq=12) | Outline: L12 struct Parser, L45 fn parse_expr, L118 fn consume_token]`
     （注：`bash` 的 exit_code、耗时与最后 15 行错误尾部始终保留）。
   - **显式缓存断点标注（`CacheBreakpoint`）**：投影结果显式标记最多 4 个逻辑断点（`ToolsEnd`, `SystemEnd`, `PinnedBaselineEnd`, `TailWindowStart`），供 Anthropic/兼容层映射为 `cache_control: {"type": "ephemeral"}`。
   - **内存与归档同步释放**：Canonical Transcript 与投影视图通过 `Arc<str>` 共享工具输出缓冲；仅在 Step-Down Compaction 将消息替换为存根时同步释放超 4 MiB 的底层旧大块内存，废除独立于归档之外的静默 FIFO 改写。

### 7.2 内存双层硬上限
- `max_total_tool_output_bytes`（默认 **4 MiB**）：限制内存中 Canonical Transcript 保留的未压缩原始工具输出总量；超限时按 FIFO 释放最旧回合的原始内存大块（磁盘事件日志不受影响）。
- 若完成 Step-Down Compaction 后单次请求体仍超过硬上限 $B_{\text{high}}$（例如最近 4 轮全部塞满了超大输出），则进一步收缩尾部保护窗口至最近 1 轮；若仍超限，则以 `TerminalReason::ContextBudgetExceeded` 优雅终止。

---

## 8. 循环/震荡检测器（Loop & Ping-Pong Guardrail）

模型在编码受阻时极易陷入无效死循环。`ToolRuntime` 在执行前维护最近 12 次调用的滑动窗口，计算规范化调用签名 `sig = SHA256(tool_name + canonical_json(args))`：

1. **连续重复调用拦截（Consecutive Identical Calls）**：同一 `sig` 连续出现达 **3 次** 时，拒绝第 3 次执行，返回 `REPEATED_CALL_BLOCKED` 错误及行动指令（要求模型改变参数或换用其他工具）。
2. **二元乒乓震荡拦截（Period-2 Ping-Pong Cycle）**：若调用序列呈现 `(sig_A, sig_B) -> (sig_A, sig_B) -> (sig_A, sig_B)`（典型场景：`edit` 失败 → `read` 同一区域 → 完全相同的 `edit` 再次失败），在第 3 个周期立即拦截并注入强制反思提示。
3. **同文件连续编辑失败熔断（Per-File Edit Failure Streak）**：对同一文件路径连续发生 **3 次** `edit` 失败且中间无成功写入时，在返回第 3 次失配诊断的同时，强制要求模型先调用 `read` 重新获取目标行附近最新原文，禁止继续盲猜 `edit`。

---

## 9. 预算指标与默认参数表（基于实战轨迹校准）

所有配置值在启动时校验 `> 0` 且无整数溢出：

| 预算维度 | 参数名 | P0 v0.5 默认值 | 相比 v0.4 的调整与依据 |
|---|---|---:|---|
| 模型回合上限 | `max_model_turns` | **50** | v0.4 为 20（过紧，中等 Bug 修复常需 25–40 轮） |
| HTTP 请求尝试上限 | `max_model_request_attempts` | **60** | 含网络瞬断/429 有限重试 |
| 工具调用总数上限 | `max_tool_calls` | **80** | v0.4 为 40；含被拒绝或校验失败的调用 |
| 单轮最大并行工具数 | `max_tool_calls_per_response` | **16** | 超出时整批在副作用前拒绝 |
| 整次 Run 墙钟限时 | `max_run_duration` | **15 分钟** | 含人工审批等待与编译测试耗时 |
| Provider 连接 / 空闲 / 单请求超时 | `connect / idle / request_timeout` | **10s / 45s / 180s** | 兼容推理模型（Reasoning Models）较长的思考间隔 |
| 静态前缀体积预警线 | `max_static_prefix_bytes` | **8 KiB** | System Prompt + 7 Tool Schemas 保持轻量 |
| 上下文高水位 / 低水位 | `context_high / low_watermark_bytes` | **1.5 MiB / 768 KiB** | v0.4 为 512 KiB 逐轮裁剪；现改为高低水位批量归档 |
| 内存原始工具输出保留上限 | `max_total_tool_output_bytes` | **4 MiB** | v0.4 为 512 KiB；放宽以防频繁丢弃近期文件快照 |
| 单次工具内联输出上限 | `max_tool_inline_output_bytes` | **64 KiB (2000 行)** | `bash` 采用 16 KiB Head + 48 KiB Tail |
| 单次 Shell 溢出落盘 / 硬熔断上限 | `bash_spill / observed_hard_cap` | **4 MiB / 16 MiB** | 超 4 MiB 停止写盘，超 16 MiB 杀进程组 |
| 单文件可编辑最大尺寸 | `max_edit_file_bytes` | **2 MiB** | 超大生成文件拒绝内存编辑 |
| 单次写入最大内容 | `max_write_content_bytes` | **1 MiB** | 防止模型一次吐出巨型二进制/文本块 |

---

## 10. 诊断事件日志（可观测性与故障降级）

- **存储位置与权限**：默认写入 OS 用户状态目录（Linux: `$XDG_STATE_HOME/harness/runs/<run_id>.jsonl` 或 `~/.local/state/harness/runs/`；macOS: `~/Library/Application Support/harness/runs/`），绝对不污染被操作的 Workspace 仓库。文件创建权限固定为 `0600`。
- **结构化事件 Schema**：每行 JSON 包含 `schema_version`、`run_id`、`seq`、`timestamp_ms`、`event_type` 与脱敏负载：
  - `RunStarted`（记录 `prompt_hash`, `tool_schema_hash`, `static_prefix_bytes`, `policy`, `budget_config`）
  - `ModelRequestStarted` / `ModelRequestCompleted`（记录 `request_body_bytes`, `message_history_bytes`, `input_tokens`, `cached_input_tokens`, `output_tokens`, `prefix_cache_hit_ratio`, `latency_ms`）
  - `ToolBatchReceived` / `PolicyDecision` / `ToolStarted` / `ToolCompleted`（记录工具名、参数 SHA-256、相对路径、耗时、是否截断、退出码；默认不记录完整文件内容、环境变量或 API Key）
  - `ContextCompacted`（记录触发前字节数、归档后字节数、折叠的消息序号列表）
  - `LoopGuardTriggered`（记录重复调用或乒乓震荡签名）
  - `RunTerminated`（唯一终态事件，含汇总账单、缓存命中率、耗时与 `log_degraded` 标记）
- **写故障弹性降级（Resilient Logging，修正 v0.4 过度 Fail-Closed）**：
  - 诊断日志定性为**可观测性与排障工具**，而非金融级防篡改审计系统。
  - 若运行期间遇到日志磁盘满或写入 I/O 错误，Harness **不中断正在执行的编码任务**，而是立即向终端 `stderr` 输出显眼告警、置位内存状态 `log_degraded = true`，并在最终 CLI 退出摘要中报告未落盘事件数。
  - 提供可选 `--strict-log` 标志：仅在显式开启该标志时，日志写入失败才触发 Fail-Closed 中止 Run。

---

## 11. 测试金字塔与发布阻断项（Release Gates）

### 11.1 三层自动化测试（离线 CI 必过，零 API Key 依赖）
1. **Provider & SSE 协议契约测试（`tests/provider_contract.rs`, `tests/sse_contract.rs`）**：
   - 覆盖随机字节分块（1 字节到任意切分）的 SSE 流、`choices=[]` 尾随 usage 块、缺失 `[DONE]`、连接中途断开、`finish_reason=length` 截断 JSON、重复/缺失 `tool_call_id`、429 `Retry-After` 重试。
2. **ACI 工具与安全边界测试（`tests/tool_contracts.rs`, `tests/filesystem_security.rs`, `tests/shell_supervisor.rs`）**：
   - `read`：UTF-8 边界截断、超长行截断、二进制拒绝、SHA-256 校验值。
   - `edit`：Tier 1 精确匹配、Tier 2 行尾空白宽容匹配且保留未改行原样、行号前缀污染诊断、缩进失配行区间提示、最近似候选行推荐、重叠区间拒绝、`STALE_FILE` 拦截、CRLF/BOM 保持。
   - `bash`：16 KiB Head + 48 KiB Rolling Tail 保尾测试（验证 200 KiB 输出的最后一行报错不丢失）、16 MiB 洪泛杀进程组、取消时子孙进程清理、敏感环境变量剥离、未授权零 spawn。
   - 安全：`..` 与外部 symlink 越界拦截、`.env` / `id_rsa` 默认拦截。
3. **上下文缓存稳定性与循环守卫测试（`tests/context_cache_stability.rs`）**：
   - 验证在未跨越 $B_{\text{high}}$ 的连续 15 轮对话中，每一轮发给 Provider 的前 $N$ 条历史消息与 `tools` JSON 序列化结果 **字节级 100% 恒等**。
   - 验证跨过 $B_{\text{high}}$ 时 Step-Down Compaction 一步回落至 $B_{\text{low}}$，且保持全部 `tool_call_id` 配对完整，并在下一轮恢复 Append-Only 字节恒等。
   - 验证连续 3 次同参调用与 `read ↔ edit` 乒乓震荡在第 3 轮被准确拦截。

### 11.2 真实模型探索性基准评估（非 CI 门禁）
- 在固定模型、固定种子仓库任务集上对比迭代时，必须同时报告：**任务解决率（Pass Rate）**、**总输入 Token vs 缓存命中 Token（`cached_input_tokens`）**、**缓存命中率（Cache Hit Ratio）**、**Edit 首次命中率 vs 一轮恢复率**、**端到端墙钟耗时**与**多次运行方差**。

---

## 12. P0 v0.5 发布核对清单（Release Blockers）

- [ ] 每个已接受的 Assistant `tool_call_id` 在下一轮请求前恰有一个对应 `ToolResultMessage`（含取消/超预算合成结果）。
- [ ] SSE 未正常结束或 `finish_reason=length` 时，工具执行数为 0。
- [ ] `--policy read-only` 同时在模型 Schema 和 Host Dispatcher 两层移除/拒绝 `edit/write/bash`。
- [ ] 未跨越 $B_{\text{high}}$ 时，连续轮次的 System Prompt、Tool Schemas 与已发送历史消息前缀保持字节级 100% 不变；跨过 $B_{\text{high}}$ 时按高低水位线批量归档。
- [ ] `bash` 采用 16 KiB Head + 48 KiB Rolling Tail 流式捕获，长输出末尾报错绝不丢失，16 MiB 触发进程组强杀。
- [ ] `edit` 支持 1–16 处同快照原子替换、Tier 2 行尾空白宽容匹配，并在失配时输出行号污染/缩进差异/最相近候选行诊断。
- [ ] 所有工作区文件访问走 `cap-std` 目录能力，默认拦截 `.env` / 私钥等敏感路径。
- [ ] 循环守卫能拦截连续 3 次相同调用、周期为 2 的乒乓死循环及同文件连续 3 次失败编辑。

---

## 13. 推荐 Rust 项目结构

```text
harness/
├── Cargo.toml
├── Cargo.lock
├── crates/
│   ├── harness-core/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── agent_loop.rs          # ReAct 主循环、批次闭合与终态机
│   │       ├── loop_guard.rs          # 连续重复、Ping-Pong 震荡与同文件失败计数检测
│   │       ├── messages.rs            # Provider-neutral 消息与缓存 Usage 模型
│   │       ├── prompt.rs              # 精简确定性 PromptBuilder (< 8 KiB 静态前缀)
│   │       ├── budget.rs              # 轮次/调用/字节/时间预算器
│   │       ├── context_projector.rs   # Cache-Aligned 高低水位线步进归档器
│   │       ├── events.rs              # 脱敏 JSONL 诊断日志（默认状态目录 + 故障降级）
│   │       ├── workspace.rs           # cap-std 目录能力封装与 per-path 异步修改锁
│   │       ├── sensitive_paths.rs     # 默认敏感凭据路径匹配规则
│   │       ├── provider/
│   │       │   ├── mod.rs
│   │       │   ├── chat_completions.rs
│   │       │   └── sse.rs
│   │       ├── tool/
│   │       │   ├── definition.rs      # 模型可见 Schema 与宿主元数据分离
│   │       │   ├── registry.rs        # ASCII 稳定排序与 Policy 过滤
│   │       │   ├── runtime.rs         # 统一验证、审批、预算与分发管线
│   │       │   ├── policy.rs          # read-only / ask / allow-write / allow-shell
│   │       │   └── output_buffer.rs   # 16 KiB Head + 48 KiB Rolling Tail UTF-8 对齐缓冲
│   │       └── tools/
│   │           ├── read.rs
│   │           ├── edit.rs            # Tier 1/2 匹配 + 行号/缩进/候选区间诊断器
│   │           ├── write.rs
│   │           ├── bash.rs            # Process Group Supervisor
│   │           ├── ls.rs
│   │           ├── find.rs
│   │           └── grep.rs
│   └── harness-cli/
│       └── src/
│           ├── main.rs
│           └── approval_ui.rs         # TTY 交互式 Diff / 命令审批与非 TTY 拒绝
├── tests/
│   ├── provider_contract.rs
│   ├── sse_contract.rs
│   ├── context_cache_stability.rs
│   ├── tool_contracts.rs
│   ├── edit_diagnostics.rs
│   ├── filesystem_security.rs
│   ├── shell_supervisor.rs
│   └── golden_smoke.rs
└── fixtures/
    └── sample_repo/
```
