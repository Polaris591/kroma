# C00-02 资源 envelope 与预算合同

代码：`crates/kroma-core/src/envelope.rs`（新增模块，`lib.rs` 导出）。

## 设计取舍：合同，不是第二个账本

蓝图要求 14 个边界（`CAPABILITY_CONTRACT.json` → `resource_envelope_required_fields`），
语义是「**未受控执行前，所有上限必须解析为显式有限界**」。所以 `ResourceEnvelope` 只做两件事：

1. 缺字段就解析失败（`serde` 全部必填 + `deny_unknown_fields`），不提供 `Default`。
   「没写」不等于「无限」，而是「未知」，未知才是让运行悄悄长大的方向。
2. 能强制的那部分**交给已有权威**：`token_budget()` 返回现有的 `TokenBudget`，
   `apply_to(&mut BudgetConfig)` 写 `max_total_tokens` / `max_run_duration` /
   `max_total_tool_output_bytes`。模块内没有自己的计数器，所以 envelope 不可能和它约束的账本对不上。

尚未有强制者的字段（memory / processes / fds / disk / queue / switch_count）留在 envelope 里，
**没有**被复制成「没人检查的建议值」——那只会制造一种看起来被限制了的错觉。

## C00-02-A1 必填字段校验

`every_required_bound_is_required`：逐个删掉 14 个字段之一，14 次都必须解析失败。
`unknown_keys_are_refused`：拼错键名（`memory_byte`）同样拒绝——拼错等于缺失，只是更隐蔽。
`zero_bounds_are_rejected_by_name`：字段存在但为 0 时按名字报错。0 读起来像「关闭」，
行为上却等于「第一个请求就失败」，属于看起来配好了其实什么也做不了的静默状态。

## C00-02-A2 超额拒绝

`token_ceiling_is_enforced_by_the_ledger_not_a_copy`：
`envelope.authorized_token_limit + 1` 的预占被拒；等于上限的预占被允许；结算后再借 1 也被拒
（额度不可复用）。拒绝由 `TokenBudget` 完成，envelope 只是把界交过去。

## C00-02-A3 未知用量不归零

`unknown_usage_is_not_zeroed`：`settle_unknown()` 之后 `Settlement::PossiblyDispatchedUnknown`，
`snapshot.unknown_tokens == 1000`、`snapshot.spent == 1000`、`reserved == 0`。
即「不知道花了多少」记为未知负债，不记为免费。

## 收口

- 定向：`cargo test -p kroma-core --lib envelope`（6 条）
- 全量：1032 passed / 0 failed / EXIT=0
- clippy `-D warnings` 0 条；`cargo fmt --all --check` 通过
- 证据：`/opt/kroma-v4/evidence/c00-02/`
