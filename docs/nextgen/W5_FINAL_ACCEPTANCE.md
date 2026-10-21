# 最终验收报告

日期：2026-10-22
基线提交：`61ffcc30`（交付源码原样）
门禁提交：`cb8f764`（本报告引用的门禁日志在这个提交上产生）
账本：`docs/nextgen/progress.json`

## 一、账本分布（**没有完成 120 项**）

| 状态 | 数量 |
|---|---|
| VERIFIED（有测试或 live 收据） | 88 |
| IMPLEMENTED_UNVERIFIED | 0 |
| BLOCKED_EXTERNAL（附探测） | 28 |
| DEFERRED_WITH_RATIONALE | 0 |
| REJECTED_WITH_EVIDENCE（实测判负） | 4 |
| TODO | 0 |

未完成的 25 项：`KR-F05, KR-H05, KR-H06, KR-I02, KR-I03, KR-I04, KR-I05, KR-I06, KR-J04, KR-J06, KR-L06, KR-M04, KR-M06, KR-N04, KR-O02, KR-O03, KR-O05, KR-Q06, KR-R05, KR-R06, KR-S02, KR-S03, KR-S06, KR-T02, KR-T04`。

## 二、门禁（在 `cb8f764` 上跑）

`evidence/final-gate-cb8f764.log`：`cargo build` + `nextgen_rust_gate.sh`（含 [6] 全量回归与 [7] 严格
clippy）+ `run-sandbox-tests.sh`。具体数字见该日志的 `[6]` 段与 `RUST OFFLINE GATES PASSED` 之后。

88 条 VERIFIED 收据绑定这份日志（commit `cb8f764`、日志 sha256、该提交树）。

## 三、真正做出来的东西

**修掉的 bug（各有红→绿证据）**：窗口检查被 `CapabilitySource::Configured` 挡住（B04）；动态工具晚于
估算与检查（B01）；WriterLease 重入放第三写者进来、flock 提前释放（D01）；未创建路径的多种拼写各得
一个写者槽位（D02）；MCP SSE 等连接关闭，旧 5.000958537s → 新 0.00s（K02）；三套 token 估算口径对 CJK
差 2 倍（B03）；工具参数无统一校验层（H02，新增 `tool/schema_check.rs`）；DNS 解析两次的 rebinding 窗口
+ 公网名解析进私网（E06）。

**真实实验（11 个配对运行，全部 live API）**：

| 机制 | 质量 | 成本 | 处置 |
|---|---|---|---|
| 工具菜单 7 个 vs 全量（F04） | 相同 6/6 | **−55%** | 最大杠杆，方向是减工具 |
| on_demand_tools（H03） | 相同 | dev +8% / holdout −10% | 不晋升（矛盾） |
| adaptive_tournament k=2（N03） | 相同 | +17% | 判负 |
| counterfactual_probe | 相同 | +9% | 判负 |
| efficient-candidate 提示（H04） | 相同 | +22% | 判负 |
| masking/P1 vs stub/P2（J02） | 相同 | +3% 且丢可恢复性 | 不晋升 |
| cache off/implicit/explicit（G03） | 6/6、5/6、6/6 | 命中率 0.299→0.665→0.76 但 token 也涨 | 默认 off |
| projection_p3 | 相同 | −2.6% | 噪声内 |
| causal_pruning | 相同 | −0.5% | 噪声内 |
| require_verified | 相同 | −3.7% | 噪声内 |
| max_subagents=0 | 相同 | **−13.3%** | 唯一超噪声，方向是关功能 |

**噪声事实**：同一 baseline arm、同一 dev 套件，不同轮次通过率 6/6 与 4/6，单次重复漂移约 2 个任务。

**真实性能整改（R02）**：graph 缓存命中时仍读全文+算 hash，加 `(len,mtime)` 快速路径后热缓存
1217ms→891ms（省 27%），全量测试无回归。

## 四、明确没做完的

- **16 项 IMPLEMENTED_UNVERIFIED**：实现与测试都在，但验收要求的那个测量没做（多为 experiment 类：
  需要为机制加 arm 并跑预注册配对）。含 I02 那条链（I02/I03/I06/R05/R06/S02/S03）。
- **7 项 BLOCKED_EXTERNAL**：T02（无 Windows/macOS 真机）、Q06（无第三方安全工具链）、S06（无部署可
  灰度）、T04（依赖 Q06+T02）、F05（27 个 harness 中只有 pi/codex 可得）、J06/L06（端点不支持 provider
  native compaction / tool search）。探测在 `blocked_probes.md` 与 `adr_j06_l06_o02_f05.md`。
- **2 项 DEFERRED**：I05（增量 RepoGraph，依赖 I03）、J04（CADOC 对象卡，无 held-out 证据）。

I02 的收益已测得极清楚（同 limit=10：100 字节文件 63.8µs，2 MiB 文件 110.4ms，1732 倍），但实现要把
`tools/read.rs` 的 streaming read 抽成函数再加 seek 旁路（约 150 行），两次尝试都因会在测试全绿前留下
半接线路径而撤回，顺序写在 `i02_line_index_attempt.md`。

## 五、交付

远程 `git@github.com:Polaris591/kroma.git` 分支 `nextgen-r2-rustfix`；`main` 未改动，未打 tag。
`SHA256SUMS.txt` 覆盖证据文件，`sha256sum -c` 全通过。

## 六、证据自校验方式

```
cd docs/nextgen && sha256sum -c SHA256SUMS.txt    # 全部 OK
python3 ../../scripts/verify_ledger_binding.py . progress.json   # bound: true
python3 taskctl.py validate --progress docs/nextgen/progress.json  # ok（结构校验）
```

`SHA256SUMS.txt` 不包含 `progress.json` 自身（它写完就会变），账本的身份由 git 提交固定。
