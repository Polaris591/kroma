# C00-07 修订失效、压缩事务、证据 GC

代码：`crates/kroma-core/src/evidence_lifecycle.rs`（`FactBinding` / `FileRevision` / `PinnedSet`）。
压缩事务复用既有 WAL 路径，不新增实现。

## C00-07-A1 外部修改触发失效

`facts_go_stale_when_their_subject_or_environment_moves`：一条事实记录它观察时的
`(path, len, mtime_ns, sha256)` 与工具链。四种结局都被钉住：

- 三项全同 → `Fresh`；
- **长度与 mtime 相同、字节不同 → `Stale`**（这正是 `len`/`mtime` 看不见的窗口）；
- 文件消失 → `Missing`（缺失不是新鲜）；
- 工具链从 1.85.1 变成 1.86.0 → `EnvironmentMoved`，即使源码一字未改。

`bindings_serialise_with_their_dependencies`：依赖必须随事实一起持久化，否则重启后无法判失效。

## C00-07-A2 压缩事务：崩溃只回旧或新状态

**处置：复用既有 WAL，不新建事务层（no-change + 证据）。**

压缩结果通过 session 的 append-only overlay 落地（`session.rs` 的
"Persist compaction-induced changes to the in-memory canonical transcript without rewriting"），
即不重写历史条目、只追加新投影；恢复时按 WAL 校验（`session.rs` 里比对
`meta.compaction` 与既有条目）。崩溃窗口由两条已有测试覆盖，本次实跑均绿：

- `crash_mid_wal_append_discards_the_torn_line`：半行写入被丢弃，回到旧状态；
- `crash_mid_restore_finishes_the_recorded_restore`：恢复意图存在时补齐，得到新状态。

因此「新摘要配旧边界」这种中间态不会持久化：要么整条 append 在，要么被当成半行丢掉。
**这条的边界**：这里证明的是 WAL 追加的原子性，不是「摘要内容与 kept boundary 在语义上一致」；
后者要等投影器消费 C00-06 的标签后才有断言对象。

## C00-07-A3 GC 不误删 pin 证据

`pins_survive_gc_until_the_last_reason_is_released`：`PinnedSet` 支持同一份证据多个 pin 理由
（checkpoint、active-task）。`partition(live)` 把 live 集合分成「可回收」与「保留」两份：

- 有 pin 的一律保留，即使它在 live 列表里；
- 释放一个理由后仍保留（另一个理由还在）；
- 释放最后一个理由后才进入可回收；
- 重复释放同一个理由不报错，但也不会伪造成功（返回 false）。

## 明确还没做的

`FactBinding` 与 `PinnedSet` **还没有被任务状态层与 GC 调用方使用**：
`EvidenceStore::gc` 目前仍只接受调用方给的 live 集合。接线点很明确——
`task_state` 在记录「验证通过」时存 `FactBinding`，GC 前用 `PinnedSet::partition` 过滤 live 集合。
这两处接线与 C00-06 的标签消费是同一批工作。

## 收口

- 定向：`cargo test -p kroma-core --lib evidence_lifecycle`（3 条）+ 两条崩溃恢复测试
- clippy 0；全量见 `evidence/c00-07/full-suite.log`
