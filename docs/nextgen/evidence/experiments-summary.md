# 已完成的配对实验（真实 API，gpt-6.1-sol）

前置：实验设施的三层缺陷已修（见 `exp00-diagnosis.md`）。下表全部是修好之后跑的，
prereg 由 bench 记录 sha，dev/holdout 分开，配对观察数非零。

## 1. on_demand_tools（H03 方向）

| 集合 | baseline | candidate | 质量 |
|---|---|---|---|
| dev（6 任务） | 117015 tok | 126349 tok（+8%） | 6/6 vs 6/6 |
| holdout（2 任务） | 48635 tok | 43680 tok（−10%） | 2/2 vs 2/2 |

**处置：不晋升。** 两个集合结论相反，样本也小。这是负实验登记的第一条。
原始：`exp00-real-dev.json`、`exp00-real-holdout.json`。

## 2. adaptive_tournament k=2（N03）

| 集合 | baseline | candidate | 质量 |
|---|---|---|---|
| dev（6 任务） | 108459 tok | 126948 tok（+17%） | 6/6 vs 6/6 |

**处置：判负，不启用。** 质量零增益、成本 +17%。与 v4 的 E07（k=2 +30.5%）同向。
原始：`exp-tournament-dev.json`。

## 3. projection_p3（压缩档位）

| 集合 | baseline（P2） | candidate（P3） | 质量 |
|---|---|---|---|
| dev（6 任务） | 120061 tok | 116926 tok（−2.6%） | 6/6 vs 6/6 |

**处置：无结论。** 2.6% 落在单次 6 任务的噪声内，不能据此改默认档位。要判定需要多重复。
原始：`exp-p3-dev.json`。

## 汇总

三个机制，一个不晋升（矛盾）、一个判负（更贵）、一个无结论（差异太小）。没有一条支持
"打开某个实验机制会更好"。默认配置因此保持不变——这正是 W4 契约要求的处置方式：
"无需为了做完强行默认开启"。
