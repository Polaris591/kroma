# 最终验收报告

日期：2026-10-22
基线提交：`61ffcc30`（交付源码原样）
账本：`docs/nextgen/progress.json`

## 一、账本分布（120 项未全部完成）

| 状态 | 数量 |
|---|---|
| VERIFIED | 99 |
| IMPLEMENTED_UNVERIFIED | 4 |
| REJECTED_WITH_EVIDENCE | 6 |
| DEFERRED_WITH_RATIONALE | 4 |
| BLOCKED_EXTERNAL | 7 |
| TODO | 0 |

未完成的 21 项：`KR-F05, KR-G03, KR-H04, KR-H06, KR-I02, KR-I06, KR-J02, KR-J03, KR-J04, KR-J06, KR-L06, KR-N03, KR-O02, KR-Q06, KR-R05, KR-R06, KR-S02, KR-S03, KR-S06, KR-T02, KR-T04`

- 7 项外部阻塞：F05（二十六个 harness 不可得）、J06/L06（端点不支持 provider-native compaction
  与 tool search）、Q06（无第三方安全工具链）、S06（无部署可灰度）、T02（无 Windows/macOS 真机）、T04（依赖前两项）。
  探测见 `blocked_probes.md` 与 `adr_j06_l06_o02_f05.md`。
- 4 项研究范围不成立、不做：I02（根因是整文件 IO+SHA256，行索引动不了主项，见
  `evidence/i02_root_cause.md`）、I06（树里没有语法解析器，是新建子系统）、J04/O02（ADR 决定不实现）。
- 4 项机制与门禁测试在、但没有自己的留出配对测量：R05、R06、S02、S03。

## 二、门禁

`evidence/final-gate-cb8f764.log`：[0]–[7] 全过；[6] 全量回归 1314 通过 / 0 失败；[7] strict clippy 通过；
`run-sandbox-tests.sh` 段 56 通过 / 0 失败。

## 三、做出来的东西

7 个 bug 红→绿修好：窗口检查被来源条件挡住、动态工具晚于估算、WriterLease 重入、路径别名双写、MCP SSE
5.0009s→0.00s、三套 token 估算口径、DNS 二次解析+私网落地；另加工具参数统一校验层。

I03 结果缓存真实现（`tools/read.rs`，opt-in），随机化状态变更测试当场抓出两个 stale bug（(mtime,len) 在第 6 轮
返回第 3 轮字节；tmpfs 时间戳粗导致 ctime 也救不了；最后前 8 KiB hash 修好）。全量 1274 通过 / 0 失败。

13 个 live 配对实验：工具菜单 7 个 vs 全量省 55%（最大杠杆）；锦标赛 +17%、反事实探测 +9%、efficient-candidate
提示 +22%、masking +3% 且丢可恢复性、H06 内联减半 −0.16% —— 六项判负；缓存三臂命中率涨 token 也涨；I05 graph
缓存开 250s vs 关 383s；M06 约束/陷阱 17631 vs 22222、22211 vs 20572。默认只保留有实测收益的部分。

一次真性能整改（R02）：graph 缓存 (len,mtime) 快速路径，热缓存 1217ms→891ms（省 27%）。

## 四、证据自校验

```
cd docs/nextgen && sha256sum -c SHA256SUMS.txt
python3 ../../scripts/verify_ledger_binding.py . progress.json
```

## 五、交付

`git@github.com:Polaris591/kroma.git` 分支 `nextgen-r2-rustfix`；`main` 未动，无 tag。
