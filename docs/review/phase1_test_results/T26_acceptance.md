# T26 · 代码定位与 ACI 的模型适配实验 — 离线部分完成 / 模型对照 **BLOCKED**

阶段 RESEARCH-OPTIONAL｜依赖 T12, T16, T24｜关联 KR-31

## 一、找到的真缺口

`RepoGraph::query` 的种子只来自 **文件路径** 与 **符号名**：

```rust
if lowered_terms.iter().any(|t| lowered_path.contains(t)) { mark_seed(..., PATH_MATCH_WEIGHT) }
if symbols.any(|s| s.name.contains(term))               { mark_seed(..., SYMBOL_DEFINITION_WEIGHT) }
```

T16 加进索引的注释词（`extract_doc_tokens`）只影响**依赖边权重**，没有参与检索——
所以「自然语言 Issue 找修改位置」实际上没生效：不含符号名的查询没有任何种子，
个性化 PageRank 退化成度中心性，命中的是「最像核心」的文件而不是相关文件。

（T16 那条 `graph_corpus_has_natural_language_issues_not_only_symbol_names` 断言的正是这一点，
但它只验证了「查询结果里出现 retry.rs」，而当时的实现是靠中心性侥幸命中的——这是测试强度不足，
本轮用真正的基线对照补上了。）

## 二、改了什么

1. `RepoGraph` 保存每文件的 token 集（`file_tokens`，构建时本来就算过，只是没留）。
2. `query` 在路径/符号之外增加**内容 token 种子**：`CONTENT_MATCH_WEIGHT = 90.0`
   （低于符号定义 150，高于路径匹配 60）。命中符号定义时 `continue`，避免同一文件被重复加权。
3. 语言能力矩阵与「无增益撤回」记录：`docs/release/language-matrix.md`。

## 三、实测（离线、可复算、无模型调用）

语料：12 个文件（5 个目标 + 1 个符号目标 + 6 个干扰），文件名全部是 `module_*.py`，
**文件名不含答案词**；5 条 Issue 形如「retry backoff jitter upstream connection」，
答案只能从注释读出来。

| 指标 | 符号名基线 | 当前图 | 门槛 |
| --- | --- | --- | --- |
| rank-1 命中 | **0 / 5** | **5 / 5** | ≥ 4 / 5 |
| rank-3 命中 | — | 5 / 5 | — |
| 符号名查询 | 2 / 2 | 2 / 2 | 不回退 |
| 截断索引时 | 词法回退仍能定位 | 同 | 必须有回退 |

复现：`cargo test -p kroma-core --test t26_localization`

### 变异验证

把内容种子那一支关掉（`if content_match && false`），rank-1 命中从 **5/5 掉到 3/5**，
低于 4/5 门槛，测试变红。说明这条能力是真实生效的，不是测试凑出来的。

## 四、语言能力矩阵（要点）

| 语言 | 符号抽取 | 注释词 | 依赖边来源 | 置信等级 |
| --- | --- | --- | --- | --- |
| Rust / Python / TS-JS / Go / Java / C-C++ | ✓ | ✓ | 引用与导入 token 共现（**词法**） | **低** |
| YAML / TOML / Markdown / Shell 等 | ✗ | ✗ | — | 不索引 |

**没有任何语言接入 LSP**，依赖边不是解析结果，所以置信等级如实标为「低」，
没有包装成「语义图」。任务卡的停止条件明确禁止同时引入向量库 / 全仓语义图 / 多规划器。

## 五、BLOCKED：模型对照实验（EXP-03）

任务卡第 3 步「分模型比较原生 edit/write 与简化工具界面」**没有执行**：

- 需要真模型对照，依赖 T24 的预算授权（T24 = BLOCKED，无费用上限）；
- 在无数据前给所有模型套同一套简化界面，正是任务卡要避免的事。

因此本项**不声称**任何模型侧收益；工具界面现状是原生工具始终可用，`--on-demand-tools` 是可选机制。

## 六、证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t26_localization` | **2 passed / 0 failed** |
| `cargo test -p kroma-core --test t16_repo_graph_budget` | 3 passed / 0 failed（预算语义未回归） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | **974 passed / 0 failed**（`logs/test_t26.log`） |

## 七、边界与撤回条件

- 无增益撤回条件写在 `docs/release/language-matrix.md` 第 5 节（命中未达基线 +2、建图超预算、
  需要 LSP 才能超过词法基线、EXP-03 显示无收益），一旦命中就回退到路径/符号或纯词法搜索。
- 词法回退是与 `grep` 等价的子串扫描，不是第三套索引。
- 没有改默认工具集，没有为任何模型隐藏原生工具。
