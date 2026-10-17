# T12 · 完成工具发现到调用的原生闭环 — 验收

阶段 G2｜依赖 T01, T05, T11｜关联 KR-09

## 一、缺陷

`discover_tools` 是**只读**的：它把目录（可选带 schema）打印进一条 tool result，然后什么都不做。
`on_demand_tools` 打开时，下一次请求的 `tools` 数组仍然只有核心集——于是严格按「只能调用已声明工具」
校验的 provider（线上协议都这样）**根本没法调用它刚刚发现的工具**。环断在最关键的那一格：
模型拿到了 JSON，却拿不到调用权。

## 二、方案选择（任务卡第 1 步要求明确记录）

选**受控 schema 激活**，不引入 `invoke_discovered_tool` 包装器：

- 包装器会让所有工具调用退化成「一个万能工具 + JSON 参数」，丢掉原生 tool_call 的类型校验、
  并行调用和 provider 侧的结构化错误；
- 受控激活保持原生协议不变，只改变「下一次请求声明哪些工具」。

## 三、改了什么

### 1. 激活状态机（`tool/activation.rs`）

- `ToolActivation`：只存**名字 + 激活时的 schema hash + 字节数 + 单调递增的激活序**。
  定义本身不存，请求时从**活的 registry + 活的 policy** 解析——所以扩展卸载或策略收紧会
  自动让 schema 消失，不会留下一份陈旧的副本。
- 拒绝全部显式化：`UnknownTool` / `DeniedByPolicy` / `AlreadyActive` / `TooManyActive` /
  `SchemaTooLarge` / `SchemaBudgetExceeded` / `NotActivatable`。
- 预算：`MAX_ACTIVE_TOOLS = 8`、`MAX_ACTIVE_SCHEMA_BYTES = 64 KiB`（单工具与总量都查）。
- `digest()`：按激活序对 `(name, hash)` 求 sha256，用于「两次请求拿到的是不是同一套 schema」。
- `prune_to(&registered)`：把已经不存在的工具从激活集里摘掉，返回被摘掉的名字。

### 2. 发现即激活（`discover_tools`）

参数新增 `activate: [names]` 与 `deactivate: [names]`（`serde(default)`，旧的纯查询调用行为不变）。
工具结果里追加一段 `--- activation ---`：每个名字的结果（激活成功带 schema 字节数与 hash 前 12 位，
或拒绝原因）、当前激活集与 digest。这段文本进 WAL，就是机制活动的证据。

**策略检查在激活时做一次、在请求时再做一次**：`activate_tools` 用 `is_tool_enabled_for_policy`
判定，`activated_tool_definitions` 解析时再判一次。发现不可能提权。

### 3. 请求侧接线（`agent_loop`）

每次构造 `ProviderRequest` 时，把 `activated_tool_definitions()` **按激活序追加**到
`state.visible_tools` 之后，同名不重复。于是：

- 排序稳定（激活序），两次请求可以直接 diff；
- 卸载/策略收紧后自动消失（因为是从活 registry 解析的）；
- 子代理不继承激活集：`ToolRuntime` 是每个 run 一份，子代理用自己的 runtime，
  它的激活集从空开始，不会因为父代理发现了什么就多出权限。

## 四、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t12_tool_discovery_loop` | 4 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 905 passed / 2 failed（`logs/test_t12.log`） |

那 2 条是台账记过的既有 flake：`b05_execution_context::run_deadline_clamps_mcp_timeout`（§2.1）、
`b08c_git_metadata::carries_a_sha256_commit_through`（§2.2，需 `--test-threads=1`）。
串行复跑分别 14/14、5/5 全绿。

### 测试清单（4 条）

- `strict_provider_can_call_newly_discovered_tool`（**完整 round-trip**）：
  真实 `AgentRunner` + 一个**严格 provider**——只要模型调用未在本次请求 `tools` 里声明的工具就记违规。
  4 轮：① `discover_tools{activate:["code_graph"]}`；② 调 `code_graph`；③ `discover_tools{deactivate:[...]}`；
  ④ 收尾。断言：请求 1 没有 `code_graph`、请求 2 有、请求 3（携带 deactivate 的那次）仍有、
  请求 4 没有；**违规列表为空**（证明调用合法而不是被宽容）；tool result 里有
  `activated code_graph` 与 `digest`。
- `discover_never_activates_denied_tool`：ReadOnly 策略下激活 `write` → `DeniedByPolicy`，
  激活集为空、拿不到 schema；未知名字 → `UnknownTool`；允许的 `code_graph` 正常激活。
- `unmount_removes_schema_and_dispatch`：注册一个扩展工具 → 激活 → 有 schema；`unregister_extension`
  之后 schema 与激活记录同时消失。
- `schema_activation_budget_and_cache_transition_are_logged`：第 9 个激活被 `TooManyActive` 拒绝
  （不是静默截断）；每条记录都有 64 位 hash 与非零字节数；激活序是 0..N；
  超大 schema 被字节预算拒绝；卸载后 digest 变化。

### 变异验证

把 `agent_loop` 里追加激活工具的那段换成空迭代器，`strict_provider_can_call_newly_discovered_tool`
立刻红：`the model called an undeclared tool: ["code_graph called but not declared"]`。
恢复后 4/4 绿。

## 五、边界

- 激活是**进程内、单 run 作用域**：不写 WAL、不跨 `--resume` 继承。这是有意的——激活是「本次对话
  现在要哪些 schema」，不是持久授权；跨 resume 复用会让 schema 预算随历史无限增长。
- 没有做 `unmount_removes_schema_and_dispatch` 的**同名覆盖**场景（同名注册被 registry 拒绝，
  见既有扩展测试），也没有做 session scope 的激活隔离（单 run 即单 session）。
- 子代理继承的是允许的工具目录（`without_subagents` / 工具过滤），不是激活集——这是任务卡
  「发现行为不能提高权限」的要求，已有 `gate5` 系列测试覆盖工具集继承，本项没有改动那条路径。
