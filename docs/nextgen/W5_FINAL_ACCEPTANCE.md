# 最终验收报告

日期：2026-10-22
基线提交：`61ffcc30`
代码提交：`e091fbc`（门禁 `evidence/final-gate-8f63776.log`：GATE_EXIT=0 / SANDBOX_EXIT=0，1410 通过 / 0 失败）
账本：`docs/nextgen/progress.json`（`taskctl validate --progress` ok:true）

## 一、账本分布

| 状态 | 数量 |
|---|---|
| VERIFIED | 101 |
| REJECTED_WITH_EVIDENCE | 6 |
| BLOCKED_EXTERNAL | 7 |
| DEFERRED_WITH_RATIONALE | 6 |
| IMPLEMENTED_UNVERIFIED | 0 |
| TODO | 0 |

外部阻塞：`KR-F05, KR-J06, KR-L06, KR-Q06, KR-S06, KR-T02, KR-T04`
实测判负：`KR-G03, KR-H04, KR-H06, KR-J02, KR-M04, KR-N03`
研究范围不成立：`KR-I02, KR-I06, KR-J03, KR-J04, KR-J05, KR-O02`

探测证据：`blocked_probes.md`（T02/Q06/S06/T04）、`evidence/j06_l06_probe.txt`（端点无 provider-native
compaction 与 tool search）、`evidence/f05_disposition.md` + `f05_availability.txt`（27 项逐项处置，本机只有 pi
可运行）、`evidence/i02_root_cause.md`、`adr_j06_l06_o02_f05.md`。

## 二、门禁与收据

`evidence/final-gate-8f63776.log`：[0]–[7] 全过，全量 1410 通过 / 0 失败，strict clippy 通过，sandbox 通过。
测量与判负项各自绑自己的文件（`exp-*.json`、`e05-canary-*.jsonl`、`k06-live-mcp-everything.jsonl`、
`report.json`、`r02_graph_stat_cache.log`、`r06_baseline.log` 等），实验用的 arm 定义在 `evidence/arms-*.json`。

## 三、做出来的东西

7 个 bug 红→绿修好：窗口检查被来源条件挡住、动态工具晚于估算、WriterLease 重入、路径别名双写、MCP SSE
5.0009s→0.00s、三套 token 估算口径、DNS 二次解析+私网落地；另加工具参数统一校验层。

三个真实现：I03 结果缓存（随机化测试抓出两个 stale bug，前 8KiB hash 修好）、S02 影子选择器（接入
`prepare_run`，每 run 写「推荐/实际/反事实估计/是否应用」到 `<state_dir>/shadow_selector.jsonl`，不改控制流）、
N04/M04/R05/H06 的 arm 接线。

16 个 live 配对实验，7 个判负：H04 +22%、N03 +17%、J02 +3% 且丢可恢复性、G03 命中率涨 token 也涨、H06 −0.16%、
M04 +4.5%、R05 全开 +7.5%。最大杠杆是工具菜单（省 55%）；唯一超噪声收益是关掉子代理 −13.3%。

R02 用自带测试重测：冷 1030271µs → 热 448052µs（−56.5%），stat 快速路径实测没命中，先前的 27% 已撤回。

## 四、证据自校验

```
cd docs/nextgen && sha256sum -c SHA256SUMS.txt
python3 ../../scripts/verify_ledger_binding.py . progress.json
python3 taskctl.py validate --progress progress.json --source-root ../..
```

## 五、交付

`git@github.com:Polaris591/kroma.git` 分支 `nextgen-r2-rustfix`；`main` 未动，无 tag。
