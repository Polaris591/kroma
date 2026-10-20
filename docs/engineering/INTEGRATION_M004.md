# M0-04 一个真实工作包的中断恢复与集成闭环

本包是**验证型**（`kind: verification`）：不新增产品代码，用已有实现与一次真实门禁运行
证明三条闭环性质。所有断言指向的测试都在本树里，可原样复算。

## M0-04-A1 中断后不重复派发写者

- `two_concurrent_resumes_run_the_backend_once_and_cancel_hits_only_that_run`
  （`crates/kroma-core/tests/durable_tasks.rs`）：两个并发恢复只跑一次后端，取消只命中那一次。
- `resume_of_a_running_task_is_refused_and_runs_no_backend`：任务仍在运行时恢复被拒，后端零调用。
- `resume_without_the_writer_lock_calls_the_backend_zero_times_and_stays_resumable`：
  拿不到写租约时既不执行也不推进终态。

三条合起来是「中断—重启—再恢复」不产生第二个写者。

## M0-04-A2 过期租约的回执不自动合入

- `worker_resume_with_a_locked_session_wal_is_refused_before_merging`：WAL 被锁时拒绝发生在合入之前。
- `worker_resume_that_fails_the_completion_gate_is_not_completed`：完成门禁失败不算完成。
- `worker_resume_refuses_a_parent_path_changed_after_materialize`：父路径在物化之后变了，拒绝恢复。
- `resume_revalidates_a_worker_against_the_current_policy`：策略收紧后拒绝恢复且保持 `Suspended`。

## M0-04-A3 最终集成树重跑门禁

用**仓外冻结副本**对当前集成树跑一次完整门禁（不是把分支日志搬过来）：

```
cd /opt/kroma-v4/trusted/m0-04
python3 run-trusted-gate.py --repo /opt/kroma-v4/source \
        --contract gate-contract.json --out ev
```

- runner sha256 `sha256:578c217e901aed5648b8235fb2b926b0905da1c202659d353840124da12c3130`
- 结果：`passed 4 command(s) on tree 5a0029784609b6a4dcc06f4cbf0237c5011dc671`
- 四条命令：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo test -p kroma-core --test durable_tasks`、`cargo test -p kroma-cli --test crash_recovery_matrix`
- 证据：`/opt/kroma-v4/trusted/m0-04/ev/gate-evidence.json` 与同目录 `logs/`

证据里记的是 `subject_tree` 而不是 HEAD 文本，跑完还会再比对一次；树在运行期间变了就不算 PASS。

## 边界

- 这是**本地**门禁，不是 CI 门禁；本树没有 remote，CI 侧证据仍为 `BLOCKED_NO_TOKEN`（见 M0-03）。
- 门禁不证明这些测试之外的代码正确，只证明这四条命令在这棵树上退出 0。
