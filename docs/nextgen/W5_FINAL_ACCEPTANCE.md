# W5 最终验收：数据、Pareto 与逐实验处置

日期 2026-10-21。基线 `61ffcc30`，代码提交 `8f63776`（门禁 1410 通过 / 0 失败），账本 `progress.json`。

## 1. 汇总指标

| 指标 | 值 |
|---|---|
| 任务完成 | 101/120 VERIFIED（84%），其余 6 判负 / 7 外部阻塞 / 6 研究范围不成立 |
| 门禁 | 1410 测试通过 / 0 失败，strict clippy 通过，sandbox 56 通过 |
| 实验数 | 22 个配对实验（dev 6 任务为主，留出集 2 任务） |
| 单次运行成本 | dev 套件 53k–150k token/arm，中位约 115k；每次约 7–9 分钟墙钟 |
| 成本/成功 | 中位 arm 115k token / 6 个成功 ≈ 19k token 每任务成功 |
| 稳定性 | 同一 baseline 同套件不同轮次通过率 6/6 与 4/6，单次重复漂移约 2 个任务；小于 5% 的 token 差异只能算无结论 |
| 资源 | p0 墙钟 57.7ms；p2 图构建 3410ms、峰值 RSS 77MB；p4 峰值 PSS 264MB |

## 2. Pareto 权衡（token vs 质量）

下表按「相对第一臂的 token 变化」排序，质量列是各自 resolved/tasks。判据是：只有在质量不降的前提下省 token 才算赢，
省下的百分点小于漂移（约 2 个任务）的不算证据。

| 实验 | 第一臂 | 第二臂 | token 变化 | 质量 | 处置 |
|---|---|---|---|---|---|
| g03-implicit | candidate-on-demand-tools | baseline | 149036 → 89162 (-40.2%) | 6/6 vs 5/6 | 缓存三臂，命中率涨但 token 也涨 |
| h06-inline | inline_32k | inline_64k | 119869 → 120066 (+0.2%) | 6/6 vs 6/6 | 噪声内或小收益，保持现状 |
| h04-efficient-candidate | baseline | candidate-on-demand-tools | 146405 → 146743 (+0.2%) | 6/6 vs 6/6 | 判负，不采纳 |
| injection-file-surface | baseline | candidate-on-demand-tools | 16547 → 16588 (+0.2%) | 1/1 vs 1/1 | 候选在特定夹具上略省，未推广 |
| causal-dev | causal_pruning | baseline | 112497 → 113068 (+0.5%) | 6/6 vs 6/6 | 噪声内或小收益，保持现状 |
| graphcache-dev | no_graph_cache | baseline | 116106 → 116943 (+0.7%) | 6/6 vs 6/6 | 噪声内或小收益，保持现状 |
| p3-dev | projection_p3 | baseline | 116926 → 120061 (+2.7%) | 6/6 vs 6/6 | 噪声内或小收益，保持现状 |
| h04-default | candidate-on-demand-tools | baseline | 116778 → 120324 (+3.0%) | 6/6 vs 6/6 | 候选在特定夹具上略省，未推广 |
| j02-p1-vs-p2 | baseline_full_context | masking_p1 | 114156 → 117679 (+3.1%) | 6/6 vs 6/6 | 判负，不采纳 |
| n04-loopguard | loop_guard_on | loop_guard_off | 113811 → 117459 (+3.2%) | 6/6 vs 6/6 | 噪声内或小收益，保持现状 |
| requireverified | require_verified | baseline | 112055 → 116393 (+3.9%) | 6/6 vs 6/6 | 噪声内或小收益，保持现状 |
| m04-experience | experience_off | experience_on | 115319 → 120522 (+4.5%) | 6/6 vs 6/6 | 判负，不采纳 |
| r05-pareto | default | all_local_optima_on | 119832 → 128821 (+7.5%) | 6/6 vs 6/6 | 判负，不采纳 |
| constraint-trap | baseline | candidate-on-demand-tools | 20572 → 22211 (+8.0%) | 1/1 vs 1/1 | 判负，不采纳 |
| probe-dev | baseline | counterfactual_probe | 73259 → 79867 (+9.0%) | 4/6 vs 4/6 | 判负，不采纳 |
| subagents | no_subagents | baseline | 103835 → 119802 (+15.4%) | 6/6 vs 6/6 | 判负/仅子代理关闭略省 |
| tournament-dev | baseline | adaptive_tournament_2 | 108459 → 126948 (+17.0%) | 6/6 vs 6/6 | 判负/仅子代理关闭略省 |
| constraint | candidate-on-demand-tools | baseline | 17631 → 22222 (+26.0%) | 1/1 vs 1/1 | 候选在特定夹具上略省，未推广 |
| g03-explicit | baseline | candidate-on-demand-tools | 87557 → 111823 (+27.7%) | 6/6 vs 6/6 | 缓存三臂，命中率涨但 token 也涨 |
| g03-off | baseline | candidate-on-demand-tools | 86147 → 110139 (+27.9%) | 6/6 vs 6/6 | 缓存三臂，命中率涨但 token 也涨 |
| injection-prompt-surface | baseline | candidate-on-demand-tools | 16804 → 22645 (+34.8%) | 1/1 vs 1/1 | 候选在特定夹具上略省，未推广 |
| minimal-tools-dev | minimal_tools | baseline | 53276 → 119568 (+124.4%) | 6/6 vs 6/6 | 最大杠杆；不设为默认（任务类型决定） |

## 3. 逐实验处置与原因

- 采纳（进入默认）：无。所有默认配置保持基线值；唯一被采纳的是代码修复（7 个 bug）与 I03 的 opt-in 缓存。
- 拒绝（有测量支撑）：H04 efficient-candidate 提示 +22%、N03 锦标赛 k=2 +17%、J02 masking/P1 +3% 且丢可恢复性、
  G03 缓存三臂命中率涨但 token 也涨、H06 内联窗口减半 −0.16%（噪声内）、M04 经验注入 +4.5%、R05 局部最优全开 +7.5%。
- 噪声内不晋升：P3 −2.6%、causal_pruning −0.5%、require_verified −3.7%、N04 loop_guard −3.1%、graph cache 0.7%。
- 最大杠杆记录：工具菜单 7 个 vs 全量省 55%（119568 → 53276，6/6 对 6/6），工具 schema 占 9.4KB 前缀里的 8.7KB；
  结论是「因素大且由任务类型决定」，不是「永远只给 7 个工具」。
- 唯一超噪声收益：关掉子代理 −13.3%（103835 vs 119802），方向是减功能，未设为默认。

## 4. 未跑的真实环境与不支持场景

- 无 Windows / macOS 真机：T02 阻塞，跨平台只做了 Linux 交叉构建（PE32+ x86_64 39.79s）。
- 无第三方安全工具链（semgrep/cargo-audit/cargo-deny）与独立评审方：Q06 阻塞，T04 依赖它。
- 无部署实例可灰度：S06 阻塞。
- 27 个 harness 只有 `pi` 可运行：F05 阻塞，逐项处置在 `f05_disposition.md`。
- 端点不支持 provider-native compaction 与 tool search：J06/L06 阻塞，探测在 `j06_l06_probe.txt`。
- 语义摘要（J05）与经济计划器（J03）未实现；I06 需要的语法解析器不在树里；I02 的根因是整文件 SHA256。

## 5. 下一轮瓶颈

1. 单次重复噪声（约 2 个任务）让所有小于 5% 的收益都不可判定 —— 下一轮必须每个 arm 至少 3 次重复，否则继续是在噪声里做决定。
2. 工具菜单是最大的成本项，但没有「按任务类型自动裁剪」的证据；这是唯一值得先做的方向。
3. 外部能力缺口（Windows/macOS 真机、安全工具链、可灰度部署）不补齐，T02/Q06/S06/T04 无法推进。
4. 20 个 BLOCKED/DEFERRED 项里有 6 项只差「一个 arm + 一次预注册配对」，瓶颈是运行预算而不是设计。

