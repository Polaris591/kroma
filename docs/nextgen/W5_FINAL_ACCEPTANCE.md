# 最终验收报告

日期：2026-10-22
基线提交：`61ffcc30`（交付源码原样）
最终门禁提交：`8f63776`（原始日志 `evidence/final-gate-8f63776.log`：GATE_EXIT=0 / SANDBOX_EXIT=0，
全量 1410 通过 / 0 失败，strict clippy 通过，sandbox 通过）
账本：`docs/nextgen/progress.json`

## 一、账本分布

| 状态 | 数量 |
|---|---|
| VERIFIED | 102 |
| REJECTED_WITH_EVIDENCE | 7 |
| BLOCKED_EXTERNAL | 7 |
| DEFERRED_WITH_RATIONALE | 4 |
| IMPLEMENTED_UNVERIFIED | 0 |
| TODO | 0 |

未完成的 18 项：

- 外部阻塞 7 项：`KR-F05, KR-J06, KR-L06, KR-Q06, KR-S06, KR-T02, KR-T04`
- 实测判负 7 项：`KR-G03, KR-H04, KR-H06, KR-J02, KR-J03, KR-M04, KR-N03`
- 研究范围不成立 4 项：`KR-I02, KR-I06, KR-J04, KR-O02`

阻塞与推迟的探测在 `blocked_probes.md`、`adr_j06_l06_o02_f05.md`、`evidence/i02_root_cause.md`。

## 二、门禁（`8f63776`）

[0]–[7] 全过；全量回归 1410 通过 / 0 失败；strict clippy 通过；`run-sandbox-tests.sh` 通过。
103 条收据绑定这份日志，判负与测量项各自绑到自己的 JSON/日志（`evidence/exp-*.json`、
`evidence/r06_baseline.log`、`evidence/r02_graph_stat_cache.log` 等），实验用的 arm 定义也复制进了
`evidence/arms-*.json`。

## 三、做出来的东西

7 个 bug 红→绿修好：窗口检查被来源条件挡住、动态工具晚于估算、WriterLease 重入、路径别名双写、MCP SSE
5.0009s→0.00s、三套 token 估算口径、DNS 二次解析+私网落地；另加工具参数统一校验层。

三个真实现：I03 结果缓存（opt-in，随机化测试抓出两个 stale bug，前 8KiB hash 修好）、S02 影子选择器
（`shadow_selector.rs`，已接入 `prepare_run`，每次运行把「推荐 / 实际 / 反事实估计 / 是否应用」写进
`<state_dir>/shadow_selector.jsonl`，控制流不受影响）、N04/M04/R05/H06 的 arm 接线。

16 个 live 配对实验，7 个判负：H04 +22%、N03 +17%、J02 +3% 且丢可恢复性、G03 命中率涨 token 也涨、
H06 −0.16%（噪声内）、M04 +4.5%、R05 全开 +7.5%。最大杠杆是工具菜单（7 个 vs 全量省 55%）；唯一超噪声收益
是关掉子代理 −13.3%。默认配置只保留有实测支撑的部分。

R02 用自带测试重测：冷 991ms → 热 442ms（−55%，parse cache 的功劳）；我加的 stat 快速路径实测没命中，
先前报告里的 27% 已撤回。R06 用 measure-local-baseline.py 留了 p0/p2/p3/p4 原始数据。

## 四、证据自校验

```
cd docs/nextgen && sha256sum -c SHA256SUMS.txt
python3 ../../scripts/verify_ledger_binding.py . progress.json
```

## 五、交付

`git@github.com:Polaris591/kroma.git` 分支 `nextgen-r2-rustfix`；`main` 未动，无 tag。
