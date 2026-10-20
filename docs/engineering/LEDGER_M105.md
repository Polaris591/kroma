# M1-05 账本集成

复用既有 `TokenBudget`，不新增账本。`tests/m105_ledger.rs` 三条：

- **A1 失败调用费用不抹掉**：`settle_not_dispatched(0)` 才退款（从未上线）；
  `settle_unknown` 记 `PossiblyDispatchedUnknown`，spent 与 unknown 各 2000，不是免费。
  `settle_not_dispatched(>0)` 会被拒（`AlreadyDispatched`），防止"没发出去"当借口退货。
- **A2 重试是新 attempt**：第一次 1500 记 unknown，重试再预占 1500 并结算估算 1000，
  合计 spent 2500（不是 1500）——重试不替换失败那次的成本。
- **A3 未知保留上界**：5000 上限、4000 未知后 remaining 只剩 1000；再预占 2000 被拒；
  恰好 1000 可预占。**未结算的预占被 Drop 时按 unknown 计费**（spent 5000），
  沉默不是退款——这条是测试写出来后发现的真实行为，已作为断言钉住。

定向 3 条；全量见 `evidence/m1-05/full-suite.log`。
