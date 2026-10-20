# ADR-0002：任务状态用结构，语义压缩默认关闭（T25）

- 状态：**已接受（Stage 1 落地；Stage 2 默认关闭）**
- 依赖：T14（缓存断点）、T15（证据存储）、T24（真实基线 — BLOCKED）
- 证据：`crates/kroma-core/tests/t25_task_state.rs`（4 条）

## 1. 问题

上下文压缩到后面，「用户到底要求了什么」很容易丢：早期那句「不要改 tests/」被压成 stub，
模型继续动手，改的正是不能改的地方。反过来，让模型生成一段「思考摘要」来保信息也会引入新问题：
摘要可能和用户的原话不一致、被反复引用后当成事实、每次生成还要花钱并打断前缀缓存。

## 2. 决策

**Stage 1（已实现）：只用类型化结构，不生成摘要。**

`TaskState` 从**规范转录**派生，成本为零，不调用模型：

| 字段 | 内容 | 关键约束 |
| --- | --- | --- |
| `goal` | 用户首条任务陈述 | 原样保存 + 出处（canonical_index / quote） |
| `constraints` | 用户明确要求的限制（"do not…" / "不要…"） | **逐字引用**，不做改写；压缩视图里同时显示 statement 与 quote |
| `verification` | 用户说过的验证方式 | 同上 |
| `failure_hypotheses` | 失败假设 + `HypothesisStatus` | 新建的一律 `Unverified`；没有证据的 `Supported` 仍然不是事实 |
| `path_mentions` | 工具输出里出现的路径 | 一律 `mention_only: true`，不表达因果 |

因为状态从规范转录派生，而转录在 `--resume` 时由 WAL 恢复，**压缩和恢复都不会丢约束**——
这一点由 `early_user_constraint_survives_compaction_and_resume` 实测（先强制压缩，再从 WAL 重开比对）。

**Stage 2（语义压缩器）：默认关闭，`SemanticCompression::Disabled`。**

启用条件（缺一不可）：

1. EXP-01/02 的**留出集**显示质量不退化（`promotion_verdict` 的 `Promote`，不是开发集）；
2. 压缩成本进 ledger：compressor input/output tokens、reread 字节、cache 重建次数
   （`CompressionCost`，`summary_cost_includes_compressor_and_rereads` 钉住「不能只算调用费」）；
3. 摘要必须以 schema 输出，且**不得改变权限**：摘要里写「可以改 tests/」不会让策略放行。

在满足这些条件之前，唯一的支持配置是「派生结构」，它不花钱、不打断缓存、也不会说错话。

## 3. 为什么不让摘要当事实

`HypothesisStatus` 的三个值刻意不对称：

- 新建 → `Unverified`；
- `Supported` **且**至少一条 `EvidenceRef` → 才算 fact；
- `Contradicted` 即使有证据也不算 fact。

所以「重复写」「模型说它是对的」都不能把猜测变成事实。
`unsupported_summary_claim_is_not_promoted_to_fact` 直接测这四种情形。

## 4. 不承诺

- 不做「25% 更激进的压缩阈值」这种调参并称之为语义智能——阈值只是阈值。
- 不复制 R02–R05 的论文数字；本 ADR 不含任何外部性能声称。
- 不改默认 P2 投影档位。Stage 1 的结构是**旁路信息**，不改变现有压缩算法。
- 摘要内容不参与策略判定：权限只由 `PolicyEngine` 与执行边界决定（T09）。

## 5. 何时重新评估

当 EXP-01/02 有留出集结果且 `CompressionCost` 显示净收益时，再把 `SchemaConstrained` 打开。
在此之前它保持 `Disabled`，并且配置里选它不会有任何效果——这比「开了但静默零操作」诚实。
