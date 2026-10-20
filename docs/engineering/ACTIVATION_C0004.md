# C00-04 激活租约与单次初始化协议

代码：`crates/kroma-core/src/activation.rs`（新增 `ActivationRegistry` / `ActivationLease`）。

## 为什么单独一个模块

蓝图的激活协议有九步（§5.4）。基线里没有对应的东西：`RunPlan` 记录「配置想要什么」，
`capability.rs` 记录「证据到哪一步」，都没有「谁正在初始化、还有谁在用它、失败怎么收」。
缺了这层，懒激活就会变成三种事故：同一个组件被并发初始化两次；共享依赖被第一个释放者拆掉；
初始化失败却留下了可调用句柄或半个租约。

## 三条验收规格

### C00-04-A1 单次初始化（single-flight）

`initialisation_runs_once_for_concurrent_callers`：8 个线程同一时刻抢同一个能力，
初始化函数只执行 1 次，8 个租约都拿到同一个结果，全部释放后状态回到 `Dormant`、持有数归 0。
实现上「第一个到达者把状态置为 `Starting` 并执行初始化，其余人在 condvar 上等它settle」，
不是「各自加锁后各自检查再各自初始化」。

### C00-04-A2 共享依赖租约

`a_shared_dependency_outlives_the_first_release`：`a` 与 `b` 都依赖 `dep`。
两个都激活后 `dep` 持有数为 2；释放 `a` 后 `dep` 仍 `Active`（因为 `b` 还持有）；
释放 `b` 后 `dep` 才回到 `Dormant`，且 `dep` 的初始化函数全程只跑了 1 次。
关键实现点：**依赖的 id 会并入父租约**，父租约 Drop 时统一释放——第一版把依赖租约存在
局部变量里，函数返回时就 Drop 了，依赖立刻被拆掉；测试直接抓到了这个 bug。

### C00-04-A3 失败回滚与不超额预占

`a_failed_activation_rolls_back_and_stays_failed`：`broken` 依赖 `dep`、初始化返回错误。
失败后 `broken` 状态为 `Failed`、持有数 0，`dep` 回到 `Dormant`、持有数 0——
失败尝试拿到的依赖租约全部释放，不留「已预占但没人用」的资源。
后续调用者得到同样的失败原因，不会静默重试初始化函数（初始化次数仍是 1）。

`cycles_and_unknown_dependencies_are_refused`：`a → b → a` 报 `Cycle(["a","b","a"])`；
依赖不存在时报 `Unknown("missing")` 并点名缺的是谁，父能力记为 `Failed`。

## C00-04-A4 撤销（revocation）与其派发门禁

`revoke(id)` 把注册表的 `generation` 加一，并把 `id` 以及**所有传递依赖它**的能力标成
`revoked_at = 新 generation`（返回 `id` 在前的被撤销列表；未知 id 返回空表、不动 epoch）。
是否还有授权一律换算成同一个判据：

    授权仍有效  <=>  revoked_at(capability) <= 授权签发时的 generation

- `ActivationLease` 记住签发时的 generation，`still_valid()` 覆盖它持有的整条链，所以被撤销的
  依赖会让父租约一起过期；`claim()` 把同一份授权变成可长期保存的纯数据（运行时跨 turn 要用）。
- 撤销把 `holders` 归零、`state` 归 `Dormant`、清掉记录的失败。旧租约 Drop 时**不再**递减 holder
  （`release(id, generation)` 只在 `revoked_at <= lease.generation` 时扣），因此旧租约不会把
  重新激活的新一代持有者一起拆掉。
- 撤销发生在初始化过程中时，那次尝试返回 `ActivationError::Revoked { id, generation }`，
  状态留 `Dormant`，不返回成功、也不签发租约。撤销期间停在 `Starting` 状态不动，等这次尝试
  settle 才放行等待者，所以不会有两个初始化函数同时跑。
- 撤销后 `activate` 得到新 generation 并正常工作；旧 lease/claim 依旧失效（不是"复活"，是
  新一代）。

测试：`crates/kroma-core/tests/c0004_revocation.rs`（5 条）——① 旧 lease/claim 被拒且点名能力；
② 门禁在派发前拒绝、工具未执行；③ 重新激活新 generation 可用、旧 claim 仍死；④ 在飞初始化遇撤销
不报成功；④' 撤销与并发调用：revoke 返回之后开始的调用必被拒。

## 接线状态（截至 C00-04 收口；只写实际接到的位置）

已接：

- `ActivationRegistry`：`revoke` / `generation` / `authority(&CapabilityClaim)`；
  `ActivationLease`: `generation` / `claim` / `still_valid`。
- 工具派发：`ToolRuntime::with_capabilities` / `gate_tools(claim, tools)`；
  `execute_call_inner` 在**最前面**（hooks / policy / approval / 实现之前）查
  `dispatch_authority`，拒绝时返回 `ErrorCode::PolicyDenied` 与决策标签
  `deny_capability_revoked`，正文点名能力 id 与 epoch。不是"先执行再报错"，也不是静默回退到裸
  `ToolRegistry`。
- **运行路径（c00-04 wiring）**：`AgentRunner::prepare_run` 调
  `ToolRuntime::bind_dispatch_authority()`——按需 `declare` `RUN_DISPATCH_CAPABILITY`
  （`"tool-dispatch"`）、`activate` 取租约、把 `claim()` 绑成整个 run 的派发权威；租约随即释放，
  run 跨 turn 持有的是 claim（见 `CapabilityClaim`）。门禁是"run 的权威"而不是工具名清单，所以
  运行中新注册的工具（extension mount / MCP `tools/list`）同样在门禁之内。撤销后下一次
  `execute_call_inner` 即拒绝，工具不执行。
- **loop 自派发的两个工具**：`recall_evidence` / `spawn_subagent` 在 `agent_loop` 里先问
  `dispatch_authority`；权威被撤销时它们不进入各自的专用分支，而是落到 `execute_call_inner`，
  由同一道门禁按工具名拒绝（决策标签同样是 `deny_capability_revoked`）。
- **权威签发不出来时 fail-closed**：能力已由所有者声明了真实初始化函数、而撤销落在初始化过程中
  时，`bind_dispatch_authority` 返回 `ActivationError::Revoked` 并把原因记在运行时里；此后每个
  派发都以该原因被拒，run 不会退化成"无门禁"。

未接（本包不做，不声称完成）：

- **谁签发/谁撤销 lease 未定**：运行边界现在是"每个 run 在 `prepare_run` 自己签一张"，所以撤销
  只终结**当前** run 的权威；新 run 会重新签发（`a_later_run_issues_its_own_authority_after_a_revoke`
  把这条边界钉住）。是否应该由一个进程级所有者签发、撤销是否对进程终局，仍是所有者待决事项。
- **能力粒度是单一 id**：`"tool-dispatch"` 一张 blanket 权威，撤销即全禁；按家族（read / write /
  exec）拆分需要"哪个工具属于哪张授权"的所有者决定，未自行发明。
- `gate_tools(claim, tools)`（按工具名的窄门禁）在生产里仍无调用点，只有测试用；运行路径用的是
  上面那张 run 级权威。
- 子代理 run 自己建运行时（`build_runtime_with_registry` 不继承父 run 的 `ActivationRegistry`），
  所以撤销父 run 的权威不会停掉已经启动的子代理 run。
- 门禁只判"是否被撤销"，不判"当前是否 Active"：lease 持有者仍是激活责任方，派发不要求 Active。
- 撤销不打断**已经通过门禁**的在飞调用（它只保证 revoke 返回之后开始的调用必被拒）；要中断在飞
  工具需要取消通道，那是另一件事。
- C00-05 的 `CapabilityScheduler`（最短驻留/冷却/切换上限）仍独立，撤销没有接进切换流程。

## 明确还没做的

- **能力声明不来自 `RunPlan`**。运行路径确实由租约驱动了，但驱动它的不是配置：`RunPlan` 仍然只
  记录「配置想要什么」，`declare`/`activate` 发生在 `AgentRunner::prepare_run`，用的是一张固定 id
  的 run 级权威。哪份配置应该声明哪些能力、`RunPlan` 是否要承载这份声明，仍未决。
- 失败状态是终态：同一进程内不会自动重试（重试需要显式重新 `declare`，或显式 `revoke` 后重新
  `activate`）。这是有意的——静默重试会把「初始化不稳定」藏起来。

## 收口

- 定向：`cargo test -p kroma-core --lib activation` → 7 passed（A1/A2/A3 + 环/未知依赖 +
  C00-05 的释放探针与两条切换刹车，全部仍绿）
- 撤销：`cargo test -p kroma-core --test c0004_revocation`（5 条）
- 运行路径接线：`cargo test -p kroma-core --test c0004_wiring`（5 条，走真实 `AgentRunner`）
- clippy `-D warnings`：0
- 证据：`evidence/c00-04-revocation/`（逐条原始日志与 exit code）；接线证据见
  `evidence/c00-04-wiring/`
