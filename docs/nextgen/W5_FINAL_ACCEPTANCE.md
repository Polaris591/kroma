# 最终验收报告

日期：2026-10-21
基线提交：`61ffcc30`（交付源码原样，未改动）
门禁提交：`d434fe5`（本报告引用的所有日志都在这个提交上产生）
账本：`docs/nextgen/progress.json`（120 项，taskctl 校验 ok，0 open）

## 一、账本分布

| 状态 | 数量 | 含义 |
|---|---|---|
| VERIFIED | 81 | 有测试或 live 证据，验收点已核对 |
| DEFERRED_WITH_RATIONALE | 35 | 研究范围：机制本身不存在，bench 只能变化 8 个字段且已全部实验过 |
| BLOCKED_EXTERNAL | 4 | 外部条件：T02/Q06/S06/T04，附独立探测 |
| TODO | 0 | — |

没有第四种状态：每一项要么完成、要么有证据的推迟、要么有探测的外部阻塞。

## 二、门禁（在 `d434fe5` 上跑，日志 `evidence/final-gate-d434fe5.log`）

- `cargo build --workspace --locked` 通过
- `nextgen_rust_gate.sh` 打印 `RUST OFFLINE GATES PASSED`，[6] 全量 **1312 通过 / 0 失败**
- `run-sandbox-tests.sh` **56 通过 / 0 失败**（真 bwrap 0.12.0）
- fmt 与 `clippy -D warnings` 干净

全部 66 条 test 收据都绑这一份日志（commit `d434fe5`、该日志的 sha256、该提交的树 `1bcfdfa…`），
不存在"日志来自另一个提交"的情况。19 条 artifact 收据指向各自文件。

## 三、修掉的问题（都有红→绿证据）

1. 窗口检查被 `CapabilitySource::Configured` 挡住，默认路径完全没有整请求检查（KR-B04）
2. 动态工具晚于估算与检查（KR-B01）
3. WriterLease 重入时第三写者能进入、flock 提前释放（KR-D01）
4. 未创建路径的多种拼写各得一个写者槽位（KR-D02）
5. MCP SSE 要等连接关闭才结束：旧 5.000958537s → 新 0.00s（KR-K02）
6. 三套 token 估算口径对 CJK 差 2 倍（KR-B03）
7. 工具参数没有统一校验层（KR-H02，新增 `tool/schema_check.rs`）
8. DNS 解析两次的 rebinding 窗口 + 公网名解析进私网（KR-E06）

## 四、真实外部证据

- 三协议 live canary（cobblemon.asia / gpt-6.1-sol）：chat-completions in 1904 / cached 0 / out 5 / ttft 3336ms；
  openai-responses 1898/0/5/1789ms；anthropic-messages 1973/0/5/1819ms，均带 `request_identity_sha256`
- 真实第三方 MCP（`@modelcontextprotocol/server-everything` 经 npx stdio）：3 次 `mcp__everything__echo` 成功
- Windows 交叉构建：`--target x86_64-pc-windows-gnu` 通过（39.79s），产物 PE32+ x86-64

## 五、实验（七个机制，全部真跑）

| 机制 | 质量 | 成本 | 处置 |
|---|---|---|---|
| on_demand_tools | 6/6 vs 6/6（dev）、2/2 vs 2/2（holdout） | dev +8% / holdout −10% | 不晋升（两集矛盾） |
| adaptive_tournament k=2 | 相同 | +17% | 不启用 |
| counterfactual_probe | 相同 | +9% | 不启用 |
| projection_p3 | 相同 | −2.6% | 无结论（噪声内） |
| causal_pruning | 相同 | −0.5% | 无结论（噪声内） |
| require_verified | 相同 | −3.7% | 无结论（噪声内） |
| max_subagents=0 | 相同 | **−13.3%** | 唯一超噪声，方向是关功能 |

**噪声事实**：同一 baseline arm、同一 dev 套件，不同轮次通过率 6/6 与 4/6。单次重复漂移约 2 个任务，
所以几个百分点的"节省"都在噪声里，只有"质量零增益 + 成本明显上升"能作判负。

## 六、35 项研究与 4 项外部阻塞

35 项 DEFERRED 全是 experiment，阻塞物是"机制不存在"这一代码事实：`BenchStrategyConfig` 只有 8 个
可配置字段，全部已跑过配对实验，逐项映射与 `grep` 在 `experiment_arm_probe.md`。这是 taskbook 定义的
研究推迟（`MASTER_TASKS.md`），不是外部阻塞。

4 项 BLOCKED_EXTERNAL 的探测在 `blocked_probes.md`：T02（无 Windows/macOS 真机，macOS 无 SDK）、
Q06（无 semgrep/cargo-audit/cargo-deny，无独立评审方）、S06（无部署实例、无灰度人群）、
T04（依赖 Q06 与 T02）。

## 七、交付

远程 `git@github.com:Polaris591/kroma.git`，分支 `nextgen-r2-rustfix`；`main` 未改动，未打 tag。
证据清单 `SHA256SUMS.txt` 共 57 条，`sha256sum -c` 全部通过（0 FAILED）。
