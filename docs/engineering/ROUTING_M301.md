# M3-01 有界规则路由

`crates/kroma-core/src/routing.rs`。三道刹车与一条不可协商的规则：

- **滞回**：`up_threshold=0.7`、`down_threshold=0.3`，落在带内一律 `Held`。测试用 0.69/0.65/0.5/0.31/0.7 连续观察，只发生 1 次切换。
- **低置信回退**：`confidence < 0.4` 时无论复杂度多高都退回 `Simple`（宁可便宜不可乱猜），且回退本身也受刹车约束。
- **驻留与上限**：`min_dwell` 内一切不动（包括低置信回退）；`max_switches` 用尽后保持当前策略并报 `SwitchCap`。

接线状态（E08 之后）：`Router` 已接进运行路径——`agent_loop` 每轮用**实测**信号
（本轮工具返回字节 → 复杂度、工具错误率 → 置信度）调用一次 `observe`，决策逐条写进
`RunSummary.route_decisions` 与 bench 报告，并只拥有一项权力：是否允许动用昂贵策略
（`Plan` 把修复竞赛门槛从两次校验失败降到一次）。E08 实测：73 次决策零切换、零升级。
当时能说的只是"路由决策不会震荡"，现在可以说"决策被记录且可审计"；
"Kroma 的运行已经按规则路由"。

定向 3 条 + clippy 0 + fmt 干净；全量见 `evidence/m3-01/full-suite.log`。
