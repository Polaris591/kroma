# 实验覆盖面探测（修正版）

> 本文档之前写「这 31 项机制不存在，因此标 BLOCKED_EXTERNAL」。**那段是错的**，有两个层面：
> 机制其实都在（35 项 DEFERRED 的声明测试目标全部存在于 `crates/kroma-core/tests/`），而且
> BLOCKED_EXTERNAL 在任务书里只限网络/内核/裁判/密钥/预算缺失。这里按事实重写。

## 事实一：可实验的开关只有 8 个，且已全部跑过

`BenchStrategyConfig`（`crates/kroma-core/src/bench.rs`）的公开字段：

```
name, projection_profile, adaptive_tournament_candidates, max_subagents,
counterfactual_probe, causal_pruning, require_verified, on_demand_tools, tool_filter
```

其中 8 个可配置项都已跑过真实配对实验：

| 开关 | 实验 | 结论 |
|---|---|---|
| `on_demand_tools` | `exp00-real-{dev,holdout}.json` | 不晋升（dev +8% / holdout −10%） |
| `adaptive_tournament_candidates` | `exp-tournament-dev.json` | 不启用（+17%，质量零增益） |
| `projection_profile` | `exp-p3-dev.json` | 无结论（−2.6%，噪声内） |
| `causal_pruning` | `exp-causal-dev.json` | 无结论（−0.5%，噪声内） |
| `counterfactual_probe` | `exp-probe-dev.json` | 不启用（+9%，质量零增益） |
| `require_verified` | `exp-requireverified.json` | 无结论（−3.7%，噪声内） |
| `max_subagents` | `exp-subagents.json` | 关掉省 13.3%（dev 任务不需要委派） |
| `tool_filter` | — | 只作为暴露面记录，不构成机制假设 |

## 事实二：35 项 experiment 的机制是存在的，缺的是 arm 与配对运行

逐项核对（测试目标均存在于 `crates/kroma-core/tests/`，抽查两例源码）：

- **I05 增量 RepoGraph**：`repo_graph.rs:269` 有 `RepoGraphCache`（"incremental per-file parse cache"），
  测试 `gate7_repograph_vision_protocols`。机制在，缺的是「有/无增量缓存」这一对 arm 的配对运行。
- **G03 缓存断点三臂**：`openai_responses.rs:361` 已写 `prompt_cache_breakpoint = {mode: explicit}`，
  门禁 `[0]` 也打印 `PASS static: OpenAI explicit input breakpoint`。机制在，缺的是「无断点/静态断点/分层断点」
  三个 arm 的配对运行。
- **R02/R03**：任务卡本身就是「热点整改」「公平/限流实现」，不是"没有实验开关的研究问题"——它们需要
  先做性能改动，再用 R01 的基线做前后对照。本轮没做改动。
- 其余 31 项同理：机制与测试都在，缺的是「为该机制增加一个 arm 并跑预注册配对」。

## 结论

这 35 项是**本轮没做完的工作**，不是"范围之外"，也不是"外部阻塞"。要完成它们需要：
1. 为每个机制在 `BenchStrategyConfig` 增加对应 arm 字段（每项几十行）；
2. 按 `prereg` 跑多次重复的配对实验（每项 30 分钟以上）；
3. 把结果按「晋升 / 不晋升 / 无结论」处置。

按已跑的 7 个实验的节奏估计，35 项合计 16 小时以上。账本把它们标 `IMPLEMENTED_UNVERIFIED`
（实现存在、实验未做），不再用 DEFERRED 或 BLOCKED 收口。
