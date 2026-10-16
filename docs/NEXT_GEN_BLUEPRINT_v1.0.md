# HARNESS v1.0 — 下一代编码智能体基座（Next-Gen Agentic Harness Substrate）完整架构蓝图

> **定位**：融合 2026 多套主流系统（Claude Code、Codex-rs、DeepSeek DSH、Pi、GrokBuild、JCode、ZCode、OpenCode）的工程精华，做成纯 Rust 极速内核；并**按可核验证据分项采纳**前沿思想（平台化解剖、可逆作用域、熵审计、AHE 等）。这些工作多为预印本或工程报告，本蓝图**不宣称“首个在工业界落地”，也不把它们称为“顶会理论”**；每一项都必须先在本项目自己的任务集上验证，再决定是否默认启用。

---

## 一、为什么现有的 8 大 Harness 还不够「下一代」？（四大代差突破）

我们对 2026 年 7 月 11 大系统 400 万行源码（`arXiv:2609.00006`）和最新论文进行交叉剖析后，提炼出当前第一代工具普遍存在的 **4 个天花板**，以及 Harness v1.0 的 **4 大代差级解法**：

| 维度 | 第一代 Harness（Claude Code / Codex / OpenCode 等现状） | 下一代 Harness v1.0（我们的代差设计） | 理论与工程来源 |
| :--- | :--- | :--- | :--- |
| **1. 状态模型** | **聊天记录与磁盘文件割裂**：`--fork` 只退聊天不退文件，或者像 OpenCode 只做单线 `shadow-git`，无法多分支并发试修 | **Merkle 时空同构（World-State Algebra，Kroma 自己的设计）**：每个 WAL 节点绑定工作区内容寻址 `Merkle Tree Hash`，支持「记忆+代码」同步穿越与双分支推测执行（赛马修 Bug）。**该世界状态不变量由 Kroma 自己的回归测试证明，Cordis 并不证明它** | Cordis 只提供可逆注册/作用域组合的工程先例 |
| **2. 纠错机制** | **盲目试错 + 事后撞墙**：改完等最后跑测试挂了再盲猜，连撞 3~5 次才触发死循环熔断，白烧数万 Token | **候选归因 + 可选隔离反事实探针**：结构检查与排序后的候选归因是 Kroma 的独立工程设计；反事实探针默认关闭、需审批、有预算，且**启发式排序永不自动回滚代码**。AHE 研究的是“agent 自动修改 harness 组件并在下一轮任务验证自声明预测”，**不是“源码 edit 语法检查”的实证依据** | AHE（预印本，`arXiv:2604.25850`）提供动机，非证明 |
| **3. 上下文工程** | **死板按轮次（LRU）裁剪**：3 轮前误读的废文件被豁免留着，12 轮前读的核心接口定义反而被折叠；死胡同排查占满窗口 | **因果引用价值 + 死胡同负样本压缩**：按后续 Assistant 的真实引用给文件更高的留存价值；失败尝试折叠为 1 句「负样本教训」并保留证据/缓存/配对不变量。**P3 是更激进的压缩档，不等于 ACE playbook**；ACE 目前是预印本（`arXiv:2510.04618`），不是 ICLR 正式接收论文 | 熵审计/ACE 提供动机，收益需本项目消融测量 |
| **4. 扩展与多端** | **静态插件或重进程堆叠**：插件挂上后无法按子目录/子任务干净卸载 | **可逆作用域微内核 + 按需工具 schema**：工具/Hook/Skill 皆为带生命周期的 `Scoped Effect`，可插拔可卸载；工具 schema 按需发现。**“每增 1 路并发仅 +2MB”是待测目标，不是 Cordis 或任何论文已经证明的结论** | Cordis 提供可逆注册工程先例 |

---

## 二、分步制作路线图（从 v0.8.1 到 v1.0，共 5 次迭代，每次均可独立编译验收）

为了保证每一步都稳如磐石、绝不写出半成品，我们将整个建设拆解为 **5 次清晰的迭代（v0.8.1 → v0.8.5 → v0.9.0 → v0.9.5 → v1.0.0）**：

### 🔹 第 0 步：v0.8.1 —— 零死角安全封板与 Git 建仓（当前正在后台收尾）
* **制作内容**：
  1. 修复 `--resume` 先校验 WAL Header 与 `symlink_metadata` 再允许修复半截尾部，彻底杜绝误伤非 WAL 文件（F-01）；
  2. 将 `grep / ls / find` 全部切换至 `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` 目录与文件句柄，消灭最后的 TOCTOU 窗口（F-02）；
  3. `prompt.rs` 通过 `openat2` 读取 `AGENTS.md`/`CLAUDE.md`，转义文件名控制字符，包裹不可信数据隔离边界（F-03）；
  4. 严格校验 URL 控制字符、HTTP `Content-Length` 早夭断流与冲突头、审批 UI 终端转义与 `Ctrl+C` 异步取消、基于 FD 的 `InodeMutationLock`（F-04 ~ F-09）；
  5. 初始化 `/opt/harness` Git 仓库并打下 `v0.8.1` 基准 Tag。
* **阶段交付物**：55+ 项契约与防攻击测试全绿的 `harness v0.8.1` 安全基线。

---

### 🔹 第 1 步：v0.8.5 —— 可逆作用域插件微内核（Reversible Plugin & Hook Microkernel）
> **核心目标**：把硬编码内核彻底改造为「一切皆插件、按作用域动态挂载与回滚」的微内核（吸收 DeepSeek `dsh` Cordis + Claude Code Hooks + ZCode + GrokBuild）。
* **制作内容**：
  1. **动态工具注册表（`ToolRegistry` + `ToolPlugin` trait）**：
     - 将内置 6 大工具（`read/write/edit/grep/find/bash`）全部解耦为标准 `ToolPlugin`；
     - 实现 **BM25 按需工具延迟加载（`Deferred Tool Loading` + `tool_search`）**：当挂载外部插件或 MCP 导致工具总数 > 15 个时，仅将核心工具放入初始 Prompt，其余工具通过 `tool_search` 按需展开 Schema，节省 40% 静态上下文；
     - 支持 **多模型工具方言（Tool Dialect Adapter，学 GrokBuild）**：针对 DeepSeek / Claude / GPT 自动微调 Schema 描述与参数约束。
  2. **四级瀑布流生命周期钩子（`HookPipeline`，100% 兼容 Claude Code / Codex / ZCode 词表）**：
     - 内置 6 个标准拦截点：`UserPromptSubmit`、`PreStep`、`PreToolUse`、`PostToolUse`、`BeforeStop`、`SubagentStop`；
     - 支持 **Rust 原生插件钩子** 与 **外部脚本钩子（`.harness/hooks.toml`）**：外部脚本从 `stdin` 读 JSON，返回 `allow / deny / ask / rewrite_input`（退出码 `2` 代表硬拦截 Deny，策略钩子超时强制 Fail-Closed）。
  3. **可逆作用域技能与子目录即时感知（`ScopedEffect` + `SKILL.md` + JIT Subtree Context）**：
     - 原生扫描 `.agents/skills/*/SKILL.md` 与 `.claude/skills/*/SKILL.md`（启动仅注入 YAML frontmatter 索引，支持 `paths` 匹配按需激活）；
     - **JIT 子目录感知**：当 `read/edit` 触碰子目录 `packages/foo/src/lib.rs` 时，自动将该作用域下的 `packages/foo/AGENTS.md` 动态挂载为 `ScopedEffect`，离开该工作集后自动卸载。
  4. **新增 `harness inspect` 命令（学 GrokBuild）**：一键透视当前工作区生效的配置层、`AGENTS.md`、已挂载插件、Skills 与 Hooks。
* **阶段交付物**：支持热插拔工具、`.harness/hooks.toml` 拦截器与 `SKILL.md` 生态的微内核版 `v0.8.5`。

---

### 🔹 第 2 步：v0.9.0 —— Merkle 时空同构时光机与因果整枝引擎（Time-Travel & Causal Context）
> **核心目标**：让对话树与磁盘源码同步回滚（Kroma 自研的世界状态不变量），并用「因果引用 + 死胡同整枝 + 候选归因」作为可选机制取代死板的轮次压缩。这些是工程假设，收益必须由本项目任务集测量；论文只提供动机。
* **制作内容**：
  1. **内容寻址影子仓库（`ShadowStateStore` / Merkle Snapshot）**：
     - 在 `~/.local/share/harness/snapshots/<run_id>` 维护轻量内容寻址对象池（完全不污染用户项目的 `.git`）；
     - 每次 `write / edit / bash` 产生文件变动后，毫秒级计算工作区增量快照并生成 `tree_hash`，直接写入当前 WAL 节点的 `workspace_tree_hash` 字段；
     - **真·时光倒流**：执行 `harness --resume <id> --fork <entry_id>` 或 `harness undo` 时，不仅对话记忆回到 `<entry_id>`，**工作区磁盘上的所有源码文件也原子级恢复到 `<entry_id>` 时的精确字节状态**（恢复前自动把当前未保存改动存为备份分支，绝不丢代码）。
  2. **写入即检与候选归因门禁（`EditTimeMicroCheck` + `AttributionGuard`，独立工程设计，非 AHE 的实证结论）**：
     - `edit / write` 落盘瞬间自动执行零开销语法闭合检查（括号/引号/代码块平衡 + 多语言结构快检），当场在 `ToolResult` 尾部报警；
     - 每次通过 `CompletionGate` 或测试命令发现报错时，比对最近一次 Merkle 快照 Diff，生成**候选归因提示**（排序后的假设，不宣称根因；启发式排序永不自动回滚代码）。可选启用隔离反事实探针（默认关闭、需审批、有预算）来验证候选。
  3. **因果引用评分与死胡同整枝（`CausalContextProjector`，可选实验档，独立工程设计）**：
     - 追踪每个 `read` 出来的源码文件在后续 Assistant 推理与 `edit` 中的真实引用，被引用的文件延展 Pinning 寿命（受大小上限约束，预算不变量优先）；
     - **死胡同负样本压缩（Dead-End Pruning，可选）**：失败的排查尝试折叠为 1 条有界的负样本摘要（保留 `source_entry_id` 以便回取原文），同时保留证据链接、缓存前缀与工具配对不变量；“缓存命中 99%+/容量 ×3”是待测目标，不是已证结论。
* **阶段交付物**：具备「代码+记忆同步回滚」、「候选归因（可选隔离反事实探针，默认关闭）」与「可选死胡同负样本压缩」的 `v0.9.0`。

---

### 🔹 第 3 步：v0.9.5 —— 进程内防踩踏子代理集群（In-Process Swarm & Speculative Branching）
> **核心目标**：吸收 `jcode` 内存怪兽与 `Claude Code` 子代理架构，在单个 17MB Rust 进程内实现零开销分治与双分支赛马。
* **制作内容**：
  1. **原生第 7 原语 `subagent` 工具（In-Process Scoped Child Session）**：
     - 支持两类内置角色与 `.harness/agents/*.md` 自定义角色：
       - **`scout`（只读探路兵）**：仅挂载 `read / grep / find / ls`，使用低成本思考档位（如 `low/medium`）翻阅几十个文件，跑完销毁海量中间上下文，仅向父会话返回 <= 1000 字的结构化侦察报告；
       - **`worker`（隔离突击手）**：在独立工作区快照或临时 Git Worktree 中执行修改并独立跑通 `CompletionGate`。
  2. **同仓「脚下代码变动」实时防踩踏广播（Code-Shifting Notification，学 JCode）**：
     - 共享进程内 `WorkspaceWatchTable`：当子代理 A 修改了子代理 B 曾读取过的文件时，自动在子代理 B 的下一拍注入 `[Notice: File src/x.rs was modified by another agent (sha256: old -> new)]`，彻底消灭多代理并发覆盖。
  3. **双分支推测执行（Speculative Repair，下一代独有大招）**：
     - 针对疑难 Bug 支持开启 `--speculative 2`：基于 Merkle 快照同时克隆两个轻量隔离分支尝试不同修法，哪个分支先通过 `CompletionGate` 就原子合入主工作区，另一分支瞬时回收。
* **阶段交付物**：支持只读探路与双分支赛马的 `v0.9.5`。（“每增 1 路仅 +2MB”是待测目标，须报告机器/OS/版本/并发/测量方法，不作为交付承诺。）

---

### 🔹 第 4 步：v1.0.0 —— 「一核四态」终极完全体（Interactive TUI + ACP IDE 协议 + Desktop SDK Daemon）
> **核心目标**：完成从「命令行工具」到「全场景平台与桌面级 SDK 基座」的终极跃迁（吸收 GrokBuild + OpenCode + JCode + Zed ACP）。
* **制作内容**：
  1. **形态一：交互式终端工作台（`harness` 默认进入 REPL / TUI）**：
     - 无需每次敲 `--prompt`，直接多轮丝滑对话，实时打字机流式输出（`AssistantDelta` 事件）；
     - 底部实时状态栏（显示当前分支、上下文水位、缓存命中率、累计耗时、快照数）；
     - 内置高频斜杠指令：
       - `/undo`：一键把「对话 + 磁盘代码」同时回滚到上一拍；
       - `/diff`：彩色展示本轮会话对工作区做的所有源码改动；
       - `/tree` & `/fork <id>`：可视化打印会话分支树并瞬间切换到任意历史分支；
       - `/init`：扫描全工程符号图谱与构建命令，自动生成高质量 `AGENTS.md`；
       - `/compact`：手动触发因果整枝压缩；
       - `/inspect`：查看已加载插件、Skills、Hooks 与权限策略。
  2. **形态二：无头自动化与 CI 模式（`harness -p "..."` / `--output-format jsonl`）**：
     - 即现有的极速单次执行与脚本流式管道模式。
  3. **形态三：标准 ACP 协议服务端（`harness acp`）**：
     - 实现 Agent Client Protocol v1（JSON-RPC 2.0 over stdio），无需开发任何专用插件，直接挂载进 **Zed、JetBrains 全家桶、Neovim**，并能被 **OpenHands / Omnigent** 作为标准后端大脑调用，原生支持 IDE 内 diff 预览与一键 rewind。
  4. **形态四：常驻多会话 SDK 守护进程（`harness serve` + Rust `HarnessSession` SDK）**：
     - 在 `harness-core` 暴露常驻多轮 `HarnessSession` API（`send_turn`、`stream_events`、`undo`、`fork`、`approve`）；
     - `harness serve` 启动轻量本地 Unix Socket / HTTP+SSE 服务（并提供 OpenAPI 规范），任何 **Tauri / Electron 桌面软件、Web 前端或 pi-qq** 只需连上这个本地端口，就能零成本拥有完整的多会话编程智能体内核！

---

## 三、Harness v1.0 最终完全体形态全景

当这 5 步全部落地后，你将拥有一个怎样的产物？

```text
================================================================================
                HARNESS v1.0 · 下一代 Rust 智能体基座最终形态
================================================================================
  [四态统一入口层 (Interface & Protocol Surface)]
   ├─ 1. 交互式 TUI (harness)        → 多轮聊天 + /undo 代码回滚 + /diff + /tree + /init
   ├─ 2. 无头脚本态 (harness -p)      → CI/CD 自动化 + --resume latest + JSONL 事件流
   ├─ 3. IDE 标准态 (harness acp)    → 零插件直连 Zed / JetBrains / Neovim / OpenHands
   └─ 4. 桌面 SDK 态 (harness serve) → 常驻 Daemon + HTTP/SSE/UnixSocket 供桌面端调用
--------------------------------------------------------------------------------
  [可逆作用域微内核层 (Reversible Scoped Microkernel)]
   ├─ 动态 ToolRegistry (>15 工具自动 BM25 tool_search 延迟加载 + 5 种模型方言适配)
   ├─ 瀑布流 HookPipeline (PreToolUse / PostToolUse / Stop + 退出码 2 硬拦截)
   └─ ScopedEffect 引擎 (.agents/skills 按路径触发 + 子目录 AGENTS.md 摸文件即时挂载)
--------------------------------------------------------------------------------
  [下一代三大前沿理论引擎 (2026 Frontier Engines)]
   ├─ 1. Merkle 时空同构时光机  → WAL 节点绑定磁盘快照，--fork 与 /undo 记忆+代码同步回滚
   ├─ 2. 因果引用与死胡同整枝    → 目标：更高缓存命中 + 死胡同折叠为 1 句负样本（收益待消融测量）
   └─ 3. 写入即检 + 候选归因      → 微门禁 + CompletionGate 候选排序（可选隔离反事实探针；不自动回滚）
--------------------------------------------------------------------------------
  [进程内多代理与内核级安全底座 (Swarm & Kernel Sandbox)]
   ├─ 进程内 Subagent (Scout 探路 0 污染 + Worker 隔离 + 同仓脚下代码变动通知 + 双分支赛马)
   └─ Linux openat2 硬件级防软链逃逸 + O_NONBLOCK 防管道死锁 + FD 级 Inode 原子写盘
================================================================================
  体积与性能：以下均为待测目标（非承诺）：单二进制 ~15MB | 单会话内存 ~17MB (每增1路目标 +2MB) | 启动目标 < 5ms
================================================================================
```
