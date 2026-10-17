# T07 · 对齐 WAL 写入与恢复限制 — 验收

阶段 G1｜依赖 T05, T06｜关联 KR-05

## 一、缺陷

写入侧和读取侧的限额是两套独立写死的数字：reader 拒绝超过 8 MiB 的行和超过 64 MiB 的文件，
writer 却只管往里写。后果是**写成功但下次起不来**——会话在磁盘上是完整的，只是没有任何受支持的
reader 愿意读它。

## 二、改了什么

1. **写前按 reader 的限额判据预检**（`validate_wal_append_capacity`）
   - 追加前计算：`pre_write_len + (序列化行字节数 + 换行)`，以及行本身的 JSON 字节数（不含换行）。
   - 行超 `MAX_WAL_LINE_BYTES`（8 MiB）或总量超 `MAX_WAL_FILE_BYTES`（64 MiB）→ 在任何字节落盘前返回错误。
   - `checked_add`：加法溢出也拒绝，不回绕。
   - 边界是「等于即允许」：`json_len == 8 MiB` 允许（reader 判据是 `> 8 MiB`），
     `total == 64 MiB` 允许（reader 判据是 `len > 64 MiB`）。writer 与 reader 用同一组常量、同一套比较方向。

2. **容量契约写进代码**（`MAX_WAL_FILE_BYTES` 的文档注释）
   - 转录条目、checkpoint、projection overlay 全部走 `append_payload`，共用**同一份**预算；
     没有任何一条路径更宽松。三者都经 `append_payload` 是结构事实（不是约定），已用测试钉住。
   - 原始工具输出的预算另算：`context_projector::MAX_EVIDENCE_BYTES = 16 MiB`，超限走显式淘汰，
     **不靠丢 WAL 记录**来省空间。
   - 超限时 fail-closed，由调用方决定怎么办；journal 永不为了不超限而静默忘掉历史。

3. **磁盘满与撕裂尾部**
   - 新增测试钩子 `WalFault::DiskFullMidWrite`：写半行后报 `ENOSPC`，回滚路径健康 → 截断回已提交前缀，
     句柄不中毒，后续追加仍可用，重开可恢复。
   - 尾部残缺（无换行的半行）→ 打开时截断到最后一个完整行（既有行为，现在有测试）；
     中间行损坏 → 拒绝打开，不修（只修合法 WAL 的合法尾部）。

4. **首版明确 fail-closed，不做分段 WAL**：因此任务卡里 `old_session_resume_and_fork_after_rotation` /
   `rotation_crash_at_each_commit_boundary` 这两条**没有 rotation 可测**——首版合同是「到上限就报错」，
   不是「轮转后仍可恢复」。按任务卡第 2 步，分段 WAL 需要 manifest + 段 hash + 原子提交 + 迁移合同，
   属于后续版本，不在 T07 实现。这一点写在这里，避免被当成漏做。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --lib session::tests` | 21 passed（原 16 + 新 5） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 见 `logs/test_t07.log` |

### 新增/加固的测试

- `wal_line_capacity_boundaries_are_dual_with_the_reader`：`limit-1` / `limit` / `limit+1` 三个点，
  前两个接受、第三个拒绝，且拒绝理由必须是「maximum line size」。
- `wal_file_capacity_boundaries_are_dual_with_the_reader`：`total == limit` 接受、`limit+1` 拒绝、
  `limit-1` 接受、`u64::MAX` 溢出拒绝；reader 侧对 `MAX_WAL_FILE_BYTES + 1` 的稀疏文件报尺寸错误。
- `writer_never_commits_a_record_reader_would_reject`：先量出 JSON envelope（实测 316 字节），
  用 writer 自己的判定找到最大可接受 payload，然后验证「该尺寸追加成功且能重开」与
  「+1 字节被拒、文件字节未变、原 WAL 仍能重开」。
- `disk_full_mid_write_rolls_back_and_the_session_stays_readable`：ENOSPC 半写 → 截断回前缀、
  seq/hash 不变、句柄可用、重开后转录完整。
- `torn_tail_is_repaired_but_corrupt_middle_is_refused`：尾部半行被截断到已提交长度；
  中间行被替换成非法 JSON 后打开失败。
- `oversized_checkpoint_is_refused_transactionally`：超限 checkpoint 一个字节都不写，
  seq/hash 不变，WAL 仍可重开——证明 checkpoint 与转录共用同一份预算。
- `oversized_projection_overlay_is_refused_transactionally`：超限 overlay 同样被拒，
  磁盘转录保持可恢复。

## 四、边界

- 64 MiB / 8 MiB 的**数值**没动（任务卡明确禁止「只是把 64MiB 改成更大」）。
- 没有在恢复时自动删除超限历史；超限只发生在写入侧，读取侧对既存超限文件是拒绝打开。
- 分段 WAL / 轮转不在本项范围（见第二节第 4 点）。
- `MAX_EVIDENCE_BYTES` 的淘汰策略本身属于 T23，本项只声明它与 WAL 预算是两套独立预算且不互相蚕食。
