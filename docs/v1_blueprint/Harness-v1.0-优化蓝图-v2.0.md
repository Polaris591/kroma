# HARNESS v1.0 · 深度研究优化版架构蓝图 v2.0

**研究截止日期：2026-10-12**  
**目标：** 基于现有 Rust Harness v0.8.x，构建可回滚、可观测、安全可控、上下文高效、可扩展并最终支持多入口的 Coding Agent Runtime。

> 这是一份工程蓝图，不是已实现或已通过验证的产品承诺。本文将设计分为：**已有实证支持**、**成熟工程模式**、**需要本项目验证的假设**。所有性能数字都必须来自本项目可复现的基准，不从其他项目直接继承。

---

## 0. 决策摘要

### 保留的愿景

打造一个 Rust 编写、跨模型、轻量、能控制文件和命令执行、允许上下文与插件演进、能恢复长任务的 AI 编码执行底座。上层可以提供 CLI、TUI、ACP 和 SDK/Daemon；所有入口共享一套状态、事件、权限与执行语义。

### 改写后的核心主张

1. **Workspace Revision，而不是“魔法时光机”**：将 Run/Tool 事件和受管理的工作区版本绑定；先做到单次修改可靠撤销、崩溃恢复和冲突检测，再发展分支与内容寻址优化。
2. **Evidence-linked Context Projection，而不是“读取模型真实因果”**：依据可观测的读取、编辑、测试、回滚关系构建来源图；原始证据可恢复，模型上下文只是按预算生成的投影。
3. **可撤销扩展，而不是一切都能任意卸载**：注册、事件订阅和服务可以逆序撤销；运行中的任务、副作用、权限和外部操作单独管理。安全策略属于可信内核，不允许普通插件覆盖。
4. **分层评测，而不是单个通过率**：确定性测试守住契约，真实模型测试评估能力，长任务测试评估可维护性，成本指标衡量每个成功任务的资源花费。
5. **渐进式多 Agent**：先只读 Scout，再隔离 Workspace Worker，最后实验分支赛马。文件变化通知只是提示；版本校验与隔离工作区才负责避免冲突。

### 从旧版蓝图删除的保证性承诺

以下数字不再是发布合同：`undo <1ms`、`缓存命中 99.8%`、`上下文容量 ×3`、`tool schema 节省 40%`、`每个 Agent +2MB`、`二进制 15MB`、`启动 <5ms`。这些可作为待测目标，必须报告机器、OS、版本、仓库大小、模型、上下文、并发、测量方法以及 p50/p95。

也不再使用“首个在工业界落地”“四大顶会理论”等未经可核验依据支持的宣传性表述。Cordis、AHE、ACE、SWE-Effi、JIT-Agent、HarnessDev 等研究需按其实际发表状态标注；预印本不等于同行评审通过。

---

## 1. 研究原则与证据等级

### 1.1 证据等级

- **A — 同行评审实证**：正式会议/期刊论文给出方法、对照实验或基准数据。可以借鉴其在实验设置内支持的设计，不可以超范围推广。
- **B — 官方工程实现/实践报告**：可验证源码或官方工程文章支持一种可行的工程模式；不代表独立比较已经证明它最优。
- **C — 预印本/新兴研究**：有实验结果但尚需同行评审、复现或独立验证；适合成为实验候选，不应成为关键正确性假设。
- **D — 项目自测或营销指标**：可提供待验证的测试方法与假设；不能作为其他机器、任务或版本的性能基线。

### 1.2 设计原则

1. **从基线开始**：每个新机制必须对比一个简单、已知的基线。
2. **先保证正确性，再优化延迟**：尤其是回滚、Hook、Context Pruning、Shell、并发修改等能力。
3. **模型不可信，项目内容也不可信**：源代码、`AGENTS.md`、Skills、工具输出、MCP 结果都不能提高自身权限。
4. **历史事实与模型上下文分离**：完整事件保留在受限制的本地存储里；提示词中的文本只是投影，可按需重新取回来源。
5. **保持最小可信内核**：插件可添加能力，但不能关闭 Policy、伪造执行结果、绕过预算或修改审计事实。
6. **公开兼容范围**：对 ACP/MCP/Provider Tool Schema/第三方 Hook，逐能力声明兼容性，不宣称“100% 兼容”而无兼容测试。

---

## 2. 最新知名 Harness：值得吸收什么

本节基于公开源代码与官方文档。它比较的是各项目公开可观察的工程设计，并非同条件性能排行榜。

| 项目 | 可复用的优势 | v1.0 应如何吸收 | 不应照搬/尚未证明的部分 |
|---|---|---|---|
| **Claude Code / Agent SDK** | Skills、Hooks、Subagents；长任务跨上下文窗口的 initializer + 增量任务 + progress artifacts；较成熟的工具工作流 | 做结构化任务状态、可组合 Hook、按需 Skill、每个阶段完成后可验证的交接工件 | 内部实现并未完全公开；厂商特有能力不应假设能完整复制；不要把 `CLAUDE.md`/Skills 直接提升为安全策略 |
| **OpenAI Codex CLI / App Server / Agents API** | Rust Runtime；线程生命周期、持久化事件、fork/resume、双向 JSON-RPC、稳定 UI 事件、沙箱和审批；工程实践强调仓库知识可检验、短索引和架构不变量 | 将 Runtime 与 TUI/IDE 分离；定义稳定的 Run/Event 协议；AGENTS 文件作为索引而非百科；用 CI/评估/清理闭环控制代码库漂移 | Agents API 和托管服务有供应商边界；内部工程报告不是对所有项目的可复现基准 |
| **DeepSeek DSH / Cordis** | 插件贡献服务、类型化事件、可撤销注册效果；Profile/Bundle 组合；Agent Loop、Model Adapter、Tool Registry 等可替换 | 采用 typed event、registrar handle、依赖顺序、逆序 cleanup、失败挂载回滚和 Profile | “一切皆插件”不能扩展成“安全内核也可被任意插件删除”；卸载插件不等于撤销它已经造成的外部副作用 |
| **Pi** | 极简默认工具集（read/write/edit/bash），扩展 API，交互/print/JSON/RPC/SDK 多入口 | 先保留小型核心工具集；用扩展补充领域功能；保持 core 易理解、好测试 | 扩展可执行任意代码；少工具并不自动等于更安全；是否需要 `grep/find/ls` 应以 A/B 评测决定 |
| **OpenCode** | 显式 allow/ask/deny 权限；工具/Agent 权限细分；Snapshots/undo 工作流；自定义工具与 MCP | 将权限策略作为一等 API；分别控制 read/edit/bash/外部目录；将文件修改回滚和事件历史相联系 | 快照不自动处理任意外部副作用；权限配置不等于 OS 沙箱 |
| **Gemini CLI** | 项目/用户/系统/扩展配置层级；Hooks 可拦截操作、注入上下文、记录审计；修改前 Shadow Git Checkpoint | 借鉴 Hook 的输入/输出协议、超时与项目 Hook 信任提示；写操作前创建恢复点 | 项目级 Hook 是任意代码，必须经过信任授权；Checkpoints 仍需要冲突和覆盖测试 |
| **Grok Build（xAI）** | Rust；公开源码；Session/Leader/工具注册/TUI/ACP/Headless/serve 等多入口与运行时模块 | 研究 Rust actor/Session 管理、事件流、ACP 适配和多入口共用核心的拆分方式 | 大型单体源码可用于学习但不是直接迁移模板；不能把它的自测数值视为跨机性能事实 |
| **JCode（Rust）** | 聚焦低开销多会话与 Headless Worker；公开内存、启动和多会话测试脚本 | 复用它的基准测试思路；自己定义 PSS/RSS、活跃任务和冷启动条件，找出每会话固定开销 | 仓库报告的性能数字是项目自测；不同表格/版本/工作负载不可混合，必须自行复现 |
| **ZCode** | 多入口的 Coding 工作台、Agent CLI/Runtime、扩展与工具生态 | 学习将 UI、运行时和配置分层；明确每种能力的 API 归属 | 不要把完整产品工作台的复杂性全部搬进内核 |
| **OpenHands SDK / Agent Server** | Agent SDK 与 Agent Server 分离；事件 API、REST/WebSocket 客户端、Workspace 和可选隔离环境 | 参考 SDK/Server 边界、会话事件、可选择执行环境与远程工作区 | Python/Cloud 形态不是 Rust 内核的实现模板；本地服务需要认证、会话隔离、权限和资源上限 |
| **SWE-agent（研究系统）** | Agent-Computer Interface（ACI）针对模型重新设计文件查看、搜索和编辑；编辑前 lint、受控输出窗口 | 工具设计优先于工具数量：有界 `read`、结构化搜索结果、编辑前验证和能指导下一步的错误信息 | 论文的窗口大小/提升来自具体模型、数据和基准，不是对任意模型的普适常数 |
| **Agentless（研究方法）** | 文件定位 → 位置定位 → 修复/验证的简单分阶段流程，在其 SWE-bench Lite 实验里与复杂 Agent 竞争 | 保留低复杂度的 Plan/Locate/Repair/Validate profile，避免所有任务都走复杂自主循环 | 单一基准与模型结果不证明 Agentless 永远更优；用于支持“要与简单基线比较” |

参考资料详见 `RESEARCH_EVIDENCE_MATRIX.md`。

---

## 3. 已有实证能支持哪些设计？

### 3.1 面向模型设计的工具接口（强证据）

SWE-agent 被 NeurIPS 2024 接收，明确研究 Agent-Computer Interface。它的消融实验报告：在其配置下，100 行文件窗口优于 30 行窗口和整文件输出；编辑时 lint 反馈也提高了对应基准表现。这直接支持：工具输出应该有界、与后续操作配套，编辑错误要给出可行动的诊断。它不支持把“100 行”作为所有项目的永恒常数。

**决策：** `read` 提供行窗口和字节上限；`edit` 对可识别语言可选调用真实 parser/linter；失败返回精确代码、位置和重试提示。默认四工具与七工具两套 Profile 用相同任务比较。

### 3.2 长上下文不等于有效上下文（同行评审证据）

*Lost in the Middle*（TACL 2024）发现，多个模型在长上下文中提取相关信息的表现会随信息位置变化，中间位置的相关信息可能比上下文首尾更难被使用。

**决策：** 不把所有历史按时间顺序无限拼接。使用 Context Projection，将固定任务契约、最近状态和关键证据按明确顺序组织；历史大块输出应裁剪成有来源的摘要，并且可以重新取回原文。

### 3.3 简洁的任务流水线值得保留（实证但特定基准）

Agentless 用定位、修复、验证三阶段，在其报告的 GPT-4o / SWE-bench Lite 实验中解决 300 个任务中的 96 个，并报告平均成本约 $0.70。它并未证明简单流程总是最好，但证明复杂 Agent 的抽象数量应有证据支撑。

**决策：** P0 默认顺序 ReAct；v0.9 加入可选的 `Plan → Locate → Edit → Validate` profile，与 ReAct 在同一任务集比较。不要一开始强制所有任务启动 Planner/Scout/Worker/Evaluator。

### 3.4 真实长任务需要增量交接和仓库知识（官方工程证据）

Anthropic 的长任务工程文章采用初始化阶段生成环境准备、功能清单和进度工件，后续会话只增量推进并留下交接记录。OpenAI 的 Harness Engineering 经验将 `AGENTS.md` 作为导航目录，较深知识放入有版本控制的仓库文档，并通过架构约束、测试与定期清理维持可读性。

**决策：** `TaskState` 结构化记录目标、验收标准、已完成/未完成项目、验证结果和下一步；`AGENTS.md` 短而像索引；为长任务加独立 `plan/progress/verification` 工件。不将此工程经验描述成普遍必然提高多少百分点。

### 3.5 Agent 评测必须控制运行环境与成本（基准/研究证据）

SWE-bench Pro（ICML 2026）提供更长程、更多仓库和语言的真实软件工程任务集。SWE-Effi（预印本）强调 Token 与时间预算、Token Snowball 与昂贵失败。Anthropic 2026 年工程评估报告指出环境配置在其 Terminal-Bench 2.0 实验中可造成数个百分点差异。2026 年 10 月的 *What Does a Harness Buy? Tokens, Mostly* 是最新预印本，报告同模型跨 Harness 成本差异可达约 3 倍，但成功率差异与重复运行波动交叠；这项结果仍需独立复现。

**决策：** 严禁把跨机器、跨模型、跨预算的分数直接当作架构提升。必须做配对任务、重复运行、固定环境、固定模型与显式成本报告。

### 3.6 代码长期可维护性必须进入评测（新兴研究）

SlopCodeBench（2026 预印本）以迭代扩展任务观察代码冗余与结构侵蚀；它提示单次测试通过率不足以代表多轮维护能力。但其当前结果还不是跨全部语言/项目的普适定律。

**决策：** v1.0 评测加入连续需求变更任务，追踪重复代码、复杂函数集中度、回归测试结果与跨轮次成功率。先作为探索性指标，不作为自动质量判决的唯一依据。

---

## 4. v1.0 目标架构

### 4.1 高层结构

```text
Interfaces
  ├─ CLI / Headless JSONL
  ├─ TUI
  ├─ ACP Adapter
  └─ Local SDK / Daemon (later)
          │
          ▼
Stable Runtime API + Typed Event Stream
          │
          ▼
Agent Runtime (SessionActor / RunStateMachine)
  ├─ Provider Adapter (canonical messages/tool calls)
  ├─ ContextProjector
  ├─ ControlProfile (ReAct first; Plan/Repair optional)
  ├─ Tool Dispatcher
  ├─ Policy Gateway (non-pluggable trust root)
  └─ Completion/Validation Gate
          │
          ├──────── Workspace Runtime
          │           ├─ WorkspaceRevision / SnapshotStore
          │           ├─ capability-based FileTools
          │           ├─ Shell Supervisor / output limits
          │           └─ isolated Workspace (Worktree/overlay)
          │
          ├──────── Extension Runtime
          │           ├─ ToolRegistry / typed contracts
          │           ├─ reversible registration handles
          │           ├─ Hooks / Skills / path-scoped instructions
          │           └─ MCP / external process adapters
          │
          └──────── Durable State & Observability
                      ├─ append-only RunEvent log
                      ├─ Checkpoints / WorkspaceRevision references
                      ├─ Trace / tokens / latency / costs
                      └─ Eval artifacts / replay
```

### 4.2 最小可信内核

**必须留在可信内核：**
- Run/ToolCall 生命周期与状态转换；
- Provider 协议一致性和工具调用 ID 去重/配对；
- Policy 决策、预算、取消、权限与审核记录；
- Workspace Revision 提交/恢复协调；
- 事件日志的序号、关联 ID 和完整性校验；
- 外部插件失败后的注册回滚。

**可以扩展的部件：** Provider 适配器、工具、搜索器、语言解析器、Context Ranking 策略、验证 Profile、TUI UI 组件、MCP 和 ACP adapter。插件不能通过 `allow` 输出覆盖核心的 Deny，也不能篡改原始事件。

### 4.3 关键领域对象

- `RunId` / `SessionId` / `TurnId` / `ToolCallId`：强类型 ID，不互相复用。
- `RunEvent`：带单调递增序号、时间、来源、Run/Turn/ToolCall 关联和 schema version。
- `ToolCall`：Provider 原始 ID、规范化名称与参数、策略结果、状态、deadline、idempotency/effect class。
- `ToolResult`：`ok`、结构化错误码、可见输出、artifact refs、截断元数据和执行耗时。
- `WorkspaceRevision`：revision ID、父 revision、tree hash、受管文件 manifest、dirty/conflict 状态、snapshot backend/version。
- `ContextProjection`：消息/工具 schema 的具体选择、来源事件 ID、估算 Token、实际 Provider usage（若可得）、裁剪决策日志。
- `PolicyDecision`：Allow/Ask/Deny、规则 ID、资源与路径范围、决策来源、审批者和时间。
- `PluginHandle`：服务/事件/工具注册清单、依赖、作用域、cleanup 行为和插件版本。

不要用未经定义的“因果分数”覆盖上述事实记录。

---

## 5. Workspace Time Travel：正确优先的设计

### 5.1 不先写自制 Merkle 数据库

第一实现建议复用 Git 的内容寻址对象、Tree/Commit 与 Worktree 语义，必要时创建 Harness 管理的 shadow repository，避免污染用户仓库。把 SnapshotStore 抽象出来，以便后续通过基准比较 Git 对象、manifest+CAS 和自研 Merkle Store。

自定义存储只有在证明 Git 路径在所需规模下无法满足延迟、磁盘开销或恢复语义后再引入。

### 5.2 明确快照覆盖范围

Snapshot policy 必须明确：
- 跟踪哪些文件（tracked / untracked）；
- ignored files 是否纳入以及敏感文件默认排除规则；
- symlink 存的是链接目标还是目标内容；
- 可执行位、文件模式、大小写冲突、特殊文件与大文件的行为；
- 不保存 ACL/xattr/硬件设备状态时，不宣称“精确恢复整个机器状态”；
- `bash` 可以修改众多文件，因此命令前后不能只相信工具调用的预期变更列表。

### 5.3 修改事务状态机

```text
PREPARE
  → record Intent + parent_revision + expected file hashes
  → capture before-state for mutation scope
  → execute mutation in controlled workspace
  → collect result and after-state
  → validate hashes / scope / policy
  → COMMIT revision + event

Any failure → RECOVERY_REQUIRED / COMPENSATE / CONFLICT
```

这不是对任意多文件系统操作的 ACID 原子事务。多文件恢复应可恢复、可续跑并在冲突时停住；不能假装多文件 Rename 具备全局原子性。

### 5.4 Undo / Fork 语义

- `undo` 不删除既有 transcript。它产生新的 `UndoApplied` 事件，并把当前 Context Projection 指向前一个合适的逻辑状态。
- 对受管理工作区的文件恢复，先检查 live hashes；如果文件在快照之后被用户/其他 Agent 修改，展示冲突并默认不覆盖。
- 恢复前保留当前未提交状态的备份 revision。
- `fork` 创建新 Session/Run lineage，并默认拥有独立工作区（Git Worktree/overlay）；不因 fork 自动修改主工作区。
- Shell 的网络、数据库、部署等外部副作用不能靠快照撤销；必须由权限、审计和补偿逻辑单独管理。

### 5.5 验收

故障注入覆盖 WAL/事件提交前后、快照生成中断、文件替换、磁盘不足、外部修改冲突、符号链接边界与恢复重入。指标为恢复一致率、数据丢失率（目标 0）、冲突正确检测率、p50/p95 延迟和增量写入字节。

---

## 6. Context Runtime：从轮次裁剪转向证据化投影

### 6.1 真相源与模型输入分开

1. **Canonical Event Store**：受限制保存完整事件和原始工具输出；设置大小配额、保留策略和脱敏规则。
2. **TaskState**：目标、验收条件、进度、未解决问题、验证状态；模型可以修改建议，Host 通过事件确认事实状态。
3. **Evidence Graph**：只记录可观测关系，如 `ToolOutput → Edit`、`Edit → TestRun`、`Revision → ReadResult`、`Rollback → InvalidatedEvidence`。
4. **ContextProjection**：选择本次请求发送的消息/证据/Schema，不是原始记录本身。
5. **Derived Summary**：AI 生成摘要是可再生缓存，必须带来源 ID、revision 与版本；摘要不应成为唯一事实。

不要声称直接获得模型隐藏推理的“真实因果图”。仅以工具、代码、测试和显式事件构建证据关系。

### 6.2 选择与压缩顺序

硬保留：System/Security policy、当前用户目标、验收条件、未解决的关键错误、当前活动修改、pending tool call 关联、最近验证状态。  
优先保留：最近的有用工具结果、与正在修改文件/符号直接相关的来源、最近失败证据。  
可投影/裁剪：早期完整文件读取、已重复搜索、已验证过时的假设、已关闭探索分支；必须保留可恢复 artifact reference。

死胡同摘要必须包含“尝试的假设、证据、失败原因/未知项、来源 ID”，不能只存一句断言。被回滚的修改可以作为负样本候选，但仍保留原始事件。

### 6.3 Token 成本工程

- 将固定 system prefix、工具 Schema、历史/TaskState、工具输出分别计量。
- 可用 Provider usage 时记录实际 input/output/cached tokens；不可用时标记为估算，不能伪称精确。
- 固定前缀放前面，动态上下文放后面；Provider-specific caching 采用显式 adapter，不猜测缓存命中。
- 工具 Schema 延迟加载不以“>15 个工具”硬编码。建立工具 Catalog，按任务候选筛选，Schema 加载策略用四工具/七工具/动态发现 A/B 测。
- 先用确定性排序与保留规则，后试 ACE 式结构化增量 playbook 更新；生成的经验须可审阅、带来源、版本化，不能自动修改安全政策。

### 6.4 验收

比较 baseline（当前高低水位）/确定性投影/来源图排序/可选摘要策略。在同模型、同任务、同预算的配对任务上报告成功率、输入 Token、cached tokens、调用数、失败后证据找回率和 p95 latency。不能仅以压缩率或缓存命中率作为成功标准。

---

## 7. 工具与 ACI：少量强原语 + 有证据的专用工具

### 7.1 Profile

- **Core profile**：`read`、`edit`、`write`、`bash`。
- **Coding profile**：另加 `ls`、`find`、`grep`。
- **Deferred tools**：外部 MCP/领域工具默认不全量塞进 prompt；用可观测的工具目录与按需 Schema 加载。

四工具只是默认起点，不设为真理。SWE-agent 的消融实验支持有界文件查看、搜索摘要、编辑前 lint 的价值，但最佳行数需由当前模型/任务复验。

### 7.2 Tool Contract

每个工具定义：
- 模型可见：name、concise description、input schema、必要示例；
- Host-only：capabilities、effect class（read-only / reversible mutation / external side effect / irreversible）、timeout、output budget、idempotency、parallelism、workspace scope、approval requirement；
- 结果：成功/错误码、简短可执行诊断、artifact refs、截断原因、hash/revision。

### 7.3 `read/edit/write`

- `read`：line window + byte cap；大输出保存 artifact，返回摘要和行范围；默认敏感文件保护。
- `edit`：只做唯一匹配；统一换行表示用于匹配后恢复原风格；不默认 trim/模糊匹配。返回 diff、前后 hash、错误码 `NOT_FOUND` / `MATCH_NOT_UNIQUE` / `STALE_FILE`。
- `write`：区分 create 与 overwrite；create 要用经测试的 no-clobber 方式；overwrite 需版本校验；临时文件+提交，失败不会留下半文件。
- 编辑前 lint/parser 是语言插件能力，不是“零开销通用语法检查”。在不可识别语言中只做基础内容检查，诊断默认不一定硬阻断。

### 7.4 Shell

- Shell 必须被视为高权限执行入口，不因工作区路径过滤而被视为沙箱。
- 使用独立工作目录、最小环境变量白名单、超时、过程组/Job Object 管理、限流 stdout/stderr、同时读取双通道、取消/TERM/KILL 宽限阶段和退出码。
- 区分采集上限、保留上限、输出总量与运行时间。仅在事后截断完整缓冲不算有界内存。
- 禁用自动重定向或明确校验 Provider 端点与 TLS；API Key 不得进入 URL/日志；请求超时但服务端是否执行未知时，标记 `OUTCOME_UNKNOWN`，不能盲目重试有副作用操作。

### 7.5 Tools A/B

在内部 Golden Tasks 中将 Core 4 tools、Coding 7 tools、动态加载 profile 做配对比较。记录 tool invocation count、输出 Token、任务成功率和失败归因。只有在质量不下降且成本收益显著时，才改变默认 profile。

---

## 8. 可逆插件与 Hook 微内核

### 8.1 三种插件信任级别

1. **Built-in Rust crate**：编译期、强类型、最低开销；适合核心 Provider/Tool/Policy adapter。
2. **外部进程/脚本/MCP**：版本化协议、独立权限与超时；适合系统工具和跨语言扩展。
3. **WASM（后续）**：需要可移植并具有受限 Host capabilities 的插件才引入；WASM 不会自动成为安全沙箱，Host API 决定边界。

### 8.2 可逆注册

插件 mount 返回 `PluginHandle`，包含注册的服务、工具、事件订阅、Hook、子任务和 cleanup；初始化失败按逆序清理。unmount 前拒绝新任务，再取消或等待在途任务，执行 cleanup、检查副作用并移除注册表。

### 8.3 Hook 分类与失败策略

- **Policy hooks**：会影响执行许可，必须受可信 Policy 约束；超时/错误按明确 fail-closed（拒绝或 ask）规则处理。
- **Observer hooks**：只用于 metrics/log/UI，超时可 fail-open 并记录故障。
- **Mutator hooks**：允许改工具输入或上下文，但变更需结构化 diff、来源与版本；不能降低权限级别或覆盖用户目标。

建立内部 canonical event，再由产品适配器映射至 Claude Code / Gemini CLI / Codex 等事件。逐项声明兼容性，不承诺 100% 词表兼容。

### 8.4 Skills 与目录指令

- 仅加载短的 frontmatter/index；按用户请求、工具目标路径或显式 Skill 调用装入正文。
- 子目录 `AGENTS.md` 按明确的路径优先级与大小限制加载，内容始终作为不可信项目数据，而不是 System Policy。
- 任何项目 Hook/插件第一次启用前需信任确认或显式策略；不要因打开仓库就执行项目脚本。

---

## 9. Predictive Validation：从猜罪魁祸首转为证据驱动诊断

### 9.1 每次修改可记录的事实

- 编辑前后 WorkspaceRevision 和文件 hash；
- 微型检查是否执行、使用了哪个 parser/linter 版本；
- baseline test 状态与之后测试状态；
- 测试环境、命令、退出码、耗时与产物；
- 变更与失败之间的候选关联。

### 9.2 Attribution 不是简单的“错误数增加”

错误数量受随机测试、缓存、环境及多次编辑影响。系统先输出“候选引入的变更”，通过复跑测试、缩小 diff/回滚复测来增加证据；只有可重复复现才提高归因置信度。禁止因单次错误计数上涨就自动回滚用户变更。

### 9.3 CompletionGate

- 支持按任务类型配置：编译、单测、指定测试、lint、typecheck、diff 检查、未解决失败。
- Gate 输出机器可读证据和人类可读摘要。
- Gate 通过不等于整体质量证明；需防止测试覆盖不足和测试被修改绕过。
- 对外部副作用不在自动回滚承诺范围内。

---

## 10. Subagents 与 Speculative Branching

### 10.1 递进设计

**Scout（第一版）**：只读、无写权限、独立 Context，向父 Run 返回结构化事实与来源 ID。  
**Worker（第二版）**：独立 workspace/worktree、独立 budget、独立事件流和验证结果。  
**Speculative（实验版）**：最多 2 个隔离分支，任务明确适合多个方案且成本预算允许才启动。

### 10.2 避免并发踩踏

- 每次 Edit 附 `expected_file_hash`；提交时做 compare-and-swap/版本检查。
- 文件变更通知只负责提醒 Agent 重新读取，不能代替写冲突检测。
- worker 不允许共享可变工作树；共享只读仓库对象或基线快照。
- 外部 Shell/联网/部署默认禁用或受限，避免赛马带来重复副作用。

### 10.3 “谁先通过测试谁赢”不可作为唯一策略

选择流程：
1. 两个 branch 在相同基线和环境中独立工作；
2. 运行相同、固定的验证套件；
3. 检查 diff 范围、是否删除/削弱测试、lint、安全约束与任务验收；
4. 比较成本和时间；
5. 合入到目标分支后再次跑集成测试；
6. 失败则恢复原状态。

内存指标由基准测量，不给每 Agent 的固定 MB 承诺。优先保障隔离与数据正确性。

---

## 11. 一核四入口

### 11.1 Shared Runtime Contract

所有入口使用相同的 Session/Run/Tool/Event/Approval API，不让 TUI 独有状态。事件 schema 带版本、序号和未知字段的向前兼容规则。

### 11.2 推荐顺序

1. **Headless**：`harness -p ... --output-format jsonl`，用于 CI 与实验；最容易验证。
2. **ACP Adapter**：按 ACP 官方协议实现 `initialize`, `session/new`, `session/prompt`, `session/update`, `session/cancel` 等声明的 capability；不支持的 capability 明确不声明。
3. **TUI**：建立在同一事件流上，支持 `/diff`、`/undo`、`/fork`、`/inspect`、审批与状态显示。
4. **Local SDK/Daemon**：最后实现；默认 Unix socket、严格 socket 权限/owner、会话隔离、访问令牌或 peer credentials、并发与预算限额；HTTP 端点另设认证与浏览器访问保护。

“零插件直连所有 IDE”改为“实现协议标准的 Client/Agent Adapter，并逐个通过互操作测试”。

---

## 12. 正式评测方案（v1.0 必须具备）

### 12.1 四层测试

**L0 — Deterministic contracts**：单元、属性测试、协议兼容、路径/符号链接、终端控制字符、ToolCall ID 配对、流中断、预算耗尽、Shell 取消、WAL 故障注入。Fake Provider 测试必须不依赖付费模型。

**L1 — Task capability**：固定模型版本、Prompt、工具 profile、机器镜像、依赖与预算。至少覆盖 bug fix、功能扩展、代码导航、测试修复、重构、失败恢复。

**L2 — Long-horizon maintainability**：连续多轮新增/修改需求；测任务成功率随轮次的变化、重复代码/复杂度变化、测试质量和修改扩散范围。SLO 由自身基线设定。

**L3 — Security & resilience**：恶意仓库指令、提示词注入、凭据路径、路径竞态、子进程逃逸、Provider 重定向/坏响应、磁盘满、断电/崩溃恢复和外部修改冲突。

### 12.2 结果指标

- task resolve/pass@1 与多次运行成功率；
- cost per successful task，而非单纯平均 Token；
- input tokens 分解：system prefix / schema / history / tool output；Provider cached tokens（若可得）；output tokens；
- LLM calls、tool calls、重复签名率、昂贵失败率；
- wall time p50/p95、首事件延迟、峰值 RSS/PSS、快照字节量；
- policy violations、数据丢失/覆盖、恢复一致性；
- 长期质量：连续检查点的 solve rate、复杂函数比例、重复代码与变更范围。

### 12.3 实验纪律

- 每次只改变一个机制；先固定模型与任务，使用配对任务和多次运行。
- 固定依赖、OS、CPU/RAM、缓存状态和网络条件；Infrastructure noise 需单独记录。
- 小样本适合冒烟测试，不适合声称小幅成功率提升。最新的 Harness 比较预印本发现，任务胜负可能与重复运行噪声同量级；报告置信区间与实际任务 discordance。
- 使用公开 SWE-bench Pro/其他公开基准时，同时保留自己的私有/未见任务集，避免只优化公开任务。
- 每个优化必须报告任务效果和资源成本。只有 Token 降低但任务质量下降，不算成功。

### 12.4 建议发布门禁

- L0/L3 的安全和数据完整性阻断项全部通过；
- 无未解释的版本回归；
- 在固定内部任务集上的完成率不能超过预先约定的非劣界限；
- 若声称有性能提升，必须提供配对实验、重复运行、置信区间与相同预算下的质量/成本分析；
- 所有性能宣传须注明基准方法与适用范围。

---

## 13. 迭代路线（以 Gate 而非发布日期为准）

### Gate 0 — v0.8.1 安全封板

完成已知漏洞修复、干净构建、可复现测试、版本/Tag 一致性与残余风险清单。

**验收**：所有已知问题有复现测试；不出现“零死角”这种无法验证的承诺。

### Gate 1 — Workspace Revision 与最小 Undo

先基于 Git/shadow repository 或可验证 SnapshotStore 实现单文件和受控多文件恢复。WAL/事件状态机与文件版本绑定。

**验收**：故障注入后状态可重建；冲突时不覆盖外部修改；undo 生成审计事件；测试崩溃恢复和备份。

### Gate 2 — Context Projection v1

做确定性裁剪、活动任务/文件保护、来源 ID 和 Token 计量；不启用自动因果学习。

**验收**：模型历史不破坏协议，旧证据可恢复，成本数据可区分估算与 Provider 真实用量。

### Gate 3 — 工具/Hook/Skill 作用域

先静态 Rust crate 与可撤销注册句柄，再引入外部脚本 Hook/MCP。Policy hook 与 observer hook 分开处理。

**验收**：初始化失败回滚；unmount 清理所有注册；权限不可由插件提升；项目扩展必须经过信任决策。

### Gate 4 — 预测验证与归因实验

对 edit 前后做 parser/lint/测试；输出疑似归因，按需复跑或回滚复测。所有自动判断带证据。

**验收**：测归因精度、误报率、额外运行成本；未经证实不得自动回滚。

### Gate 5 — Scout/Worker Subagent

只读 Scout + 隔离 Worktree Worker；并发预算、取消、版本比较与验证归并。

**验收**：并发修改不会静默覆盖；候选分支相互隔离；合入后集成测试再次通过。

### Gate 6 — Headless/ACP/TUI/SDK

从 Headless 和协议稳定性开始，ACP/TUI 共用 Runtime 事件；最后常驻 Server。

**验收**：每种入口声明的协议能力有互操作测试；同一 Run 状态语义一致；本地 Daemon 具备最小权限与会话隔离。

### Gate 7 — 选择性研究增强（可选）

ACE 风格 playbook 更新、动态工具搜索、语义因果排序、复杂分支赛马。只在任务集上显示收益后才默认启用。

**验收**：与简单基线比较，报告效果、成本、稳定性；支持回退到 baseline。

---

## 14. 风险登记表

| 风险 | 失败表现 | 控制机制 | 验收证据 |
|---|---|---|---|
| Snapshot scope 不完整 | undo 后未跟踪/ignored 文件状态变化 | 明确受管文件清单，恢复前展示差异 | 各文件类型和文件模式的 round-trip tests |
| 外部并发修改 | Undo 覆盖用户新改动 | expected hashes、冲突状态、默认拒绝覆盖 | 人工与另一进程并发修改集成测试 |
| Shell 副作用不可回滚 | 文件快照恢复但网络/数据库已变更 | Shell 权限/沙箱、命令分类、审计与补偿操作 | 受限执行与未知结果测试 |
| Hook 权限绕过 | 插件用 allow 覆盖 Deny | Policy 为不可覆盖的可信内核服务 | 插件恶意返回值测试 |
| Context 摘要丢证据 | 模型被误导、修错文件 | 摘要有来源 ID，原始证据可再取回 | 对照压缩前后任务结果与证据恢复率 |
| 多 Agent 踩踏 | 旧读取覆盖新代码 | Worktree 隔离、版本 CAS | 并发编辑压力测试 |
| 性能结论失真 | 用单次跑分声称提升 | 固定环境、重复运行、配对样本 | 评测报告和原始运行记录 |
| Daemon 暴露 | 本机其他进程/浏览器访问模型会话 | Unix socket 权限、peer auth、会话 ACL | 本机未授权访问测试 |

---

## 15. 最终产品定义

Harness v1.0 不以“采用了最前沿名词”作为成功，而以以下能力为定义：

1. 模型、工具、运行状态和外部入口之间有稳定、可版本化的协议；
2. 每次受管理的代码修改都能追溯和可恢复，恢复冲突不会静默覆盖用户数据；
3. Context Projection 有预算、证据来源和原文恢复能力；
4. 插件和 Hooks 可按生命周期组合与撤销，但不能突破内核 Policy；
5. 多 Agent 以独立工作区和验证门禁减少互相覆盖；
6. TUI/Headless/ACP/SDK 共用同一运行时语义；
7. 所有能力升级必须通过确定性测试、真实任务评测、成本测量和安全回归。

**最终路线：先可靠的 Workspace Revision，再证据化 Context Projection，再可逆插件，再隔离 Subagents，再多入口平台化；自适应因果排序和分支赛马属于经实验证明后才默认启用的研究增强。**
