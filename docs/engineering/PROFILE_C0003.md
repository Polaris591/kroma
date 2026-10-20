# C00-03 Profile 兼容收口

代码：`crates/kroma-core/src/context_projector.rs`（新增 `ProjectionCandidate` / `ProjectionSelector`
与 `ProjectionProfile::LEGACY_IDS`）。测试：同文件 `mod c0003_profile_tests`。

## C00-03-A1 四个 legacy ID 语义不变

`legacy_profile_semantics_are_unchanged` 逐条锁死基线语义，而不是只断言「能解析」：

- 默认仍是 `P2`（`#[default]` 未动）；
- `baseline` 不压缩、不记录证据；
- `p1` 压缩但不铸 `source_entry_id`；
- `p2` 压缩且记录证据、不激进；
- `p3` 在 `p2` 上再激进 25% 低水位。

这些行为由 `compacts()` / `records_evidence()` / `is_aggressive()` 三个方法承载，
测试断言的是方法返回值，所以将来若有人改枚举顺序或把 p3 改成语义压缩，测试会红。

## C00-03-A2 未知 ID 拒绝

`unknown_ids_are_refused`：`p4`、`p0`、`taskstate`、`ctx.unknown.v1`、`ctx.taskstate.v2`
一律拒绝。注意 `p4` 也拒绝——不是「没定义就默认 p3」，而是明确报错。

## C00-03-A3 新候选用 ctx.* 独立 ID

新增 `ProjectionCandidate`：`ctx.taskstate.v1`、`ctx.semantic-extractive.v1`、
`ctx.guideline-optimized.v1`。三条性质由测试钉住：

1. 候选 ID 都以 `ctx.` 开头，且**不能**被 `ProjectionProfile::parse` 接受——
   候选不会伪装成 legacy profile；
2. `enabled_by_default()` 恒为 false——未测量的算法不能因为「代码存在」而成为默认；
3. 每个候选声明它组合到的 `base_profile()`（当前都是 `P2`）：候选是叠加一层，
   不重写底层语义。

`ProjectionSelector` 是统一的配置入口：`ctx.` 前缀走候选命名空间，其余走 legacy。
本轮**没有**实现任何候选算法（那是 M3-02），这里只建立命名空间与拒绝规则，
避免「先塞进 p 序号、以后再拆」这种会把历史配置含义改掉的路径。

## 收口

- 定向：`cargo test -p kroma-core --lib c0003_profile`（3 条）
- clippy `-D warnings`：0
- 全量：见 `evidence/c00-03/full-suite.log`
