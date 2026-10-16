# Harness 研究与工程证据矩阵（截至 2026-10-12）

这份清单用于区分“有正式实证支持”“官方工程可行”“预印本候选”和“项目自测”。“有效”必须理解为特定任务、模型、设置下观察到效果，不是所有 Harness 的普遍定律。

## A 级：正式发表、含经验评估

### 1. SWE-agent: Agent-Computer Interfaces Enable Automated Software Engineering
- Venue: NeurIPS 2024.
- 原始论文：https://proceedings.neurips.cc/paper_files/paper/2024/file/5a7c947568c1b1328ccc5230172e1e7c-Paper-Conference.pdf
- 相关实现：https://github.com/SWE-agent/SWE-agent/blob/main/docs/background/aci.md
- 有证据支持：ACI 工具设计会影响模型表现。论文消融实验报告，特定 benchmark 上 100 行文件窗口好于 30 行与整文件输出；编辑时 lint guardrail 改善对应结果；文件搜索结果需要足够简洁。
- 不支持：所有语言都该固定 100 行；语法检查总是零开销；任意工具设计都有同样幅度的收益。
- 架构采纳：有界 read、针对编辑结果的精确反馈、结构化搜索输出、工具之间的操作语义连续。

### 2. SWE-bench: Can Language Models Resolve Real-world GitHub Issues?
- Venue: ICLR 2024.
- 原始论文：https://proceedings.iclr.cc/paper_files/paper/2024/hash/edac78c3e300629acfe6cbe9ca88fb84-Abstract-Conference.html
- 有证据支持：真实仓库 Issue 是跨文件、长上下文、工具交互的困难评估任务；应评测 agent 在仓库和执行环境中的任务解决，而非只测静态代码生成。
- 不支持：某种 Harness 架构会必然提升得分。它是 benchmark 研究，不是插件/时光机/上下文算法的证明。
- 架构采纳：固定仓库与依赖、公开失败 patch 以外的任务集、真实测试验证。

### 3. Lost in the Middle: How Language Models Use Long Contexts
- Venue: TACL 2024 / NAACL 2024.
- 原始论文：https://aclanthology.org/2024.tacl-1.9/
- 有证据支持：在其多文档问答与检索任务中，关键事实所处的上下文位置会影响模型表现；长上下文容量不等于均匀使用上下文。
- 不支持：某个特定的 Harness context order 对所有模型都最优。
- 架构采纳：控制请求长度、关键状态有明确区域、对 Context Projection 做位置敏感的消融测试。

### 4. SWE-Bench Pro: Can AI Agents Solve Long-Horizon Software Engineering Tasks?
- Venue: ICML 2026, PMLR 306.
- 原始论文：https://proceedings.mlr.press/v306/deng26f.html
- 规模：论文页面描述 1,865 题、41 个活跃仓库、123 种语言，包含公开和保留评测分区。
- 有证据支持：更长程、更真实和更难的问题是评估软件工程 Agent 的必要补充。
- 不支持：某种内部实现能通过这套 benchmark；使用公开任务就足以排除污染。
- 架构采纳：公开 benchmark + 自有留出任务，长程与多文件任务不只依赖 SWE-bench Lite。

### 5. Agentless: Demystifying LLM-based Software Engineering Agents
- Venue: ICSE Companion Proceedings 2024（注意是 Companion 论文集，不应误称 ICSE 主会论文）。
- 原始论文：https://arxiv.org/abs/2407.01489
- 其作者报告：在 GPT-4o / SWE-bench Lite 的设置中，三阶段方法解决 300 题中的 96 题（32%），平均成本约 $0.70。
- 有证据支持：较简单的 locate → repair → validate 管线在特定 benchmark 上可以与复杂 Agent 竞争；减少检索候选和分阶段定位可能降低成本。
- 不支持：简单 Agent 一直强于工具型 Agent，或上述成绩可直接迁移到 2026 的模型和任务。
- 架构采纳：提供简单 ControlProfile 作为对照，而不把 Planner/多 Agent 设为默认路径。

## B 级：官方工程实现或公开工程报告

### 6. OpenAI — Harness engineering: leveraging Codex in an agent-first world (2026-02-11)
- https://openai.com/index/harness-engineering/
- 重点：把 `AGENTS.md` 当作短导航而非完整手册；仓库内版本化文档作为 agent 可发现的事实；架构约束机械验证；测试、评审和周期性清理进入反馈环。
- 证据范围：OpenAI 团队具体工程实践，非独立对照实验。

### 7. OpenAI — Unlocking the Codex harness: how we built the App Server (2026)
- https://openai.com/index/unlocking-the-codex-harness/
- 重点：Thread 生命周期和持久化、Core Runtime、工具执行/沙箱、MCP/Skills 以及双向 JSON-RPC App Server；事件让多种 UI 共享同一核心。
- 采纳：Runtime 和 UI/server adapter 分层，稳定的线程事件协议。

### 8. Anthropic — Effective harnesses for long-running agents (2025-11-26)
- https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents
- 重点：initializer agent 准备环境/功能清单，后续 agent 每次增量实现并留下进度工件与干净工作区。
- 证据范围：官方工程案例；适合作为长任务模式，不是所有任务类型的统一最优方案。

### 9. Anthropic — Effective context engineering for AI agents (2025-09-29)
- https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
- 重点：上下文是一种有限资源；目标是用正确的配置、指令、状态和工具信息提高行为稳定性，而不是无差别扩大输入。

### 10. Anthropic — Scaling Managed Agents: Decoupling the brain from the hands (2026-04-08)
- https://www.anthropic.com/engineering/managed-agents
- 重点：模型/Harness 控制与执行环境分离，工具与沙箱作为执行端可替换；长任务环境故障不必直接破坏会话。

### 11. DeepSeek Harness / Cordis 架构
- https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/architecture.md
- 重点：服务、类型化事件、可撤销注册效果、Profile/Bundle；Agent Loop、模型适配、工具注册和会话日志可配置组合。
- 采纳：PluginHandle、mount/unmount cleanup、依赖解析、失败时逆序清理。
- 不支持：Cordis 不证明磁盘文件与对话的原子同步穿越（world-state），也不证明“每个并发会话 +2MB”或任何内存数字；其文档没有这样的性能结论。这些是 Kroma 自己的待测目标，必须用本项目 benchmark 测量。

### 12. Gemini CLI — Checkpointing / Hooks / Extensions
- https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/checkpointing.md
- https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/index.md
- 重点：文件修改前的 shadow Git checkpoint；同步 Hook 可验证动作、阻止工具、记录审计；扩展可提供 skills、agents、policy。
- 采纳：修改前保存点、Hook timeout、外部 Hook 的信任边界。

### 13. OpenCode — Tools / Agents
- https://docs.opencode.ai/docs/tools/
- https://opencode.ai/docs/agents/
- 重点：显式 allow/ask/deny、细化到工具的权限、不同职责的 Agent 和自定义工具。
- 采纳：权限是 Runtime 状态而非提示词建议；Plan Agent 默认只读。

### 14. OpenHands Software Agent SDK / Agent Server
- https://github.com/OpenHands/software-agent-sdk/
- https://github.com/OpenHands/software-agent-sdk/tree/main/openhands-agent-server
- 重点：Agent SDK 和 Agent Server、事件、工作区、REST/WebSocket API 与 UI 分层；执行环境可本地或临时隔离。
- 采纳：共享事件协议和 Workspace 抽象；服务端安全单独验收。

### 15. xAI Grok Build（2026-07 开源）
- 官方公告：https://x.ai/news/grok-build-open-source
- 源码：https://github.com/xai-org/grok-build
- 重点：Rust 实现、公开 Agent loop/context/tool dispatcher/TUI/extensions/subagents 与 ACP 表面。
- 采纳：学习模块拆分和多入口 adapter；不把源码规模当作可维护性证明。

### 16. ZCode
- https://github.com/zai-org/ZCode
- 重点：桌面、浏览器和终端工作台共享 Agent CLI/Runtime。
- 采纳：产品形态可统一，但 UI 与核心运行时分开。

### 17. JCode
- https://github.com/1jehuang/jcode
- 重点：Rust、高并发 headless session、公开内存与启动 benchmark。
- 证据范围：项目自测，具体硬件、版本和 workload 有限定。复现脚本可以帮助构建本项目 benchmark，但不能作为 Harness 的通用性能承诺。

### 18. Pi
- https://github.com/badlogic/pi-mono
- 重点：四个默认工具（read/write/edit/bash）、Extension API、TUI/print/RPC/SDK、多数附加能力按需扩展。
- 采纳：默认工具集尽量小；扩展满足领域需求；安全仍由执行环境保证。

## C 级：有实验结果但尚需独立验证/同行评审

### 19. Agentic Context Engineering (ACE), arXiv:2510.04618 (2025)
- https://arxiv.org/abs/2510.04618
- 报告的结果：在作者所用 Agent 与 finance benchmark 上比其强基线提高；摘要报告 agent +10.6%、finance +8.6% 等结果。
- 适用方式：把“增量累积的结构化 playbook，而非反复改写成短摘要”作为可选实验。
- 不要：直接把 ACE 机制当成 v1.0 必须依赖的运行核心或保证固定收益。

### 20. SWE-Effi, arXiv:2509.09853 (2025)
- https://arxiv.org/abs/2509.09853
- 提出任务正确性与 Token/时间资源联合的有效性指标，讨论 Token Snowball 与昂贵失败。
- 适用方式：把 cost per successful task、expensive-failure rate、任务预算纳入评测。

### 21. SlopCodeBench, arXiv:2603.24755 (2026)
- https://arxiv.org/abs/2603.24755
- 研究多轮演化任务中冗余代码和复杂度集中等长期质量信号。
- 适用方式：在自己的长任务评测中增加多轮需求和代码可维护性指标。
- 不要：把预印本数字当成所有 Agent 都必然劣化的定律。

### 22. What Does a Harness Buy? Tokens, Mostly, arXiv:2610.04433 (2026-10-03)
- https://arxiv.org/abs/2610.04433
- 报告同一模型/任务中 Harness 间单任务成本最多约有 3 倍差异；部分成功率差异与重复运行的波动重叠。样本与模型数有限，论文刚提交，尚需独立复现。
- 适用方式：高度重视 system prefix / tool schema / 每次请求静态成本，并要求 Harness 对比做重复、配对实验。
- 不要：用此单篇新预印本直接确定某家 Harness 永远最好/最差。

### 23. JIT-Agent / HarnessDev 与其他自动演化 Harness 研究
- JIT-Agent：https://arxiv.org/abs/2608.25593
- HarnessDev：https://arxiv.org/abs/2609.01437
- 作为研究候选：自动选择/调整执行策略可能有价值；v1.0 的 Policy、安全边界与状态协议不能交给未经评测的自生成逻辑修改。

## D 级：项目自测，不是独立证据

- JCode RAM/启动时间比较、各项目 README 里的 performance claims，以及任何“每 Agent 几 MB”“5ms 启动”“99.8% cache hit”等数字，都只能在版本、机器和 workload 明确时作为该项目的自测数据。
- 使用它们构造基准脚本是有益的；将它们原样写进 Harness 的目标或承诺则不可取。

## 建议引用核验原则

1. 区分会议/期刊论文、arXiv 预印本、厂商工程博客、源代码和项目自己的 benchmark。
2. 摘要中的数值只能应用到论文实际使用的模型、任务、指标和对比基线。
3. “已证明”最多表示研究在限定条件下观察到结果，不表示它对所有未来模型、仓库和系统都普遍成立。
