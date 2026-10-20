# C00-11 导航分级与索引失效

三条验收规格逐条处置。代码在 `crates/kroma-core/src/repo_graph.rs`，
测试在 `crates/kroma-core/tests/gate7_repograph_vision_protocols.rs` 与 `tests/t16_repo_graph_budget.rs`。

## C00-11-A1 边来源分级

**处置：只存在一种证据类别，如实标注，不冒充调用图（no-change + 描述修正）。**

`EdgeKind` 只有三个变体，都是词法层的：`Import`（显式 `use`/`import`/`#include`）、
`CallOrReference`（标识符共现启发式，`#[default]`）、`TypeLike`（类型定义）。
仓库里没有 AST 解析器也没有 LSP 客户端，因此不存在"AST edge"或"LSP resolved edge"可分级——
给不存在的类别造标签才是失真。

`code_graph` 的工具描述已改成明说这一点：

> Edges come from explicit imports and identifier co-occurrence heuristics, not a resolved call
> graph, so treat them as navigation hints rather than verified call chains.

`called_by` / `calls_into` 这两个字段名保留（对外合同不变），但描述不再暗示它们是解析后的调用链。

## C00-11-A2 修订或工具链变化使缓存失效

**处置：PASS，且补了一条此前没有测试钉住的腿。**

每工作区解析缓存的新鲜度判定是 `(path, len, mtime_ns, content_sha256)` 四项全等才复用。
原有的 C00-11 测试驱动三次构建，覆盖"新增文件"与"重命名"（路径变了自然要重建）。
但**没有任何测试覆盖"路径、长度、mtime 都不变而字节变了"**——这正是 `len`/`mtime`
看不见的窗口。变异验证：把 `content_sha256` 从判定里删掉，原有 26 条测试全绿，
说明这一条腿此前是裸的。

因此新增 `test_c0011_cache_rebuilds_when_bytes_change_under_same_length_and_mtime`：
同路径、同长度、同 mtime（用 `File::set_times` 显式还原），内容从 `alpha_entry` 换成等长的
`beta__entry`，要求重建后只有新符号。删掉 `content_sha256` 判定时该测试 FAILED，还原后 ok。

红绿日志：`evidence/c00-11/a2-mutation-red-green.log`。

工具链变化这条腿由缓存键设计覆盖：缓存只在进程内、按工作区存活，不跨进程持久化，
因此工具链升级后不会读到旧进程的解析结果；`RepoGraphCache::retain_paths` 在每次构建末尾
按实际索引到的路径裁剪，消失的文件不留条目。

## C00-11-A3 有扫描预算

**处置：PASS，包含一次诚实性修正。**

`build_from_workspace(cap, MAX_GRAPH_FILES)` 限制文件数，`MAX_GRAPH_FILE_BYTES` 限制单文件字节；
`t16_repo_graph_budget.rs` 覆盖缓存与不缓存两条路径受同一总上限约束、以及早期文件改动后
尾部缓存仍受界。

原实现在"预算把所有候选都排除"时输出 `no ranked files (empty workspace or no indexable
source files)`，把"被预算截断"说成"空工作区"——两个完全不同的结论。现在 `truncated` 为真时
改报 `coverage is truncated, not an empty workspace`，由
`budget_excluded_workspace_is_reported_as_truncated_not_empty` 钉住。

## 收口

- 定向测试：`cargo test --test gate7_repograph_vision_protocols --test t16_repo_graph_budget`（26 条全绿）
- 全量：76 个测试二进制、1020 passed、0 failed、EXIT=0
- 证据：`/opt/kroma-v4/evidence/c00-11/`
