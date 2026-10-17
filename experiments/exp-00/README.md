# EXP-00：冻结基线的可执行配置与运行手册（T24）

## 1. 这个目录里有什么

| 文件 | 作用 |
| --- | --- |
| `manifest.json` | 冻结的实验清单（T17 版本化 schema）：2 arms × 8 tasks × 3 repeats = 48 jobs，`basis=holdout` |
| `tasks.json` | 8 个真实任务（6 开发 / 2 留出），每个都有可执行验证命令与后置条件 |
| `prices.json` | 单一模型的价目表（含 **cache-write** 单价，T18 要求读写分价） |

**配置身份（可复算）**：`kroma --experiment experiments/exp-00/manifest.json` 打印
`digest=05757bf3fe69c9f9`（完整 64 位见 dry-run 输出），多次运行一致。

## 2. 冻结的内容

- **arms**：`baseline`（默认配置）与 `candidate-on-demand-tools`（`--on-demand-tools`）。
  模型、价目表在 manifest 里逐 arm 声明；candidate 必须先有 sandbox（T17 的 `MissingSandboxForCandidate`）。
- **任务**：6 个开发任务用于排错，2 个留出任务（`refactor-duplication`、`holdout-atomic-write`）
  **只用于验收**。留出集里出现任何调参即视为违规。
- **重复**：每 task × arm 3 次，用于量化噪声（任务卡要求「相同简单基线重跑的噪声已量化」）。
- **总预算**：`max_total_tokens = 4_000_000`（T17 的 `BudgetTooSmallForMatrix` 会拒绝明显偏小的值）。
- **oracle**：`python3 -m pytest -q tests/`；env 为 `bwrap` 沙箱（T09/T10 的执行边界）。

## 3. 运行前必须由所有者确认的一件事

**总费用上限。** 任务卡的停止条件写明：真实模型运行需要单独明确的总费用上限；
没有授权就只能交付可执行配置、不调用付费服务。

需要确认的三项：

1. 允许的总花费（美元或 token 上限）；
2. 允许使用的 provider 与模型（本配置假定 `gemini-3.8-flash-high` + cobblemon 代理）；
3. 允许运行的最长时间窗（48 个 job，串行约 1–2 小时，取决于每个任务的轮数）。

## 4. 授权后执行的命令

```bash
# 0) 环境（不要把密钥写进仓库或日志）
export KROMA_API_KEY=...            # 由所有者提供
export KROMA_BASE_URL=https://cobblemon.asia/v1
export KROMA_MODEL=gemini-3.8-flash-high
export KROMA_PROTOCOL=chat-completions
export KROMA_EXECUTION_SANDBOX=bwrap

# 1) 先跑 2 个开发任务的诊断（验证 plumbing，不当作统计优势）
kroma --experiment experiments/exp-00/manifest.json \
      --task "$(python3 -c 'import json;print(json.load(open("experiments/exp-00/tasks.json"))[0]["source"])')"

# 2) 预注册评估（48 jobs）
node scripts/run-exp00.js --manifest experiments/exp-00/manifest.json \
     --max-cost-usd <OWNER_CAP> --out experiments/exp-00/runs/<timestamp>/

# 3) 报告：配对 + 簇级 bootstrap，只看留出集
node scripts/run-exp00.js --report experiments/exp-00/runs/<timestamp>/
```

`scripts/run-exp00.js` **尚未实现**（它属于「授权后」的接线；见 T24 验收文档的 BLOCKED 明细）。
它要做的事已经由 T17/T18 的库能力覆盖：job 列表来自 `ResolvedExperiment::jobs`，
成本来自 `CostLedger`（measured/estimated/unknown 分开），晋升规则来自 `promotion_verdict`
（只看 holdout）。

## 5. 记录什么（任务卡要求「避免只发布成功截图」）

每个 job 落盘：

- 任务 id、arm id、重复号（hex 编码目录名，T17 保证不碰撞）；
- 状态与终止原因、轮数、工具调用数；
- 分项 token：input / cached read / cache write / output，以及 basis（measured/estimated/unknown）；
- 成本：measured 与 estimated 分开，`incomplete=true` 时不给单一数字；
- 失败分类：官方验证失败 / 超时 / 沙箱拒绝 / provider 错误 / 预算耗尽；
- 每个失败候选的原始 patch（不裁剪）。

## 6. 判决规则（预注册，不得事后更改）

1. 只看**留出集**：`promotion_verdict(basis=Holdout, holdout=...)`。
2. 留出集均值 > 0 且置信下界 > 0 → `Promote`；均值 < 0 → `Reject`；其余 → `Inconclusive`。
3. 开发集的收益只用于排错，**不参与**判决（`development_only_gain_does_not_promote` 已钉住）。
4. 未匹配任务、漏测、未知成本 → 结论必须是 `inconclusive`，不能把缺失当成收益。
5. 噪声：同一 arm 的 3 次重复给出簇级区间；区间宽度大于效应量时结论为 `Inconclusive`。
