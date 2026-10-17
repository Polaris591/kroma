# T11 · 统一 Runner 装配并验证机制接线 — 验收

阶段 G2｜依赖 T04, T07, T09｜关联 KR-08, KR-33

## 一、缺陷

1. **各入口各建各的 runner**：CLI、bench、RPC、worker、`--resume` 各自拼装，同一份配置可能得到
   不同的预算/策略/执行边界，而没有任何地方能证明它们相等。
2. **「启用了」和「跑了」混在一起**：一个实验机制可以在配置里打开、实际上一次都没执行，
   报告里却算作收益。任务卡的原话是「不输出『启用但零操作』的有效 arm」。

## 二、改了什么

### 1. `assembly::RunPlan`（唯一装配入口）

`RunPlan::from_resolved(&ResolvedConfig, Surface) -> Result<Self, AssemblyError>`：

- 只依赖已解析的配置（+ 可选的 `PlanEnvironment`，用于「快照存储能否构造」这类非配置事实）。
- `Surface` 只记录门（cli/rpc/bench/worker/resume），**不参与 digest**。
- `digest()` 是预算、策略、投影档位、锦标赛候选数、每个机制的 configured/eligible/reason 的
  规范化 sha256。同一配置经不同门 → 同一 digest；改一个值 → digest 变（测试里两条都验）。
- 依赖在**配置阶段**校验，缺依赖直接 `AssemblyError::MissingDependency`：

  | 机制 | 依赖 |
  | --- | --- |
  | `counterfactual_arm` | 隔离执行后端（`bwrap` / `container:...`）+ 修订快照存储 |
  | `context_compaction` | 投影档位不是 `baseline`（且必须是 p1/p2/p3） |
  | `snapshot_store` / `tool_discovery` | 无（核心组件） |
  | `experience` | 运维提供的候选文件 |

  这条规则直接堵住「在宿主上开锦标赛」：候选会在没有任何边界的地方执行，等于把实验变成不可控的
  真实改动。现在它启动即报错。

### 2. `FeatureLedger`（configured / eligible / executed / skipped 分开）

- 四个计数互不推导：`configured` 是配置说了，`eligible` 是前置条件满足，`executed` 是**真的跑了**，
  `skipped(reason)` 记跳过原因与次数。
- `headline_gains()` **只**返回 `executed > 0` 的项——这是报告里唯一允许当成收益的数字。
- `zero_execution_report()` 列出所有 `executed == 0` 的项，供报告写「没有结果」而不是编一个。
- `summary()` 给证据日志用，一行一个机制。

### 3. 接线（做了什么、没做什么）

已接：
- `kroma-cli` 的**两个** run 入口（bench 与主 run）在解析配置后立刻 `RunPlan::from_resolved`，
  失败即 `exit(2)`——`missing_feature_dependency_fails_at_start` 的生产路径。
- bench 把 digest 与 eligible 集合写进 stderr 证据（`--quiet` 时静默）。
- 主 run 用 plan 播种 `FeatureLedger` 并打印摘要（`configured/eligible/executed/skipped`）。

未接（明确声明，不宣称生效）：
- **RPC / worker / resume 三个门还没有调用 `RunPlan`**：它们在 `kroma-core::surface` 内部，拿不到
  `ResolvedConfig`。当前证明的是「同一配置经五个门构造出的 plan 相同」，不是「五个门都已经在用这个
  plan」。这是 T11 的残留项，已记入 T30 台账。
- 机制自身的 `record_executed` 调用点还没接（agent_loop 的投影压缩、bench 的反事实 arm）。
  因此生产 run 的 `executed` 目前恒为 0，报告会显示「0 次执行」而不是收益——这正是
  `zero_eligible_events_is_reported_not_invented_gain` 想要的行为，但「每个声明的机制都有 executed
  证据」这一条要等 T24/T26 的实验跑起来才算满足。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t11_assembly` | 5 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | **903 passed / 0 failed**（`logs/test_t11.log`） |

### 测试清单（5 条）

- `feature_matrix_covers_cli_rpc_bench_worker_resume`：五个门用同一配置构造 plan，
  digest 集合大小为 1、eligible 集合大小为 1，且 counterfactual/compaction/snapshot 都在 eligible 里。
- `same_run_plan_has_same_policy_across_all_surfaces`：逐门比较 `policy` 与 `budget` 字段相等；
  再改 `max_tokens` 一位，断言 digest 变化（证明相等不是空转）。
- `missing_feature_dependency_fails_at_start`：`adaptive_tournament = 2` 且无隔离后端 → 报
  `MissingDependency{requires: "an isolated execution backend..."}`；加 `execution_sandbox = "bwrap"`
  后通过；非法投影档位 → `InvalidValue`。
- `missing_snapshot_dependency_is_configuration_error`：`snapshot_store_available = false` +
  锦标赛 → `MissingDependency{requires: "the revision snapshot store"}`；默认环境通过；
  experience 只在真有运维文件时 eligible。
- `zero_eligible_events_is_reported_not_invented_gain`：configured 但未执行 → `headline_gains()` 为空、
  `zero_execution_report()` 有它、`summary()` 显示 `executed=0`；eligible 且 executed 才进收益；
  eligible 但 skipped 仍不算收益。

## 四、边界

- 没有重写 `agent_loop`，只抽了最小公共装配（任务卡第 1 步的要求）。
- `probe_never_reverts_live_parent`、`refactor_preserves_golden_protocol_traces` 这两条不在本项新增：
  前者由 `attribution.rs` 既有的 `revert_probe_and_restore` 测试覆盖（探针失败后无条件复原父工作区），
  后者由既有 golden trace 用例覆盖；本项没有改动协议序列化路径，重跑全量即回归证据。
- `provider_bench_counterfactual_arm_reaches_real_probe` 需要真实 provider 额度（T24），记 BLOCKED 于 T24。
