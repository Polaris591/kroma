# T24 · 建立真实模型的冻结基线 — **BLOCKED（无授权的费用上限）**

阶段 G4｜依赖 T17, T18, T19, T20, T23｜关联：无独立 KR

## 一、结论

**BLOCKED**。可执行配置已冻结并通过 dry-run；真实模型跑量**没有执行**，因为没有所有者明确的
总费用上限。这不是环境缺件，而是任务卡自己写明的停止条件：

> 真实模型运行需单独明确总费用上限；没有授权只交付可执行配置，不调用付费服务。

缺失依赖（三项，缺一不可）：

1. 允许的总花费（美元或 token 上限）；
2. 允许使用的 provider 与模型（本配置假定 `gemini-3.8-flash-high` + cobblemon 代理）；
3. 允许运行的时间窗（48 jobs，串行约 1–2 小时）。

## 二、已交付（可执行配置 + 可复算身份）

| 文件 | 内容 |
| --- | --- |
| `experiments/exp-00/manifest.json` | 冻结清单：2 arms × 8 tasks × 3 repeats = **48 jobs**，`basis=holdout`，`max_total_tokens=4_000_000` |
| `experiments/exp-00/tasks.json` | 8 个真实任务：6 开发 + **2 留出**（`refactor-duplication`、`holdout-atomic-write`），每个都有 `verify_command` 与后置条件 |
| `experiments/exp-00/prices.json` | 价目表（含 **cache-write** 单价，读写分价，T18 要求） |
| `experiments/exp-00/README.md` | 冻结内容、授权清单、执行命令、落盘字段、**预注册判决规则** |

**配置身份可复算**：dry-run 连续运行得到同一摘要 `digest=05757bf3fe69c9f9`
（`logs/t24_exp00_dryrun.log`，48 行 job 列表）。改任意字段（arms/tasks/holdout/repeats/预算）都会改变它，
这一点由 T17 的 `effective_manifest_is_exported_and_hashed` 覆盖。

## 三、为什么 dry-run 不能代替真实跑量

dry-run 验证的是：清单能解析、依赖校验通过、job 列表确定、目录名不碰撞、摘要稳定。
它**不能**证明：任务真的能被模型完成、成本估算是否接近真实、留出集判决是否显著。
按任务卡要求，「先运行少量诊断任务验证 plumbing」那一步本身也要花钱，所以同样需要授权；
本项没有把 dry-run 包装成「plumbing 已验证」的结论。

## 四、授权后要做什么（已写进 README 第 4–6 节）

1. 2 个开发任务的诊断跑（验证 plumbing，不算统计优势）；
2. 预注册的 48 个 job；
3. 报告：配对 + 簇级 bootstrap，**只看留出集**；开发集只用于排错；
4. 落盘每个 job 的分项 token（input / cache read / cache write / output）、basis、
   measured/estimated 成本、失败分类与原始 patch；
5. 判决用 `promotion_verdict(basis=Holdout, …)`，噪声区间宽于效应量时结论必须是 `Inconclusive`。

## 五、BLOCKED 明细（还差什么代码）

`scripts/run-exp00.js` 尚未实现——它是「授权后」的接线脚本：遍历 `ResolvedExperiment::jobs`、
为每个 job 起一次带 sandbox 的 run、把 T18 的 `CostLedger` 汇总进报告。
其依赖的库能力都已就绪并有测试（T17 的 job 列表与恒等校验、T18 的逐请求成本账目、
T19 的隔离实验器、T20 的能力清单）。在没有预算的情况下写这个脚本无法用真实数据验证，
所以按任务卡选择「只交付可执行配置」。

## 六、证据

| 项 | 结果 |
| --- | --- |
| `kroma --experiment experiments/exp-00/manifest.json`（dry-run） | exit 0，48 jobs，digest `05757bf3…`（`logs/t24_exp00_dryrun.log`） |
| 价目表结构校验 | 四个费率齐全且非负（`uncached/cached/cache_creation/output`） |
| 真实模型跑量 | **NOT RUN — BLOCKED** |
| 噪声量化 / 配对报告 | **NOT RUN — BLOCKED** |

## 七、边界

- 没有调用任何付费服务，没有产生任何费用。
- 没有把 dry-run 当成「plumbing 已验证」；诊断跑本身也在待授权清单里。
- 留出集的 2 个任务在冻结后不得修改；改了就等于改验收标准（T17/T20 的规则会拒绝不一致的清单）。
