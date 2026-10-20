# T28 · 长任务与可撤销经验记忆 — 验收

阶段 RESEARCH-OPTIONAL｜依赖 T15, T18, T19, T23, T24｜关联 KR-31

## 一、缺陷

经验记忆原本只有「提出 / 接纳 / 晋升 / 撤销」四个动作与 `version` 绑定，缺三件事：

1. **撤销之后不能说清「为什么失效」**：没有否证与过期统计，也没有「因为文件变了 / 策略变了 /
   分支切了」而失效的通道——只有手写 `revoke`。
2. **没有交接工件**：长任务跨轮次时，要么把整段对话复制过去（无界、多半无关、还把过期结论带过去），
   要么什么都不带。
3. **跨分支/跨变更的一致性与审计**：一条经验在文件被改写、策略切换或 fork 之后仍然被注入，
   就等于把过期建议当事实。

## 二、改了什么

### 1. `ExperienceStore::all()`（审计入口）

只读地列出全部记录，供统计与失效计算使用；状态变更仍只能走
`accept` / `promote` / `revoke`（受 `TrustedActor` 约束）。**没有任何 API 把模型总结自动批准成事实**。

### 2. `ExperienceStats`（否证 + 过期统计）

`from_records(records, now_ms)` 产出 `total / candidates / accepted / revoked / falsified / expired`，
以及 `still_usable()`（= accepted − expired − falsified）。
`falsified` 统计**带反例**的已接纳记录——那正是「它不是普遍真理」的证据。
状态按**当前**计（撤销后算 revoked，不算 accepted），测试里钉住了这一点。

### 3. `invalidate_on_change`（换文件/换策略/切分支即失效）

| 变更 | 失效范围 | 理由 |
| --- | --- | --- |
| `changed_paths` 命中记录的 premise/action/evidence | 仅相关记录 | 与变更无关的经验不该被牵连 |
| `policy` before≠after | **全部**已接纳记录 | 无法证明任何一条在新策略下仍成立，宁可再生 |
| `branch` before≠after | **全部**已接纳记录 | 兄弟分支的经验不是本分支的经验 |
| `version` 变化 | 版本不同的记录 | 版本号就是「这条经验描述的世界」 |

报告是 `(record_id, cause)` 列表 + `kept` 计数，`is_clean()` 便于门禁判断。

### 4. `HandoffArtifact`（交接工件，不是对话副本）

字段：`goal`（用户原话）、`verified: Vec<VerifiedClaim{claim, check}>`、`unresolved`（**写成问句**）、
`evidence`（路径 / evidence id / 验证命令，**不含文件内容**）、`experience_ids`（本轮依赖过的经验，
便于后续发现被撤销）。

`render()` 有三条硬规则：

- 没有 `goal` → 拒绝（`Err`）；
- 渲染上限 `MAX_HANDOFF_BYTES = 8 KiB`，超了截断并在末尾写明 `[handoff truncated at the byte cap]`；
- 结构里没有地方放 transcript：测试断言渲染结果里不出现 `tool_result` / `[Compacted:` /
  `assistant:` / `user:`。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --lib handoff` | **5 passed / 0 failed** |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 988 passed / 2 failed（`logs/test_t28.log`） |

那 2 条是台账记过的既有 flake：`mcp::call_lifetime_tests::subsequent_call_fails_closed_when_poisoned`（§2.4）、
`b08c_git_metadata`（§2.2，需 `--test-threads=1`）。复跑分别 20/20、5/5 全绿。
（本轮曾同时跑两份全量，加剧了这两个时序敏感的用例，已确认与本次改动无关。）

### 测试清单（5 条）

- `revocation_is_effective_on_the_next_retrieval`：接纳后可检索 → `revoke` → **下一次检索为空**，
  且记录里保留撤销原因。
- `statistics_separate_falsified_from_expired`：3 条记录（1 条带反例、1 条被撤销）→
  `accepted=2 / revoked=1 / falsified=1 / expired=0`，`still_usable=1`。
- `a_changed_file_policy_or_branch_invalidates_the_right_records`：路径命中只失效相关记录；
  策略变更、分支变更、版本变更各失效全部；无变更时 `is_clean()`。
- `the_handoff_carries_claims_and_evidence_not_the_transcript`：goal / verified(check) / unresolved /
  evidence 都出现，transcript 标记一个都不出现，空 goal 被拒。
- `an_oversized_handoff_is_truncated_and_says_so`：500 条 claim → 截断到 8 KiB 附近并显式声明。

## 四、BLOCKED：EXP-05（经验相对无经验基线的留出收益）

任务卡验收要求「经验相对无经验基线有留出收益且无明显污染」。这需要真实模型跑量（EXP-05），
依赖 T24 的预算授权（T24 = BLOCKED）。因此：

- 本项**不声称**任何收益数字；
- 交付的是可审计的机制：否证/过期统计、四类失效原因、交接工件与经验 id 绑定；
- 若实验被禁止，经验通道保持**默认关闭**（`ExperienceSession::enabled()` 默认 false，既有行为未改）。

## 五、边界（任务卡停止条件）

- 不把模型总结自动批准成永久事实：接纳仍要求 `TrustedActor` + 真实证据。
- 不启用 Work 模式执行持续自主任务：`control_policy::MultiCandidateGate::allows_autonomous_work_mode()`
  恒 false（T27），本项没有新增任何自主执行路径。
- 跨分支无泄漏：分支变更 → 全部已接纳经验失效（保守但安全），交接工件只带 id 不带内容。
- 交接工件不含完整聊天：结构上不存在放 transcript 的字段。
