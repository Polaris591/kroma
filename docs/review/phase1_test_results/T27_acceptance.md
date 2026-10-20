# T27 · 自适应执行策略与反事实验证 — 验收

阶段 RESEARCH-OPTIONAL｜依赖 T01, T04, T10, T11, T24｜关联：无独立 KR

## 一、缺陷

困难任务需要「按证据升级」，简单任务要保持「低成本单代理」。原来的代码里只有一堆分散的开关
（`--counterfactual-probe`、锦标赛候选数、子代理上限），没有一条**确定的**规则说：
什么时候该继续、什么时候该多看一眼、什么时候值得付一次探针、什么时候该停。
结果就是「模型说自己不确定」这种不可审计的信号在实际决策里没有对手，而升级的代价没人算总账。

## 二、改了什么

新模块 `crates/kroma-core/src/control_policy.rs`：

### 1. 四个确定性动作（决策顺序固定）

| 动作 | 触发条件 |
| --- | --- |
| `Stop(BudgetExhausted)` | 剩余 token = 0（优先级最高） |
| `Stop(RepeatedIdenticalFailure)` | 连续失败且同一失败签名重复达阈值 |
| `Stop(IsolationUnavailable)` | 该探针了但没有隔离副本可建 |
| `Probe` | 连续失败达阈值 + 有显式上限 + 运维授权 + 隔离可用 + 剩余预算 ≥ 探针下限 |
| `Inspect` | 连续失败达阈值，但探针不可用/未授权/已达上限/预算不足 |
| `Continue` | 其它（单代理基线） |

输入只有计数器（`ControlObservation`）：连续失败数、相同失败签名连击、剩余 token、已用探针数、
是否授权、隔离是否可用。**模型自报置信度不是输入**——任务卡禁止「因为模型自信低就无限加代理」。

默认配置 `max_probes_per_run = 0`：生产环境**永不**探针；只有 `ControlPolicyConfig::with_probes(n)`
（运维显式给上限）才会产生 `Probe`。

### 2. 探针只能跑在隔离副本上

「策略说 Probe」到「真的执行」之间只有一条路径：`AttributionEngine::verify_candidate_in_isolated_worktree`，
它先物化一次性 worktree，探针闭包拿到的是副本的 `WorkspaceCap`，结束后 worktree 被删除。
策略层还额外要求 `isolation_available`：建不出副本时直接 `Stop(IsolationUnavailable)`，
不会退化成「在真实工作区上跑一次试试」。

### 3. 多候选是实验，不是默认

`MultiCandidateGate::default() == SingleCandidate`；只有
`MultiCandidateGate::from_experiment(experiment_id, max_candidates)`（实验 id 非空且上限 ≥ 2）
才会给出 `Experimental`。`allows_autonomous_work_mode()` 恒为 `false`——本模块没有任何路径
开启自主 Worker 工作流（任务卡的禁止项）。

### 4. 败者成本计入总账

`CandidateSummary::from_records` 汇总**每个**候选的 token（含败者）：
`total_tokens`、`wasted_on_losers`、`mean_diversity`，以及
`resolved_per_100k_tokens()`（无结果时返回 `None`，不编造 0）。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --lib control_policy` | **8 passed / 0 failed** |
| `cargo test -p kroma-core --test t27_control_policy` | **3 passed / 0 failed** |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 见 `logs/test_t27.log` |

### 策略单元测试（8 条）

- `the_single_agent_path_is_the_default`：0/1 次失败 → `Continue`；2 次 → `Inspect`；
  3 次但探针默认关闭 → 仍是 `Inspect`。
- `probing_requires_an_explicit_cap_and_authorization`：给上限后 3 次失败 → `Probe`；
  达到上限 → 回到 `Inspect`（不是 `Stop`，还能继续干活）；未授权 → `Inspect`。
- `a_probe_never_starts_below_its_budget_floor`：预算差一个 token → `Inspect`，够了才 `Probe`。
- `a_probe_without_isolation_stops_instead_of_touching_the_workspace`：无隔离 → `Stop(IsolationUnavailable)`。
- `repeating_the_same_failure_stops_escalation`：同一签名连击 4 次 → `Stop`；
  同样失败数但签名不同 → 仍 `Probe`（新信息）。
- `an_exhausted_budget_wins_over_everything_else`：预算 0 + 连击 9 → `Stop(BudgetExhausted)`。
- `multi_candidate_is_experimental_and_never_autonomous`：默认单候选；显式实验才多候选；
  空实验 id / 上限 1 被拒；自主模式恒 false。
- `loser_cost_is_part_of_the_total`：赢家 1000 + 败者 700/300 → 总计 2000、浪费 1000、
  多样性 60、每 10 万 token 50 个解；无解时 `None`。

### 集成测试（3 条，真实探针路径）

- `a_probe_action_runs_only_on_an_isolated_copy`：策略判 `Probe` → 走
  `verify_candidate_in_isolated_worktree` → 探针闭包收到的路径 **≠ 父工作区**、
  worktree 真的存在过 → 探针后**父工作区字节不变**（仍是 `compute_v2`）、worktree 已清理、
  结果被写进候选 rationale（`verified in isolated worktree`）。这就是任务卡要的「有明确 executed
  探针证据，原工作区不变」。
- `unauthorized_or_unisolated_probes_are_never_executed`：未授权 → `Inspect`；无隔离 → `Stop`；
  默认关闭 → `Inspect`；预算不足 → `Inspect`。四种情况都不会调用探针。
- `every_candidate_is_accounted_including_the_losers`：败者 5000 > 赢家 4000，总账 9000 如实反映。

## 四、BLOCKED：EXP-04（与简单基线的同预算对照）

任务卡要求「相同总预算下优于或不劣于简单基线」。这需要真实模型跑量（EXP-04），依赖 T24 的预算授权
（T24 = BLOCKED）。因此：

- 本项**不声称**任何「优于基线」的结论；
- 交付的是阈值说明与控制动作合同：一旦有预算，用 `CandidateSummary` 与
  `resolved_per_100k_tokens()` 就能算「同预算下的解出率」；
- 若实验被禁止，默认路径就是单代理 `Continue`/`Inspect`，与本次改动前一致（无回归）。

## 五、边界（任务卡停止条件）

- 不因模型「自信低」加代理：决策输入里没有置信度字段。
- 任何禁止的模式保持禁用：`allows_autonomous_work_mode()` 恒 `false`，没有默认开启的自主 Worker。
- 不按最小 diff 自动等同于最好修复：`CandidateSummary` 只报告多样性/成本/解出数，
  选择仍由既有 `promotion_verdict`（T17，只看留出集）与人工裁决。
- 探针不触发自动回滚：它只读副本并把结论写进证据（既有 Gate 4 语义未改）。
