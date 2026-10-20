# T25 · 上下文下一代增强：TaskState 与按需语义摘要 — 验收

阶段 RESEARCH-OPTIONAL｜依赖 T14, T15, T24｜关联 KR-31

## 一、缺陷

上下文压到后面，「用户到底要求了什么」最先丢：早期那句「不要改 tests/」被压成一行 stub，
模型继续动手。反过来，让模型生成「思考摘要」来保信息也引入三个新问题——摘要可能和用户原话不一致、
被反复引用后当成事实、每次生成都要花钱并打断前缀缓存。

## 二、做了什么（Stage 1：只加结构，不生成摘要）

新模块 `crates/kroma-core/src/task_state.rs`，`TaskState` 从**规范转录**派生（不调用模型，成本为零）：

| 字段 | 内容 | 防的是什么 |
| --- | --- | --- |
| `goal` | 用户首条任务陈述 + 出处 | 任务被摘要改写 |
| `constraints` | "do not…" / "不要…" 等限制，**逐字引用** + `Provenance{canonical_index, quote}` | 约束在压缩中丢失或被改写 |
| `verification` | 用户说过的验证方式，同样带出处 | 验收标准被忘记 |
| `failure_hypotheses` | 失败假设 + `HypothesisStatus` | 猜测变成事实 |
| `path_mentions` | 工具输出里出现的路径，一律 `mention_only: true` | 路径匹配被当成因果 |

`HypothesisStatus::is_fact(evidence)` 的规则是刻意不对称的：

- 新建 → `Unverified`；
- `Supported` **且**至少一条 `EvidenceRef` → 才算 fact；
- `Contradicted` 即使有证据也不算。

`constraint_lines()` 给压缩视图用的格式是 `- <statement> (user: "<原话>")`——statement 和 quote 都出现，
审阅者能直接对回用户原话。

## 三、Stage 2（语义压缩器）：默认关闭

`SemanticCompression::Disabled` 是 `Default`。开启 `SchemaConstrained` 的三个前置条件
（写进 ADR-0002）：留出集显示质量不退化、压缩成本进 ledger、摘要不得改变权限。

`CompressionCost` 把成本拆成四块——compressor input/output tokens、**reread 字节**、
**cache 重建次数**——`describe()` 输出全部四项。`validate()` 拒绝「disabled 却记了 compressor tokens」
这种自相矛盾。

## 四、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t25_task_state` | **4 passed / 0 failed** |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 971 passed / 1 failed（`logs/test_t25.log`） |

那 1 条是台账 §2.2 的既有 flake `b08c_git_metadata`（需 `--test-threads=1`），串行复跑 5/5 绿。

### 测试清单（4 条，逐条对应任务卡回归）

- `early_user_constraint_survives_compaction_and_resume`：用户首句含「Do not modify anything under
  tests/」+ 验证方式；塞入 8 轮大输出**强制压缩**（断言 `compaction_event.is_some()`，
  否则测试是空转）；派生出的 `TaskState` 约束逐字保留、出处 index=0、压缩视图含 `user:`；
  再把同一转录写进 WAL、重开后重新派生，`constraints` **完全相等**。
- `path_mentions_do_not_claim_causal_truth`：工具输出里出现 `src/parser.rs` →
  记为 `mention_only: true`，且不产生任何 `failure_hypotheses`。
- `unsupported_summary_claim_is_not_promoted_to_fact`：新假设不是事实；重复写入不是事实；
  `Supported` 但无证据仍不是事实；补上 `EvidenceRef` 才是；被 `Contradicted` 的即使有证据也不是事实。
- `summary_cost_includes_compressor_and_rereads`：默认档位是 `Disabled`、结构派生的成本为 0；
  真实压缩 2000+300 tokens + 64 KiB reread + 1 次 cache 重建 → `describe()` 里三项都在；
  「disabled 却记了 tokens」被判为错误。

## 五、边界（任务卡停止条件）

- **不把更激进的压缩阈值称为语义智能**：ADR 里明确写「阈值只是阈值」。
- **不复制 R02–R05 的论文数字**：本项没有任何外部性能声称。
- **不改默认 P2**：Stage 1 的结构是旁路信息，压缩算法本身没动。
- **摘要不得改变权限**：`TaskState` 不参与策略判定，权限只由 `PolicyEngine` 与执行边界决定。
- EXP-01/02 未跑（依赖 T24 的预算授权，T24 = BLOCKED），所以「无质量退化 + 预注册门槛」
  这一条**没有声称满足**，实验开关保持关闭——这正是任务卡要求的「未证明则默认关闭」。
