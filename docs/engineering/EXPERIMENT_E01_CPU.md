# E01（CPU 半）：工具暴露模式的启动与驻留成本

脚本：`scripts/measure-exposure-cost.py`；数据：`/opt/kroma-v4/evidence/e01/exposure-cost.json`。

## 预注册与本次测量范围

预注册（`experiments/prereg/E01.json`）的主指标是交互启动、空闲进程树 PSS、空闲唤醒、
激活延迟、取消后平台期。**本次只测了前两项的 CPU 部分**：三臂 × 5 次重复、顺序交替、
每次空闲 3 秒、每 100ms 采样 `/proc/*/smaps_rollup` 求和。

三臂：`full`（默认全部工具）、`on-demand`（`--on-demand-tools`，只钉核心 schema）、
`minimal`（`--tool-filter read,edit,write,bash`）。

## 结果

| 臂 | init 中位 (ms) | 空闲 PSS 中位 (KB) | 同臂 PSS 区间 (KB) |
|---|---|---|---|
| full | 6.14 | 18 880 | 11 201 – 19 096 |
| on-demand | 6.18 | 19 016 | 11 213 – 19 228 |
| minimal | 6.13 | 18 692 | 11 269 – 18 884 |

**臂间中位差 324 KB，臂内极差 8 027 KB——臂间差异是噪声的 1/25。**
按预注册规则（差异小于噪声带 3% 视为无税，实际是 4%），结论是
**没有测到工具暴露模式带来的存在税**，而不是"更少 schema 更省内存"。

这个零结果有一个明确解释：ACP 的 `initialize` 响应在三种模式下**逐字节相同**（323 字节，
不含工具 schema），说明 schema 只在组装模型请求时才物化。所以空闲态本来就不该有差异——
测到的正是"这个阶段不存在该成本"。

## 因此

- 可以主张：在当前 acp 空闲路径上，钉住更少的工具 schema 不改变启动与驻留（噪声内）。
- **不能**主张：工具暴露不影响成本。schema 字节数与"无效模型动作数"都要等一次模型请求，
  而这台机器现在没有可用的 provider 凭据（见 `evidence/e04/ATTEMPTS.md`，HTTP 401）。
- E01 仍是 **partial**：主指标里的空闲唤醒、激活延迟、取消后平台期未测；
  模型侧的 schema 成本与 E04 的"无效动作数"未测。
