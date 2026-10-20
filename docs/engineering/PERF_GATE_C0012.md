# C00-12 性能晋升门禁

代码：`crates/kroma-core/src/perf_gate.rs`（`PerformanceBudget` / `Verdict` / `promote`）。

预算 8 个字段全部必填；`promote` 的判定顺序是：没有预算 → `NoBudget`；预算不完整
（任一方法/噪声/样本计划为空，或容差为 0）→ `Uncalibrated{field}`；超过绝对上限 →
`AbsoluteLimitExceeded`（**与基线无关**）；超出容差的退化 → `Regressed`；容差内 →
`WithinTolerance`（不下结论）；只有真正变快才 `Promoted`。

三条测试：A1 缺字段按名字拒绝（含"容差 0 不算容差"）；A2 无预算/无基线一律不晋升；
A3 退化+20% 被拒、+2% 记 `WithinTolerance`、-10% 才晋升、600ms 对 500ms 上限即使基线更差也拒。

## 接线现状（C00-12 返工后）

已接：
- `PerformanceBudget` 新增 `approved_by` / `source`，`validate()` 拒绝没有批准人与来源的阈值；
- `Verdict::Inconclusive`：样本自身离散度超过容忍度时返回 INCONCLUSIVE，而不是"非劣"；
- `CapabilityManifest.safety_capabilities`：清单里点名安全能力，`validate()` 拒绝其中任何一项
  被删除或不再 wired——休眠税不能靠删安全能力来降；
- `docs/engineering/PERF_BUDGETS.json` 的三条阈值由测试逐个喂给 `promote`，所以它们是可消费的
  而不是散文。

仍未接：`promote` 没有被 `capability.rs` 的 `default_on` 判定**正向**调用（即"每个
性能敏感能力必须有晋升结论"这条未强制）。接线点：把某能力从
`contract_tested` 抬到 `default_on` 时，必须持有通过 `promote` 的预算记录。
在此之前"没有标定就不许默认开启"只是本模块的规则，不是产品行为。

## 收口

定向 3 条 + clippy 0 + fmt 干净；全量见 `evidence/c00-12/full-suite.log`。
