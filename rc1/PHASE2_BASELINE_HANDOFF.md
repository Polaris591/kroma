# 阶段 2 基线交接（只读档案，不含新实现）

## 1. 源码身份

- 源码包：`kroma-p1-sealed-source.tar.gz`，`sha256 e34ac0c1…e23b`
- Git：见 `RELEASE_MANIFEST.json`（本仓库线性历史）
- 版本：以 `Cargo.toml` / `package.json` / `kroma --version` 实际输出为准，本文不宣称已发布版本号
- 可运行平台（本轮真实验证）：**linux x86_64**；其余平台见 `KNOWN_LIMITATIONS.md` L1

## 2. 已实现且已验证的机制（阶段 2 不要重复开发）

| 机制 | 位置 | 证据 |
| --- | --- | --- |
| Runtime 装配五门 | `runner_assembly.rs` / `agent_runner.rs` | `t11_assembly` 5/5（digest 相等 + 依赖闸门） |
| 工具发现闭环 | `tool/catalog.rs` / `agent_loop.rs` | `t12_tool_discovery_loop` 4/4（严格 provider 4 轮） |
| 冻结实验清单 | `experiment.rs` | `t17_experiment_manifest` 10/10 |
| 请求级成本与身份 | `usage_cost.rs` / `provenance.rs` | `t18_*` 12 条 |
| 任务状态（约束保真） | `task_state.rs` | `t25_task_state` 4/4（压缩+恢复后约束逐字不变） |
| 内容 token 检索 | `repo_graph.rs` | `t26_localization` 2/2（5/5 vs 符号基线 0/5） |
| 控制策略与隔离探针 | `control_policy.rs` | 8 单测 + 3 集成（探针只在副本） |
| 经验撤销与失效 | `handoff.rs` / `experience.rs` | 5 条（撤销下次生效、四类失效） |
| 演化轮次合同 | `harness_evolution.rs` | 5 条（冻结验收标准、单任务不晋升） |

## 3. 已存在但"真实效果未证明"的机制

- **Tournament / 多候选**：T24 留出集只有 2 个任务簇，判 `INCONCLUSIVE`；默认关闭。
- **P2 投影档位与 TaskState 联动**：机制有测试，真实收益未测（EXP-01/02 未跑）。
- **RepoGraph 语义图 / LSP**：未实现（只有词法 token），EXP-03 未跑。
- **AHE 离线演化**：机制有测试，EXP-06 未跑。
- **工具发现（on-demand）**：默认关闭，收益未测。

## 4. 公平对照必须固定的项

模型与版本（含 reasoning level）、provider 与协议、工具 schema 全文与 hash、系统提示词 hash、
缓存策略、每任务 token 预算与超时、冻结任务集与留出集划分、裁判/验证器（必须放工作区外）、
重复次数与随机种子、成本口径（读/写/缓存分别计价）。

Kroma 侧已有可直接复用的记录：`kroma bench --executor provider` 的 `source_fingerprint`
（binary sha256、prompt_hash、tool_schema_hash）、逐 run 的 `tokens/budget_overage/cap_ok/terminal_reason`。

## 5. 已知统计限制

- T24：2 个任务簇 × 3 重复 = 6 对观测，配对差 95% 区间 [+0.0%, +33.3%] → 不可用于宣称领先。
- 验证器写实现细节会误判正确解（本轮踩过三次：`range(1` / `is_symlink` / `tmp`）。
  新实验的判定必须只认行为（跑测试通过），字符串断言仅限提示词明确要求的字面。

## 6. 建议的阶段 2 三分法

- `READ_ONLY_RESEARCH`：论文/竞品口径分析，不改代码。
- `BENCH_EXPERIMENT`：只加任务集与统计，不改产品默认值；结论必须过留出门槛。
- `PROPOSAL_NOT_IMPLEMENTED`：架构提案，先写 RFC 与回滚方案。
