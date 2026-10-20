# E07（多 Agent / Tournament 有界）：预注册运行

数据与判定见 `/opt/kroma-v4/evidence/e07/RESULT.md`，原始报告 `report.json`、stderr `stderr.log`。

## 结果

| 臂 | 通过 | 95% CI | token | 墙钟 |
|---|---|---|---|---|
| single_agent | 6/6 | [0.61, 1.00] | 652 748 | 483.8 s |
| adaptive_tournament_2 | 6/6 | [0.61, 1.00] | 851 987 | 545.5 s |

配对差 0.0 pp，簇级 bootstrap 95% CI [0.0, 0.0]，6 个任务簇，共 1 504 735 token。

预注册要求锦标赛「高出 5 个百分点以上且成本不更高」。质量持平、成本 +30.5%，两条都不满足，
**判负**。结论是默认保持 `MultiCandidateGate = SingleCandidate`：在当前 dev 套件上，
第二个候选没有换来任何通过率，只多花三成 token。

## 为此新增的开关

bench 的默认臂把锦标赛候选数写死为 3，无法表达预注册的 k=2。新增 `--tournament-k`，
并把 `tournament_candidates` 写进报告元数据——报告必须自证跑的是 k=2 还是 k=3，
否则「k=2 的结果」只是一句话。`strategy_pair_with_tournament_candidates` 是唯一的臂构造点，
默认仍是 3，所以既有行为不变。

## 运行时踩到的两件事

1. 预注册里的套件路径按 workspace 根解析；第一次启动被门禁拒绝（`M2-01`），
   把冻结套件放到该路径下才放行。门禁没有偷偷替换套件，行为正确。
2. 报告里 `cost_usd` 是 null：provider 回报的模型标识是 `gemini-3.8-flash-n`，
   价格表只有 `gemini-3.8-flash-high`，`cost_unknown_reason` 写明了原因。
   token 是 provider 回报的真实用量，美元数不编造。
