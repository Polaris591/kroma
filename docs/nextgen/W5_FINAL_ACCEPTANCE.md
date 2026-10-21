# W5 最终验收报告

日期：2026-10-21
基线提交：`61ffcc30`（560 文件，交付原样，未改源码）
账本：`docs/nextgen/progress.json`（120 项，taskctl 校验通过）

## 一、最终门禁（在最终提交上重跑）

| 门禁 | 结果 | 日志 |
|---|---|---|
| cargo build --workspace --locked | 通过 | evidence/w5-final-gate.log |
| nextgen_rust_gate.sh | RUST OFFLINE GATES PASSED，[6] 全量 1295 通过 / 0 失败 | 同上 |
| run-sandbox-tests.sh | 7 target / 56 通过 / 0 失败（真 bwrap 0.12.0） | 同上 |
| fmt / clippy -D warnings | 通过（gate 脚本内执行） | 同上 |
| taskctl validate --progress | ok | — |

一次失败记录：全量并行下 `session_ownership::tests::different_workspaces_are_independent_and_same_workspace_is_exclusive` 偶发失败（跨进程锁分支返回了带 pid 的 owner 串）。独占重跑两次 14/14 通过，全量 lib 并行重跑 402/402 通过。日志：evidence/w5-flake-session-ownership-run1.log。按门禁规则记录，未删除或跳过。

## 二、默认配置不依赖未晋升实验

`AgentRunConfig` 默认：`projection_profile: None`（即 P2）、`projection_candidate: None`、`causal_pruning: false`、`on_demand_tools: false`。没有任何实验开关默认开启。

## 三、120 项终态分布

- VERIFIED：48
- IMPLEMENTED_UNVERIFIED：37（实现与测试目标都在，批量跑出 56 harness / 362 项全过，但未逐条核对验收点）
- DEFERRED_WITH_RATIONALE：35（全部是 experiment；48-job 预注册评估缺 `scripts/run-exp00.js`，README 自己标注未实现）
- TODO：0

## 四、真正修掉的问题（都有红→绿证据）

1. **窗口检查被来源条件挡住**（KR-B04）：`source == Configured` 让默认（未配置）路径完全没有整请求窗口检查。删掉多余条件，并把 resolved 窗口喂给压缩器，避免 1.5 MiB 字节水位远高于真实窗口导致压缩永不触发。红测：`ModelStop → ContextBudgetExceeded`。
2. **动态工具晚于估算与检查**（KR-B01）：激活的工具在估算和窗口检查之后才并入请求。改为先构造并冻结工具集，再测量。红测同前。
3. **WriterLease 重入语义错误**（KR-D01）：Drop 无条件删注册表，重入 lease 还活着时第三 owner 能拿到工作区，跨进程 flock 也被提前释放。改为引用计数 + 共享 `Arc<File>`。两条红测在旧实现下 FAILED。
4. **未创建路径的别名双写者**（KR-D02）：`canonicalize` 失败时回退到调用方字面拼写，`ws`、`./ws`、`ws/`、绝对路径各得一个写者槽位。加词法规范化。红测在旧 fallback 下 FAILED。
5. **MCP SSE 要等连接关闭才结束**（KR-K02）：整包读取 body 后才解析，服务器保持连接时调用一直等到 deadline。改为增量解析，终结帧即返回。红测：旧实现 `took 5.000958537s`，新实现 `0.00s`。
6. **估算口径三套并存**（KR-B03）：reserve 用 bytes/4，窗口检查用 ascii/4+非ASCII/2，对 CJK 差最多 2 倍。统一为一条保守估算。

## 五、真实外部证据

- **三协议 live canary**（cobblemon.asia，gpt-6.1-sol）：chat-completions in 1904 / cached 0 / out 5 / ttft 3336ms；openai-responses 1898/0/5/1789ms；anthropic-messages 1973/0/5/1819ms。每条都带 provider_reported 模型 id 与 `request_identity_sha256`。
- **真实第三方 MCP**（`@modelcontextprotocol/server-everything` 经 npx stdio）：3 次 `mcp__everything__echo` 成功，4 轮对话，model_stop。不是本地 fixture。
- **延迟归因**：`scripts/nextgen_latency_breakdown.py` 显示 canary 里 94.9% 时间在首 token 之前（3336/3524ms），并明确标注 connect+TLS+序列化+prefill 仍是混合的。
- **实验 plumbing**：exp-00 dry-run digest 复算一致（05757bf3），单任务诊断真跑（4 轮 / 9 工具 / 缓存命中 8.9% / 26.2s）。

## 六、明确没有做到的事

- 35 个实验没有跑统计结论：缺 `scripts/run-exp00.js`。单任务诊断只能证明 plumbing。
- 37 项没有逐条核对验收点：只有"测试目标存在且全绿"的证据。
- 端到端延迟未拆分到 connect/TLS/序列化：`ProviderCompletion` 有 62 处构造点，拆分是独立改动。
- 真实远端 MCP（HTTP/SSE）未验证：只有 stdio 第三方实现。
- 跨平台构建未验证：本机是 Linux，Windows/macOS 无法证明。
- ACP `workspace.graph` 在 560 文件仓库上 60 秒无响应（KR-R01 记录），未修复。
- 跨平台构建未验证（同上）。
- ~~没有 push 到 GitHub~~：**已推送**。`git@github.com:Polaris591/kroma.git` 分支
  `nextgen-r2-rustfix` = `651d1e1`，`main` 仍为 `91597c4` 未被改动，未创建任何 tag。

## 六点五、交付分支

远程：`git@github.com:Polaris591/kroma.git`
分支：`nextgen-r2-rustfix`（本地 `main` → 远程新分支，独立历史）
本地 HEAD：`651d1e18e318f5ebea487bb14cfa8935d64113af`
远程 `main`：`91597c4a7e44e5ec6b95e1bca3fe8b039e2a29be`（未改动）
未创建 tag，未发布。

## 七、证据目录

`docs/nextgen/` 下：baseline.json、host_capabilities.json、coverage_ledger.json、spawn_points.json、mcp_contract.json、dependency_licenses.json、gate_receipt.json、e05_canary_summary.json、progress.json，以及 `evidence/` 下的全部门禁日志、live canary 日志、MCP 互操作日志、flake 日志。`SHA256SUMS.txt` 覆盖以上全部文件，可 `sha256sum -c` 自校验。
