# M3-02：`ctx.taskstate.v1`

接线状态：候选已接进请求路径——`AgentRunConfig.projection_candidate` 由 `agent_loop` 渲染为
ephemeral 请求上下文并计入 `message_history_bytes`，因此与 legacy profile 同一条 token 账。
运行期数据在 `evidence/e02/RESULT.md`（taskstate 臂 6/6、1 560 601 token）。
默认关闭：E02 的结论是它只增不减（追加块而非替换历史），所以存在但不启用。

## 已经落地的两件事（各有测试）

1. **投影阶段**：`crates/kroma-core/src/task_state_projection.rs`
   `project(state, freshness, sensitive, max_bytes)` 把派生出的 `TaskState` 渲染成一个有界块：
   目标、约束、验证项、以及仍为 `Fresh` 的事实。三条验收性质在代码里强制，不是文档承诺：
   - 不是 `Fresh` 的事实**丢弃**（`Stale` / `Missing` / `EnvironmentMoved` / 无观测都算）；
   - 敏感事实只出 key，值替换为 `[redacted]`；
   - 块自己报告 `charged_tokens`，调用方据此把费用记进**同一次 run** 的账。
   三个测试分别钉住这三条（`stale_and_missing_facts_never_reach_the_block`、
   `a_sensitive_fact_contributes_its_key_and_nothing_else`、`the_block_is_bounded_and_reports_what_it_costs`）。
2. **选择器**：`assembly::resolve` 与 `config_file` 都接受 `ctx.taskstate.v1`，
   并让计划按**候选自己的 id** 命名这次投影（不再改写成 `p2`）。另外两个保留候选
   （`ctx.semantic-extractive.v1`、`ctx.guideline-optimized.v1`）继续按名字拒绝，
   配置文件层与计划层同一套判断。

## 还没做的接线（明确位置）

`ctx.taskstate.v1` 目前**不会改变任何一次请求的内容**：`AgentRunConfig.projection_profile`
是 `Option<ProjectionProfile>`，装不下候选 id；`ContextProjector::project()`
（`agent_loop.rs:1116` 调用）也不会插入这个块。

要接线需要三步，任何一步都不能省：

1. `AgentRunConfig` 增加 `projection_candidate: Option<ProjectionCandidate>`；
2. 在投影器里把块作为**单独的一类投影项**插入，并让它进 `ContextBudgetBreakdown`，
   而不是当成普通历史消息（否则预算记账会把它和 transcript 混在一起）；
3. bench 的 `BenchStrategyConfig::resolved_projection_profile()` 只解析 `p*`，
   要让 E02 的 taskstate 臂可选，还得让它接受候选 id。

## 因此

M3-02 是 **partial**：算法与选择器有证据，运行期行为未改变。E02 的第三个臂仍需第 3 步。
这不是"跑一次就能补上"的缺口，是接线工作。
