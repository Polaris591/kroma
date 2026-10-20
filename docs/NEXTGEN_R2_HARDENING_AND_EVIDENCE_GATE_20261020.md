# Kroma NextGen R2 — 本轮继续开发与交付门禁（2026-10-20）

基于先前的 `Kroma_NextGen_缓存_工具_上下文压缩_完整源码_2026-10-20.zip` 工作树持续修改。**源码开发并不等于编译成功或生产级验收**。本容器没有 `rustc`、`cargo`，也无法解析 rustup 下载地址；所有 Rust 更改需在有工具链的机器运行全量门禁。

## 1. 缓存/Responses API：语义不丢失

- `provider/openai_responses.rs`：非流式 JSON 必须有数组形状的 `output`；不允许缺字段却被解释为成功的空答复。
- 增加 `tool_search_call`、`tool_search_output`、`additional_tools` 及其它尚不支持的 native tool output 项的 **fail-closed** 保护，JSON/SSE 路径都拒绝。否则“工具目录已变”的事实在 Kroma 持久化 transcript 里丢失，恢复后工具调用的权限及上下文语义就不可重现。
- **不是**声称 Kroma 已支持原生 hosted/client-executed tool search。本次只能安全拒绝无法重放的事件；后续需扩展 `TranscriptItem`，完整存储 search call / search output / injected tools，验证授权范围、session replay 和跨模型降级。

## 2. 工具/MCP：边界与缓存完整性

- HTTP MCP `x-mcp-header` 注解数量上限 32、单参数值 4 KiB、全部注解头合计 16 KiB，Bearer 最长 8192 字节，防止小请求变成巨大 HTTP 头。
- `tools/list` 的 `ttlMs`（非负整数）与 `nextCursor`（字符串）有严格类型校验。类型错误不能静默解释为“无限缓存”或“分页完毕”。缓存仍按 server/client instance 隔离；已注册 Schema 发生变更时 fail-closed。
- JSON-RPC 响应必须有且仅有 `result` 或 `error`；SSE 帧包括 notification 必须是 `jsonrpc=2.0`；同一 request ID 的重复 SSE 完成事件拒绝。
- 新增 Rust 单元测试：超大 header、多注解 header、恶意 SSE、重复 final。

## 3. 上下文压缩：估算不能冒充可回收空间

- `ContextProjector` 区分两项字节：**硬预算**仍计算全部工具输出和图片；**主动压缩估算**只计健康、未归档、无图像、且达到最小尺寸的文本工具输出。不因大图片或已压缩 stub 错误触发主动压缩。
- `KROMA_CACHE_AWARE_COMPACTION` 仍默认关闭，强制水位不能被实验策略绕过。

## 4. 可执行 A/B 统计门禁（新增，离线）

`python3 scripts/triad_paired_gate.py --report provider-run-report.json --prices operator-rates.json --min-tasks 20`

工具做 task-level bootstrap（同任务重复试验汇总为一个 cluster），检查：

- `baseline/candidate` 的 (task_id, repetition) 严格成对，不能缺失、重复或串模型；质量使用任务成功布尔值；低于最小任务数一律 `insufficient_tasks`。
- 非劣性预设置信区间下界 > `-delta_pp`。质量未过门禁即使省钱也**不能**报告收益。
- 每条账单都必须是 provider-reported usage，且有 exact reported_model 费率；cache read/write 分量缺失时保持 unknown，不推算为 0。缓存读+写不得超过总输入 tokens。
- 只有质量非劣、所有成本均可计算、候选相对基线成本差异 95% bootstrap CI 的上界 < 0，才允许 `cost_saving_claim_allowed=true`。
- 附带缓存命中、工具 Schema 体积、上下文压缩数据的配对差异；不会把“文本字节变小”当作“美元变便宜”。

**注意**：该脚本只审查录入的数据，不会请求 provider，不会证明采样真实/随机，也不是账单认证。需要使用原来的 bench 锁定计划和外部签名验收收集可信结果。运行 `python3 scripts/test-triad-paired-gate.py` 可离线执行 11 项正反例测试。源文件首部列出 JSON 输入格式。

## 5. 当前验收状态

- PASS：`python3 scripts/test-triad-paired-gate.py`：11 项通过。
- PASS：`python3 scripts/verify_nextgen_static.py` 与 `git diff --check HEAD`。
- PASS：此前运行 `node scripts/test-bench-foundation.js`：36 项，`node scripts/test-bench-phase2.js`：18 项；其它工作流 Python 检查通过。
- **NOT RUN**：Rust `cargo fmt`、`cargo test`、`clippy`，Rust 行为回归、真实 provider canary、真实 MCP 互操作/模糊测试。

在 Rust 环境进入源码根目录运行 `sh scripts/nextgen_rust_gate.sh`，处理任何失败。**生产合并仍需 Rust 全绿与正式 API/MCP/质量基准。**
