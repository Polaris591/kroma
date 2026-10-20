# T17 · 冻结实验配置、标识与留出集 — 验收

阶段 G3｜依赖 T04, T11, T12, T13, T16｜关联 KR-12, KR-13, KR-14

## 一、缺陷

三处，都让「这次实验到底比了什么」无法复现：

1. **没有版本化入口**。arms / models / price table / sandbox / oracle / repeats / holdout 散落在 CLI 参数里，
   「比了什么」只能靠翻命令历史重建。
2. **身份没有纪律**。没有地方阻止两个 arm 或两个 task 用同一个 id；job 目录用有损 sanitize 生成，
   于是 `a/b` 和 `a_b` 会落进同一个目录，互相覆盖。
3. **holdout 不可能失败**。`holdout_task_ids` 只在**报告阶段**被查询：一个都没匹配上时得到 `None`，
   裁决静默回落到「全部任务」——这正是 holdout 存在的意义所要防的事（拿开发集当选拔依据）。

## 二、改了什么

### 1. `experiment::ExperimentManifest`（版本化、可校验）

字段：`version / id / arms[] / tasks[] / holdout[] / repeats / sandbox / oracle / max_total_tokens`，
`serde(deny_unknown_fields)`——拼错的键是错误，不是被忽略的设置。

`validate()` 在**任何 provider 调用之前**拒绝：

| 情况 | 错误 |
| --- | --- |
| 版本不是 1 | `UnsupportedVersion` |
| arms / tasks 为空 | `EmptyArms` / `EmptyTasks` |
| arm id 或 task id 重复 | `DuplicateArmId` / `DuplicateTaskId` |
| baseline 不是恰好一个 | `BaselineCount { found }` |
| arm 缺 model 或 price table | `MissingModel` / `MissingPriceTable` |
| repeats == 0 | `RepeatsZero` |
| 声明了 holdout 但为空 | `DeclaredButEmptyHoldout` |
| holdout 里有未声明的 id | `UnknownHoldoutId` |
| holdout 里重复 | `DuplicateHoldoutId` |
| holdout 覆盖全部任务 | `HoldoutOverlapsEverything` |
| 候选 arm 没有 sandbox | `MissingSandboxForCandidate` |
| 矩阵 > 10000 jobs | `MatrixTooLarge` |
| 预算 < jobs × 256 | `BudgetTooSmallForMatrix` |

「声明了 holdout 但为空」与「根本没写这个字段」被区分开：`serde` 分不出「缺失」和「空数组」，
所以解析时额外看原文里有没有 `"holdout"` 键。前者是错误（读起来像留出集其实不是），
后者是合法的「不划分」（`DecisionBasis::AllTasks`，报告里如实标注）。

### 2. 身份：编码而不是清洗

`JobId { task_id, arm_id, repetition }`，`dir_name()` 把两个 id 都**十六进制编码**再拼重复号，
分隔符 `_` 不会出现在十六进制里。`resolve()` 还会检查「两个 job 产生同一个目录名」并报错——
所以 `a/b` 与 `a_b` 不可能共用工作区。

### 3. dry-run 与真实调度同源

`resolve()` 产出 `jobs: Vec<JobId>`，`job_lines()` 就是 dry run 打印的列表。CLI 的 `--experiment <path>`
先校验再打印：

```
experiment exp-1: 2 arms x 3 tasks x 2 repeats = 12 jobs, basis=holdout, digest=…
  job <hex_task>_<hex_arm>_0000
  …
```

单独给 `--experiment`（没有 `--task`、不是 bench）= 纯 dry run，退出码 0，**不花钱**。
manifest 有问题 = 退出码 2，且一个 provider 都不会被创建。

### 4. 有效 manifest 导出 + 摘要

`effective_manifest_json()` 输出的是**将要执行的那份**（不是用户编辑的文件），
`digest` 是它的 sha256。改 `repeats` 会改 digest，证明摘要覆盖了真正影响执行的设置。

### 5. 晋升规则：holdout 说了算

`promotion_verdict(basis, development, holdout)`：

- `AllTasks` → `CannotPromoteWithoutHoldout`（没有留出集就没有晋升资格）
- holdout 均值 > 0 且区间下界 > 0 → `Promote`
- holdout 均值 < 0 → `Reject`
- 否则 → `Inconclusive`

development 数字只被接收、记录、并排展示，**不参与裁决**。开发集 0.40、holdout −0.05 的结果是 `Reject`。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t17_experiment_manifest` | 10 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | **935 passed / 0 failed**（`logs/test_t17.log`） |

### 测试清单（10 条，逐条对应任务卡的回归名）

- `duplicate_task_or_arm_id_fails_before_provider_call`：重复 arm / task id 分别报错。
- `declared_but_empty_holdout_never_accepts`：空 holdout 报错；**不写** holdout 字段则合法且
  `basis = all_tasks`；holdout 覆盖全部任务报错。
- `unknown_holdout_id_is_configuration_error`：未声明 id、重复 id 各自报错。
- `oversized_experiment_matrix_is_refused`：200×60×2 = 24000 jobs 被拒（报出实际 jobs 与上限）；
  预算 10 tokens 对 8 个 job 被拒（报出 required 2048）。
- `effective_manifest_is_exported_and_hashed`：导出再解析得到同一 digest 与同一 job 列表；
  改 repeats 后 digest 变化；digest 长度 64。
- `unknown_or_unused_manifest_field_is_rejected`：`holdout_tasks`（拼错）与 `price_tabel`（拼错）
  都被 `deny_unknown_fields` 拒绝；version 99 报 `UnsupportedVersion`。
- `sanitization_collision_cannot_reuse_workspace`：`a/b` 与 `a_b` 目录名不同；30 组 id 组合两两不碰撞。
- `each_pair_has_exactly_one_baseline_and_candidate`：两个 baseline、零个 baseline、候选无 sandbox、
  缺 model、缺 price table 全部拒绝。
- `development_only_gain_does_not_promote`：开发集大胜 + holdout 负 → `Reject`；
  开发集大胜 + holdout 不显著 → `Inconclusive`；holdout 真赢 → `Promote`；无 holdout → 不能晋升。
- `cost_and_quality_split_basis_is_explicit`：`decision_basis` / `is_holdout` / `baseline_arm` /
  每个 job 的 arm 与 repetition 都可直接查询。

### 变异验证

去掉「声明了但为空的 holdout」这条检查，`declared_but_empty_holdout_never_accepts` 立刻红，
并且失败信息里能直接看到它变成了 `decision_basis: AllTasks` 的合法实验——
这正是要防的静默回落。

## 四、边界

- **没有接线到真实 bench 执行**：`--experiment` 目前做的是校验 + dry run（+ 与 bench 同进程时的前置校验）。
  真正的「按 manifest 调度 provider bench」需要 provider 额度，属于 T24；本项不声称它已跑通，
  台账里 T24 记为待执行。
- `cli_manifest_roundtrip_reaches_all_runtime_settings`（无同名测试）只覆盖到「manifest → ResolvedExperiment」这一层：
  sandbox / oracle / budget 进入了结构与 digest，但还没有逐个落到 `ProviderBenchSettings` 的字段上
  （那一步在 T24 的调度器里）。
- 矩阵上限 10000 jobs 与每 job 256 tokens 的下限是**保守的估计值**，不是精确成本模型；
  真实成本仍以 T18 的请求级记账为准。
- 没有引入新的统计实现：配对与 cluster bootstrap 沿用既有 `bench.rs`，本项只加强输入验证。
