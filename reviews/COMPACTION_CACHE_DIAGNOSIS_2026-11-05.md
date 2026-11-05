# 提前压缩与连续零缓存命中：分开诊断

日期：2026-11-05。结论状态：第 1–6 节保留首轮排查结论；后续已修正提前压缩/最终准入的计数冲突，见第 7 节。截图中连续零缓存的端到端根因仍未确认。

## 1. 和截图对应的事实

- 当前会话本地保存的最后一条前台用量：输入 46,855、输出 838、cached=0，与截图最后一条一致。不是仅凭 UI 推测。
- 会话模型选择为 gpt61sol，thinking=xhigh；该 profile 配置 context_window=500,000、max_output_tokens=128,000、Responses 协议。全局 active profile 不是会话选择，不能混用。
- 本地累计记录里，最近一次压缩请求 input=258,458、cached=258,048，约 99.84% 命中。因此该压缩请求本身并非完全没有复用旧前缀。
- 截图对应的后续保存历史处于同一个 checkpoint revision=5，covered_until=564；没有证据表明这些请求之间反复压缩。
- 对这段保存历史按 Responses 的消息/工具/带来源标记 reasoning item 顺序做只读重放：检查到的旧 input items 均保持原样，后续只追加。没有输出正文或加密内容。
- 重放不是原始出站抓包：未捕获当时的 system/tools、动态 hook 最终修改、全部协议字段及网关内部键，不能据此排除所有客户端因素或认定服务商有错。

## 2. 为什么看到约 200k 时，有时压缩、有时不压缩

[src/context.rs](../src/context.rs#L1233) 修复前同时使用两条触发线：

1. 输入水位达到 soft limit。前缀仍匹配时，可以用上次服务端 input 加新追加内容的保守估算；前缀不匹配或没有测量时，退回本地估算。
2. 完整 JSON 的本地估算达到 hard limit，即使输入水位尚低也触发。

第二条不是服务端真正已经用了那么多 token。当前通用估算按序列化 JSON 字节除以 3，加固定开销和相关附件预算。工具结构、转义、Responses 的加密 reasoning 等都可能使字节估算与服务端计数相差较大。

按当前源码、500k 共享窗口、32,768 输出预留且没有更低独立输入上限时，soft=450,000、hard=462,232。实际启动配置、服务端/catalog 上限和输出预留不同，结果也会不同；不能只看 profile 写了 500k 就当生效值一定相同。

这能解释为什么相近的服务端 input 不一定作出相同压缩决定。但缺少截图前那次的触发日志，不能声称已还原其所有判断输入。

本地现有回归直接复现了该行为：输入水位 6,144，soft=12,288，但 JSON 估算 13,876 超过 hard=13,312，仍然压缩。该测试不是实际模型 200k 的运行复现。

## 3. 为什么压缩后缓存为零不能一概说正常

压缩改变旧历史前缀，紧随其后的请求出现冷缓存并不意外；但不能保证下一条必然高命中，更不能拿这个解释无限多次零命中。

本次连续零命中还需区分：真实请求的前缀/参数是否变化、网关转换和路由、缓存保留/驱逐，以及服务端的命中报告。截图不能区分这些原因。HTTP 200/SSE 也不保证请求一定命中缓存。

当前会话没有启用 cache trace。发现的同工作区历史 trace 属于 10 月 30 日，不是截图这次：其中也有 append_only 后 cached=0、再恢复命中的记录。这只能说明“预算投影不变仍可能零命中”曾发生，不能拿旧记录替这次定因。

已有 [cache trace](../src/cache_trace.rs#L131) 由 KROMA_CACHE_TRACE=1 显式启用，仅记录计数、角色和指纹，不写正文或密钥；但它明确标为 provider_budget，不是网关原始 wire。需要复现时应保留这一证据边界，不能把预算指纹相同当成网关缓存键必然相同。

## 4. 首轮仅补诊断（后续实修见第 7 节）

只在自动压缩尝试前增加一条诊断日志：input_watermark、soft_limit、local_json_estimate、hard_limit、model_window、output_reserve。以后能看清是输入水位还是本地估算先触线。

没有更改压缩阈值、token 估算算法、输出预留、重试、并发或缓存参数；没有关闭硬保护，也没有为“预热”重复发送请求。这是补齐诊断，不是宣称连续零命中已经修好。

运行进程路径为用户 cargo/bin 下的 kroma.exe，而非仓库 target/release 下的构建；检查时两者文件修改时间和哈希不同。源码修改不会自动改变已运行进程。未替换安装文件、更新配置或重启用户进程。

## 5. 首轮验证

- rustfmt --edition 2021 --check src/context.rs：通过。
- cargo test --lib context::tests：40 通过、2 个本地性能测试按原声明忽略；包含匹配 plugins::context::tests 的 1 项。
- 硬估算优先触发的具名测试另用 --exact --nocapture 通过，并看到新增日志中的两组水位/阈值。
- provider::tests::checkpoint_payload_preserves_the_cached_prefix_and_appends_one_instruction：通过，覆盖源码中 checkpoint payload 前缀契约，不是线上命中率测试。
- cache_trace::tests：3 通过。
- git diff --check -- src/context.rs：通过。
- 测试均为本地/模拟验证，没有向真实服务商发起缓存探测；未安装或更新运行版本。

## 6. 尚需的证据

在用户原启动入口显式开启指纹日志后复现；同时保留压缩触发行、零命中前后请求的时间/服务端请求 ID，以及“更多指标”中如果存在的上游路由信息。不要分享 API key、Authorization、原文或 encrypted_content。

先定位是哪一层变动再修改。单纯调大窗口、移除保护、强加 prompt_cache_key、反复重发预热，都不能算已经证明的修复。

## 7. 后续实际修复：统一压缩、裁剪和最终准入的用量依据

### 修改

- 已有成功请求的服务端 input 且前缀、模型、工具、提示和附件预算匹配时，统一使用“服务端 input + 新增消息保守估算（保留 25% 安全余量）”。完整 JSON 的粗估算不再单独抢先触发压缩，也不会在最终发送前重新否决同一个已校准请求。
- 预算内的匹配前缀不再因 JSON 总字节数过大而提前裁剪工具结果；真正需要容量保护时，裁剪路径也使用同一有效水位。
- 历史/提示/工具/模型/额外预算变化、服务端用量缺失或 input=0 时，回到原保守估算。最终系统消息不匹配也不能复用测量。仍检查原硬上限；没有调高窗口、修改压缩百分比、放弃输出预留或更改重试。
- 未改写请求的模型 Hook 不再重复用粗估算拒绝 Agent 已校准的请求；实际改写了消息或工具的 Hook 仍独立执行原保守检查，改写后用量不能校准原请求。
- 外部 context policy 默认不提供校准值，保持原保守准入。没有重写 Agent Loop 或改变工具执行顺序。

### 复现与回归

先让回归在旧代码上失败：单元测试确认匹配测量仍被 JSON 硬门触发压缩；端到端模拟确认它还会导致任务最终报上下文超限。

修复后相关验证共 79 项通过，2 项原有性能测试忽略：

- context::tests：42 通过、2 忽略；包括过早压缩、最终硬保护、7 类测量失效、真实增长触发和 500k 边界。
- context_compaction_e2e：7 通过。新增用例持续报告 cached=0，在前缀有效的情况下跨过 JSON 粗估上限仍完成全部任务、不压缩；同时覆盖无 Hook 与无操作 Hook 两条路径。
- plugins_e2e 的相关模型 Hook 测试：3 通过；覆盖无操作包装的契约、改写消息/工具以及超大改写被拒绝。
- architecture_backends 8、architecture_repair_regressions 8、lifecycle_contract 4、model_protocol_mock_e2e 4、runtime_customization 3：全部通过。
- 5 个修改的 Rust 文件 rustfmt 检查及 git diff --check：通过。

以上为本地/模拟测试，不是对截图服务商缓存命中的线上验收。没有增加缓存预热请求，没有声称连续零命中已修复。

修改文件：src/context.rs、src/agent.rs、src/plugins/model.rs、tests/context_compaction_e2e.rs、tests/plugins_e2e.rs。原有脏工作树已保留，改动前副本在忽略的 target/compaction-calibration-fix-2026-11-05/baseline/，用于区分本次增量。

源码/新构建不会热更新当前进程；未替换用户安装路径、重启进程或修改真实会话/配置。
