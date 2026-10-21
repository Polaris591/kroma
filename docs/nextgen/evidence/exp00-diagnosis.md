# EXP-00 配对实验：三次运行与诊断

## 三次运行（全部真实 API，gpt-6.1-sol）

| 运行 | 条件 | baseline | candidate | 结果 |
|---|---|---|---|---|
| run1 | 无 --allow-shell | 0/6 | 0/6 | 全部 rejected |
| run2 | +--allow-shell，verify 用 `/tmp/kv/bin/python` | 0/6 | 0/6 | 全部 rejected |
| run3 | verify 改用 `/usr/bin/python3`（沙箱内可见） | 0/6 | 0/6 | 全部 rejected |

原始报告：`exp00-dev-run1.json`、`exp00-dev-run2-noshell.json`、`exp00-dev-run3.json`。

run3 的真实用量（不是 0）：baseline 112805 tokens / 197867ms；candidate 156105 tokens / 250114ms。
单个 job 的 request_usage 里带 provider 报告的 input/cached/output，例如 fix-off-by-one
candidate：23821 input、17664 cached、318 output。

## 诊断：模型不是问题

把同一个任务（fix-off-by-one）手工 materialize 到 /tmp/t1，用同一个二进制、同一模型跑：

```
$ kroma run --workspace /tmp/t1 --task "tests/test_calc.py fails: total(3) returns 3, expected 6. ..." \
    --allow-write --allow-shell --max-tokens 100000
🧠 [turn 4] model 3364ms | in=2876 (cached=2560, hit=89.0%) out=27
🔧 [tool:bash] ... success (144ms, exit=0)
🏁 [kroma] Completed (ModelStop) in 25454ms | turns=6 tools=6 | in=16412 (cached=14336, hit=87.4%) out=258
Fixed `src/calc.py` to include `n` in the sum using `range(n + 1)`. Now `total(3)` returns `6`.
Verified: `python3 -m pytest -q tests/test_calc.py` — **1 passed**.
```

结果文件确实是 `def total(n): return sum(range(n + 1))`，即模型真的修好了，并且缓存命中 87–93%。

验证命令本身也可用：在沙箱内手工执行同一 argv，未修状态正确报告 `assert 3 == 6`（失败），说明
判定链路没有坏。

## 结论

模型能完成、verify 命令能跑、沙箱能看见解释器，但 bench 的 provider executor 在这 12 个 job 上
一律判 rejected，且 `BenchArmResult` 不携带失败原因字段（只有 `gate_status`），无法从报告里定位。
这属于实验设施缺陷（bench executor 与手工 `kroma run` 的行为不一致），不是候选方案的负面结论。

因此 35 个 experiment 任务不能标 REJECTED（那会谎称"候选被判负"），只能标
DEFERRED_WITH_RATIONALE，并把"设施缺陷 + 三次真实运行 + 手工成功对照"作为理由证据。
需要修 bench executor 才能产出统计结论。
