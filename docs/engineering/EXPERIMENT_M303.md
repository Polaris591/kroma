# M3-03：跨模型校准（gemini-3.8-flash vs gpt-6-luna）

数据：`/opt/kroma-v4/evidence/m3-03/`（`report-gpt6luna.json`、`stderr-gpt6luna.log`）与
`/opt/kroma-v4/evidence/e07/`（gemini 同套件同臂）。预注册：`experiments/prereg/M3-03-cross-model.json`。

| 模型 | single_agent | tournament k=2 | 差 |
|---|---|---|---|
| gemini-3.8-flash | 6/6，652 748 tok | 6/6，851 987 tok | +30.5% |
| gpt-6-luna | 6/6，93 752 tok | 6/6，112 591 tok | +20.1% |

结论三条：

1. 机制方向与模型无关：两个模型上锦标赛都零提升、多花 token，`MultiCandidateGate` 保持
   `SingleCandidate` 这条可以在两个模型上主张。
2. token 绝对值不能跨模型比（相差约 7 倍），只能做「相对自身单代理基线」的比较。
3. 通过率区分不出模型（都是 6/6），说明 dev 套件对这两个模型都没有难度梯度；
   E04 里区间跨零不是采样不足，而是套件区分力不够。

没做的一件事：没有用第二个模型去跑 E04/E07 之外的机制（因果剪枝、延迟发现），
所以「因果剪枝没用」目前仍只有 gemini 一个模型的证据，不能声称跨模型成立。

## 留出集确认（一次，已用完）

`evidence/e07/report-holdout.json`，套件为冻结留出集 2 任务，臂与 dev 相同：

| 臂 | 通过 | token |
|---|---|---|
| single_agent | 2/2 | 844 767 |
| adaptive_tournament_2 | 2/2 | 1 441 253（+70.6%） |

配对差 0.0 pp，CI [0.0, 0.0]。留出集上锦标赛的成本惩罚更大（`atomic-write` 单个任务
1 139 349 vs 559 129 token），质量仍然零提升。

按 M3-03 的规格：「heldout 质量与资源门槛均满足」才晋升；这里两条都不满足，
因此**保留规则、机制保持关闭**（`MultiCandidateGate = SingleCandidate`）。留出集已消耗，
再要确认性结论必须先冻结新的留出集。
