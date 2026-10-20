# C00-06 上下文来源/作用域/敏感度类型化投影

代码：`crates/kroma-core/src/context_authority.rs`（`AuthorityClass` / `Sensitivity` /
`ContextSegment` / `SegmentLedger`）。

## 三条规格

### C00-06-A1 摘要不提升权限（负向）

`a_summary_cannot_escalate_authority_or_launder_sensitivity`：两个 `ToolOutput` 父段派生摘要。
声明 `HostPolicy` → `AuthorityEscalation { declared: HostPolicy, allowed: ToolOutput }`，被拒。
不声明 → 允许，权限为父段里**最不**特权的那个，敏感度为父段里**最严**的那个
（`Workspace` + `Public` → `Workspace`）。即摘要既不能升权，也不能把敏感内容"洗白"。

第一版把「最不特权」写成 `min`，而枚举顺序里 `HostPolicy` 最小，于是任何摘要都被允许
声明宿主权限——测试直接抓到，已改成按序取最大（最不特权）。

### C00-06-A2 召回重查 scope（负向）

`recall_is_refused_across_scopes_and_after_the_segment_is_gone`：
`ws-b` 拿合法 id 读 `ws-a` 的段 → `WrongScope`；同 scope 可读；`forget` 之后同一 id → `Unknown`。
每次召回都重新比较，不缓存"上次查过"的结论。

### C00-06-A3 字段完整

`segments_round_trip_with_every_field_and_refuse_cross_scope_merges`：
序列化形式包含 `id / origin_kind / authority / sensitivity / scope / derivation_parents /
observed_revision` 七个字段，反序列化后与原段相等；跨 scope 的两个父段不能合并
（`ScopeMismatch`），避免把两个工作区的上下文缝成一段。

## 明确还没做的

**这套标签还没有被投影器与召回路径消费。** 本包交付的是标签集合与它的两条强制规则；
要让它产生端到端效果，`context_projector` 生成 stub 时必须带上 authority/sensitivity/scope，
`recall_evidence` 读取时必须走 `SegmentLedger::recall`。这两处接线在 C00-07（压缩事务）
与 M1-03（评分隔离）之前完成，否则只能说"标签存在"，不能说"投影不会升权"。

## 收口

- 定向：`cargo test -p kroma-core --lib context_authority`（3 条）
- clippy 0、fmt 通过、全量见 `evidence/c00-06/full-suite.log`
