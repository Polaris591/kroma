# T16 · 修正 RepoGraph 缓存路径预算 — 验收

阶段 G2｜依赖 T06｜关联 KR-25

## 一、缺陷

`RepoGraph::build_from_workspace_cached` 里，**缓存命中分支绕过了字节预算**：

```rust
if let Some(cached) = cache.get_fresh(...) {
    cache_hits += 1;
    total_bytes = total_bytes.saturating_add(cached.byte_len);   // 只加，不检查
    ...
    continue;
}
if total_bytes + text.len() > MAX_GRAPH_TOTAL_BYTES {             // 只有解析分支检查
    truncated = true; break;
}
```

后果：同一工作区、同一上限，**冷构建索引 3 个文件、热构建索引 8 个**。声明的覆盖范围随缓存状态变化，
而调用方看到的只是「图变大了」，没有任何信号。

另一半（任务卡第三条）：`extract_reference_tokens` 会剥掉注释和字符串，所以索引里**只有符号名**。
「错误是怎么重试的」这种自然语言问题，只有在某个标识符恰好叫这个名字时才能命中。

## 二、改了什么

1. **预算检查移到两条分支的共同入口**：先算出 `entry_bytes`（命中 → `cached.byte_len`，未命中 →
   `text.len()`），统一检查 `max_total_bytes`，再累加，然后才分派到命中/解析分支。
2. **`GraphLimits { max_files, max_total_bytes }`**：预算成为显式参数（默认值仍是原来的两个常量），
   于是测试可以用 64 KiB 的预算精确验证，而不必造一个 24 MiB 的工作区。
   `build_from_workspace_cached` 保持原签名，委托给 `_with_limits`。
3. **`RepoGraph::file_hashes: Vec<(path, sha256)>`**：覆盖范围与 `truncated` / `skipped_files` /
   `cache_hits` 一起返回，调用方能知道**具体是哪些文件**进了图，而不只是「几个」。
4. **`extract_doc_tokens`**：从注释里抽词（`//`、`#`、`/* */`，启发式扫描，不解析字符串），
   与符号名一起进入文件的 token 集。注释是意图写下来的地方，这是「能回答一个问句」和
   「只能 grep 符号名」的差别。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t16_repo_graph_budget` | 3 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | **925 passed / 0 failed**（`logs/test_t16.log`） |

### 测试清单（3 条）

- `cached_and_uncached_graph_obey_same_total_cap`：先用无预算上限的构建**预热缓存**（8 个文件全部入缓存），
  再用 64 KiB 预算构建：冷构建（新缓存）索引 3 个文件、`truncated = true`；
  热构建（预热缓存）**命中数 == 冷构建索引数**、覆盖的文件与哈希**逐一相同**、`truncated` 相同；
  两次构建实际索引的字节数都 ≤ 64 KiB。
- `changed_early_file_plus_cached_tail_is_bounded`：预热后改写最早的文件 → 重建仍受 64 KiB 约束，
  被改文件的哈希**变了**（没有吃到陈旧缓存），尾部文件仍从缓存命中。
- `graph_corpus_has_natural_language_issues_not_only_symbol_names`：
  `extract_reference_tokens` 里**没有** `backoff`/`jitter`/`exponential`（注释被剥掉了），
  `extract_doc_tokens` 里**有**；端到端用 `code_graph {"query": "exponential backoff jitter"}` 查询，
  结果里出现 `retry.rs`——自然语言问句真的能命中注释。

### 变异验证

把预算检查改回「只在解析分支检查」（`cached.is_none() && ...`），
`cached_and_uncached_graph_obey_same_total_cap` 立刻红：

```
every indexed file must be served from the primed cache
  left: 8     <- 热构建索引了 8 个
 right: 3     <- 冷构建只能索引 3 个
```

第一次尝试变异时测试**没有红**——因为预热缓存只覆盖了冷构建读到的前 3 个文件，第 4 个永远是未命中，
预算检查照样生效。这说明原来的测试布局是空转的，已改为「先用无预算构建预热全部文件」再验证。

## 四、边界

- 没有引入 AST / LSP / 向量库：任务卡明确禁止把正确性修复扩大成新索引系统。注释抽词是最小的
  自然语言信号，排序权重沿用现有实现。
- `extract_doc_tokens` 是启发式的：字符串里的 `//` 会被误判为注释开头。它只影响排序信号，
  不影响任何正确性判断，且比「完全忽略注释」更接近任务卡的要求。
- 预算的**数值**没变（1200 文件 / 24 MiB / 单文件 512 KiB），只是让热路径遵守同一套数值。
- 微基准脚本没有新增：`cached_and_uncached_graph_obey_same_total_cap` 已经同时给出冷/热的
  覆盖与命中数，任务卡要求的「三类缓存边界测试」由这三条覆盖；性能数字不在此任务声称范围内。
