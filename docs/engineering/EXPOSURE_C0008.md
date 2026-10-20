# C00-08 工具暴露与权限分离

代码：`crates/kroma-core/src/tool/exposure.rs`（`Exposure` / `ExposureSet` / `RequestSnapshot`）。

暴露（模型看到什么）与授权（网关允许调什么）分成两件事。四条测试：

- **A1 暴露不等于权限**：`shell` 是 `Direct` 但网关未授权 → 仍被声明、但 `may_call` 为 false；
  `mcp__deferred` 已授权但未声明 → 可调用、不在 `declared()` 里；`mcp__hidden` 即使网关放行也
  不可达（hidden 的定义就是"注册但不可达"）。
- **A2 在途请求 schema 不被改写**：请求 1 冻结后，动态刷新新增 `write`、撤回 `read`；
  请求 1 仍只看到 `read`，也调不到 `write`。
- **A3 刷新在下一个边界生效**：请求 2 才看到刷新后的集合。
- 撤回对旧请求不可见、对新请求立刻生效。

第一版把 `Hidden` 写成"可经脚本到达但不声明"，与 hidden 的定义冲突，测试直接判红；
现在 hidden 一律不可达，用 `Deferred` 表达"可达但不声明"。

## 尚未接线

`RequestSnapshot` 还没有被 `ToolRegistry` / 工具分发路径使用——真实请求仍由现有 registry
组装声明列表。接线点：`ToolRegistry::all_registered` 的调用方按 `ExposureSet` 过滤声明，
并把 `authorized` 取自 `PolicyEngine` 而非 registry 自身。

## 收口

定向 4 条 + clippy 0 + fmt 干净；全量见 `evidence/c00-08/full-suite.log`。
