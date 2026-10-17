# T19 · 把 AHE 变成独立且不可自改裁判的实验器 — 验收

阶段 G3｜依赖 T01, T06, T10, T17, T18｜关联 KR-20, KR-21, KR-22

## 一、缺陷

三处，各自都能让「自修复实验」变成自我认证：

1. **baseline 在源目录里跑**。`evaluate_all(source_root, ...)`——验证命令的 cwd 就是真实工作区，
   一个会写文件的验证器（或一个会删文件的任务）直接污染被实验的源树。而且源树本身没有前后对比，
   污染了也没人知道。
2. **临时副本靠显式删除**。`let _ = remove_dir_all(&isolated);` 只在成功路径上；第 3 步写文件失败
   用 `?` 提前返回、或验证过程 panic，整份工作区副本就留在 `/tmp`。
3. **只有一个 `Accepted`**。「我预测这个任务仍然失败，它果然失败」也算 Accepted——
   预测为真 ≠ 有改进，更 ≠ 可以提升。

## 二、改了什么

### 1. 两侧都从冻结副本开始，源树只读

- `TempTree::create_from(source)` 生成副本，`Drop` 无条件删除——**所有退出分支**（成功、提前返回、
  panic）都会清理。
- baseline 在副本 A 上评估，候选补丁打到副本 B 上评估。源树从不作为评估根。
- 评估前后各取一次源树指纹（T18 的有界哈希）：不一致 → `source_untouched = false`，
  这次比较直接判为无效实验，而不是给出一个「通过」的结论。

### 2. 裁判与它自己的证据受保护（`self_repair_guard`）

新增受保护项（此前只保护 policy / secure_fs / sandbox / provenance / 信任根 / 参考解 / 成本表）：

| 路径 | 为什么 |
| --- | --- |
| `crates/kroma-core/src/bench.rs` | 评分器与配对/聚类统计 |
| `crates/kroma-core/src/completion_gate.rs` | 完成闸门（决定任务何时算完成） |
| `crates/kroma-core/src/ahe.rs` | 自修复实验策略与裁决规则本身 |
| `crates/kroma-core/src/experiment.rs` | 实验 manifest 与提升规则 |
| `crates/kroma-core/tests/`（整棵子树） | 可信回归测试——能改测试就能改自己的分数 |

新增 `ProtectedArtifact::prefix`，所以「整棵目录」是一条可审计的条目，而不是逐个文件列举。
这些拒绝**发生在任何执行之前**，且拒绝信息会点名具体 artifact。

### 3. 四级裁决拆分（`RepairVerdict`）

| 级别 | 含义 |
| --- | --- |
| `prediction_confirmed` | 预测真的应验了。**不代表有价值** |
| `experiment_valid` | 比较本身成立：两侧同源冻结副本、裁判未变、源树未被触碰 |
| `improvement_accepted` | 有效 + 预测应验 + 零回归 + 至少一个改进 + 预测的是「将开始通过」 |
| `promotion_approved` | **本版恒为 false**：提升是人工在独立全量回归之后做的决定 |

`RepairOutcome.decision` 保留为粗粒度摘要（`improvement_accepted` → `Accepted`），
`verdict.as_str()` 给出可读状态（`prediction_confirmed_only` / `improvement_accepted` / …）。

### 4. 裁判身份（`frozen_evaluator_identity`）

任务 id、prompt、后置条件、`verify_command`、`regression_guard` 与限制（超时/输出上限）一起求 sha256，
写进 `RepairOutcome.evaluator_sha256`。`compare_outcomes(before, after)` 在身份不同、或任一侧源树被
触碰时返回 `None`——**不同裁判下的两次测量不是一对前后对比**。

### 5. 本版不自动合并

实验器只产出候选补丁与报告。`promotion_approved` 恒为 false，`a_real_repair_is_accepted_but_never_promoted`
直接钉住这一点。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t19_ahe_isolation` | 9 passed / 0 failed |
| `cargo test -p kroma-core --lib ahe::` | 5 passed / 0 failed（既有用例） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 见 `logs/test_t19.log` |

### 测试清单（9 条）

- `ahe_baseline_verifier_cannot_mutate_source_root`：一个会写 `pwned.txt` 并覆盖 `src/calc.py` 的
  验证任务跑完后，源树字节不变、没有新文件、`source_untouched == true`。
- `ahe_candidate_and_baseline_start_identical`：验证任务把未改动文件的 sha256 追加到树外日志；
  两次评估写出的摘要**完全相同**（两侧同源）。
- `allowed_source_change_cannot_replace_frozen_judge`：给一个「允许一切」的前缀，
  六个裁判/证据路径仍全部被拒（含 `bench.rs`、`ahe.rs`、`completion_gate.rs`、`experiment.rs`、
  一个 tests/ 下的文件、`smoke_solutions.json`），拒绝信息点名 artifact。
- `predicting_existing_failure_is_not_promotion`：预测「仍然失败」且应验 → `prediction_confirmed=true`、
  `improvement_accepted=false`、`promotion_approved=false`、状态 `prediction_confirmed_only`。
- `zero_effect_candidate_remains_experimental`：改了无关文件、什么都没改进 → 全部 false。
- `a_real_repair_is_accepted_but_never_promoted`：真修复 → 三级为真、`promotion_approved=false`，
  源树仍是原始 buggy 版本。
- `changing_evaluator_invalidates_comparison_identity`：同一套任务同一限制 → 同一身份；
  改 `verify_command`、改超时、改后置条件 → 身份全部变化。
- `the_judge_and_its_evidence_are_protected`：受保护清单确实包含裁判与 tests 子树，且每条都有理由。
- `ahe_apply_failure_and_cancel_clean_temp`：写目录失败 → 返回 `UnsafePath` 且 `/tmp` 里没有新增
  `kroma-ahe-*`；成功路径同样不残留。

### 变异验证

把 baseline 改回在 `source_root` 上评估，`ahe_baseline_verifier_cannot_mutate_source_root` 立刻红，
并且失败信息正是新加的源树指纹对比：
`the source tree changed during the experiment (5e1d2429… -> 413f8849…)`。
恢复后 9/9 绿。

## 四、边界

- `cost_saving_requires_noninferior_quality_and_complete_usage` 与
  `oracle_hash_is_unchanged_after_candidate` 没有单独用例：前者属于 T18 的成本账目（`CostSummary`
  已要求 `incomplete` 时不给结论），后者由 `source_untouched` + 裁判身份覆盖（源树与裁判都未被触碰）。
  这两条记为 T30 台账的合并项，不是遗漏声明。
- 没有实现「在线自修改」：任务卡明确要求第一版只输出补丁与报告。`promotion_approved` 恒为 false。
- `self_repair_cannot_modify_bench_or_completion_gate` 的语义由
  `allowed_source_change_cannot_replace_frozen_judge` + `the_judge_and_its_evidence_are_protected` 覆盖。
- 候选补丁的应用仍是整文件覆盖（不是 diff/patch），所以「写目录失败」这类路径错误能被稳定触发；
  真实 diff 应用留给后续版本。
