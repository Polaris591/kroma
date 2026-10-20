# M2-01 冻结实验协议

`experiments/prereg/E01..E08.json` 八份预注册，字段全部来自蓝图
`EXPERIMENTS.json` 的 `preregister_required`：任务簇定义、模型身份可观测性、质量裕度、
噪声与功效计划、缓存顺序、预算、停止规则、holdout 政策。全部 `status: PREREGISTERED_NOT_RUN`，
并写明未跑的原因（需要冻结的 dev/holdout 套件、工作区外验证器、真实 provider 运行）。

`scripts/verify-prereg.py` 机器校验八项：缺字段、dev 与 holdout 相同、验证器落在工作区内、
没有结果却宣称状态——四种都会被点名并 exit 1。`scripts/test-verify-prereg.py` 用变异副本
逐条证明它会红（8 项检查，含基线正例）。

设计要点：预注册先落盘，所以将来跑实验时不能"看了结果再定标准"；holdout 政策写明**只查一次**、
用过即退役为 validation，再要确认性结论必须重新冻结一个 holdout。

收口：验证器 8 份全过；变异测试 8/8；无 Rust 改动，全量沿用上一包结果。
