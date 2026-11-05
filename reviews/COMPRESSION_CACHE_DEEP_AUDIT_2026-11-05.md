# 压缩与缓存深度审查：证据复核修订

原审查日期：2026-11-05。本修订替代初稿结论。范围是当前工作树中的上下文压缩、协议投影、prompt cache 和诊断，不是已安装二进制的线上验收。

> **更正：初稿把源码行为、条件性协议风险和优化建议混在一起，部分结论明显说重了。不能把初稿列出的项目都称为已确认故障，更不能说它们解释了某次连续零缓存。**

## 1. 先纠正明确错误与过度结论

1. **“配置 128k，实际请求就发 128k 输出”错误。** 内置 ModelClient 的 effective_output_tokens 会先夹到 MAX_TURN_OUTPUT_TOKENS = 32,768，再受上游/catalog 限制（src/provider.rs:45、1040–1048）。正常应用构造预算也使用解析后的输出值。只有人为设置更小的 KROMA_OUTPUT_RESERVE_TOKENS 等条件，才存在预留与 wire 上限不一致的风险，不能把它当作默认配置的 bug。
2. **“刷新 Prompt 就会使每轮缓存冷启动”错误。** refresh_for_task 每个用户任务刷新一次，不是每个工具轮刷新；重读文件不等于字符串发生变化。只有指令内容、引用到的 nested AGENTS.md、贡献顺序等实际变化时，才会改变前缀。没有命中率测量，不能声称这是“最大损失来源”。改变某处前缀也不意味着它之前的全部可缓存内容必然丢失。
3. **“拒绝 cache_control 后该进程永远零缓存”说过头了。** 确认的是一个 Anthropic ModelClient 实例会记住字段被拒，并在后续请求中省略它；新建客户端/切换模型可重置，不会持久化为全局永久开关。不能据此断言网关自身也不做隐式缓存，或返回值必定为 0。
4. **“会报 400”不是本地复现结果。** 本地没有向真实 Anthropic 发请求；具体错误还受模型、thinking 模式、账户和服务商实现影响。官方文档明确列出了部分模型/账户前缀失配时不报错的情况。
5. **“诊断日志没有请求间隔证据”不准确。** cache trace 已有 time_ms，可用相邻记录计算结束记录之间的间隔；但这不等于准确的请求开始时间或服务端缓存寿命。增加 since_previous_ms 是便利性改进，不是从无到有的证据。
6. **“冷启动粗估触发压缩”不是已证明的 bug。** 测量缺失时退回保守估算是容量保护。没有对应现场数据，不能把正常保护说成错误；初稿“无测量时延迟到 hard 才压缩”的建议撤回，不应无证据放宽保护。
7. **“连续角色、assistant 首条导致 Bedrock/代理 400”没有当前端点复现。** 初稿依赖历史差异，不足以列为已确认兼容性缺陷。只作为待测场景保留，不据此改写角色序列。

## 2. 能从当前源码确认的行为

### A. 前台投影与检查点 source 的 thinking 处理不一致

- view_with_state 在 revision > 0 且 assistant 的 kroma_context_revision 不匹配时，删除其 thinking/redacted_thinking（包括 native blocks 中的块），不改 durable raw transcript：src/context.rs:897–941。
- summary_source_view 则把 covered_until 后的 raw 消息直接 clone，未复用上述清理：src/context.rs:946–954。summary_source 保留 provider provenance，随后由 payload builder 决定 replay：src/context.rs:1037–1085。
- replay_opaque_reasoning 检查 origin，不检查压缩 revision：src/provider.rs:395–404。

**确认程度：** 两条代码路径的差异确定。多次压缩时，如果第二次 source 覆盖第一次压缩后保留的旧 revision assistant，就可能重新带回前台投影已剥除的 thinking。这是值得补回归的具体风险；本次没有真实服务商 400 或缓存下降的端到端证明。

**初稿探针的限制：** 探针手工构造 revision=1、covered_until=1、summaries 为空，且 assistant content 与 native text 不一致。这不是正常首次压缩提交产生的状态，不能称为完整真实复现。它只证明了上述局部函数行为。

### B. 工具循环中的压缩可能删除最新 tool_use assistant 的 thinking

Agent 在成功 completion 返回后、把新 assistant 和工具结果加入历史之前调用 mark_consumed(self.messages.len())：src/agent.rs:1352–1355。初稿“推进到刚返回的 assistant 之后”描述错误；实际边界在它之前。

select_boundary 可以在这个边界之前压缩已确认历史，并保留最新 assistant/tool 组；compact_once 提交会增加 revision：src/context.rs:1013–1034、1185–1193。随后前台投影可能剥除保留 assistant 的旧 revision thinking。

**确认程度：** 本地删除行为和可达代码路径可以确认。**不能据此宣布所有 Anthropic 请求都会失败。** 人工/extended thinking 与 adaptive thinking 的校验规则不同；官方要求工具循环保留原始 thinking，但此处尚未对真实端点验证具体结果。初稿引用的“Expected thinking ... found tool_use”不应直接套用到所有 adaptive 模型。

修复也不能简单地“强行保留 thinking”：客户端压缩已经改了它之前的上下文，保留旧块又可能触发前缀失配。需要针对目标协议验证并同时处理这两个约束，不能据此全局关闭工具循环压缩。

### C. 只用 compaction revision 不能覆盖全部前缀变化

当前清理看 compaction revision；system/tools 实际变化、图片老化或请求级工具预览并不一定改变这个 revision。预算测量失效检查不能自动代替签名 thinking 的有效性检查。

**确认程度：** 失效检查范围可以从源码确认；线上影响仍是条件性的。Anthropic 的 preserved-thinking 文档确实把重建 system/tools、裁剪旧结果、客户端压缩列为前缀失配来源；但是否拒绝请求取决于模型和账户等条件。Responses encrypted reasoning 不能直接套用 Anthropic 的签名规则。

### D. Anthropic 缓存字段兼容回退确实存在

src/provider.rs:1501–1511 仅在 Anthropic 协议、尚未收到协议数据、错误匹配 cache_control 拒绝时设置 anthropic_cache_rejected。后续该客户端的主请求和检查点省略顶层 cache_control：1248–1250、1963–1965。

**确认程度：** 有本地 HTTP mock 测试，机制确定。它是已有的有意兼容回退，缺少块级断点回退可列为改进，但“关掉显式标记”不自动等于线上缓存故障。该分支不适用于 Responses。

### E. 诊断和 Hook 的范围有限

- cache trace 记录的是 provider_budget，不是实际 wire；缺少 model/protocol、字段回退状态、provider request id、检查点请求等信息：src/cache_trace.rs、src/agent.rs:1234–1277。这是已知观测边界，不能把 append_only 当成实际缓存键相同的证明。
- 失败 trace 只从 Ok completion 取 usage，可能遗漏错误中已有的 reported_error_usage：src/agent.rs:1271–1284。
- Usage 没有单独 cache_creation；Anthropic 已将创建量计入 input，cached 表示读取量。首次写入显示 0% 读取率是算术上正确的，不等于记账错误；展示写入量可改进理解。
- PluginModelHooks 的主请求运行 before_model_request，检查点/标题等直接转发 inner：src/plugins/model.rs:456–521、583–638。这是确定的调用范围差异。把该 Hook 当“所有模型出站请求的脱敏层”会有风险，但本次未确认用户启用了相关 Hook，也未证明数据泄露。

## 3. 关于之前记录的连续零缓存现场

已有 reviews/COMPACTION_CACHE_DIAGNOSIS_2026-11-05.md 记载，该现场选择的是 gpt61sol、Responses 协议，不是 Anthropic；本次未重新读取真实会话或抓取原始出站请求。

因此：

- **Anthropic 的 cache_control 回退和 thinking 签名规则，不能作为该 Responses 现场的已确认根因。**
- 既有记录中压缩请求曾高命中、之后前台请求为 0，只能说明两类请求的观测结果不同，不能单凭它锁定 Hook、网关路由或 TTL。
- 本地预算前缀重放不等于当时 system/tools、Hook 后 wire 和网关缓存键相同。
- **连续零命中的端到端根因仍未确定。** 缺的主要是目标协议真实出站配置/前缀、回退、时间和 provider request id 等关联证据。

## 4. 性能与改进建议的正确定位

每轮完整序列化、WAL 记录携带压缩状态、提交整份快照、检查点图片重编码等路径确实存在。但本次没有对应 workload benchmark，不能从“有这些操作”直接推断明显卡顿或严重内存问题，更不能说它们造成服务端零缓存。

TTL 配置、cache write 展示、更完整的 trace、块级断点回退都是可评估改进，不是仅凭缺少功能就成立的故障。诊断应先于调大窗口、放宽 hard 保护或重复付费预热。

## 5. 验证与尚缺的证据

本次复核重新执行且通过：

- cargo test --lib provider::tests::anthropic_cache_rejection_retries_once_and_remembers_gateway_capability -- --exact（1 passed）：模拟网关拒绝后，同一客户端后续省略字段。
- cargo test --lib context::tests::compacted_prefix_invalidates_old_signed_thinking_without_rewriting_raw_history -- --exact（1 passed）：revision 投影删块、原文不变。

这些是本地测试，不是线上错误、命中率或特定用户问题的复现。上一轮 context、cache_trace、usage 和 context_compaction_e2e 测试的通过记录也不能填补真实协议验收的缺口。

下一步若要提升确定性：用合法的会话状态完成两次压缩，捕获对应最终 payload（仅本地 mock），验证保留工具组和 stale thinking；然后在目标服务商的明确模型/thinking 模式下验收。缓存现场应针对 Responses 取证，不要混用 Anthropic 推论。

本次只更正审查文档，没有修改生产代码、配置或真实会话，也没有付费 provider 请求。

## 6. 本次直接核读的官方资料

- Preserved thinking（含默认前缀失配行为按模型/账户区分）：https://platform.claude.com/docs/en/build-with-claude/preserved-thinking
- Thinking 故障排查（区分修改块、签名失配等错误）：https://platform.claude.com/docs/en/build-with-claude/thinking-troubleshooting
- Thinking 配置与模式：https://platform.claude.com/docs/en/build-with-claude/thinking
- Prompt caching（顶层/块级标记与 TTL）：https://platform.claude.com/docs/en/build-with-claude/prompt-caching
