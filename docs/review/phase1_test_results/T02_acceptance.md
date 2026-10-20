# T02 验收 · 审批绑定真实且规范化的赛马请求

状态：**PASS（本任务范围内）**，2026-10-17。

## 1. 核心改动：CanonicalTournamentIntent

新增 `subagents::CanonicalTournamentIntent`（+ `CanonicalTournamentBudget`）。一份意图同时供三处使用，三处不可能再漂移：

• Hook / 审批看到的 payload（`canonical_json()`）
• 绑定 grant 的 digest（`digest()` = sha256(规范化 JSON)）
• 真正执行的 `TournamentSpec`（budget / timeout / 保护集 / 验证策略都取自这份意图）

字段：task、candidates、**展开后的全部 strategies**（含默认策略补位）、allowed_paths、max_turns、timeout_ms、disallow_test_tampering、protected_paths（宿主集已合并）、require_verified、verification_policy_source、budget（max_total_tokens / max_model_turns / max_model_request_attempts / max_tool_calls / max_run_duration_ms / request_timeout_ms）、predictive_check。

规范化 JSON 由 `tool::policy::canonical_json` 生成：对象键排序、数组保序，所以「相同意图必得相同摘要」，与构造顺序无关。**没有 `role=worker` 占位对象**。

## 2. 校验顺序（fail before approval）

RPC 顺序：参数类型检查（candidates / max_turns / timeout_ms / strategies / protected_paths / allowed_paths / allowed_write_globs / disallow_test_tampering 任一类型不符即 `INVALID_PARAMS`）→ 共用 `parse_tournament_request` 校验（候选数区间、timeout>0、max_turns>0、路径不含 `..`/根）→ 解析宿主策略与保护集 → 构造意图 → Hook → 配额/审批。**任何一步失败都不会产生候选 worktree，也不会弹审批。**

## 3. 审批等待期间的再校验

AllowOnce/AllowCategory 分支在拿到决策后重新校验两件事，任一变化即拒绝：

• **revision**：审批期间 `revision.restore` / `--undo` 改变了工作区 → `revision changed while approval was pending`
• **token 预算**：审批期间 `/config set max_tokens` 改变上限 → `token budget changed while approval was pending`

grant 本身也绑 revision：revision 变了，`consume_grant` 直接拒绝（B12a 机制），RPC 的显式比较是第二道保险。取消（cancel_rx）在等待期间触发 → 直接报错返回，不启动候选。

## 4. 证据

| 测试 | 覆盖点 |
|---|---|
| `intent_digest_is_stable_and_changes_with_every_field` | 相同意图摘要相同；**13 个字段逐个改**都会换摘要（含 allowed_paths 被移除、budget、验证来源） |
| `one_shot_tournament_grant_cannot_be_replayed_or_retargeted` | 一次性 grant 只能用一次；换意图摘要拒绝；**revision 前进后拒绝** |
| `rpc_hook_receives_the_real_task_scope_and_candidate_budget` | RPC 的 Hook 看到真实 task / 3 条展开策略 / max_turns / timeout / 合并后的 protected_paths（`oracle/**` + `tests/**`）/ require_verified / 验证来源 / budget；且**没有 role 字段** |
| `malformed_rpc_fields_fail_before_approval` | 5 类类型错误全部在审批前被拒 |
| `tool_hook_sees_the_canonical_intent_not_the_raw_model_arguments` | 工具入口的 Hook 也看到规范化意图：模型只说 `mode=tournament`+task+candidates，Hook 仍能看到 2 条策略与生效预算 |
| `t01_rpc_tournament_follows_the_host_verification_policy`（T01 新增） | RPC 与工具入口共用同一验证策略 |

**变异验证（证明断言非空转）**：把工具入口传给 Hook 的 `raw_arguments` 换成 `{}` 后，`tool_hook_sees_the_canonical_intent…` 立刻失败，并打印出「Hook 只看到模型的原始参数」——说明这条测试真的在拦「审批对象 ≠ 执行对象」。

## 5. 相关既有测试

`connection_ownership`(36) / `gate8_config_and_tournament`(35) / `b11a_grant_binding`(9) / `b12a_revision_binding`(3) 全绿。

## 6. 未在本任务关闭的项

• **审批等待期间外部进程写文件造成的工作区内容漂移**：本任务只校验 revision 身份（以及 budget），没有独立的内容指纹监控。revision 只在 kernel 自己捕获快照时前进，外部 `sed -i` 不会推进它。完整做法是「审批前后各算一次工作区内容摘要」或使用文件系统监视，留给后续（T23 快照身份天然覆盖这一层）。
• **CLI / 工具 / RPC 共用同一份 preflight 代码**：现在共用的是同一个意图类型、同一个解析函数、同一个验证/保护策略解析；但三条入口的 preflight *函数体* 仍各自一份（RPC 走 `tournament_preflight`，工具走 `execute_spawn_tournament` 内联）。合并成一个函数需要重排工具入口的审批时机（会改变用户可见的审批次数），属于行为变更，未在本任务内做。
