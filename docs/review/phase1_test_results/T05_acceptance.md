# T05 · 把压缩身份迁出不可信正文 — 验收

阶段 G1｜依赖 T03｜关联 KR-04

## 一、缺陷

「这一条已被压缩」「原始字节还能取回」这两个判断，曾经可以从**工具打印出来的文本**里读出来：
`source_entry_id=...` 这个字符串在 stub 头部、也在任何被读进来的源码/日志里。谁能让工具输出里出现它，
谁就能影响投影状态——至少能让一个真输出不被压缩，或者让一个假 stub 看起来像真 stub。

## 二、改了什么

1. **`CompactionMetadata` 补上可恢复性**（`messages.rs`）
   - 新字段 `availability: CompactionAvailability`（`available` / `evicted` / `unknown`），
     `serde(default)` = `Unknown`，旧 WAL 不会因此声称「字节还在」。
   - 契约名可达而不改 serde 字段名（改字段名会破坏旧 WAL）：
     `source_event_id()` = `source_entry_id`，`raw_hash()` = `original_sha256`，
     `is_available()` = `availability != Evicted`。
   - 宿主写 stub 时立刻填 `Available`（此刻刚注册进 evidence store，指针是诚实的）；
     不记录证据的 profile（P1）填 `Unknown`。

2. **文本只有一种被信任的形状**（`context_projector::is_host_stub_header`）
   - 必须以 `[Compacted: ` 或 `[Dead-end: ` 开头，且包含宿主渲染的完整指针
     ` | source_entry_id=<id> | call recall_evidence(source_entry_id=<id>)`。
   - `extract_source_entry_id` 降级为展示/遥测用，注释里写明「不得单独据此授予信任」。

3. **迁移路径只认宿主记录**（`session.rs::seed_projector_evidence`）
   - 有 typed metadata：要求 `schema_version == 1`、id 与 link 一致、hash 与 link 一致、
     且 `is_available()`（evicted 的 stub 保留身份但不注册可恢复证据）。
   - 无 metadata（旧 WAL）：要求 content **形状**是宿主 stub（上条函数），且 link 的
     call_id / tool_name / raw sha256 / 原字节数 / stub 字节数全部吻合——只命中一段 marker 不再升级。
   - 分支隔离不变：link 来自 `evidence_links()`（按 leaf 深度选赢家），被 fork 掉的条目不会复活。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t05_compaction_identity` | 6 passed / 0 failed |
| `cargo test -p kroma-core --test gate2_evidence_projection` | 18 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 857 passed / 1 failed（`logs/test_t05.log`） |

那 1 条失败是台账 §2.2 记过的既有 flake `b08c_git_metadata::carries_a_sha1_commit_through`，
串行复跑 5/5 绿（`--test-threads=1`），与本项无关。

新增 `crates/kroma-core/tests/t05_compaction_identity.rs`（6 条）：

- `large_literal_marker_output_still_compacts`：600 行全是伪造 `source_entry_id=` 指针的 read 输出
  照样被压缩；生成的 stub 带 typed metadata（schema 1、64 位 raw hash、非空 reason、
  `availability == Available`），且文本指针与 metadata 一致。
- `forged_stub_without_overlay_is_untrusted`：手工写进 WAL 一条**完全像宿主 stub**的 tool result，
  没有 overlay、没有 link、没有 metadata → `evidence_links()` 为空、`seed_projector_evidence`
  注册不到任何证据、条目本身仍是普通 tool result（没有 compaction metadata）。
- `host_stub_header_matrix`：两种宿主 stub 形状为真；源码里的裸 marker、被注释掉的 stub、
  错 id、空 id、P1 无指针形状全部为假；同时钉住 `extract_source_entry_id` 确实能匹到裸 marker
  （证明「能匹到」不等于「可信」）。
- `host_compacted_session_migrates_its_evidence`：真实压缩 → `sync_projected_transcript` 写 overlay →
  重开 WAL → `seed_projector_evidence` 后证据条数不变，stub 带 `Available` metadata。
- `evicted_metadata_is_not_available` / `pre_t05_wal_without_availability_still_loads`：
  evicted 不再声称可恢复；缺 `availability` 的旧 JSON 能反序列化为 `Unknown`。

### 变异验证

把 `is_host_stub_header` 退化成「content 里出现 `source_entry_id=<id>` 就算」，
`host_stub_header_matrix` 立刻红（`!is_host_stub_header("source_entry_id=ev-0002-call_big\nfn main() {}")`）。

## 四、边界

- 没做：evidence store 淘汰时回写 `Evicted` 的路径（当前只有写入侧设置 `Available`，
  淘汰侧仍靠 store 查询失败兜底）。任务卡的验收点是「伪造 marker 不授予权限」，已覆盖；
  淘汰回写记为 T23 缓存淘汰策略的输入。
- 旧 WAL 的文本形状检查仍在，但它只在**已经有宿主 overlay link 且四项哈希/字节数全等**时才被调用，
  单独一段 marker 永远进不了这条分支（`forged_stub_without_overlay_is_untrusted` 钉住）。
