# C00-05 生命周期回收与冷却/切换上限

代码：`crates/kroma-core/src/activation.rs`（在 C00-04 的注册表上补 `ReleaseProbe`、
`CapabilityScheduler`、`SwitchPolicy`）。

## C00-05-A1 回收由真实计数验证

`release_is_measured_by_the_registry_not_by_drop_being_called`：
注册表自己记 `outstanding`（发出的租约数减已释放数）。测试走三步：

1. 拿一个租约 → `outstanding == 1`，`probe_released` 说没释放；
2. 正常 Drop → `outstanding == 0`，`probe_released` 同时看持有数与状态，两者都说已释放；
3. `mem::forget` 一个租约 → `outstanding == 1`，`probe_released` **不**说已释放。

第三步是关键：如果只看「Drop 被调用过」，泄漏的租约会被当成已回收；现在它必须留下痕迹。

## C00-05-A2 冷却与最短驻留

`dwell_and_cooldown_block_early_switches`：`min_dwell=40ms`、`cooldown=80ms`。
切换后立刻再切 → `MinDwell`；等 50ms（超过驻留、不到冷却）→ `Cooldown`；
再等 50ms → 允许切换，计数为 2。两个刹车分开报错，因为它们防的是两件事：
驻留太短等于刚激活就被拆掉；冷却太短等于在两个策略之间来回震荡。

## C00-05-A3 每 Run 切换上限

`the_run_switch_cap_is_enforced`：`max_switches=3`。a、b 用掉 2 次；
切到初始化失败的 `broken` 时得到 `Activation` 错误且**不消耗额度**（仍是 2）；
切到 c 用掉第 3 次；再切到 d 得 `CapReached { max: 3 }`。
「先获取新租约、成功后才记账」是这条的实现要点：失败的切换不该花掉预算。

## 明确还没做的

调度器**尚未接进 `RunPlan`**：它现在是可用的协议对象，不是运行路径的一部分。

注意分工：C00-05 的三条验收规格（未使用能力不创建大索引/worker、共享依赖释放不终止其他 run、
反复启停内存进入有界平台期）由 `PROCESSTREE_C0005.md` 与
`evidence/c00-05/process-tree-cost.json` 回答；本文只描述 activation 的回收计数与冷却/切换上限，
不要拿它当 C00-05 的证据。
把它挂到真实的能力声明上需要先确定「哪些能力算一次切换」，这属于 M3-01 的有界路由。
在此之前不能说「Kroma 的运行已经受切换预算约束」。

## 收口

- 定向：`cargo test -p kroma-core --lib activation`（7 条）
- clippy `-D warnings` 0；`cargo fmt --all --check` 通过
- 全量：见 `evidence/c00-05/full-suite.log`
