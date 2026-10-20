# E03：修订绑定知识

数据 `/opt/kroma-v4/evidence/e03/`，harness `scripts/run-revision-experiment.py`。

`kroma bench` 在 agent 停止后判分，表达不了"你干活时文件被改了"。这个 harness 直接跑
`kroma run`，在 t=25 s / t=55 s 改写任务指定的文件，再用工作区外验证器判分。

| 臂 | 通过 | token | 读取被改文件 | 墙钟 |
|---|---|---|---|---|
| legacy (p2) | 3/4 | 966 432 | 20 | 81/125/420/420 s |
| taskstate (`ctx.taskstate.v1`) | 4/4 | 637 695 | 18 | 420/420/420/420 s |

判定 inconclusive：质量方向偏向 taskstate，但 6/8 次运行被 420 s 上限截断，token 不可比。
可直说的只有方向：taskstate 让运行变长（四跑全顶上限），与"事实失效后必须重读"一致。

那条 legacy 失败是真实信号：它把读到的旧值 5 写死，而配置在跑到一半改成了 60。
要把这条做实，需要把上限提高到自然收敛或把任务改小——两项都未做，如实记录。
