# Kroma NextGen：缓存、工具、上下文压缩三系统代码交付说明

> 2026-10-20 · 源码基线 `e95845b688729abc9b7f9ef2c0b824feda1ef7e7`，基线已叠加此前保存的 **KH-001 / P0 缓存写入计费补丁**。
>
> **代码已写入；Rust 编译、单元测试、全量回归和真实模型端到端测试均尚未执行**。这里的“完整源码交付”不等于“完成生产验收”或“全部研究任务落地”。

## 目标与边界

本轮以最小侵入原则，将三组能够离线写出明确协议/回归夹具的能力接入 Kroma；保留原先真实权限策略、调用身份、证据仓储和硬水位保护。未有真实 provider API 与 cargo 工具链的部分，保持 fail-closed 或实验默认关闭。不声称原生 `tool_search`、MRTR、真实 paired-A/B 或 provider-native compaction 已实现。

## 1. 缓存体系（Cache / Provider）

**具体变更**

- `crates/kroma-core/src/provider/openai_responses.rs`：`KROMA_OPENAI_CACHE_MODE=off|implicit|explicit`。默认 **off** 保留旧请求。只对正向标记的 GPT-5.6+/GPT-6 型号开启新参数；未知型号会拒绝启用（不推测不兼容代理 API）。
- 明确缓存模式：`prompt_cache_options={mode:explicit,ttl:30m}`；将原本静态 `instructions` 转成输入序列的首个 `developer` `input_text` 并打 `prompt_cache_breakpoint`。模型工具定义保持稳定按名称排序，现有 durable reasoning replay 保持原样。隐式模式仅填 top-level cache options。
- 可选 `KROMA_OPENAI_CACHE_KEY` 用作隔离标签（<=128 bytes，禁止控制字符）；`KROMA_OPENAI_CACHE_TRACE=1` 打开不输出提示词内容的 provider usage/cache miss 观测。日志原因来自 provider 事实，不能将“请求逻辑等价”自动视为“实际 cache hit”。
- 保留前轮 P0 修复：`input_tokens_details.cache_write_tokens` 解析到 `TokenUsage.cache_creation_input_tokens`；未知不等于零。新增 cache reads+writes 不能超过总输入量校验。
- `crates/kroma-core/src/usage_cost.rs`：若价格表明确规定 cache-write 费率，但 provider 没有写入用量，账目返回 unknown，而不是当作免费或零写入。

**重要限制**：GPT-5.6+/GPT-6 具体 API 响应、提示词前缀稳定性和计费字段需真实 provider canary 校验；目前有源码级夹具、无 API 通过证明。用户指定非正向名单上的精确模型名，请先增加正向夹具再放开，不要全局跳过检查。

## 2. 工具体系（MCP / Catalog）

- 增加 `crates/kroma-core/src/mcp_http.rs` **MCP 2026-07-28 stateless Streamable HTTP**，与旧的 2024-11-05 stdio adapter 并存；避免复用旧握手假装新协议兼容。
- Trusted config 位置仅限全局 `~/.kroma/config.toml`（或代码明确允许的可信注入），工作区 `.kroma.toml` 无权创建 MCP 配置，继续遵守现有授权、审批和 mutating/read-only 权限门。
- 新版请求使用同一 HTTPS/loopback-only transport；POST 包含 `MCP-Protocol-Version`、`Mcp-Method`、`Mcp-Name` 与 `_meta`，同时接收 JSON 和 bounded SSE。只处理本次请求的事件；拒绝未实现的 InputRequired / MRTR，不伪造继续结果。HTTP 重定向不自动跟随。
- 解析并约束 `tools/list` 的分页和 `ttlMs`（上限 30min）；所有 cache 对象在**当前独立 MCP 客户端实例**内，按实际全局配置和 bearer 生存期隔离，不存在跨服务端/用户的全局共享。过期目录恢复时对比最初注册的 tool descriptor；schema 变更要求显式重新挂载。
- `inputSchema.properties.*.x-mcp-header` 明确白名单 + 层级检索，仅原始 primitive string/integer/boolean 能抽取到 `Mcp-Param-*`。对不可安全解析的 annotation 不公开对应工具；对注入、不安全字符和重复名称拒绝；非 ASCII 值使用 `=?base64?...?=` sentinel。
- 新增 `tests/mcp_http_2026.rs` 回环 TCP Rust fixtures，断言协议请求头、元数据、参数头、TTL 目录复用、工具结果 SSE；新增配置隔离测试。

**重要限制**：这不是 OpenAI/Anthropic provider-native `tool_search`，没有任何“自动发现即可赋权”的后门。自定义 header 模式还需与真实 MCP 服务器做兼容性和恶意输入模糊测试。远端 token 只通过受控环境变量名解析，勿写到配置文件。

全局配置示例：

```toml
[mcp.internal_search]
http_url = "https://trusted-mcp.example.com/mcp"
bearer_token_env = "KROMA_SEARCH_MCP_TOKEN"
read_only = false
```

`read_only=false` 是安全默认值；只在受信任的只读服务器经过审计后才可设置 `true`。启用之前在启动 Kroma 的终端导出 `KROMA_SEARCH_MCP_TOKEN`，不要将值提交到代码仓库。

## 3. 上下文压缩（Context / Economics）

- 新增 `src/cache_aware_compaction.rs` 的三态 planner：`Mandatory / Proactive / Defer`。输入只接受上一轮 provider **实际报告的缓存读取 Token**；没报告则为 unknown，始终不主动压缩。候选触发需要足够的上下文占用、可回收的工具输出及保守的冷/热前缀代价估计，计算结果仅是启发式 token-equivalent 而非计费实测。
- `agent_loop` 加 `KROMA_CACHE_AWARE_COMPACTION=1` **实验开关**；默认关闭。超过原有高水位时，mandatory compaction 优先级最高，绝不为了 cache hit 越过上下文硬预算。提前压缩采用较温和的目标。
- `ContextProjector.authoritative_history_preserved` 在真实投影路径运行强校验：用户指令、助手规划、工具调用内容须逐项不变；仅工具结果的可归档内容可变化，且 call_id/tool_name/outcome 不变。验证失败返回 `ProviderProtocolError`，不允许送出变更后的用户指令。
- `tests` 覆盖热缓存不要提前压缩、未测缓存不等于零、硬预算先行以及指令未更改等。

**重要限制**：目前没有语义压缩模型的 provenance/held-out 非劣性结果；本轮没有引入会自行修改目标、权限或补写“缺失证据”的神经网络摘要流程。调参之前必须实测数据和长期任务完成率。

## 已实现 / 尚缺内容对照（对齐原开发单）

| 编号 | 本轮状态 | 说明 |
| --- | --- | --- |
| KH-001 | 补丁已应用、未编译 | 缓存写入统计和成本语义，需 Rust + 实网 usage gate |
| KH-002 | 部分源码实现、未编译 | 新缓存能力清单和 explicit/implicit 请求；缺真实 wire golden |
| KH-003 | 部分 | 沿用稳定工具排序；未落地 provider-native deferred-tool discovery |
| KH-004 | 部分源码实现、未编译 | 新旧双适配、ttlMs/cacheScope、基础 headers；缺 MRTR、真实服务兼容和 fuzz |
| KH-005 | 未完成 | 提供后续 live A/B 要求；未实现真实收费 provider 验证平台 |
| KH-006 | 未完成 | 原生 OpenAI/Claude tool_search/defer_loading + replay 尚未实现 |
| KH-007 | 未完成 | 受限只读 programmatic aggregation 尚未实现 |
| KH-008 | 部分 | 新增非内容 cache miss telemetry；未完整接入 request_observation 分析链 |
| KH-009 | 部分源码实现、未编译 | 保守决策、硬约束校验；缺长期案例和回滚验收 |
| KH-010 | 未完成 | schema 约束语义压缩与 held-out noninferiority |
| KH-011 | 未完成 | 原生 compaction 与 opaque replay |
| KH-012 | 未完成 | tenant-scoped 文件内容/version cache 与写后失效 |

## 最低交付门禁

当前容器不具备 `cargo` / `rustc` / `rustfmt` 和真实服务端凭据，因此不可报告测试通过。开发者机器请运行：

```sh
sh scripts/nextgen_rust_gate.sh
```

依次运行 Python 14 项源码结构检查、`cargo fmt --all -- --check`、OpenAI 缓存夹具、MCP 回环契约、上下文门禁、P0 accounting 测试、`cargo test --workspace --locked` 与 `cargo clippy --workspace --all-targets --locked -- -D warnings`。**Rust 任何红项都必须修复；这只是离线门禁，仍不等于可上线。**

生产前还需要：

1. 使用脱敏真实 Responses API 调用验证真实 cache_write_tokens / cached_tokens、总用量不重复计算、未知计费不可报节省，检查不同模型版本的兼容性。
2. 真实 MCP 服务器版本 2026-07-28 JSON/SSE、headers、TTL、schema refresh、分页、恶意 HTTP header 和 auth 断连；保留 2024-11-05 stdio 兼容；确保工具 side-effect/cancel/timeout 行为可观测。
3. 配对 held-out 长上下文任务，分别开/关显式缓存与主动压缩，记录真实缓存命中率、美元计费、任务通过率、上下文错误回归；无非劣性证据不开启实验选项。
4. 对接原有证据账本与工具调用日志，完成所有待办 KH-005..KH-012 分项。

## 参考协议文档（供重新核对）

- OpenAI Prompt caching: https://developers.openai.com/api/docs/guides/prompt-caching
- OpenAI Tool search（尚未接入）: https://developers.openai.com/api/docs/guides/tools-tool-search
- MCP 2026-07-28 Streamable HTTP: https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http
- MCP MRTR（本轮拒绝而非默默跳过）: https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr
