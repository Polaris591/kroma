# S05: disposition of all 27 papers

Every paper in the taskbook's `papers.json` gets one label: `reproduce` (we re-ran its measurement),
`adapt` (we took the mechanism, changed it for this harness), `observe` (read and used for architecture
decisions, no transplant), `defer` (out of scope until a stated condition holds), `reject` (not doing it).
The label is derived from the paper's own `kroma_decision` in the taskbook plus what this round actually did;
where the round did nothing beyond reading, the label is `observe` rather than a claimed adaptation.

| id | paper | label | what was actually done / why |
|---|---|---|---|
| P01 | Harness Engineering: Anatomy, Architecture, and Evolution  | observe | 用于架构覆盖和取舍，不要求移植所有模式 |
| P02 | The Empire, Long Divided, Must Unite | observe | 优先保护 WAL 事实层、投影层、扩展层的分离 |
| P03 | A Programming Paradigm for Spatiotemporal Composability | observe | 给插件 owner/cleanup/revocation 建契约，不迁移整个核心 |
| P04 | Finding the Right Fit: Model-Harness Interactions across A | observe | 先做 Kroma 同模型消融，再做冻结配置的跨 Harness 对照 |
| P05 | Harness-Bench | adapt | 用来补任务类型与失败分类，独立 oracle 保持盲测 |
| P06 | Don't Blame the Large Language Model | observe | 每次发布固定任务模型复评；模型升级与 Harness 升级拆开 |
| P07 | Security Assessment of DeepSeek Harness with A.I.G | adapt | 建立真实工具权限与提示注入分离的测试集，同时检查正常任务可用性 |
| P08 | ToolBottleneckBench: Measuring Tool-Agent Performance unde | adapt | 正交测试菜单大小和输出预算；不能把工具选择失败全归模型，也不能用 oracle 选工具 |
| P09 | HarnessSecurity-Bench | adapt | 加入上下文、文件、插件、MCP、恢复等攻击与合法成对用例 |
| P10 | What's in Your Agent's Context? Context Privilege Escalati | observe | 跨检索、记忆、压缩、子代理和恢复保留来源与权威级别 |
| P11 | CADOC: Cache-Aware Dynamic Object Context for Long-Horizon | adapt | 优先做关闭默认的经济调度候选；扩展实际 cache write、恢复和输出费用，独立检验质量非劣 |
| P12 | Logos: An Agent Harness on a Cross-Process Bus | observe | 进程外扩展仅用于需要隔离的插件，保留核心直接调用热路径 |
| P13 | Mem2ActBench | observe | 用长期记忆指导代码行动的任务，检查过期事实与误用率 |
| P14 | The Complexity Trap: Simple Observation Masking Is as Effi | observe | 现有可恢复 stub 作为强基线，语义摘要必须超过它才晋升 |
| P15 | ACE: Agentic Context Engineering | observe | 在候选经验层实现差量更新与独立审查，不自动写入全局可信层 |
| P16 | AgentFold: Long-Horizon Web Agents with Proactive Context  | defer | 只列研究/接口实验，未有训练和许可资源则明确延期，不伪造实现 |
| P17 | LongMemEval | adapt | 补版本、时间、撤销与遗忘用例；与行动任务联合评估 |
| P18 | RepoGraph: Enhancing AI Software Engineering with Reposito | adapt | 测覆盖、更新和定位，再选择性加入语法解析 |
| P19 | LocAgent: Graph-Guided LLM Agents for Code Localization | observe | 精确路径小任务跳过图，大任务比较图+词法混合定位 |
| P20 | SWE-agent: Agent-Computer Interfaces Enable Automated Soft | observe | read/edit/grep 的错误反馈与输出消融，不追求工具数量 |
| P21 | PolyKV | defer | 明确排除本轮产品实现；只有自托管推理需求和资源成立时另立项目 |
| P22 | Harness-Search: Guiding Long-Horizon Search through Multi- | adapt | 仅在已有主/子代理与证据系统上实验角色分离；记录全部审查费用 |
| P23 | Self-Evolving Agent Harness | observe | 冻结最终裁判；候选可自演进但自改进必须另行证明；保留历史回归预算 |
| P24 | Chronos Enables Code Agents to Reason over Software Evolut | observe | 在候选经验层试验带时间/仓库过滤的历史卡片；图扩展同样禁止引入未来解决方案 |
| P25 | Agent-Controlled Forgetting for Tool-Using Agents: Reversi | adapt | 补强J02/J05强基线，分别统计输入、输出、请求数、时延与恢复，不把可恢复称无损决策 |
| P26 | Recurrent Harness for Agentic Continual Learning | observe | 在候选经验层修正过度泛化/过期规则；原始WAL和证据不改写，生产晋升仍有独立保护 |
| P27 | MESH-Harness: Self-Improving Agent Harnesses via Bandit-Gu | adapt | 在已有AHE候选库上离线比较固定/随机/带不确定性的评估调度，限制池大小与全部预算 |

Totals: adapt=10, defer=2, observe=15

No paper is labelled `reproduce`: re-running another harness's measurement needs that harness, which is the
same blocker recorded for F05. None is labelled `reject` outright -- `defer` is used where the taskbook itself
says the condition is not met (training/self-hosting resources), which is the honest form of 'not now'.
