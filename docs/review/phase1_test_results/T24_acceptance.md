# T24 · 建立真实模型的冻结基线 — 验收（已执行，结论 Inconclusive）

阶段 G4｜依赖 T17, T18, T19, T20, T23｜关联：无独立 KR

## 一、执行前提（所有者授权）

所有者 2026-10-18 明确指示：**「t24无花费上限，gemini3.8flash随便用」**。据此执行真实模型跑量，
模型通道：`gemini-3.8-flash-high`（cobblemon 代理，`https://cobblemon.asia/v1`，chat-completions，
上游回报 `gemini-3.8-flash-n`），价格表见 `experiments/exp-00/prices.json`。

## 二、冻结的评估设计

| 项 | 值 |
| --- | --- |
| 任务集（开发） | `experiments/exp-00/suite-dev.json`，6 个任务 |
| 任务集（留出） | `experiments/exp-00/suite-holdout.json`，2 个任务（`refactor-duplication`、`atomic-write`） |
| 臂 | `single_agent`（单代理基线） vs `adaptive_tournament_3`（锦标赛，max_subagents=8） |
| 每任务预算 | 第一轮 150k → 300k → 最终 **600k** token（改动见第四节，只依据开发集） |
| 判定 | 每个任务：`verify_command`（pytest）退出 0 **且** 提示词明确要求的字面存在 |
| 重复 | 开发 1 次；**留出 3 次**（12 个 run） |
| 验证器位置 | `/opt/kroma-exp/`（工作区之外，代理由 bench 阻断修改 oracle） |
| 执行身份 | `source_fingerprint.binary_sha256` 与 `prompt_hash`/`tool_schema_hash` 逐次记录在报告里 |

## 三、结果（全部为真实模型调用）

| 轮次 | 臂 | 通过 | 通过率 | token |
| --- | --- | --- | --- | --- |
| 开发第 1 轮（150k） | single_agent | 5/6 | 83% | 679,676 |
| | adaptive_tournament_3 | 6/6 | 100% | 791,040 |
| 开发第 2 轮（600k） | single_agent | **6/6** | **100%** | **643,374** |
| | adaptive_tournament_3 | **6/6** | **100%** | **756,370** |
| 留出第 1 轮（300k，1 rep） | single_agent | 1/2 | 50% | 571,845 |
| | adaptive_tournament_3 | 2/2 | 100% | 548,959 |
| **留出第 2 轮（600k，3 reps）** | **single_agent** | **5/6** | **83.3%** | **2,420,349** |
| | **adaptive_tournament_3** | **6/6** | **100%** | **2,356,357** |

留出第 2 轮（判定用）：配对差 **+16.7%**，bootstrap 95% CI **[+0.0%, +33.3%]**，
`paired_observations=6`、`cluster_count=2`。

那 1 个失败是 `atomic-write` 的 rep=0：单代理跑到 **607,766 token**（超出当时 600k 上限 7,766）
被截断（`TokenBudgetExhausted`），是**预算下限**而非能力差异——同一任务的另外两次重复在 287k / 615k 都通过。

## 四、按预注册规则得出的结论：Inconclusive

规则（`experiments/exp-00/README.md` 第 6 节，实验前写定）：留出集均值 > 0 **且置信下界 > 0** 才 Promote。

- 均值 +16.7% > 0，但**置信下界 = 0.0**（n=6，只有 2 个任务簇）；
- 所以结论是 **Inconclusive**，锦标赛臂**不获得默认开启资格**；
- 成本上两者几乎相同（2.36M vs 2.42M token，锦标赛甚至略低），但"略低"来自单代理那次被截断的 607k，
  不能当作机制优势。

因此 `MultiCandidateGate` 默认仍是 `SingleCandidate`（T27），本轮**没有**改动默认值。

## 五、过程中修正的三个判定缺陷（值得记录）

真实跑量最有价值的产出不是那点通过率，而是暴露了「拿实现细节当验收标准」的系统性问题——
同一个错误我犯了三次，都是**功能正确却被判失败**：

| 任务 | 错误断言 | 模型实际写法 |
| --- | --- | --- |
| `fix-off-by-one` | 要求出现 `range(1` | `sum(range(n + 1))` |
| `reject-symlink-config` | 要求出现 `is_symlink` | `os.path.islink` |
| `atomic-write` | 要求出现 `tmp` | `tempfile.NamedTemporaryFile` |

改法：后置条件只保留**提示词自己明确要求的字面**（`def compute_sum`、`ValueError`、`65536`、
`def normalize_name`、`os.replace`），其余全部交给 `verify_command`（pytest）判定。
这三处若不修，留出集会得出"单代理 50%"的假结论。

## 六、成本

| 项 | 数量 |
| --- | --- |
| 总 token（4 轮，28 个 run） | **8,767,970**（input 8,469,599 / output 298,371） |
| 估算花费（价目表 150/600 nano-USD） | **≈ $1.45** |
| 单位成本 | 约 $0.05 / run，约 $0.12 / 任务对 |

每个 run 的 `tokens`、`budget_overage`、`cap_ok`、`terminal_reason`、`verify_time_ms` 都在原始报告里：
`docs/review/phase1_test_results/logs/exp00/{dev,holdout}-round{1,2}.json`。

## 七、可复现

```bash
cp experiments/exp-00/suite-*.json /opt/kroma-exp/          # 验证器放在工作区之外
export KROMA_API_KEY=… KROMA_BASE_URL=https://cobblemon.asia/v1 \
       KROMA_MODEL=gemini-3.8-flash-high KROMA_PROTOCOL=chat-completions
kroma bench --workspace /tmp/exp00-ws --suite /opt/kroma-exp/suite-holdout.json \
  --executor provider --budget-tokens 600000 --repetitions 3 --allow-write --allow-shell --json
```

## 八、边界与未做的

- 留出集只有 2 个任务簇 → 置信区间天然宽，这是样本量问题，不是机制问题；要判"锦标赛是否有用"
  需要更多任务簇，本轮**不声称**任何机制优势。
- 未做：多模型对照（只用了一个模型）、跨任务类别分层、真实仓库规模的任务。
- 开发集第 1 轮的 3 个失败里有 2 个是本文第五节的判定缺陷，第 3 个（`bound-output-size`）是预算截断——
  所以**不要**引用第一轮数字做结论，只能引用第二轮的干净结果。
- 没有因为"看起来更好"就改默认值：改动默认值必须过 Promote 门槛（本项未过）。
