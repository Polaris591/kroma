# 最终验收报告

日期：2026-10-22
基线提交：`61ffcc30`（交付源码原样）
最终门禁提交：`69678a2`（build + nextgen_rust_gate.sh + run-sandbox-tests.sh 都在这个提交上跑，原始日志
`evidence/final-gate-69678a2.log`）
账本：`docs/nextgen/progress.json`

## 一、账本分布（120 项未全部完成）

| 状态 | 数量 |
|---|---|
| VERIFIED | 97 |
| IMPLEMENTED_UNVERIFIED | 6 |
| REJECTED_WITH_EVIDENCE | 6 |
| DEFERRED_WITH_RATIONALE | 5 |
| BLOCKED_EXTERNAL | 6 |
| TODO | 0 |

未完成的 23 项：`KR-F05, KR-G03, KR-H04, KR-H06, KR-I02, KR-I04, KR-I06, KR-J02, KR-J03, KR-J04, KR-J06, KR-L06, KR-M04, KR-N03, KR-N04, KR-O02, KR-Q06, KR-R05, KR-S02, KR-S03, KR-S06, KR-T02, KR-T04`

- 6 项外部阻塞：F05（26/27 harness 不可得）、J06/L06（端点不支持 provider-native compaction 与
  tool search）、Q06（无第三方安全工具链）、S06（无部署可灰度）、T02（无 Windows/macOS 真机）、T04（依赖前两项）。
- 5 项研究范围不成立：I02（根因是整文件 IO+SHA256，行索引动不了主项）、I06（树里没有语法
  解析器，是新建子系统）、J04/O02（ADR 决定不实现）。
- 6 项机制与门禁测试在、但没有自己的留出配对测量：R05、S02、S03、M04、N04、I04。

## 二、最终门禁（`69678a2`）

`evidence/final-gate-69678a2.log`：[0]–[7] 全过，全量回归 1407 通过 / 0 失败，strict clippy 通过，
`run-sandbox-tests.sh` 通过（GATE_EXIT=0 / SANDBOX_EXIT=0）。101 条收据重绑到这份日志。

## 三、做出来的东西

7 个 bug 红→绿修好：窗口检查被来源条件挡住、动态工具晚于估算、WriterLease 重入、路径别名双写、MCP SSE
5.0009s→0.00s、三套 token 估算口径、DNS 二次解析+私网落地；另加工具参数统一校验层。

I03 结果缓存真实现（opt-in），随机化状态变更测试当场抓出两个 stale bug（(mtime,len) 第 6 轮返回第 3 轮字节；
tmpfs 时间戳粗导致 ctime 也救不了；前 8KiB hash 修好）。R02 用自带测试重测：冷 991ms → 热 442ms（−55%，parse cache
的功劳），我加的 stat 快速路径实测没命中（936ms，比热路径还慢），先前报告里的 27% 撤回。R06 用
measure-local-baseline.py 留了 p0/p2/p3/p4 原始数据。

13 个 live 配对实验，6 个判负（锦标赛 +17%、反事实探测 +9%、efficient-candidate +22%、masking +3% 且丢可恢复性、
H06 内联减半 −0.16%、G03 缓存三臂），唯一超噪声收益是关掉子代理 −13.3%；工具菜单 7 个 vs 全量省 55% 是最大杠杆。
M06 约束/陷阱 17631 vs 22222、22211 vs 20572。

## 四、证据自校验

```
cd docs/nextgen && sha256sum -c SHA256SUMS.txt
python3 ../../scripts/verify_ledger_binding.py . progress.json
```

## 五、交付

`git@github.com:Polaris591/kroma.git` 分支 `nextgen-r2-rustfix`；`main` 未动，无 tag。
