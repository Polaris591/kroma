# C00-10 可恢复副作用

三条验收规格逐条处置。代码在 `crates/kroma-core/src/task_host.rs` 与
`crates/kroma-core/src/tool/policy.rs`，测试在 `tests/durable_tasks.rs`。

## C00-10-A1 结果未知不得按未执行处理

**处置：PASS（本包前序提交已覆盖，本次复核）。**

崩溃恢复不会把「已开始、结果不明」的任务当成没跑过：`restart_recovery_marks_a_started_task_unknown_and_refuses_resume`
要求重启后进入 `TaskState::Unknown` 且拒绝恢复；
`resume_of_a_running_task_is_refused_and_runs_no_backend` 要求拒绝时不调后端；
`worker_resume_that_fails_the_completion_gate_is_not_completed` 要求门禁失败不得标 `Completed`。
`resume_without_the_writer_lock_calls_the_backend_zero_times_and_stays_resumable` 保证拿不到写租约时
既不执行也不把任务推进到终态。

同一原则在崩溃恢复路径上也要成立：`recover_pending_restore` 此前**从不**接收调用方的
`--force-restore`，于是「崩溃的恢复要重放某个路径，而用户在停机期间改过这个路径」这一情形下，
它要么默默覆盖用户的字节，要么永远卡住。现在 CLI 把 `cli.force_restore` 传下去：没有 force 时
拒结重放、保留用户字节、保留 intent 日志（恢复仍可稍后完成）；有 force 时才按用户显式覆盖。
两条测试钉住：`user_edit_during_a_crashed_restore_is_refused_and_the_journal_kept` 与
`force_restore_completes_a_crashed_restore_over_a_user_edit`。

## C00-10-A2 恢复必须校验授权与剩余预算

**处置：预算腿已存在，本次补上策略腿；授权期限腿记为不适用并说明原因。**

- **剩余预算**：`resume_task` 先算 `remaining = token_allocation - token_spent`，为 0 直接拒绝；
  恢复时把子预算收敛为 `min(spec.budget.max_tokens, remaining)`，并把 `TokenBudget::new(Some(remaining))`
  作为该次运行的硬上限。已存在，本次复核。
- **策略（本次新增）**：此前恢复用的是**记录里的** `parent_policy`，等于把旧进程的授权原样搬到新进程。
  现在改成 `recorded.meet(current)` —— 逐维度取交集，只在两者都授予的维度上保留自动权限。
  在拿写租约**之前**用交集重新校验角色授权：拒绝时后端零调用、不物化 worktree、任务仍是
  `Suspended`（可稍后重新授权恢复），并明确指出需要 `--allow-write`。
  `PolicyMode::meet` 有独立单测覆盖：交换律、任一维度都不放大、`AllowWrite ∩ AllowShell = ReadOnly`
  （两个模式互不可比，所以这是交集不是枚举序 min/max）。
- **授权期限**：产品里**没有**任何时间维度的授权到期概念——durable 记录只有
  `parent_policy`、`token_allocation`、`token_spent`，没有 `expires_at`。为这一条凭空造一个过期时间
  等于新增授权语义，且会带来「机器时钟跳变就拒绝恢复」的新故障模式。因此本包如实记为**不适用**，
  而不是用一个自造字段假装满足。恢复时的授权收紧由「策略交集 + 剩余预算 + 写租约」三条实际约束承担。

## C00-10-A3 检查—写入竞态有测试

**处置：逐路径冲突判定与租约/并发测试已存在；本次补上 CLI 的 `--force-restore` 透传。**
本段不宣称 C00-10 整体收口（由 Controller 判定）。

- **逐路径冲突判定（已在 `revision.rs`）**：`recover_pending_restore_with_force` 在 `force=false` 时
  先跑 `restore_recovery_conflicts`，逐路径核对三种可证明的 live 状态——路径不存在、已是目标内容、
  仍是意图记录的父版本内容；其它任何 live 状态（Kroma 停机期间被用户改写）一律返回
  `RestoreResult::Conflict` 并**保留**意图日志。意图没有记录父版本、或父版本清单已不在库里时，
  前置状态属于「不可证明」，同样按冲突处理（fail-closed），不当作「没改过」。
- **本次修复（CLI force 透传）**：`crates/kroma-cli/src/main.rs` 的恢复分支此前调用
  `recover_pending_restore(&workspace)`，丢掉了调用者的 `--force-restore`：用户遇到未完成的恢复意图时，
  即使明确要求强制覆盖也被拒。现在改为
  `recover_pending_restore_with_force(&workspace, cli.force_restore)`。冲突判定只看**本次调用**的 force
  （`if !force`），意图里记录的 `force` 不参与该判定——它只描述原始操作打算怎么落地，不是停机期间
  新出现改动的长期授权。`revision.rs` 内部 undo/restore 流程里调用的 `recover_pending_restore`
  （force=false）属于另一次操作的收尾，保持不变。
- **强制是显式授权，不是默认行为**：不带 `--force-restore` 的恢复仍然拒绝覆盖停机期间的用户改动；
  `--force-restore` 本身仍要求与 `--undo` / `--restore-rev` 同时出现（`main.rs` 的参数约束）。
- **测试（`crates/kroma-cli/tests/crash_recovery_matrix.rs`，真实进程 SIGKILL）**：
  - `user_edit_during_a_crashed_restore_is_refused_and_the_journal_kept`：崩溃在恢复中途，停机期间
    「用户」改写待重放路径，再跑 `--restore-rev <target>`（无 force）→ 退出码 3
    （`EXIT_RESTORE_CONFLICT`），磁盘保留用户写入的字节，恢复意图日志仍在。
  - `force_restore_completes_a_crashed_restore_over_a_user_edit`：同一状态加 `--force-restore`
    → 退出码 0，用户改动被覆盖成目标版本，意图日志被清除。
  - 变异验证：把透传临时改回 `false`，上一条用例变红（退出码 3 而非 0），改回后全绿。
- **租约/并发（已存在，本次复核）**：`resume_task` 先 `try_acquire_writer` 拿写租约，之后的状态变更与
  后端执行都在租约内；拿不到租约时零调用并保持可恢复。并发场景由
  `two_concurrent_resumes_run_the_backend_once_and_cancel_hits_only_that_run` 覆盖（两个并发恢复只跑
  一次后端）。A2 新增的策略拒绝路径同样发生在租约之前，且由测试断言「拒绝后状态仍是 `Suspended`」，
  因此不存在「检查时通过、写入时已失效」的窗口。

## 收口

- 定向测试：`cargo test --test durable_tasks`（33 条）、`cargo test -p kroma-cli --test crash_recovery_matrix`（11 条）
- 全量：76 个测试二进制、0 failed
- 证据：`/opt/kroma-v4/evidence/c00-10/`
