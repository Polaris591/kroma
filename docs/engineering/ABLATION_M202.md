# M2-02：机制消融与强简单基线

数据：`/opt/kroma-v4/evidence/m2-02/`（`report.json`、`arms-context.json`、`stderr.log`）
与 `/opt/kroma-v4/evidence/e04/`（工具因素）。

## 三条验收规格

1. **无机制开关改变安全权限** — 结构性成立：`PolicyEngine` 由 `settings.policy_mode` 构造
   （`bench.rs:2366`），不读臂的任何字段；`max_subagents = 0` 只会把 `spawn_subagent`
   从可见工具里删掉，是收窄。见 `evidence/m2-02/RESULT.md`。
2. **未知费用不剔除样本** — 报告里 `cost_usd` 为 null 时保留全部样本并写明
   `cost_unknown_reason`，不丢 run。
3. **内部候选数与单次 run 成功区别明确** — `metadata.tournament_candidates` 记录候选数，
   `children_tokens` 与 `self_tokens` 分列，k 臂的 token 含候选子运行。

## 消融结果（单因素，配对 + 簇级 bootstrap）

| 因素 | 对照 | 质量 | 每任务 token 差（95% CI） | 判定 |
|---|---|---|---|---|
| 上下文 | p2 vs p2+因果剪枝 | 0.0 pp，CI [0,0] | +28 728 [−966, +63 591] | inconclusive |
| 工具 | 全部 schema vs 延迟发现 | 0.0 pp，CI [0,0] | −12 381 [−27 044, +1 306] | inconclusive |
| 缓存 | 无开关 | — | — | 不可消融，只记录命中率 |

两个可消融因素的**方向相反**、区间都跨零：没有证据支持"打开因果剪枝更省"，
也没有证据支持"延迟发现更省"。两个机制默认都关闭，本次数据支持保持默认。
