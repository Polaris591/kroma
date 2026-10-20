# ADR-0001：快照身份与历史事件身份是分开的（T23）

- 状态：**已接受**（不迁移存量会话）
- 依赖：T07（WAL 容量）、T08（写者所有权）、T11（装配）
- 证据：`crates/kroma-core/tests/t23_snapshot_identity.rs`（5 条，全部实测）

## 1. 问题

`RevisionId` 是**内容寻址**的（`compute_revision_id` 对 tracked files 的 path+hash+mode 求摘要），
而存下来的 `WorkspaceRevision` 同时还带 `parent_revision_id` / `turn_index` / `wal_entry_id`。
于是当工作区回到更早的内容（A → B → A）时，第二次 A 必然映射到**同一个** `RevisionId`：

- 要么 manifest 被覆盖，父指针被改写成最近的 capture；
- 要么需要另一个 id 表示「这次事件」。

在继续扩展状态系统之前必须回答：**现在的实现是否因此丢历史？**

## 2. 实测事实（不是推断）

| # | 场景 | 实测结果 |
| --- | --- | --- |
| 1 | A → B → A | 两次 A 得到**同一** `RevisionId`；磁盘上 A 的 manifest 的 `parent_revision_id` 被改写为 **B**（A→B 那次事件的父指针不再存在于 manifest 中） |
| 2 | 同上的 WAL | `RevisionRecord` 保留**两个** A 事件（turn 0 与 turn 2，父分别为 `None` 与 B） |
| 3 | undo 目标解析 | `revision_at_or_before_turn(0/1/2)` 分别解析到 A / B / A —— 历史事件仍然可寻址 |
| 4 | 两个分支到同内容 | 不同 store、不同父链，但内容相同 ⇒ 同一个 `RevisionId`；各自 manifest 的父指针不同（A 有父、B 无父） |
| 5 | 崩溃重放 | 关闭后重开：3 个 revision 事件、3 个可加载的 manifest、3 个 undo 目标全部完好 |
| 6 | 外部 rename | `restore_revision(.., current_expected_rev=Some(base), force=false)` → `RestoreResult::Conflict`，冲突路径为消失的 `b.rs`，文件未被改动 |
| 7 | 外部 chmod | 同样返回 `Conflict`（路径 `a.rs`）；`force=true` 时 `Applied`，且**恢复记录的 0644** |
| 8 | 多次 undo | 取 turn 1 目标 → 恢复到 B；取 turn 0 目标 → 恢复到 A；重复恢复同一目标幂等 |

**结论：undo / 恢复 / 崩溃重放都没有因为内容去重而丢历史。** 丢的是 manifest 里的
「父指针历史」，而历史事件的真源是 session WAL 的 `RevisionRecord`。

## 3. 决策

**保留现有设计，不做存量迁移。** 两个身份明确分工：

| 概念 | 载体 | 语义 |
| --- | --- | --- |
| **SnapshotId（内容身份）** | `RevisionId`（= `compute_revision_id`） | 同一份 tracked content（path + hash + mode）⇒ 同一个 id。可去重、可跨分支复用 blob。 |
| **RevisionEventId（历史事件身份）** | session WAL 的 `RevisionRecord`（`turn` + `parent_revision_id` + `leaf_id`） | 每次 capture 是一个事件；同一内容可以对应多个事件；事件顺序由 WAL 决定。 |

配套约束（写进代码注释与本文档）：

1. **`WorkspaceRevision.parent_revision_id` 是 best-effort 提示，不是不可变历史图。**
   同一内容被再次捕获时它会被改写（事实 1、4）。任何把它当历史的代码都必须先问 WAL。
2. **undo / `--restore-rev` 的目标只能来自 WAL**（`revision_at_or_before_turn` /
   checkpoint / `latest_revision_id`），不得从 manifest 的父指针链推导。
3. **恢复的冲突判定必须传入 `current_expected_rev`**（调用方认为工作区所在的 revision）。
   传 `None` 时实现会把 live 与 **target** 比较，这是更严格的读法，会拒绝合法 undo ——
   CLI 传的是 WAL 里记录的当前 revision，测试按同一方式驱动。
4. **manifest 里只有一份内容记录**：同内容多事件时，最后写者胜。要查「A 是从哪里来的」请查 WAL。

## 4. 明确不承诺的语义

- 不能只看 manifest 目录就重建出完整的历史 DAG（父指针会被改写）。
- 不能从 `parent_revision_id` 判断「这个内容第一次出现在哪一轮」。
- 跨 session 的历史合并（两个 session 的 revision 事件合成一张图）**不在承诺范围内**。
- `RevisionId` 不编码「谁在什么时候捕获了它」；时间与轮次只在 WAL 事件上。

## 5. 故障矩阵

| 故障 | 行为 | 证据 |
| --- | --- | --- |
| 同一内容再次捕获 | manifest 覆盖，WAL 追加新事件 | 事实 1、2 |
| 内容相同但父不同（两分支） | 同一内容 id，各自 manifest 父指针不同，互不覆盖（不同 store） | 事实 4 |
| 进程崩溃（未刷盘后的重开） | 已提交事件与 manifest 全部可加载；未链接的临时 manifest 由 `remove_unlinked_revision` 清理 | 事实 5 + 既有 Gate 5 用例 |
| 外部 rename | `Conflict`，不写文件 | 事实 6 |
| 外部 chmod | `Conflict`；`force` 时恢复记录的 mode | 事实 7 |
| 多次 undo | 按 WAL 事件逐个回退，幂等 | 事实 8 |
| 目标 revision 的 blob 缺失/损坏 | `load_revision` / blob 校验失败 → 明确错误，不半恢复 | 既有 `read_blob` 哈希校验用例 |

## 6. 迁移触发条件（现在**不**做）

只有当出现下列需求时，才引入独立的 `RevisionEventId` 并保留 `RevisionId` 作为 SnapshotId：

1. 需要在**没有 WAL** 的情况下查询历史 DAG（例如跨机器的状态服务）；
2. 需要对「同一内容的多个人类事件」分别附加不可变元数据（审批、来源、签名）；
3. 需要把两个 session 的历史合并且保留父子关系。

届时必须是**追加式**迁移：新增字段 + sidecar manifest + 新版本号，**不重写旧对象**，
并且保留按旧方式读取的能力。在此之前不做存量会话大迁移。

## 7. 为什么这不是「已确认的 Undo 缺陷」

任务卡要求「在得到动态证据前不得标为已确认 Undo 缺陷」。实测结果是 undo/resume/branch 全部正常：
目标来自 WAL，内容身份稳定，冲突检测有效。真正的缺陷只是**文档语义**层面的
（父指针被当成历史图），已通过本 ADR 的约束 1–4 明确掉。
