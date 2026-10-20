# M4-01 有界离线演化

`crates/kroma-core/src/evolution.rs`。候选池 + 评估预算在同一处记账：

- 池容量上限；同 `config_digest` 的候选被拒（重复配置只增加成本，不增加信息）；
- 评估**先扣费再跑**，超出预算在开跑前就拒（`requested/remaining` 都报出来）；
- 基线永不被删（赢了也要有对比对象）；未评估的候选不能当赢家；重复打分被拒。

定向 3 条 + clippy 0 + fmt 干净。尚未接线：`ahe.rs` 的搜索路径还没用这个池。
