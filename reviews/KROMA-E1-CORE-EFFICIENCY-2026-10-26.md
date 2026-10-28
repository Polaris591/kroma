# Kroma 核心效率：测量基线与第一轮优化（2026-10-26）

## 范围和方法

- 基线 Git：`e042f6e`，本轮只优化 Rust `context.rs` 的 **CPU 规划路径**，并加入可选任务性能观测。
- 同一服务器、同一 Rust Release 配置、同一合成消息夹具，对旧实现与新实现分别运行 3 次，取中位数；基准用例同时断言输出等价。运行方式：`cargo test --release --locked -- --ignored --nocapture`。
- 性能测量属于本地合成基准，不能外推为模型端到端响应提速或者真实 API Token 成本下降；本轮服务器环境没有提供可用的 `KROMA_BASE_URL`、`KROMA_API_KEY`、`KROMA_MODEL`，因此不能声称完成真实 Provider 对照。

## 同进程对照结果

| 工作负载 | 原始算法中位数 | 优化算法中位数 | 规划 CPU 加速 | 输出一致性 |
|---|---:|---:|---:|---|
| TRC：641 条消息、160 个候选工具输出，约 1.41 MB 原始 JSON | 806,267 μs | 5,026 μs | 约 160 倍 | 160 个工具结果的清理决定相同 |
| APEC：1,001 条消息、约 608 KB JSON 的摘要边界扫描 | 95,246 μs | 1,519 μs | 约 63 倍 | 摘要边界和原始历史指纹相同 |

独立首轮测量（优化前/后）约为 TRC **810/10 ms**，APEC **97/1 ms**；同进程对照更适合比较，但仍会受到缓存、服务器负载、编译器和样本分布的影响。速度比并非整个 Agent 的效率提升比例。

控制组（本轮未优化）：3,001 条历史消息连续构建 8 次上下文视图约 **14–15 ms**；约 4.2 MB 会话连续 12 次全量写入约 **104–106 ms**。这些数字仅用于判断优先级，不能解释为优化失败或倒退；正式运行采用 WAL 而不是每条消息全量保存。

## 具体代码改进

1. **TRC**：一次性构建待发送上下文视图，之后根据某条工具消息替换前后的精确 JSON 字节差增量计算序列化长度和粗估 Token。缓存感知条件中的原始历史后缀长度用反向累积代替重复扫描。
2. **APEC**：增量累计每条历史消息的 JSON 长度、分隔符及摘要引用成本；不再在每个候选摘要边界重新序列化整个前缀。仍然用原始完整证据做快照与指纹。
3. **等价性保护**：保留仅用于测试的慢参考算法，覆盖缓存未知、缓存为零/高命中、不同窗口、有无旧摘要、过滤器投影、中文 Unicode；新算法的 TRC 决策集、边界和指纹必须一致。
4. **E1 观测**：`KROMA_PROFILE=1` 输出任务总耗时、模型、上下文、Shell、WAL、APEC 完成耗时，模型/工具调用次数，投影前后 JSON 字节；默认关闭以避免额外的序列化和日志。

## 可复现离线 Agent 验证

`python3 benchmarks/mock_profile_smoke.py` 启动本地 SSE 测试模型并运行 Release Kroma，验证两轮模型请求、一次 `shell` 调用、`reasoning_content` 在第二轮正确回传、消息写入 WAL，以及 Provider 返回的输入/输出/缓存 token 数被合计。示例观测（本机本地 mock，不是网络延迟）：

```
mock_e2e: two requests, one tool, thinking replay, WAL, usage OK
[profile] wall_ms=6.24 | context_ms=0.03 | model_ms=1.50 | shell_ms=4.56 | journal_ms=0.09 | apec_finish_ms=0.00 | model_requests=2 | tool_calls=1 | peak_context_json_bytes=1134 | sent_messages_json_bytes=1915 | raw_history_json_bytes=1261
```

## 明确未做的事情

- **未声称节约了任何可计费 Token 或金额**：需要在同一真实 Provider、同一任务集、控制缓存冷热的条件下测量调用量、输出质量、缓存 token 和费用。
- 没有改变 TRC 缓存经济模型、APEC 触发阈值、摘要调用频率、Shell 并发、首次工具输出去重规则；没有因追求 Token 下降而删除语义独特行。
- 规则式首轮输出过滤与长历史 WAL 的旧优化保持原样；目前没有能证明 E2/E3/E4 在真实任务上带来净收益的数据。

## 下一轮量化验收建议

使用 **同一实际模型 ID、同一反代端点和相同思考强度**，在 5–10 个真实 Coding Agent 任务上记录完整端到端耗时、模型请求次数、API input/output/cached tokens、工具调用次数、成功率以及 APEC 次数。先建立对照组，再决定是否调整输出预处理、TRC 经济规则或 APEC 阈值。
