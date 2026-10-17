# T29 · 受约束的离线 Harness 演化 — 验收

阶段 RESEARCH-OPTIONAL｜依赖 T19, T24, T25, T26｜关联 KR-22

## 一、缺陷

AHE 已能保证「实验在副本上跑、裁判不可改」（T19），但**实验本身缺少声明**：
没有地方写「这一轮的假设是什么、允许改哪些组件、预算上限多少、什么算失败」，
也没有规则阻止候选调整验收标准（换更弱的验证器、砍掉留出任务、换个便宜价目表）；
日志只保留赢的轮次，搜索成本与负面样本无处可查。

## 二、改了什么（`harness_evolution.rs`）

### 1. `EvolutionRound`：先声明，再运行

必须有：`hypothesis`（可证伪）、`editable`（允许改的组件）、`cost_cap_tokens > 0`、
`expected_gain`、`failure_criteria`（**空则拒绝**——不能失败的实验教不了任何东西）。
缺任一项 → `RoundError`（6 种，逐条可测）。

### 2. `FrozenCriteria`：候选不能改验收标准

绑定 `baseline_revision` / `verifier_sha256` / `holdout` / `price_table`，有稳定 digest
（holdout 顺序无关）。`differences()` 返回被改动的字段名；`check_candidate()` 一旦发现差异即
`AcceptanceCriteriaChanged([...])`——比较作废，不允许「记录一个数字继续」。

### 3. 组件白名单 + 禁止清单

落在声明之外的组件 → `ComponentNotDeclared`；
`is_forbidden_component()` 对所有轮次恒 true 的包括：`bench`（评分器与统计）、`completion_gate`、
`ahe`、`experiment`、`policy`、`sandbox`、`budget`、`usage_cost`、`provenance`、`self_repair_guard`、
`bin/kroma.js`（签名链）、`tests/`（可信测试）。

### 4. `review_promotion`：四道闸 + 人工合并

| 闸 | 不通过时 |
| --- | --- |
| 有改进（且不是单任务） | `NoImprovement` / `SingleTaskGain` |
| 零回归 | `Regression` |
| 独立回归已跑 | `IndependentRegressionNotRun` |
| 未超声明预算 | `CostCapExceeded` |
| 有具名所有者 | `OwnerApprovalMissing` |

全部通过才 `promoted = true`，并且 `requires_manual_merge = true`——**本模块从不合并、不部署、
不碰安全/预算/签名链**。

### 5. `RoundLog`：保留负面结果

`negative_rounds()` 返回「无改进」与「有回归」的轮次，`total_tokens()` 汇总搜索成本。
只留赢家的日志是宣传册，不是记录。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t29_harness_evolution` | **5 passed / 0 failed** |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | **995 passed / 0 failed**（`logs/test_t29.log`） |

### 测试清单（5 条）

- `a_round_must_declare_hypothesis_components_cap_and_failure_criteria`：五种缺项各返回对应错误。
- `a_candidate_cannot_adjust_the_acceptance_criteria`：换验证器 / 砍留出任务 / 换价目表 / 换基线
  各返回具体字段名；holdout 顺序无关；digest 随内容变化。
- `a_candidate_cannot_edit_undeclared_or_forbidden_components`：声明外组件被拒；九个禁止路径全部为 true，
  `prompt.rs` 不是禁止项。
- `promotion_needs_independent_regression_trustworthy_gain_and_owner`：完整通过 → promoted 且需人工合并；
  无改进 / 单任务 / 回归 / 无独立回归 / 超预算 / 无所有者六种情况各自只报对应原因。
- `the_round_log_keeps_the_rounds_that_produced_nothing`：3 轮（赢 / 空 / 回归）全留，
  总 token 300k，负面轮次 2 条。

## 四、BLOCKED：EXP-06 的完整来源链

任务卡验收要求「EXP-06 完整来源链，零越权/裁判修改；提升需独立回归、可信收益和所有者批准」。
机制层面已可执行（本轮声明、冻结标准、四道闸、负面日志），但**真实跑量与独立回归**依赖
T24 的预算授权（T24 = BLOCKED）。因此：

- 本项**不声称**任何「提升已发生」；
- 不自动合并、不连接生产部署、不改安全/预算/签名链——这三条由代码结构保证（无相关 API）。

## 五、边界

- 不把单任务收益当 harness 收益（`SingleTaskGain`）。
- 不因为「模型说这样更好」而提升：提升必须有独立回归 + 具名所有者。
- 定期复检已接受策略由 `RoundLog` 支持（每轮都留记录），但复检计划本身属于运维日程，不在此模块。
