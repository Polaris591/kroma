# T03 · 建立宿主资源上限与模型窗口感知 — 验收

阶段 G1｜依赖 T01｜关联 KR-03、KR-07｜提交见 git log `feat(t03)`

## 一、改了什么

1. **`budget.rs`：能力与上限的类型化表达**
   - `ModelCapabilities::resolve(configured_window, configured_max_output)` → `CapabilitySource`
     （`Configured` / `ConservativeFallback` / `Unbounded`）。**不内置任何模型昵称表**：模型名不是规格，
     只有操作者显式配置的窗口才算数。
   - `CONSERVATIVE_CONTEXT_WINDOW_TOKENS = 32_768` 作为未知模型的保守值，并带来源记录。
   - `reserved_output_tokens(cap)`：输出预留取 `min(默认预留, provider 上限)`，不会把窗口全吃掉。
   - `conservative_text_tokens`：ASCII 4 字节/token，非 ASCII 2 字节/token（CJK 每字 1 token 的保守上界）。
     **文档和代码都明确这不是 tokenizer**，只是保守上界。
   - `conservative_bytes_tokens`：只有字节数没有文本时用 3 字节/token（ASCII 高估 1/3，CJK 刚好）。
   - `HostBudgetCeiling` + `CeilingOutcome<T>`：`clamp_tokens` / `clamp_subagents` / `clamp_window`，
     返回值同时给出「生效值」和「是否被夹」。
   - `conservative_request_tokens(system, messages, tool_schema_bytes, overhead)`：一次完整请求的估算，
     包含 system、工具 schema、历史、图像负载与序列化开销。

2. **`config_file.rs`：工作区不能抬高宿主上限，超限可见**
   - `max_tokens` / `max_subagents` 变成**天花板语义**：
     - 显式调用层（CLI、`KROMA_*`）是操作者此刻的决定，工作区既不能抬高也不能悄悄压低；
     - 用户全局文件是环境默认值，工作区可以往下压（只会更省），往上抬则被夹到宿主值；
     - 被夹时写进 `clamped_workspace_keys`，`redacted_summary()` 里显示 `clamped:` 行。
   - 新增 `allowed_models`（operator-only，加入 `SECURITY_SENSITIVE_KEYS`，工作区写了会被 `rejected_workspace_keys` 记录）：
     工作区请求的模型不在白名单内 → 记入 `refused_workspace_keys` 并回落到操作者模型；
     `ResolvedConfig::load()` 遇到非空 `refused_workspace_keys` **直接返回错误**（静默换模型比拒绝更糟）。
   - `redacted_summary()` 新增 `allowed_models:` / `clamped:` / `refused:` 三行，配置来源可见。

3. **`agent_loop.rs`：发送前检查整个请求**
   - `AgentRunner` 解析并持有 `model_capabilities`。
   - `run_provider_turn` 在**任何传输之前**用 `conservative_request_tokens` 估算 system + 工具 schema +
     历史 + 图像 + 序列化开销，加上输出预留，与窗口比较；超了就不发请求，记录
     `EventPayload::ContextBudgetExceeded`（含 `estimated_input_tokens` / `reserved_output_tokens` /
     `window_tokens` / `capability_source` / `tool_schema_bytes` / `message_count`）并以
     `TerminalReason::ContextBudgetExceeded` 结束。
   - **token 压力信号**：provider 还没报 usage 时（首轮、`--resume` 无历史 usage），把
     `conservative_bytes_tokens(static_prefix + 规范历史 + 序列化开销)` 当作 `last_input_tokens` 传给投影器，
     这样小窗口会在第一次请求之前就触发压缩，而不是先发一个超窗请求再发现。
     provider 报过 usage 之后以实测为准（实测用于校准，不替代发送前估算）。
   - `canonical_history_bytes()`：用**规范历史**而不是「已投影项」求和。`--resume` 时投影器还没吸收
     恢复出来的 transcript，用已投影项会算成 0，正是这个坑让第一版实现把 735 KB 的请求原样发了出去
     （见「三、踩到的坑」）。

4. **`context_projector.rs` / `messages.rs` / `events.rs`**：`projected_bytes()`、`TranscriptItem::estimated_tokens()`、
   `EventPayload::ContextBudgetExceeded`。

## 二、验收证据

| 门禁 | 命令 | 结果 |
| --- | --- | --- |
| 编译 | `cargo check --workspace --all-targets` | 0 error |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| 定向测试 | `cargo test -p kroma-core --test t03_context_budget` | 12 passed / 0 failed |
| 全量 | `cargo test --workspace --no-fail-fast` | **844 passed / 0 failed**（`logs/test_t03.log`） |

新增测试（`crates/kroma-core/tests/t03_context_budget.rs`，12 条）：

- `workspace_cannot_raise_host_token_ceiling`：工作区压低 → 生效；抬高 → 夹到 40000 且记录；
  无宿主值时工作区值独立成立。
- `workspace_cannot_raise_host_concurrency`：`max_subagents` 同上。
- `unknown_model_falls_back_conservatively_and_records_the_source`：未知模型 → 32768 +
  `conservative_fallback`；配置过的 → `configured`；`0` → `unbounded`；输出预留不超过 provider 上限。
- `cjk_and_image_budget_is_conservative`：CJK 估算**严格大于** `bytes/4`，且 ≥ 每字 1 token；
  ASCII 仍按 4 字节/token；带图像负载的请求比纯文本请求多算 ≥ 900 token。
- `task_contract_and_tool_schema_count_toward_context`：40 KB 工具 schema 约 +10k token；序列化开销计入。
- `resume_and_switch_to_smaller_window_rebudgets`：200k → 16k 窗口后水位线按新窗口重算（12k / 7.2k）。
- `host_ceiling_clamps_and_reports`：夹取算术本身。
- `a_request_that_cannot_fit_the_window_is_never_sent`：真实 `AgentRunner` + 计数 provider，
  窗口 600 token → 终止原因 `ContextBudgetExceeded`、**provider 调用次数 0**、日志含结构化事件与来源。
- `small_context_compacts_before_first_overflow_request`：先跑一轮建立 726 KB 历史（宽水位线），
  再 `--resume` 用 200k token 窗口跑：第一版（无压力信号）**原样发出 735 KB 请求**；
  修好后第一请求 184k token ≤ 窗口，日志含 `ContextCompacted`（735196 → 245234 字节）。
- `workspace_model_outside_allowlist_is_refused`：白名单外的模型被拒且回落到操作者模型、记录可见；
  白名单内正常；未配白名单不拒；工作区**不能**自己声明白名单（进 `rejected_workspace_keys`）。
- `live_reconfiguration_does_not_expand_existing_task_grant`：审批通过后把上限从 1000/4 放宽到 5000/32，
  已批准对象的预算不变、新意图 digest 不同、且不被旧 grant 覆盖。
- `config_source_display_shows_the_ceiling_and_every_clamp`：`redacted_summary()` 显示
  `allowed_models:` / `clamped:` / `refused:`。

### 变异验证（证明测试不是空转）

- 把 `run_provider_turn` 里的窗口检查改成 `if let Some(window) = None::<u64>`：
  `a_request_that_cannot_fit_the_window_is_never_sent` 立刻红（`left: ModelStop, right: ContextBudgetExceeded`）。
- 去掉压力信号（只用实测 usage）：`small_context_compacts_before_first_overflow_request` 红——
  735 KB 请求照发，日志里没有 `ContextCompacted`。

## 三、踩到的坑（都改掉了，记在这里避免下次重犯）

1. **把保守窗口硬塞进预算会打断子代理。** 第一版在 `AgentRunner::new` 里把 32 768 的保守窗口写进
   `config.budget.model_context_window_tokens`，结果 gate5 的 6 条子代理测试全红——子代理第一轮就被判超窗。
   现在的语义：保守值只作为**报告的来源**，只有操作者显式配置的窗口才做硬拒绝；未配置窗口的运行行为不变。
2. **`workspace.model` 原来能压过一切。** 白名单之外的工作区模型必须被拒，而不是"工作区优先"。
3. **`max_tokens` 的天花板语义与既有测试冲突。** `budget_contract` 里 `env outranks workspace` 原本是
   「工作区 222 覆盖 env 333」。改成「显式层胜出、工作区只能在用户文件默认值上往下压」后，
   该测试的注释同步改写（**不是**为了让测试变绿而放宽断言：同一文件里另加了两条夹取/拒绝断言）。
4. **压力信号必须用规范历史。** 用 `projector.projected_bytes()` 在 `--resume` 首轮恒为 0，
   于是小窗口下什么都没触发。这个 bug 只有在真实 resume 场景里才暴露，单测写不到。

## 四、边界与未做

- **不做**模型昵称 → 规格的硬编码表；未知模型只记录来源。
- **不声称** `bytes/4` 或 3 字节/token 是精确 tokenizer，两者都只是保守上界，事件里写明来源。
- 保守 fallback（32 768）**不触发硬拒绝**：Kroma 自己的 system prompt + 工具 schema 就可能超过它，
  用它拒绝会打断正常运行。硬拒绝只对操作者显式配置的窗口生效。
- 图像负载按 base64 字节计入估算，未做尺寸/分辨率换算（后续 T14 缓存账目里再统一）。
