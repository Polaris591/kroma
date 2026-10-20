# Research & Design Decisions — P0 v0.5

**检索与源码核验基准日：2026-10-12**  
本文严格区分三类证据：**同行评审/公开预印本实证研究**、**官方协议与底层库规范**、**生产级开源 Coding Harness 源码剖析**。所有研究结论均转化为可验证的 P0 v0.5 架构不变量。

---

## 1. 核心实证研究及其对 P0 v0.5 的直接改造

### 1.1 前缀缓存经济学：为什么 ContextProjector 绝不能每轮改写最旧消息
- **Don't Break the Cache: An Evaluation of Prompt Caching for Long-Horizon Agentic Tasks**（2026-01, arXiv:2601.06007v2）  
  https://arxiv.org/abs/2601.06007  
  跨 OpenAI、Anthropic、Google 三家 Provider、500+ 多轮 Agent 会话的实证研究表明：Prompt Caching 可降低 **41%–80% 的 API 成本**，并将首字延迟（TTFT）缩短 **13%–31%**。但各大 Provider 的前缀缓存均依赖**字节级完全一致的前缀（exact prefix match）**。在历史消息前部发生任何单字节突变（例如动态时间戳、JSON 键顺序抖动、或逐轮把旧工具输出替换成占位符），都会导致突变点之后的全部 KV Cache 静默失效。
- **TokenPilot: Cache-Efficient Context Management for LLM Agents**（2026-06, arXiv:2606.17016v1）  
  https://arxiv.org/abs/2606.17016  
  提出 **Prefix Stabilization**（$\phi(m^{(t)}) = \phi(m^{(t+1)}) \implies \mathcal{C}'_{\text{prefix}} \subseteq \mathcal{C}'_{\text{hit}}$）与 **Ingestion-Aware Compaction**：在工具结果入账时即完成确定性截断与规范化，而非在后续每一轮动态回溯篡改已入缓存的前缀。实测将 PinchBench 上的宏观缓存命中率从 38.7% 提升至 **79.2%**，Claw-Eval 从 67.2% 提升至 **83.1%**。
- **The Harness Effect: How Orchestration Design Sets the Token Economics of Enterprise Agentic AI**（2026-10, arXiv:2607.06906）  
  https://arxiv.org/abs/2607.06906  
  在固定模型条件下，具备 **Cache-Shape Discipline（缓存形态纪律）** 与 **Failure-Spend Governance（失败开销治理）** 的 Harness 将单任务混合成本降低 **41%**（0.21 USD → 0.12 USD）、中位耗时降低 **44%**（48s → 27s）、Token 消耗降低 **38%**，且模型自身能力越强，从良好 Harness 获得的杠杆收益越高（Harness Leverage, $r=0.99$）。
- **An Empirical Study of Harness Design for Coding Agents**（2026-09, arXiv:2609.20804v1）  
  https://arxiv.org/abs/2609.20804  
  对比了 5 种上下文管理策略（T0–T4）：证明保护头部（System + 初始任务）与尾部最近窗口原样不动、仅在历史跨过阈值 $B_1$ 时对中间区域（Middle Region）大体积工具观察做批处理省略（Elision），显著优于无节制增长或破坏近期上下文的做法。

**v0.5 落地决策：**
1. **废除 v0.4 逐轮滑动替换最旧输出的策略**。
2. **入账定型（Ingestion-Gate Bounding）**：单次工具结果在写入会话前即完成有界截断，入账后字节固定。
3. **高低水位线步进归档（Watermark Step-Down Compaction）**：在完整请求体低于高水位 $B_{\text{high}}$（默认 1.5 MiB）时，消息历史保持 **100% Append-Only**；一旦突破 $B_{\text{high}}$，一次性将中间区（早于最近保护窗口 256 KiB 的旧回合）中超过 1 KiB 的只读工具输出批量折叠为结构化摘要占位符，使总大小一步回落至低水位 $B_{\text{low}}$（默认 768 KiB）以下。这样每 10–25 轮才触发一次缓存重建，其余轮次享受接近 100% 的前缀缓存命中。
4. **缓存可观测性**：Provider 层与日志强制采集 `cached_input_tokens`、`uncached_input_tokens` 与 `prefix_cache_hit_ratio`。

---

### 1.2 静态前缀、提示词诱发浪费与工具表面设计
- **Prompt-Induced Waste in Coding Agents: Reasoning, Effort, Harness Design, and End-to-End Cost**（2026-08 / v6 2026-09, arXiv:2608.01347）  
  https://arxiv.org/abs/2608.01347  
  对 6 个推理模型、多个生产级 Harness 开展预注册对照实验：
  - **固定前缀实测**：处理最简任务时，**Pi (PI.DEV)** 每次仅传输 **1,147–1,642 tokens** 的固定前缀（System Prompt + 4 工具 Schema）；而 **Claude Code** 每次传输 **15,983–20,330 tokens**（约 6.7 KB System Prompt + 24 个工具 Schema），**在所有分词器上均大 12–15 倍**。
  - **提示词诱发浪费（Prompt-Induced Waste）**：要求模型“列举多种方案并比较”或施加过度“确定性压力（certainty pressure）”的提示语，在 Claude Sonnet 5 上引发严重的自我复查循环（re-verification loops），输出 Token 暴增 **4.13 倍**、无缓存单次成功成本增加 **2.7 倍**，而正确率零提升；相反，明确目标、最小充分改动与停止条件的 `bounded_efficiency` 风格在不损失诊断质量的前提下消除了冗余推理。
- **What Does a Harness Buy? Tokens, Mostly**（2026-10, arXiv:2610.04433）  
  https://arxiv.org/abs/2610.04433  
  对比 Claude Code、mini-SWE-agent 与 OpenCode，指出不同 Harness 之间单次成功任务成本相差可达 3 倍，主要由静态前缀体积和调用轮数驱动；小样本（如 45 题）上的通过率差异往往与重复运行噪声相当。
- **The Scaffold Effect in Coding Agents**（2026-06, arXiv:2607.22585）  
  https://arxiv.org/abs/2607.22585  
  在 Terminal-Bench Pro 子集上，不同 Harness 解决同一任务的 Token 开销差距高达 40 倍。
- **When Does Restricting a Coding Agent to execute_code Help? A Regime × Agent-Design Ablation**（2026-07, arXiv:2607.10569v1）  
  https://arxiv.org/abs/2607.10569  
  交叉消融实验证明：在仓库级代码修改任务（SWE-bench）上，保留原生结构化文件编辑原语（`edit`/`write`）显著优于纯 `bash` 模式（纯 shell 改文件在复杂或失败案例上需支付高昂的 edit-friction 税）。

**v0.5 落地决策：**
1. **静态前缀硬预算**：System Prompt + 7 个标准工具 Schema 的总序列化体积严格控制在 **8 KiB（约 1,600–2,000 tokens）以内**，并在每次 Run 启动时记录 `static_prefix_bytes`。
2. **统一 7 工具表面，废除 `core` / `coding` Profile 割裂**：7 个精简原语（`read, edit, write, bash, ls, find, grep`）总 Schema 开销极小，却能避免模型在默认 `ask` 策略下为了搜索代码被迫调用高风险 `bash`（触发人工审批阻塞）。权限收敛统一由 `--policy read-only` 完成。

---

### 1.3 十一大生产级 Harness 源码解剖与自演进研究
- **Harness Engineering: Anatomy, Architecture, and Evolution of Coding Agents — A Source-Code Study of Eleven Systems**（2026-07, arXiv:2609.00006）  
  https://arxiv.org/abs/2609.00006  
  对 **Claude Code、Codex CLI、Gemini CLI、Mistral Vibe、OpenHands、Aider、Mini-SWE-Agent、Hermes、Pi、OpenCode、OpenClaw** 共约 400 万行 Python/TS/Rust 源码进行解剖，归纳出 7 大标准子系统（Loop、Tools、Context、Safety、Orchestration、Observability、Interface）、29 个设计模式与 18 条建议：所有生产系统均采用自有轻量异步循环与确定性文件检索（ripgrep/fd/glob），无一依赖重型通用 Agent 框架或向量数据库作为基础检索层。
- **Agentic Harness Engineering: Observability-Driven Automatic Evolution of Coding-Agent Harnesses (AHE)**（2026-05, arXiv:2604.25850v4，预印本）  
  https://arxiv.org/abs/2604.25850  
  消融实验报告：跨模型家族可迁移的成功率提升（+5.1 至 +10.1 个百分点，且减少 12% Token）来自 **工具接口（Tools）、中间件护栏（Middleware）与分层可观测诊断（Observability）**，而单纯改写长篇 System Prompt 散文策略无法跨模型迁移。
  **范围限定**：AHE 研究的是 agent 自动修改 harness 组件并在下一轮任务结果验证其自声明预测；它是预印本。它**不是**“每次用户源码 edit 跑括号检查、再猜哪次 edit 导致错误”的实证依据，也不证明自动回滚。Kroma 的写入即检与候选归因是独立工程设计，需要本项目自己的消融证据。
- **SWE-agent: Agent-Computer Interfaces Enable Automated Software Engineering**（NeurIPS 2024 / arXiv:2405.15793）  
  https://arxiv.org/abs/2405.15793  
  轨迹分析显示 **Failed Edit Recovery 占全部失败模式的 23.4%**；良好的 ACI 必须在工具返回与错误诊断中提供精确定位上下文，帮助模型在下一轮立即恢复。

---

## 2. 生产级开源 Harness 源码级关键机制提炼

### 2.1 Pi (`@earendil-works/pi-coding-agent`) 真实源码核验
直接审查本机生产环境中的 Pi 源码（及 GitHub `earendil-works/pi`）：
1. **`OutputAccumulator`（Head/Tail 与流式 UTF-8 边界对齐）**：
   - Pi 的 `bash` 输出累积器并不像 v0.4 那样“收满 64 KiB 后直接丢弃后续输出”，而是维护滚动尾部缓冲（`maxRollingBytes = maxBytes * 2`），切分时按 UTF-8 续字节掩码 `(byte & 0xC0) === 0x80` 与换行符 `\n` 对齐，并在超限时把完整流落盘到临时 `.log` 文件，返回末尾关键报错 + 完整日志路径。这避免了构建/测试长输出把真正的错误栈丢掉。
2. **`edit.ts` 双层归一化匹配与 `withFileMutationQueue`**：
   - 第一层做 `CRLF/CR → LF` 后的精确子串查找；
   - 若未命中，第二层做行尾空白符归一化匹配，并通过 `applyReplacementsPreservingUnchangedLines` **仅替换命中行区间、严格保留文件其余所有行的原始空白与换行**；
   - 通过 `withFileMutationQueue(realpath)` 对同一物理文件维护异步互斥锁。
3. **Pi Issue #8654（Edit 失配重试死循环修复）**：
   - 真实多会话统计发现，当 `edit` 仅返回“Could not find the exact text”时，模型会因缩进差 2 个空格或混入行号而连续盲猜 5–7 次。必须在错误结果中直接指出失配原因（缩进不一致 / 行号前缀污染 / 候选最相近行区间）。

### 2.2 OpenAI Codex CLI (`openai/codex` Rust 实现)
- 严格区分 **Sandbox Policy（技术执行边界）** 与 **Approval Policy（何时向用户请求授权）**。
- 在无 OS 级容器/Landlock/Seatbelt 沙箱时，CLI 必须诚实声明当前处于主机进程权限下，不以审批弹窗冒充沙箱隔离。

### 2.3 DeepSeek Harness 工具注册表隔离
- 模型可见的 `ToolDefinition`（仅含 `name, description, parameters`）与宿主执行元数据（timeout、effect class、policy level、output cap）在类型系统层面物理分离，序列化时按工具名 ASCII 稳定排序，杜绝因 HashMap 遍历顺序变化破坏 Prompt Cache。

---

## 3. 官方协议与 Rust 生态库约束

- **OpenAI Chat Completions / SSE & Anthropic Tool Use**：
  - `finish_reason=tool_calls` 且流式响应完整收尾后才可执行工具；`length` 截断哪怕凑出合法 JSON 也严禁执行。
  - Usage chunk 可能带 `choices=[]`；工具结果必须与 Assistant Turn 的 `tool_call_id` 形成 1:1 闭合配对，中途取消/超预算需合成 `NOT_EXECUTED_*` 占位结果。
- **Tokio `process::Command`**：
  - 严禁使用 `Command::output().await` 无界收集子进程输出；必须通过 `spawn()` + 独立 Supervisor 协程并发读取 `stdout/stderr`，结合 Unix `process_group(0)` 执行 `SIGTERM → grace → SIGKILL`。
- **`cap-std` / `ignore` / `tempfile`**：
  - 所有工作区文件操作基于打开的根目录 `cap_std::fs::Dir` capability 执行相对路径访问，杜绝 `starts_with` 字符串前缀绕过与 symlink 越界。
