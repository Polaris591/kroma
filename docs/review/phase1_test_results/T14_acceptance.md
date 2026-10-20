# T14 · 校正历史缓存断点与 Provider 缓存账目 — 验收

阶段 G2｜依赖 T03, T05, T12｜关联 KR-10

## 一、缺陷

两处，方向相反但后果一致——缓存看起来在工作，实际上没在工作：

1. **断点永远停在最初**。`pick_prefix_breakpoint_index` 无条件优先 `PinnedBaselineEnd`，而它是
   run 开始时算出的冻结基线、之后永不移动。于是每一轮新增的对话都作为**未缓存输入**重发：
   缓存只暖着开头几条消息，会话真正产生的部分全是冷的。
2. **缓存写入被当成未缓存输入**。`parse_anthropic_usage` 读到了
   `cache_creation_input_tokens` 却把它折进 `input_tokens` 后丢弃；`uncached_input_tokens()`
   用 `input - cached` 计算，于是**写入的 token 被算成未缓存**。冷缓存（全是写）看起来像普通轮次，
   命中率永远偏乐观。

## 二、改了什么

### 1. 断点选择：取「最远的稳定点」

- `pick_prefix_breakpoint_index` 改为在 `PinnedBaselineEnd` 与 `TailWindowStart` 中取**映射后最大**
  的那个。`TailWindowStart` 被投影器按 8 条消息量化，所以它**会前进**，而在两次前进之间保持稳定——
  这正是前缀缓存要的折中。基线成为下限而不是上限。
- 新增第 4 个断点：**最新一条消息**。本轮缓存到尾部，下一轮的整个对话就是「已缓存的严格前缀」，
  于是下一轮读到的是缓存而不是重写。
- 断点总数仍是 4（system / tools / prefix / tail），符合 Anthropic 的每请求上限；
  `wire_role_merge_preserves_boundary_mapping` 直接断言序列化后的 `cache_control` 恰好 4 个。

### 2. 缓存账目：写入、读取、未缓存三分

- `TokenUsage` 新增 `cache_creation_input_tokens: Option<u64>`（`serde(default)`，旧 WAL 兼容）。
- `uncached_input_tokens()` = `input - cached_read - cache_creation`（此前只减 read）。
- 新增 `cache_creation_tokens()`。
- `prefix_cache_hit_ratio()` 只按 **read** 计算：写入不是命中，把它算成命中会让冷缓存看起来是热的。
- Anthropic 解析器现在把 `cache_creation_input_tokens` 单独记录；OpenAI Responses 不单独报告写入，
  记 `None`（不臆造数据）；SSE 解析器在代理提供该字段时记录。
- `EventPayload::ModelRequestCompleted` 新增 `cache_creation_input_tokens`（serde default + skip），
  所以 WAL 里现在同时有 read / creation / uncached 三个数。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t14_cache_accounting` | 4 passed / 0 failed |
| `cargo test -p kroma-core --test context_cache_stability` | 4 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | **917 passed / 0 failed**（`logs/test_t14.log`） |

### 测试清单（4 条）

- `pre_compaction_history_breakpoint_advances`：6 轮 vs 24 轮 → `TailWindowStart` 严格前进；
  基线 ≤ 前进后的断点；序列化后 `cache_control` 出现在「已前进的位置」与「最后一条消息」上
  （`cached_indices[0] > 1` 且最后一个 == `len-1`）。
- `prefix_bytes_stable_between_compactions`：同一转录投影两次 → 断点与每条消息字节完全一致；
  追加 6 轮后，前 N 条消息字节不变（缓存前缀可复用）。
- `wire_role_merge_preserves_boundary_mapping`：一次 assistant 带 2 个 tool_call + 2 个 tool_result，
  合并成一条 wire 消息后断点仍落在正确位置（角色序列 `user / assistant / user`），
  且 `cache_control` 恰好 4 个。
- `cache_hit_measurement_separates_creation_from_read`：暖轮（read 600 / creation 300 / uncached 100）
  命中率 0.6、未缓存 100；**冷轮**（creation 1000、read 0）命中率 0、未缓存 0；
  无拆分数据时未缓存 1000、不臆造写入。

### 变异验证

把「取最大」改成「取最小」（即回到永远优先基线），`pre_compaction_history_breakpoint_advances`
立刻红：`the prefix marker must have advanced past the early messages: [1, 47]`——第 1 条消息被缓存，
24 轮对话全部未缓存。恢复后 4/4 绿。

## 四、边界

- **实网 canary 未执行（NOT_RUN）**：任务卡要求「在受控 canary 中核验，不以模拟比例证明真实命中」。
  这需要用户预算与凭据授权，本机 Kroma 的两把 key 已 401/余额不足。台账里记为 **NOT_RUN**，
  没有用模拟比例冒充真实命中。执行方式：`KROMA_API_KEY=... kroma --task ... --verbose` 跑 20+ 轮，
  读 WAL 里 `ModelRequestCompleted` 的 `cached_input_tokens` / `cache_creation_input_tokens` 曲线。
- 断点前进依赖投影器的 8 条量化步长；如果 `context_tail_window_bytes` 大到覆盖整份转录，
  稳定前缀本就不存在，断点会停在 0——这是正确行为，测试里用小窗口覆盖了有前缀的情形。
- 没有实现「压缩后重建」的显式事件：压缩后 `PinnedBaselineEnd`/`TailWindowStart` 都会变化，
  从 WAL 的投影事件与断点值可以观察到，但当前没有单独一条 `CacheBoundaryRebuilt` 事件。
  这条记为 T30 台账的已知缺口。
