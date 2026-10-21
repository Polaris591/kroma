# 最终验收报告

日期：2026-10-22
基线提交：`61ffcc30`（交付源码原样）
门禁提交：`cb8f764`（build + nextgen_rust_gate.sh + run-sandbox-tests.sh 都在这个提交上跑）
账本：`docs/nextgen/progress.json`

## 一、账本分布

| 状态 | 数量 |
|---|---|
| VERIFIED | 102 |
| IMPLEMENTED_UNVERIFIED | 5 |
| REJECTED_WITH_EVIDENCE | 6 |
| BLOCKED_EXTERNAL | 7 |
| TODO | 0 |

未完成的 18 项：`KR-F05, KR-G03, KR-H04, KR-H06, KR-I02, KR-I03, KR-I06, KR-J02, KR-J03, KR-J04, KR-J06, KR-L06, KR-N03, KR-O02, KR-Q06, KR-S06, KR-T02, KR-T04`

7 项外部阻塞（F05 二十六个 harness 不可得、J06/L06 端点不支持 provider-native compaction 与 tool
search、Q06 无第三方安全工具链、S06 无部署可灰度、T02 无 Windows/macOS 真机、T04 依赖前两项），探测见
`blocked_probes.md` 与 `adr_j06_l06_o02_f05.md`。I03/I06 有机制与测试、缺验收测量；I02 无实现（两次撤回）；
J04/O02 按 ADR 决定不实现。

## 二、门禁（`cb8f764`）

`evidence/final-gate-cb8f764.log`：[0]–[7] 全过；[6] 全量回归 1314 通过 / 0 失败；[7] strict clippy 通过；
`run-sandbox-tests.sh` 段 56 通过 / 0 失败。

## 三、做出来的东西

7 个 bug 红→绿修好：窗口检查被来源条件挡住、动态工具晚于估算、WriterLease 重入、路径别名双写、MCP SSE
5.0009s→0.00s、三套 token 估算口径、DNS 二次解析+私网落地；另加工具参数统一校验层。

12 个 live 配对实验（原始 JSON 在 `evidence/`）：工具菜单 7 个 vs 全量省 55%（最大杠杆）；锦标赛 +17%、反事实
探测 +9%、efficient-candidate 提示 +22%、masking/P1 +3% 且丢可恢复性、H06 内联窗口减半 −0.16% 噪声内 —— 五项判负；
缓存三臂命中率涨但 token 也涨；P3/causal/require_verified 噪声内；I05 graph 缓存开时 wall 250s vs 关 383s。
默认配置只保留有实测收益的部分。

一次真性能整改（R02）：graph 缓存 (len,mtime) 快速路径，热缓存 1217ms→891ms（省 27%）。

## 四、证据自校验

```
cd docs/nextgen && sha256sum -c SHA256SUMS.txt
python3 ../../scripts/verify_ledger_binding.py . progress.json
```

## 五、交付

`git@github.com:Polaris591/kroma.git` 分支 `nextgen-r2-rustfix`；`main` 未动，无 tag。
