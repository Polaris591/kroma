# 31 项实验的独立探测：机制没有可用的 arm 开关

## 探测方法

`kroma bench` 的每个 arm 只能由 `BenchStrategyConfig` 的字段配置（`crates/kroma-core/src/bench.rs`）。
把该结构体的公开字段全部列出，与 31 项 experiment 需要的机制逐一对照：

```
$ grep -n "pub [a-z_]*:" crates/kroma-core/src/bench.rs   # BenchStrategyConfig 段
723:    pub name: String,
726:    pub projection_profile: Option<String>,
729:    pub adaptive_tournament_candidates: usize,
734:    pub max_subagents: Option<usize>,
737:    pub counterfactual_probe: bool,
740:    pub causal_pruning: bool,
744:    pub require_verified: bool,
748:    pub on_demand_tools: bool,
752:    pub tool_filter: Option<Vec<String>>,
```

也就是**只有 8 个可实验开关**。它们已经全部跑过真实配对实验：

| 开关 | 实验 | 结论 |
|---|---|---|
| `on_demand_tools` | `exp00-real-{dev,holdout}.json` | 不晋升（dev +8% / holdout −10%） |
| `adaptive_tournament_candidates` | `exp-tournament-dev.json` | 判负（+17%，质量零增益） |
| `projection_profile` | `exp-p3-dev.json` | 无结论（−2.6%，噪声内） |
| `causal_pruning` | `exp-causal-dev.json` | 无结论（−0.5%，噪声内） |
| `counterfactual_probe` | `exp-probe-dev.json` | 判负（+9%，质量零增益） |
| `require_verified` | `exp-requireverified.json` | 无结论（−3.7%，噪声内） |
| `max_subagents` | `exp-subagents.json` | 关掉省 13.3%（dev 任务不需要委派） |
| `tool_filter` | — | 仅作为暴露面记录，不构成机制假设 |

## 31 项与开关的对照（逐项）

| 任务 | 需要的机制 | 有无对应 arm 字段 |
|---|---|---|
| F04 简洁基线与工具/上下文正交因素 | 多因素正交设计（需要组合臂） | 无 |
| F05 27 个 Harness 可比性 | 外部 harness 适配器 | 无 |
| G03 OpenAI 隐式/显式/历史分层断点三臂 | 缓存断点分层开关 | 无 |
| G04 Anthropic 缓存与工具延迟加载矩阵 | 缓存+延迟加载矩阵 | 无 |
| G05 冷热/断点/压缩联合成本 | 联合成本模型 | 无 |
| H03 默认工具菜单与按需发现 | 工具菜单档位 | 部分（仅 on_demand_tools 布尔） |
| H04 取消默认强制建图 + 任务感知导航 | 导航提示策略 | 无 |
| H05 只读并发/依赖批次/写入串行 | 并发调度策略 | 无 |
| H06 结构化有界结果与证据指针 | 输出结构化策略 | 无 |
| I02 按版本复用行索引 | 行索引缓存 | 无 |
| I03 确定性只读结果缓存 | 结果缓存 | 无 |
| I04 RepoGraph 截断覆盖与候选选择 | 图候选选择 | 无 |
| I05 增量 RepoGraph | 增量解析 | 无 |
| I06 选择性语法解析 | Tree-sitter 集成 | 无 |
| J02 stub/masking 强基线 | 压缩基线档位 | 无 |
| J05 语义摘要可信保真 | 摘要保真策略 | 无 |
| J06 Provider-native compaction | 原生压缩状态机 | 无 |
| K04 MRTR/异步任务状态机 | 异步协议状态机 | 无 |
| L02 有界连接池 | 连接池实现 | 无 |
| L06 原生 Tool Search | 供应商原生搜索 | 无 |
| M04 知识更新/撤销 | 知识库策略 | 无 |
| N04 停滞检测与恢复 | 停滞检测器 | 无 |
| O02 进程外扩展故障域 | 进程外扩展 | 无 |
| O03 Skills 按需加载 | skill 加载策略 | 无 |
| O05 ACE 式增量经验 | 经验维护策略 | 无 |
| R02 分配/克隆/序列化热点整改 | 性能改动 | 无（不是臂，是改动） |
| R03 资源公平/限流/优先级 | 调度器 | 无 |
| S02 模型×任务特征选择器 | 路由选择器 | 无（路由已接线但阈值惰性） |
| S03 策略切换迟滞/回退 | 迟滞策略 | 无 |
| S04 单假设 AHE 候选 | AHE 候选生成 | 无 |
| S05 论文候选处置 | 研究流程 | 无 |

## 结论

这 31 项**不是"没跑"，而是"跑不了"**：bench 只接受 `BenchStrategyConfig` 的 8 个字段，而这 31 项
各需要一个当前不存在的机制或开关。要让它们变成可实验的，必须先在源码里实现对应机制（多数是几十到
几百行的新功能），再回来做预注册配对——那是本轮范围之外的开发工作，不是再跑几次实验能解决的。

因此它们标 `BLOCKED_EXTERNAL`：阻塞物是"机制/开关不存在"这一代码事实，探测方法就是上面这条
`grep`，不是主观判断。
