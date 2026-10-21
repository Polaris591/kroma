# 最终验收报告

日期：2026-10-22
基线提交：`61ffcc30`（交付源码原样）
门禁提交：`cb8f764`（build + nextgen_rust_gate.sh + run-sandbox-tests.sh 都在这个提交上跑）
账本：`docs/nextgen/progress.json`

## 一、账本分布（**120 项没有全部完成**）

| 状态 | 数量 |
|---|---|
| VERIFIED（有测试或自己的 live 收据） | 90 |
| IMPLEMENTED_UNVERIFIED（实现与测试在，验收测量未做） | 18 |
| REJECTED_WITH_EVIDENCE（实测判负） | 5 |
| BLOCKED_EXTERNAL（外部能力缺失，附探测） | 7 |
| TODO | 0 |

未完成的 30 项：

`KR-F05, KR-G03, KR-H04, KR-H05, KR-H06, KR-I02, KR-I03, KR-I04, KR-I05, KR-I06, KR-J02, KR-J03, KR-J04, KR-J06, KR-L06, KR-M04, KR-M06, KR-N03, KR-N04, KR-O02, KR-O03, KR-O05, KR-Q06, KR-R05, KR-R06, KR-S02, KR-S03, KR-S06, KR-T02, KR-T04`

其中 7 项是外部阻塞（F05 二十六个 harness 不可得、J06/L06 端点不支持 provider-native
compaction 与 tool search、Q06 无第三方安全工具链、S06 无部署可灰度、T02 无 Windows/macOS 真机、T04 依赖前两项），
探测见 `blocked_probes.md` 与 `adr_j06_l06_o02_f05.md`。其余 18 项是**内部未完成**：
实现与测试都在，缺的是给该机制加一个 arm 再跑预注册配对；这是本轮没做完的工作，不是外部阻塞。

## 二、门禁（在 `cb8f764` 上）

`evidence/final-gate-cb8f764.log`：[0]–[7] 全部通过；[6] 全量回归 **1314 通过 / 0 失败**；[7] 严格 clippy 通过；
`run-sandbox-tests.sh` 段 **56 通过 / 0 失败**（并在这份日志里，不是引用旧日志）。

90 条 VERIFIED 收据绑定这份日志（commit、日志 sha256、该提交树）。

## 三、真正做出来的东西

**修掉的 bug（各有红→绿证据）**：窗口检查被 `CapabilitySource::Configured` 挡住（B04）；动态工具晚于估算与
检查（B01）；WriterLease 重入放第三写者进来、flock 提前释放（D01）；未创建路径的多种拼写各得一个写者槽位（D02）；
MCP SSE 等连接关闭（K02，旧 5.000958537s → 新 0.00s）；三套 token 估算口径对 CJK 差 2 倍（B03）；工具参数无统一
校验层（H02，新增 `tool/schema_check.rs`）；DNS 解析两次的 rebinding 窗口 + 公网名解析进私网（E06）。

**11 个 live 配对实验**（原始 JSON 在 `evidence/`）：工具菜单 7 个 vs 全量 **省 55%** token（最大杠杆）；
锦标赛 k=2 **+17%**、反事实探测 **+9%**、efficient-candidate 提示 **+22%**、masking/P1 **+3% 且丢可恢复性** ——
四项判负；on_demand_tools dev +8% / holdout −10% 矛盾不晋升；缓存三臂命中率涨但 token 也涨，默认 off；
P3 −2.6%、causal_pruning −0.5%、require_verified −3.7% 在噪声内；唯一超噪声收益是关掉子代理 **−13.3%**（方向是
减功能）。所有默认配置一个都没开。

**一次真性能整改（R02）**：graph 缓存命中时仍读全文+算 hash，加 `(len, mtime)` 快速路径后热缓存 1217ms → 891ms
（省 27%），全量无回归。

**方法学记录**：同一 baseline arm、同一 dev 套件，不同轮次通过率 6/6 与 4/6，单次重复漂移约 2 个任务 —— 所以
几个百分点的"节省"只能算无结论。

## 四、明确没做完的

- 18 项内部未完成（见上）。
- I02 的收益已测得极清楚（同 limit=10：100 字节文件 63.8µs vs 2 MiB 文件 110.4ms，**1732 倍**），但实现要把
  `tools/read.rs` 的 streaming read 抽成函数再加 seek 旁路（约 150 行），两次尝试都因会在测试全绿前留下半接线
  路径而撤回，顺序写在 `i02_line_index_attempt.md`。

## 五、证据自校验

```
cd docs/nextgen && sha256sum -c SHA256SUMS.txt                              # 全部 OK
python3 ../../scripts/verify_ledger_binding.py . progress.json              # bound: true
```

`SHA256SUMS.txt` 不含 `progress.json` 自身（写完就变），账本身份由 git 提交固定。

## 六、交付

远程 `git@github.com:Polaris591/kroma.git` 分支 `nextgen-r2-rustfix`；`main` 未改动，未打 tag。
