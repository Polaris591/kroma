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

## 追加诊断（返工轮）：bench 的 policy 未反映 --allow-shell

三次运行里 CLI 都传了 `--allow-write --allow-shell`，但 bench 自己打印：

```
[kroma] note: bash is not enabled for this bench run (policy 'allow-write')
```

`main.rs:1732` 的条件是 `!cli.policy_mode.allows_shell_auto()`，而
`PolicyMode::from_flags(None, true, true)` 按代码应当返回 `FullAutonomous`
（`as_str()` = "allow-write+allow-shell"）。打印出来的却是 "allow-write"，说明
`cli.policy_mode` 在 bench 分支拿到的不是 `from_flags(.., true, true)` 的结果。

未定位：可能是参数解析顺序问题，或 bench 分支在别处重新计算了 policy。需要单独复现
（最小 `kroma bench` 加 `--allow-shell` 并打印 `cli.policy_mode`）。

影响面：agent 侧 bash 不可用。但 `verify_command` 由 gate 跑、不受该 policy 约束，所以
"12 个 job 全 rejected" 不能全部归因于这一点——模型在 bench 环境里是否改对了文件仍未确认。
这也意味着"修好 bench 执行器"比我先前估计的更靠前：先确认 policy 传递，再看 gate 判定。

## 追加诊断 2：`--allow-write` 这个 flag 在 bench 里不生效

最小复现（1 task）：

```
$ kroma bench ... --allow-write --allow-shell
  baseline  bash=on                     # SideEffect 放行
  # 但 agent 的 edit 被拒：
  # {"tool_name":"edit","decision":"deny:mutating tool requires --allow-write in non-interactive mode"}
```

而用显式 base 就正常：

```
$ kroma bench ... --policy allow-write
[kroma] note: ... (policy 'allow-write')   bash=off     # 符合预期
$ kroma bench ... --policy allow-write --allow-shell
  baseline  bash=on                     # FullAutonomous
```

结论：`--allow-write` 这个 flag 在 `kroma bench` 分支里没有进入 `cli.policy_mode`，而
`--policy <base>` 可以。这是 CLI 参数解析的缺口，与 bench 执行逻辑无关。

**但修好 policy 之后 pass_rate 仍是 0/1**，说明还有第二层原因。要定位它需要保留 arm 的
workspace 与 event log（bench 跑完会清理），当前没有 `--keep-workspace` 之类的开关，需要在
下一次改动里加一个诊断开关，否则只能靠猜测——而猜测不是证据。

因此 35 项 experiment 仍然保持 DEFERRED，理由从"缺运行器"更新为"执行器有两层缺陷：
flag 传递 + 第二层未定位"，并且两次真实运行的数据都在（exp00-dev-run*.json）。
