# T04 · 重构请求与重试预算的预占生命周期 — 验收

阶段 G1｜依赖 T03｜关联 KR-02

## 一、真正的缺陷（改之前的状态）

`reserve_amount = estimated_input + max_output_tokens`，而 `max_output_tokens` 当时等于
**「运行剩余额度减去输入估算」**。于是第一次请求就把整份运行预算预占成自己的输出上限：

- 一次瞬时 503 之后，重试需要再预占同样一份，但余额已经为 0 → `RetryBudgetExhausted`，
  重试根本发不出去。`provider_contract` 里那条 `runner_retry_exhaustion_...` 当时断言的就是
  「只发一次」——它把缺陷写成了规格。
- 一次请求的输出上限被设成整份余额，provider 也被告知可以生成那么多 token。

## 二、改了什么

1. **per-request 输出上限与 run 剩余额度分离**（`budget.rs`）
   - `DEFAULT_PER_REQUEST_OUTPUT_TOKENS = 8_192`；`BudgetConfig::max_output_tokens_per_request`
     可覆盖（0 会在 `validate()` 里被拒）。
   - `ModelCapabilities::per_request_output_cap(host_cap, run_remaining)`：取
     `min(host_cap 或默认值, provider 上限, run 剩余)`。发出去的 `max_output_tokens` 与预占额都用它。
   - 无界运行（未设 `max_total_tokens`）保持原来的 `None`，不改既有行为。

2. **结算状态机**（`budget.rs`）
   - `Settlement` 四态：`NotDispatched`（可证未发送，退款）、`CompletedUsage{input,output}`（精确计费）、
     `Estimated{input,output}`（已发送但无 usage，按估算计费并打标签）、
     `PossiblyDispatchedUnknown`（不确定，按整份预占计费，永不退款）。
   - `TokenReservation` 新增 `settle_not_dispatched(wire_attempts)` / `settle_with_usage` /
     `settle_estimate` / `settle_unknown`，全部返回 `SettlementRecord`（含 `charged`、
     `over_reservation`、`budget_overage`、`spent_after`、`remaining_after`）。
   - `settle_not_dispatched` 在 `wire_attempts > 0` 时**返回 `SettlementError::AlreadyDispatched`**
     并保持未结算状态，由 `Drop` 按 unknown 计费——不允许「以为没发出去」的乐观退款。
   - 账本新增 `estimated_tokens` 维度，`settled_tokens + unknown_tokens == spent` 可断言。

3. **agent_loop 按状态结算**（`agent_loop.rs`）
   - 只有 `err.is_provably_zero_consumption() && wire_attempts == 0` 才走 `settle_not_dispatched`；
     被拒时记一条 `PolicyDecision{decision:"settlement_refused"}` 审计。
   - 成功且 `attempts_used == 1`：有 usage → `settle_with_usage`；无 usage → `settle_estimate`（标签为估算）。
   - 成功但 `attempts_used > 1`：第一次尝试的预占 `settle_unknown`（协议客户端自己结算成功的那次重试）。
   - **超支审计**：`audit_token_settlement` 在 `budget_overage > 0` 时写 `EventPayload::TokenBudgetOverage`
     （settlement 标签、reserved、charged、spent、limit、overage）。数字不被截到预算以内；
     之后任何 `try_reserve` 都会失败，因此后续**发起次数为零**。

4. **provider**：`RetryBudget::reserve` 改为 `pub`（确定性 fake transport 可直接复用同一套账目逻辑）；
   `settle_retry` 改用 `settle_with_usage`，无 usage 时保持「Drop 即 unknown」，不再伪造 measured 0。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo check --workspace --all-targets` | 0 error |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test -p kroma-core --test t04_budget_lifecycle` | 8 passed / 0 failed |
| `cargo test --workspace --no-fail-fast` | **852 passed / 0 failed**（`logs/test_t04.log`） |

新增 `crates/kroma-core/tests/t04_budget_lifecycle.rs`（8 条，对应任务卡里的四条语义）：

- `finite_budget_transient_then_success_has_two_wire_attempts`：两次 wire attempt；第一次 unknown 计费、
  第二次精确计费；`reserved == 0`；并**反向断言**「预占整份余额会让重试拿不到额度」。
- `not_dispatched_refunds_but_unknown_dispatched_does_not`：`settle_not_dispatched(0)` 全额退回；
  `settle_not_dispatched(1)` 被拒且整份按 unknown 计费；余额精确。
- `unknown_and_estimated_settlements_keep_their_basis`：三种结算的 `settlement` 标签与
  `settled_tokens + unknown_tokens == spent` 守恒；`estimated_tokens` 单独可查。
- `real_usage_above_the_reservation_is_charged_and_audited`：实际 13 000 > 预占 2 000 → 全额计费、
  `over_reservation = 11 000`、`budget_overage = 3 000`，且此后 `try_reserve` 必然失败（零发起）。
- `parallel_children_and_retry_share_one_limit`：3 个线程子代理 + 其中一个瞬时失败后重试，
  共享 30 000 上限；任意时刻 `committed ≤ limit`，结束后 `spent` 等于各子代理结算之和（无双记/漏记），
  余额只够两份分配、第三份被拒。
- `retry_allowance_competes_with_siblings_for_the_same_balance`：重试额度来自共享账本，
  兄弟占掉一半后只剩一次重试机会，第二次重试被拒。
- `zero_retry_allowance_is_refused`：`per_attempt = 0` 被拒，不会变成免费重试。
- `per_request_output_does_not_consume_entire_run_allowance`：真实 `AgentRunner` + 200 000 额度，
  捕获 provider 实际收到的 `max_output_tokens`，断言 ≤ 8 192。

### 变异验证

把 `max_output_tokens` 改回「运行剩余额度」后，`per_request_output_does_not_consume_entire_run_allowance`
立刻红，报文直接指出「一次请求吃掉了 200 000 里的 197 855」。

## 四、既有测试的改写（不是放宽断言）

三处既有断言把旧缺陷写成了规格，按 T04 语义改写并在文件里写明理由：

- `budget_contract.rs` 7 处 `ledger.spent() >= 64_000` → `>= 8_192`，并新增 `spent() <= 64_000` 上界。
  不变量没变：**post-dispatch 的不确定失败永不退成 0**；变的是「一次请求能预占多少」。
- `provider_contract.rs::runner_retry_exhaustion_retains_first_attempt_and_sends_once`：
  从「只发 1 次、整份 64 000 计为 unknown」改为「发 3 次（1 次 + 2 次重试）、每次各自预占、
  全部按 unknown 计费、`reserved == 0`、`spent <= 64 000`」。测试名里的 "sends_once" 已与语义不符，
  保留原名会误导，后续在 T30 统一改名（改名不影响语义）。

## 五、边界

- `Estimated` 与 `CompletedUsage` 在账本上分开统计，报告里不会把估算说成实测。
- 取消/超时仍走 `Drop` → unknown（T04 没有放宽这一点，只是把「一次请求能预占多少」改成 per-request 上限）。
- 未做：把 `max_output_tokens_per_request` 接到配置文件/CLI（当前只能由 API/BudgetConfig 设置）。
  任务卡修改范围是 budget/agent_loop/provider，配置键留给 T30 统一处理，已记在台账。
