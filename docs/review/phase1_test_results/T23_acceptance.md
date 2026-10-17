# T23 · 评审工作区快照身份与恢复不变量 — 验收

阶段 G3｜依赖 T07, T08, T11｜关联：无独立 KR

## 一、做法：先测量，再决策

任务卡明确要求「此项在得到动态证据前不得标为已确认 Undo 缺陷」。所以先写 5 条端到端测量用例
（`crates/kroma-core/tests/t23_snapshot_identity.rs`），看实现到底怎么表现，再写 ADR 决定是否迁移。

**测量结论：undo / 恢复 / 分支 / 崩溃重放都没有因为内容去重而丢历史。** 唯一被改写的是
manifest 里的父指针——而历史事件的真源是 session WAL。

## 二、实测事实

| # | 场景 | 结果 |
| --- | --- | --- |
| 1 | A → B → A | 两次 A 得到**同一** `RevisionId`；磁盘上 A 的 manifest 的 `parent_revision_id` 被改写为 B |
| 2 | 同上的 WAL | `RevisionRecord` 保留**两个** A 事件（turn 0 父 None、turn 2 父 B） |
| 3 | undo 目标 | `revision_at_or_before_turn(0/1/2)` = A / B / A |
| 4 | 两分支到同内容 | 不同 store、不同父链、相同内容 ⇒ 同一个 `RevisionId`；各自 manifest 父指针不同 |
| 5 | 崩溃重放 | 重开后 3 个事件、3 个可加载 manifest、3 个 undo 目标完好 |
| 6 | 外部 rename | `restore_revision(.., expected=Some(base), force=false)` → `RestoreResult::Conflict`，冲突路径是消失的 `b.rs`，未写任何文件 |
| 7 | 外部 chmod | 同样 `Conflict`（路径 `a.rs`）；`force=true` → `Applied`，并恢复记录的 **0644** |
| 8 | 多次 undo | turn 1 → B、turn 0 → A；重复恢复同一目标幂等 |

## 三、决策（ADR-0001）

`docs/adr/0001-snapshot-identity.md` 已接受：**保留现有实现，不迁移存量会话**，并把两个身份写清：

| 概念 | 载体 | 语义 |
| --- | --- | --- |
| SnapshotId（内容身份） | `RevisionId` = `compute_revision_id(tracked)` | 同一 path+hash+mode ⇒ 同一 id |
| RevisionEventId（历史事件身份） | WAL `RevisionRecord`（turn + parent + leaf） | 每次 capture 一个事件，顺序由 WAL 决定 |

配套约束（已写进 `WorkspaceRevision.parent_revision_id` 的文档注释并指向 ADR）：

1. 父指针是 **best-effort 提示**，不是不可变历史图（同内容再次捕获会被改写）。
2. undo / `--restore-rev` 目标**只能**来自 WAL（`revision_at_or_before_turn` / checkpoint /
   `latest_revision_id`）——CLI 正是这么做的（`journal.latest_revision_id()` 作为 `expected`，main.rs:1636）。
3. 恢复必须传 `current_expected_rev`；传 `None` 时实现用 live 对 **target** 比较（更严格，会拒绝合法 undo）。
4. manifest 每内容一份，最后写者胜。

**明确不承诺**：只看 manifest 目录重建完整历史 DAG；从父指针判断「内容首次出现的轮次」；
跨 session 历史合并。

## 四、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t23_snapshot_identity` | **5 passed / 0 failed** |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | **968 passed / 0 failed**（`logs/test_t23.log`） |

## 五、为什么不迁移（测量依据）

1. 所有必需不变量（undo 目标、恢复、分支、崩溃重放、冲突检测）都由 **WAL + 内容 id** 满足；
2. 迁移需要给每个事件生成独立 manifest（或 sidecar），会重写存量对象，
   而任务卡明确禁止「凭设计风险执行存量会话大迁移」；
3. 触发迁移的条件写在 ADR 第 6 节（无 WAL 的历史查询、需要给同一内容的多事件附不可变元数据、
   需要跨 session 合并历史），现在一个都不成立。

## 六、边界

- 没有修改 `compute_revision_id`、没有改 blob 布局、没有动恢复算法；只加了文档约束与测试。
- 「同一内容多事件」时 manifest 的父指针以最后一次为准，这是**已记录的非承诺**，不是缺陷。
- 测试里 undo 显式传入 `expected`（与 CLI 一致）；不传 `expected` 时冲突判定更严格，
  这一点在 ADR 里写明，避免以后有人误判为 bug。
- 故障矩阵已列在 ADR 第 5 节（同内容再捕获、两分支、崩溃重放、rename、chmod、多次 undo、
  blob 缺失）。
