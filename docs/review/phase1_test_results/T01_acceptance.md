# T01 验收 · 严格验证与防篡改策略的授权来源

状态：**PASS（本任务范围内）**，2026-10-17。

## 1. 授权矩阵（宿主 × 模型参数）

解析函数：`subagents::resolve_tournament_verification(host_required, model_requested)`，返回 `require_verified` 与**来源** `VerificationPolicySource`。工具入口（`execute_spawn_tournament`）与 RPC 入口（`session.tournament`）调用同一个函数。

| 宿主 strict | 模型参数 | 最终要求验证 | 来源 | 审计事件 |
|---|---|---|---|---|
| true | 缺省 | 是 | host_strict | 无（正常路径，不刷日志） |
| true | true | 是 | host_strict | 无 |
| true | **false** | **是** | **model_downgrade_ignored** | **PolicyDecision + 拒绝说明** |
| false | 缺省 | 否 | host_relaxed | PolicyDecision（operator 放宽留痕） |
| false | false | 否 | host_relaxed | PolicyDecision |
| false | true | 是 | model_requested_strict | PolicyDecision（模型主动加强） |

「宿主 strict」只能由 operator 配置（`KromaRuntimeConfig.tournament_require_verified` / `AgentRunner::with_tournament_require_verified`），没有任何模型工具参数能改到它。

## 2. 验证状态迁移表

候选的 `gate_status` 是五值枚举，`gate_passed` 只在 `Approved` 时为真。

| gate 终态 | strict 宿主可胜出 | relaxed 宿主可胜出 | 说明 |
|---|---|---|---|
| passed | ✅ | ✅ | 唯一真正的「通过」 |
| skipped | ❌ | ✅ | 验证从未运行；relaxed 下可合并但 `gate_passed` 仍为 false |
| failed | ❌ | ❌ | 候选补丁本身有问题 |
| infrastructure_error | ❌ | ❌ | 验证设施崩了，不是候选的错，但也不能当通过 |
| not_run | ❌ | ❌ | 候选没跑到验证（未完成/中止） |

三条「不可合并」状态在 relaxed 下同样不可合并——放宽只放开 `skipped` 一种，不是放开一切。

## 3. 防篡改保护集

• `merge_protected_globs(host, requested)` 取**并集**，宿主 glob 无法被工具/RPC 参数删除。
• `effective_disallow_test_tampering(host, requested)` 取**或**，模型只能打开测试路径保护，不能关掉宿主已开的。
• 宿主侧来源：`AgentRunner::with_host_protected_paths` / `with_host_disallow_test_tampering`，RPC 侧 `KromaRuntimeConfig.tournament_protected_paths` / `tournament_disallow_test_tampering`（默认空 / false）。
• 「修复现有功能」与「合法新增回归测试」仍然分开：`disallow_test_tampering` 保护的是既有测试文件，新增回归测试不受影响；冻结 oracle 属于 T19 的独立保护，不在本任务里用「禁止改测试」粗暴代替。

## 4. 证据（全部本机真跑）

| 测试 | 覆盖点 |
|---|---|
| `verification_policy_matrix_is_resolved_with_provenance` | 上面 6 行授权矩阵 + 来源判定 |
| `verification_state_transition_table_holds_for_every_terminal_status` | 上面 5×2 状态表，用真实 gate/backend 跑 4 种终态 × strict/relaxed |
| `host_true_and_model_false_still_rejects_skipped_through_the_tool_entry` | **真实 `spawn_subagent` 工具入口**：模型传 false、宿主 strict、gate skipped → 父工作区字节不变，日志里有 `model_downgrade_ignored` |
| `operator_only_unverified_mode_is_audited` | 宿主 relaxed（operator 决定）→ skipped 可合并，日志里有 `host_relaxed` |
| `protected_paths_and_test_tampering_cannot_shrink_from_tool_or_rpc` | 并集语义、不能清空、不能关掉宿主保护 |
| `t01_rpc_tournament_follows_the_host_verification_policy` | RPC 入口同策略：strict 不合并、relaxed 合并（已做变异验证：把 RPC 策略写死 true，relaxed 分支立刻红） |
| `strict_tournament_rejects_a_skipped_verification_instead_of_merging_it`（既有） | strict 拒绝 skipped 且父工作区不动 |

## 5. 本轮改动

• `subagents.rs`：`VerificationPolicySource` / `ResolvedVerificationPolicy` / `resolve_tournament_verification`；`merge_protected_globs`；`effective_disallow_test_tampering`。`effective_tournament_require_verified` 保留为薄封装（既有调用点不变）。
• `agent_loop.rs`：`host_protected_paths` / `host_disallow_test_tampering` 两个 operator-owned 字段 + builder；工具入口用解析结果，并按来源写 `PolicyDecision` 审计；`execute_spawn_tournament` 增加 `call_id` 参数用于审计事件。
• `surface/runtime.rs` + `surface/dispatcher/tournament.rs`：RPC 不再写死 `require_verified: true`，改用 operator 配置 + 同一解析函数；审批 intent 里带上 `verification_policy_source`，保护集取并集。

## 6. 未在本任务关闭的项

• 冻结 oracle / host-owned protected paths 的**独立评测隔离**：归 T19（本任务只保证「宿主集合不可被任务参数收缩」）。
• CLI / 模型工具 / RPC 三入口共用**同一份 canonical intent 与 preflight**：归 T02（本任务只统一了验证策略与保护集的解析）。
