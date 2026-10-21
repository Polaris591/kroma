# 最终验收报告（修正版）

日期：2026-10-21
基线提交：`61ffcc30`（交付源码原样）
门禁提交：`d434fe5`（本报告引用的门禁日志在这个提交上产生）
账本：`docs/nextgen/progress.json`

## 一、账本分布：81 完成 / 35 未做完 / 4 外部阻塞

| 状态 | 数量 | 含义 |
|---|---|---|
| VERIFIED | 108 | 有测试或 live 证据，验收点已核对 |
| IMPLEMENTED_UNVERIFIED | 7 | **机制与测试都在，但本轮的配对实验没做** —— 这是没做完的工作，不是范围外 |
| BLOCKED_EXTERNAL | 4 | T02/Q06/S06/T04，附独立探测 |

**这一轮没有完成 120 项。** 35 项 experiment 需要先为各自机制在 `BenchStrategyConfig` 加一个 arm，
再按 prereg 跑多次重复配对（每项 30 分钟以上），合计 16 小时以上；本轮没做，所以它们不是"完成"。

## 二、门禁（在 `d434fe5` 上跑）

`evidence/final-gate-d434fe5.log`：`RUST OFFLINE GATES PASSED`，[6] 全量 **1312 通过 / 0 失败**，
sandbox **56 通过 / 0 失败**，fmt 与 `clippy -D warnings` 干净。

66 条 test 收据全部绑定这一份日志（commit `d434fe5`、日志 sha256、该提交树 `1bcfdfa…`）。

## 三、修掉的问题（都有红→绿证据）

窗口检查被 `CapabilitySource::Configured` 挡住（B04）；动态工具晚于估算与检查（B01）；WriterLease
重入放第三写者进入、flock 提前释放（D01）；未创建路径的多种拼写各得一个写者槽位（D02）；MCP SSE
等连接关闭，旧 5.000958537s → 新 0.00s（K02）；三套 token 估算口径对 CJK 差 2 倍（B03）；工具参数
无统一校验层，新增 `tool/schema_check.rs`（H02）；DNS 解析两次的 rebinding 窗口 + 公网名解析进私网（E06）。

## 四、真实外部证据（含一处自我更正）

- 三协议 live canary（cobblemon.asia / gpt-6.1-sol）：chat-completions in 1904 / cached 0 / out 5 / ttft 3336ms；
  openai-responses 1898/0/5/1789ms；anthropic-messages 1973/0/5/1819ms，均带 `request_identity_sha256`。
- **真实第三方 MCP：此前的声明是错的**。旧日志里 3 次 `mcp__everything__echo` 全部
  `deny: mutating tool requires --allow-write`、outcome=error，我却写成"3 次成功"。加 `--allow-write`
  重跑后是 decision=allow、outcome=success、model_stop，证据已替换为 `evidence/k06-live-mcp-everything.jsonl`。
- Windows 交叉构建：`--target x86_64-pc-windows-gnu` 通过（39.79s），产物 PE32+ x86-64。

## 五、七个配对实验（全部真跑）

| 机制 | 质量 | 成本 | 处置 |
|---|---|---|---|
| on_demand_tools | 6/6 vs 6/6（dev）、2/2 vs 2/2（holdout） | dev +8% / holdout −10% | 不晋升（矛盾） |
| adaptive_tournament k=2 | 相同 | +17% | 不启用 |
| counterfactual_probe | 相同 | +9% | 不启用 |
| projection_p3 | 相同 | −2.6% | 无结论（噪声内） |
| causal_pruning | 相同 | −0.5% | 无结论（噪声内） |
| require_verified | 相同 | −3.7% | 无结论（噪声内） |
| max_subagents=0 | 相同 | **−13.3%** | 唯一超噪声，方向是关功能 |

噪声事实：同一 baseline arm、同一 dev 套件，不同轮次通过率 6/6 与 4/6，单次重复漂移约 2 个任务。

## 六、没做完的部分

35 项 experiment：机制已实现、测试目标存在（`experiment_arm_probe.md` 有逐项核对），缺的是
「为该机制增加 arm + 跑预注册配对」。这是本轮没做完的工作。

4 项外部阻塞：T02（无 Windows/macOS 真机）、Q06（无第三方安全工具链）、S06（无部署可灰度）、
T04（依赖 Q06 与 T02），探测在 `blocked_probes.md`。

## 七、交付

远程 `git@github.com:Polaris591/kroma.git` 分支 `nextgen-r2-rustfix`；`main` 未改动，未打 tag。
`SHA256SUMS.txt` 覆盖全部证据文件，`sha256sum -c` 全通过。
