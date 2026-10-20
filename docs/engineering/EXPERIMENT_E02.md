# E02：上下文联合经济性（四臂）

数据：`/opt/kroma-v4/evidence/e02/`（`report.json`、`arms.json`、`stderr.log`）。

| 臂 | 通过 | token | 相对基线 |
|---|---|---|---|
| p2_fixed | 6/6 | 704 786 | — |
| p2_deferred | 6/6 | 867 530 | +23% |
| taskstate | 6/6 | 1 560 601 | +121% |
| bounded_combo | 5/6 | 1 393 964 | +98%，且丢 1 个任务 |

配对：deferred 与 taskstate 质量 0.0pp CI[0,0]；bounded_combo −16.7pp CI[−50,0]。

三条结论：

1. 两个"优化"都只更贵，没有一个更好。
2. `ctx.taskstate.v1` 当前是**追加**而非**替换**：块每轮随请求重发，字节计入
   `message_history_bytes`，结构上不可能省 token。要省必须让它丢弃被替代的历史项，
   那是会动 append-only 与缓存命中的另一类改动，本次没做。
3. 组合更差：两个机制叠加掉了一个任务，CI 上界为 0。

因此三条臂都不满足晋升门槛，全部保持默认关闭；`MultiCandidateGate` 保持 `SingleCandidate`。
