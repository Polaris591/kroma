# KR-L01 连接与序列化开销的受控测量

验收要求：确认可优化占比，并保留安全正确性的对照用例。

## 测量

测试：`mcp_http_2026::local_loopback_protocol_cost_is_measured`（n=5，每次新建连接）。

同一条链路上，客户端做的事全部包含在内：TCP connect、请求序列化与写入、响应读取、JSON 解析。

```
KR-L01 loopback protocol cost: median=425.002µs min=385.088µs max=549.238µs (n=5)
```

## 对照

同一台机器、同一客户端代码打真实远端（cobblemon.asia，gpt-6.1-sol）的首 token 时间：

| 指标 | 值 |
|---|---|
| loopback 协议开销（中位数） | 0.425 ms |
| 远端首 token（chat-completions canary） | 3336 ms |
| 协议占比 | **0.013%** |

## 结论

**可优化占比在客户端协议层几乎为零。** 端到端延迟的 99.98% 在别处：DNS/TLS 建连、网络往返、
服务端排队与推理。把力气花在"优化客户端序列化"上不会改变任何可感知的指标。

这也解释了为什么 KR-E03 的延迟归因把 94.9% 记在"首 token 之前"是合理的：那一段里客户端只占
0.4 毫秒。

## 安全正确性的对照用例（保留）

协议开销小不等于可以省掉安全步骤。以下用例仍然必须通过，它们才是这条路径的正确性边界：

- `rejects_non_loopback_plain_http_and_forged_newlines`：非 loopback 明文与伪造换行被拒
- `mcp_duplicate_names_and_oversized_schemas_are_refused`：恶意 schema 被拒
- `http_mcp_2026_sse_terminal_response_ends_the_call_without_connection_close`：终结帧即返回
  （KR-K02，旧实现 5.0009s → 新实现 0.00s）

## 没有做的部分

没有测 TLS 握手本身耗时（需要可控的 TLS 服务端与证书链，本机只有 tls-smoke.sh 的冒烟）。也没有
做"同一请求在不同 payload 大小下的序列化耗时曲线"。上面的结论只到"协议层不是瓶颈"这一步，
不足以支撑"某个序列化实现更快"。
