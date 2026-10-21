# W5 最终验收：数据、Pareto 与逐实验处置

日期 2026-10-21。基线 `61ffcc30`，代码提交 `9e7662f`（门禁 1432 通过 / 0 失败），账本 `progress.json`。

## 1. 汇总指标

| 指标 | 值 |
|---|---|
| 任务完成 | 106/120 VERIFIED（88%）；其余 7 实测判负 / 7 外部阻塞 |
| 门禁 | 1432 测试通过 / 0 失败，strict clippy 通过，sandbox 通过 |
| 实验数 | 22 个配对实验 |
| 单次成本 | dev 套件 53k–150k token/arm，中位约 115k；每次 7–9 分钟墙钟 |
| 成本/成功 | 中位 arm 115k token / 6 个成功 ≈ 19k token 每任务成功 |
| 稳定性 | 同 baseline 同套件不同轮次 6/6 与 4/6，单次漂移约 2 个任务；小于 5% 的 token 差异算无结论 |
| 资源（release，3 次中位） | p0 4.197ms / 2.7–3.1MB；p2 图 397.6ms / 62–65MB（1200 文件）；p3 34.0ms / 15.6MB；p4 四会话 228.9MB |

## 2. Pareto 权衡（token vs 质量）

只有质量不降且省下的幅度超过漂移（约 2 个任务）才算赢。`配对 delta` 是同一次运行内的配对差值，
`跨 run` 是不同轮次之间的比较——两者不可混用。H04 只有跨 run 的比较（120324 → 146405，+22%），没有同 run 配对；exp-h04-efficient-candidate.json 里那个 +0.2% 是 on_demand_tools 臂的差值，不是 H04 的提示词变量。

| 实验 | 第一臂 token | 第二臂 token | 配对 delta | 质量 | 处置 |
|---|---|---|---|---|---|
| causal-dev | 112497 | 113068 | Δ通过率 +0.00 | 6/6 vs 6/6 | causal_pruning |
| constraint-trap | 20572 | 22211 | Δ通过率 +0.00 | 1/1 vs 1/1 | baseline |
| constraint | 17631 | 22222 | Δ通过率 +0.00 | 1/1 vs 1/1 | candidate-on-demand-tools |
| g03-explicit | 87557 | 111823 | Δ通过率 +0.00 | 6/6 vs 6/6 | baseline |
| g03-implicit | 149036 | 89162 | Δ通过率 +0.17 | 6/6 vs 5/6 | candidate-on-demand-tools |
| g03-off | 86147 | 110139 | Δ通过率 +0.00 | 6/6 vs 6/6 | baseline |
| graphcache-dev | 116106 | 116943 | Δ通过率 +0.00 | 6/6 vs 6/6 | no_graph_cache |
| h04-default | 116778 | 120324 | Δ通过率 +0.00 | 6/6 vs 6/6 | candidate-on-demand-tools |
| h04-efficient-candidate | 146405 | 146743 | Δ通过率 +0.00 | 6/6 vs 6/6 | baseline |
| h06-inline | 119869 | 120066 | Δ通过率 +0.00 | 6/6 vs 6/6 | inline_32k |
| injection-file-surface | 16547 | 16588 | Δ通过率 +0.00 | 1/1 vs 1/1 | baseline |
| injection-prompt-surface | 16804 | 22645 | Δ通过率 +0.00 | 1/1 vs 1/1 | baseline |
| j02-p1-vs-p2 | 114156 | 117679 | Δ通过率 +0.00 | 6/6 vs 6/6 | baseline_full_context |
| m04-experience | 115319 | 120522 | Δ通过率 +0.00 | 6/6 vs 6/6 | experience_off |
| minimal-tools-dev | 53276 | 119568 | Δ通过率 +0.00 | 6/6 vs 6/6 | minimal_tools |
| n04-loopguard | 113811 | 117459 | Δ通过率 +0.00 | 6/6 vs 6/6 | loop_guard_on |
| p3-dev | 116926 | 120061 | Δ通过率 +0.00 | 6/6 vs 6/6 | projection_p3 |
| probe-dev | 73259 | 79867 | Δ通过率 +0.00 | 4/6 vs 4/6 | baseline |
| r05-pareto | 119832 | 128821 | Δ通过率 +0.00 | 6/6 vs 6/6 | default |
| requireverified | 112055 | 116393 | Δ通过率 +0.00 | 6/6 vs 6/6 | require_verified |
| subagents | 103835 | 119802 | Δ通过率 +0.00 | 6/6 vs 6/6 | no_subagents |
| tournament-dev | 108459 | 126948 | Δ通过率 +0.00 | 6/6 vs 6/6 | baseline |

## 3. 逐实验处置

- 采纳为默认：无。默认配置全部保持基线；被采纳的是代码修复（7 个 bug）与 I03 的 opt-in 缓存。
- 实测判负（账本 REJECTED）：H04 提示词变体（跨 run +22%，无同 run 配对）、N03 锦标赛 k=2 +17%、
  J02 masking/P1 +3% 且丢可恢复性、G03 缓存三臂（命中率涨、token 也涨）、H06 内联窗口减半 −0.16%（噪声内）、
  M04 经验注入 +4.5%、I02 行索引（根因是整文件 SHA256，不是行扫描）。
- 噪声内不晋升：P3 −2.6%、causal_pruning −0.5%、require_verified −3.7%、N04 loop_guard −3.1%、graph cache 0.7%。
- R05 是组合回归守卫，不是机制判负：把局部最优全开 128821 vs 默认 119832（+7.5%，都 6/6），所以默认按整体结果走；
  账本里它仍是 VERIFIED，因为该任务验收的正是「不要全开」。
- 最大杠杆：工具菜单 7 个 vs 全量省 55%（119568 → 53276，6/6 对 6/6），工具 schema 占 9.4KB 前缀里的 8.7KB；
  结论是「因素大且由任务类型决定」，不是「永远只给 7 个工具」。
- 唯一超噪声收益：关掉子代理 −13.3%（103835 vs 119802），方向是减功能，未设为默认。

## 4. 未跑的真实环境与不支持场景

- 无 Windows / macOS 真机：T02 阻塞，跨平台只做了 Linux 交叉构建（PE32+ x86_64 39.79s）。
- 无第三方安全工具链（semgrep/cargo-audit/cargo-deny）与独立评审方：Q06 阻塞，T04 依赖它。
- 无部署实例可灰度：S06 阻塞。
- 27 个 harness 里 3 个已安装可运行（pi 1.5.0、codex-cli 0.163.0、mini-swe-agent 2.4.6），7 个在 pypi/npm 上可解析但本轮没装，
  17 个没有尝试、标为 unverified（不再声称不可得）。mini-swe-agent 是任务书点名的可控臂，本轮真装了并在 fix-off-by-one 上跑通
  （`f05_msw_run.txt`）；逐项处置在 `f05_disposition.md`。可比子集要出完整排行榜还需 codex 的 OpenAI 凭据，本机没配。
- 端点不支持 provider-native compaction 与 tool search：J06/L06 阻塞，探测在 `j06_l06_probe.txt`。
- 未实现：无（原先记为「未实现」的 J03/J04/J05/O02/I06 本轮全部实现并带测试）。

## 5. 下一轮瓶颈

1. 单次重复噪声（约 2 个任务）让小于 5% 的收益不可判定 —— 下一轮每个 arm 至少 3 次重复。
2. 工具菜单是最大成本项，但「按任务类型自动裁剪」还没有证据；这是唯一值得先做的方向。
3. 外部能力缺口（Windows/macOS 真机、安全工具链、可灰度部署、codex 凭据）不补齐，T02/Q06/S06/T04/F05 无法推进。
4. 未完成 14 项（7 阻塞 + 7 实测判负），
   其中多项只差「一个 arm + 一次预注册配对」，瓶颈是运行预算而不是设计。

