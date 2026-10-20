# M1-03 评分隔离

代码：`crates/kroma-core/src/scoring_zones.rs`。能力表是常量，四个 zone 各自只能问四个问题：
能否派发付费请求、读隐藏用例、写签名、执行候选代码。

- **A1 worker 无密钥**：`EvaluationWorker` 只能执行候选代码；写签名/派发/读隐藏用例三项全拒。
  `EvidenceSigner` 能签名但不能执行任何候选代码；`HostAuthority` 能派发但不能读隐藏用例。
- **A2 候选不能影响裁判**：能力表的唯一来源是 `Zone::capabilities()`，没有任何 setter；
  测试遍历四个 zone × 四个能力，确认 `require` 与表一致；全表只有一个 zone 能执行候选代码，
  且它就是那个没有签名权的 zone。
- **A3 Public/Hidden 分离**：`HiddenFeedback` 两种策略下 `per_attempt_visible()` 都恒为 false，
  只有 `FinalOnly` 释放一次最终结论；隐藏用例只有 controller 可读。

尚未接线：`ZoneHandle` 还没被评分执行器持用；真正跑候选的进程仍由既有 bench 路径启动。

定向 3 条 + clippy 0 + fmt 干净；全量见 `evidence/m1-03/full-suite.log`。
