# M1-04 逐 Job 隔离与证据封存

`crates/kroma-core/src/job_scope.rs`。

- **A1 独立环境**：`JobScope::open(root, job_id)` 建 `<root>/<job_id>`，同一 id 再开报 `AlreadyExists`；
  Drop 时删除自己的目录（`keep()` 表示调用方接手，bench 用它保留每个 arm 的目录作为证据）。
  两个 job 的路径不可能相同。
- **A2 取消扫清派生进程**：每个 job 持有一个 `ChildRegistry`（`(pid, pgid)` + 登记时的内核
  start-time，内存态，不落盘、不进 prompt）。经 scope 启动的子进程和**经 `ProcessSupervisor`
  为这个 job 启动的子进程**（bench 的 `verify_command`）都登记进去。`cancel` 先拒绝新启动，
  再 kill+wait 自己的子进程，然后由 registry 按记录的进程组回收并**回查 `/proc`**：仍跑着的
  以 `StillAlive{pids}` 报出（僵尸算“已停”，不算存活）。
- **A3 封存后才评分**：`seal_workspace(workspace, dest, scored)` 先从活动工作区取被测文件的
  sha256，再把整棵树复制到 `dest`，然后用 `ScoreInput::new` 按先前摘要**逐文件复核副本**，
  对不上报 `Tampered`。封存后对活动工作区的写入只改工作区，不改已封存字节。

没有 `From<String>` 这类入口：评分器只要能收到未封存字节，早晚会收到未封存字节。

接线状态（M1-04 收口见 `evidence/m1-04-wiring/RESULT.md`，子进程半边见
`evidence/m1-04-children/RESULT.md`）：

- **已接**：`run_paired_benchmark` 用 `JobScope::open` 拥有 `<root>/<strategy>/<task>__r<rep>`，
  同名再次 open 报错而不是静默复用；`BenchExecutor::execute` 收到的就是这个 scope（拿不到裸路径，
  所以一个 arm 无法跑在自己的 job 之外）；`run_arm` 与 `SyntheticBenchExecutor` 都先
  `seal_workspace` 再评分，`verify_command` 也跑在封存副本里（封存副本放在 `<arm>__state/sealed`）。
- **已接（子进程）**：`ProcessSupervisor::execute_one_shot` 在 spawn 后、第一次 await 之前把
  `(pid = pgid)` 登记进 job 的 `ChildRegistry`；`evaluate_with_sandbox` 在给出判定前调用
  `reclaim()`，回收不掉（`StillAlive{pids}`）就记 `verdict_unknown` 并列出 pid，绝不当作干净拒绝。
- **未接**：非 bench 路径的子进程（agent 的 `bash`/MCP/hook/completion-gate 工具子进程、
  `detect_git_commit` 的 provenance `git`）仍是各自 launch seam 的，不进任何 job registry；
  `JobScope::cancel` 仍无生产调用方（bench 目前没有 arm 级中止信号，生产回收走的是
  `verify_command` 超时/取消后的 reclaim）。pid 复用用 start-time 比较挡住，但 `setsid` 逃逸的
  后代不在任何记录里——按 pid 本来就列不出来。
