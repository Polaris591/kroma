# 基线差距映射（M0-01）

本文件是 v4.0 蓝图 M0-01 的产物：把蓝图的复用锚点与能力清单落到**当前源码的真实路径**，
并把陈旧验收与真实缺口分开。所有结论由本文件末尾的命令在 `integration/v4` 上复算。

## 1. 身份

| 项 | 值 |
|---|---|
| 归档 | `Kroma_BF14B1_Source_20261018.tar.gz`，sha256 `16e0b736e20bbae12718a6d6a351ae27095c15b5f991684763b14f777bf9cbba`，406 文件，逐字节比对 0 差异 |
| 基线提交 | `0933a4a9a6ca94776d0122e581b46bd156762667`（tree `7567f5c0cddd9e2860ce476d6fda4a21f8deab5c`），导入即原样，无产品改动 |
| 分支 | `integration/v4` |
| 工作区版本 | workspace `1.5.0`（`crates/kroma-core`、`crates/kroma-cli`） |
| 工具链 | rustc/cargo `1.85.1`（`rust-toolchain.toml` 钉住；fmt 结果与工具链绑定） |

## 2. 22 项能力锚点 → 实际路径

全部解析成功，零缺失。这是蓝图 `CAPABILITY_CONTRACT.json` 里 `existing_module_anchor` 的逐一核对。

| 能力 | 激活策略 | 真实路径 |
|---|---|---|
| core.loop | core | `crates/kroma-core/src/agent_loop.rs` |
| core.provider | core | `crates/kroma-core/src/provider/` |
| core.files | core | `crates/kroma-core/src/tools/` |
| core.shell | core | `crates/kroma-core/src/process_supervisor.rs` |
| tool.discovery | on_demand | `crates/kroma-core/src/tool/` |
| tool.codemode | on_demand | `crates/kroma-core/src/tool/` |
| tool.mcp | on_demand | `crates/kroma-core/src/mcp.rs` |
| extension.skills | on_demand | `crates/kroma-core/src/extensions.rs` |
| context.compact | conditional | `crates/kroma-core/src/context_projector.rs` |
| context.evidence | conditional | `crates/kroma-core/src/evidence_store.rs` |
| context.taskstate | conditional | `crates/kroma-core/src/task_state.rs` |
| navigation.repo | on_demand | `crates/kroma-core/src/repo_graph.rs` |
| strategy.plan | on_demand | `crates/kroma-core/src/control_policy.rs` |
| strategy.scout | on_demand | `crates/kroma-core/src/subagents.rs` |
| strategy.workers | on_demand | `crates/kroma-core/src/subagents.rs` |
| strategy.tournament | on_demand | `crates/kroma-core/src/subagents.rs` |
| state.recovery | core_contract | `crates/kroma-core/src/session.rs` |
| resource.authority | always_enforced | `crates/kroma-core/src/budget.rs` |
| security.execution | always_enforced_for_required_mode | `crates/kroma-core/src/sandbox.rs` |
| evaluation.frozen | on_demand | `crates/kroma-core/src/bench.rs` |
| evolution.controlled | explicit_experiment_only | `crates/kroma-core/src/ahe.rs` |
| interface.surfaces | selected_surface | `crates/kroma-core/src/surface/` |

## 3. 复用锚点清单（禁止造平行权威）

| 模块 | 行数 | 现有职责 | v4 不得做 |
|---|---|---|---|
| `assembly.rs` | 505 | 单一装配路径；`RunPlan`（配置派生的能力计划，`digest` 忽略入口）与 `FeatureLedger`（配置/可用/实际执行的诚实记账） | 新建 `runtime_assembly.rs` 维护第二套计划 |
| `capability.rs` | 202 | `CapabilityManifest`：implemented / wired / contract_tested / live_tested / default_on，且拒绝"未实现却 default_on" | 用新 `active` 状态吞掉原证据字段 |
| `context_projector.rs` | 1832 | `ProjectionProfile{Baseline,P1,P2(default),P3}` 与投影/证据边界；P3 = P2 再降 25% 低水位 | 把 p3 改写成语义压缩，或另造主压缩器 |
| `task_state.rs` | 494 | 语义 `TaskState`：goal / constraints / verification / failure_hypotheses / path_mentions，规则派生、零模型调用 | 另建 `WorkingTaskState.json` 权威 |
| `task_host.rs` | 1483 | 异步任务生命周期、owner、恢复 | 与语义 TaskState 合并成巨型对象 |
| `budget.rs` | 1083 | 共享预算：`try_reserve`/`settle` 单事务，区分 exact 与 estimated，`unknown_tokens` 不归零 | 另建不对账的策略计数器 |
| `provenance.rs` | 685 | 运行身份与来源 | 当作上下文片段授权标签 |
| `extensions.rs` | 2675 | 可逆注册、双类 hooks、子作用域 JIT 上下文 | 在 Code Mode 里绕过工具入口 |
| `evidence_store.rs` | — | 分层证据存储与可用性合同 | 另建证据索引 |
| `repo_graph.rs` | — | 符号依赖图与 Personalized PageRank 导航 | 另建导航图 |
| `control_policy.rs` | — | 确定性升级策略与反事实探针 | 另建路由表 |
| `subagents.rs` | — | Scout/Worker 与隔离 worktree | 另建编排层 |
| `sandbox.rs` | — | 验证/评分运行的 OS 级沙箱 | 宣称已有真实 Provider Broker |
| `session.rs` | — | 追加式哈希链 WAL，可分支 | 另建会话存储 |

## 4. 路径漂移

- Gate 7 验收报告写的是 `crates/harness-core/src/repo_graph.rs`；实际是 `crates/kroma-core/src/repo_graph.rs`。crate 已改名，旧报告路径不再可达，引用时必须换成新路径。
- 同理，任何 0.8.x–1.3.0 时期的报告里出现的 `harness-*` 路径都需按 `kroma-*` 重读。

## 5. 陈旧验收清单（不在 HEAD 复验，不得当作当前证据）

| 文档 | 声称的版本/提交 | 与当前 HEAD 的关系 |
|---|---|---|
| `GATE0_ACCEPTANCE_REPORT.md` | `harness 0.8.1`，commit `18d4843` | 历史；v0.8.1 安全封板 |
| `GATE1..GATE6_ACCEPTANCE_REPORT.md` | 各自版本 | 历史；对应阶段的合同 |
| `GATE7_ACCEPTANCE_REPORT.md` | `v1.2.0` | 历史；路径已漂移 |
| `GATE8_ACCEPTANCE_REPORT.md` | `v1.3.0` | 历史 |
| `V090_GATE1_REVIEW_FIX_REPORT.md` | v0.9.0 | 历史 |
| `WINDOWS_COMPAT_REPORT.md` | 未绑定提交 | 历史；Windows 行为未在 HEAD 复验 |
| `rc1/KROMA_RC1_ACCEPTANCE.md` 及同目录 | RC1 封板 | 终态 `RC1_CANDIDATE_BLOCKED`；仅证明当时字节，不是 v1.5.0 的证据 |

规则：这些文档只在**它自己声明的提交**上有效。v4 的任何"已通过"必须由 HEAD 上的命令重新产生，
不能把上述报告平移过来。

## 6. 真实缺口（基线里不存在，必须由后续包新建）

以下符号在整棵源码树中**零命中**（含 `.rs`/`.json`/`.md`，排除 `target/`）：

- 蓝图 §5.2 提议的七个合同对象：`ResolvedCapabilityPlan`、`ActivationLease`、`PolicyEpoch`、
  `ProjectionRevision`、`OperationSettlement`、`EvidenceDependency`、`ResearchDecision`。
- 蓝图 §6.4 的 `performance_budget` 必填字段。
- 蓝图 §9.1/§9.2 的 Credential Broker 与独立评分 worker（`credential_broker` / `evaluation_worker` 零命中）。
- 能力六轴（delivery/platform/authorization/lifecycle/evidence/default）：基线只有 5 个布尔字段，
  没有平台维度与生命周期维度。

对照：`bench.rs` 里真实可用的执行器只有 `SYNTHETIC_EXECUTOR_LABEL = "synthetic"`，
即当前只有合成执行路径；真实 Provider 自动评测、逐 Job 隔离、评分分权均未实现。

## 7. 复算命令

```bash
git rev-parse HEAD                     # 当前提交
git rev-parse 0933a4a^{tree}           # 基线树
grep -m1 '^version' Cargo.toml         # 工作区版本 1.5.0

# 缺口符号零命中核对（期望全 0；本文件按定义会提到这些名字，需排除）
for n in ResolvedCapabilityPlan ActivationLease PolicyEpoch ProjectionRevision \
         OperationSettlement EvidenceDependency ResearchDecision performance_budget \
         credential_broker evaluation_worker; do
  printf '%-26s %s\n' "$n" \
    "$(grep -rn "$n" --include=*.rs --include=*.json --include=*.md crates/ bin/ scripts/ docs/ \
        | grep -v 'docs/engineering/BASELINE_GAP_MAPPING.md' | wc -l)"
done

# 22 项能力锚点存在性（任一缺失打印 MISS）
# 合同文件来自蓝图交付包，不在本仓库；本次核对输出已存档：
#   /opt/kroma-v4/evidence/m0-01/commands.txt
```

蓝图合同（`CAPABILITY_CONTRACT.json` 等）属于蓝图交付包，不在本仓库。
本节所有核对的实际输出见 `evidence/m0-01/commands.txt`，全量测试见 `evidence/m0-01/full-suite.log`。
