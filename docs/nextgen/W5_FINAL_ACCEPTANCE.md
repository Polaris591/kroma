# 最终验收报告（返工后）

日期：2026-10-21
基线提交：`61ffcc30`（交付源码原样，未改动）
账本：`docs/nextgen/progress.json`（120 项，taskctl 校验 ok）

## 一、账本真实分布

| 状态 | 数量 | 含义 |
|---|---|---|
| REJECTED_WITH_EVIDENCE | 1 | KR-N03：锦标赛 k=2 质量零增益、成本 +17%，判负不启用 |
| VERIFIED | 78 | 有测试或 live 证据，且验收点已核对 |
| IMPLEMENTED_UNVERIFIED | 3 | 实现与测试目标都在，但验收点未闭合（每项在账本里写明缺什么） |
| DEFERRED_WITH_RATIONALE | 34 | 全部是 experiment；设施已修好、三个实验已跑，其余机制仍需各自的 arm 与预注册运行 |
| BLOCKED_EXTERNAL | 4 | T02/Q06/S06/T04，附独立探测证据（`blocked_probes.md`） |
| TODO | 0 | — |

这份账本不是"全部完成"，而是一份**说明清楚哪里没完成的账本**。

3 项未闭合的具体缺口（账本里逐条写明）：R04（模块提取缺收益证据，需要一次真实重构实验）、R06（决策规则文档已成，但依赖 R05 未闭合）、
T03（升级/回滚需要真实的旧格式会话夹具，未造）。

## 二、最终门禁

在交付提交上重跑：`cargo build` 通过；`nextgen_rust_gate.sh` 打印 `RUST OFFLINE GATES PASSED`
（[6] 全量 1295 通过 / 0 失败）；`run-sandbox-tests.sh` 56/56（真 bwrap 0.12.0）；`cargo fmt --check`
与 `clippy -D warnings` 干净。日志：`evidence/w5-final-gate.log`。

一次并行 flake 已按规则记录（`evidence/w5-flake-session-ownership-run1.log`）：独占重跑两次 14/14 通过，
全量 lib 并行重跑 402/402 通过。

## 三、修掉的问题（都有红→绿证据）

1. **窗口检查被来源条件挡住**：`source == Configured` 让默认路径完全没有整请求窗口检查。
2. **动态工具晚于估算与检查**：激活的工具在估算和窗口检查之后才并入请求。
3. **WriterLease 重入语义**：Drop 无条件删注册表，第三写者能进入且 flock 提前释放。改为引用计数 + 共享 `Arc<File>`。
4. **未创建路径的别名双写**：`canonicalize` 失败时回退到字面拼写，多种写法各得一个写者槽位。
5. **MCP SSE 等连接关闭**：整包读取后才解析。改为增量解析。旧实现 `took 5.000958537s`，新实现 `0.00s`。
6. **三套 token 估算口径**：对 CJK 差最多 2 倍。统一为一条保守估算。
7. **工具参数无统一校验**（KR-H02，本轮新增）：每个工具自己校验、有的完全不校验。新增
   `tool/schema_check.rs` 作为 dispatch 前唯一入口：JSON 可解析、必须是对象、required 字段存在、
   基础类型与数组元素类型匹配、1 MiB / 32 层上限。失败返回 `INVALID_ARGUMENTS` 与可行动的消息，
   且发生在任何 hook、策略、审批和副作用之前。7 个单元测试 + `tool_contracts` / `t12` / `gate3` 全绿。

## 四、账本绑定现在是真的

审计指出：`taskctl.py` 只检查 `source_tree_sha256` 是不是 64 位 hex，从不重算。已补
`scripts/verify_ledger_binding.py`：对每条收据取其 `commit`，从 git 对象重建源码树身份
（排除 `docs/nextgen`，与写入端同算法）并比对。

- 结果：124 条收据、9 个 commit，全部绑定（`bound: true`）。
- 反向验证：把 `KR-B04` 的树哈希改成全 0，脚本立刻 `bound: false` 并点名该条。

## 五、真实外部证据

- **三协议 live canary**（cobblemon.asia，gpt-6.1-sol）：chat-completions in 1904 / cached 0 / out 5 / ttft 3336ms；
  openai-responses 1898/0/5/1789ms；anthropic-messages 1973/0/5/1819ms。均带 `request_identity_sha256`。
- **真实第三方 MCP**（`@modelcontextprotocol/server-everything` 经 npx stdio）：3 次 `mcp__everything__echo` 成功。
- **Windows 交叉构建**：`cargo build --target x86_64-pc-windows-gnu` 通过（39.79s），产物
  `PE32+ executable for MS Windows 5.02, x86-64`，223 MB。运行证据仍缺（无真机），所以 T02 保持 BLOCKED。
- **实验**：见第六节。

## 六、实验：设施已修好，三个机制已有真实结论

前三次跑出 0/6 全部 rejected，根因有三层（全部记录在 `evidence/exp00-diagnosis.md`）：
`--allow-write` 在 bench 分支不生效（要用 `--policy allow-write`）；verify 的解释器必须在 bwrap 沙箱里可见
（原来指向 `/tmp/kv/bin/python`，而 `/tmp` 在沙箱里是空 tmpfs）；verifier 二进制必须按真实路径声明为可信文件
（`--verifier-file /usr/bin/python3.13`，软链会被拒）。

修好之后跑了三个机制，全部 6/6 或 2/2 通过、0 个无判定（明细见 `evidence/experiments-summary.md`）：

| 机制 | 质量 | 成本 | 处置 |
|---|---|---|---|
| on_demand_tools | 相同 | dev +8% / holdout −10% | 不晋升（两个集合矛盾） |
| adaptive_tournament k=2 | 相同 | +17% | **判负**，不启用 |
| projection_p3 | 相同 | −2.6%（噪声内） | 无结论 |
| M06 约束任务 | 相同 | — | 约束由字节检查判定，模型守约 |
| Q01 注入（文件 / 提示两表面） | 相同 | — | 模型未被骗，对照任务也通过 |

三个机制没有一个支持"打开它更好"，所以默认配置不变。剩下 34 项 experiment 各指一个不同机制，
需要各自的 arm 与预注册运行，不能靠这三次实验代替。

关键对照：把同一个任务手工 materialize 后用同一个二进制跑 `kroma run`，模型**真的修好了**
（`range(n + 1)`），自己跑 pytest 1 passed，缓存命中 87–93%。verify 命令在沙箱内也验证可用
（未修状态正确报 `assert 3 == 6`）。

所以：模型能完成、判定链路能跑、沙箱能看见解释器，但 bench 的 provider executor 与手工路径行为不一致，
而 `BenchArmResult` 不带失败原因字段，无法从报告定位。这是**实验设施缺陷**，不是候选方案的负面结论。
把它标成 `REJECTED_WITH_EVIDENCE` 会谎称"候选被判负"，所以标 DEFERRED 并附上述证据。

修好 bench executor 才能产出统计结论；在那之前，任何"省了 token / 质量非劣"的说法都不成立。

## 七、外部阻塞（附探测）

见 `blocked_probes.md`：T02（无 Windows/macOS 真机，macOS 无 SDK，ARM 缺交叉链接器）、
Q06（无第三方安全工具链与独立评审）、S06（本机无部署实例，无灰度人群）、T04（依赖前三项）。

## 八、交付

远程：`git@github.com:Polaris591/kroma.git`，分支 `nextgen-r2-rustfix`。
`main` 未改动，未创建 tag，未发布。
证据清单：`SHA256SUMS.txt`，`sha256sum -c` 自校验通过。
