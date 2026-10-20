# M4-03 能力支持矩阵

`scripts/capability-matrix.py` 从 `docs/capabilities.json`（20 项能力的五个布尔字段）生成
`docs/CAPABILITY_MATRIX.md`，补齐蓝图要求的六轴里缺的两轴，并明确拒绝编造：

- `platform_support`：全部 `unverified`——本树没有任何第二平台的能力级验证；
- `lifecycle`：全部 `not_managed`——激活租约尚未接进运行路径；
- `evidence`：由 manifest 推导（contract_tested 20 项、live_tested 0 项）；
- `authorization`/`default`：只反映 manifest 的 `default_on`（15 项 on）；
- 发布成熟度不由本矩阵给出。

矩阵是生成的，不手工维护；`evidence/m4-03/` 存了生成日志与快照。
