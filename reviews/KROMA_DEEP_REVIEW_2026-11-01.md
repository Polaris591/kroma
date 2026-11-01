# Kroma 全量源码深度审查报告（2026-11-01）

## 审查范围与方法

本次审查针对 `kroma v0.4.0`（Commit `9b125dc`，约 5.5 万行 Rust 核心、TUI、插件与 Tauri/Svelte 桌面端代码）进行全量源码级静态与逻辑走查，覆盖：

- **Agent Runtime & Session Persistence**：`src/agent.rs`、`src/app.rs`、`src/session.rs`、`src/sessions.rs`、`src/catalog_file.rs`、`src/atomic_file.rs`、`src/private_file.rs`、`src/steering.rs`
- **Context & Memory Management**：`src/context.rs`、`src/context_state.rs`、`src/history_tools.rs`、`src/prompt.rs`
- **Model & Multi-Protocol Providers**：`src/model.rs`、`src/protocol.rs`、`src/provider.rs`、`src/provider/anthropic.rs`、`src/provider/responses.rs`、`src/model_catalog.rs`、`src/usage.rs`、`src/cache_trace.rs`
- **Tools, Shell & OS Isolation**：`src/tools.rs`、`src/shell.rs`、`src/shell_redirect.rs`、`src/winsys.rs`、`src/attachments.rs`、`src/host_attachments.rs`、`src/convenience.rs`、`src/skills.rs`
- **MCP & Process Plugin System**：`src/mcp.rs`、`src/plugins.rs`、`src/plugins/context.rs`、`src/plugins/model.rs`、`src/plugins/stream.rs`、`examples/plugins/web/*`
- **Hosts (CLI, Ratatui TUI & Tauri Desktop)**：`src/main.rs`、`src/commands.rs`、`src/diagnostics_bundle.rs`、`src/tui/*`、`src/desktop_host.rs`、`desktop/src/*`、`desktop/src-tauri/src/*`

---

## 一、 总体架构评估

### 核心优势与工程亮点

1. **单一运行时内核（Single Agent Loop）**
   CLI、Ratatui TUI 与 Tauri Desktop 共享同一个 `Agent` 与 `RuntimeComponents` 组合入口（`src/app.rs`），没有维护第二套循环或状态机。
2. **高可靠会话持久化（Snapshot + Append WAL + Poison 熔断）**
   `src/session.rs` 使用 `generation` + FNV-1a 校验和的增量 `.wal` 日志，任何写入或 fsync 失败都会将 `JournalCursor.poisoned` 置位，拒绝在半写入状态下继续请求模型或执行工具；崩溃恢复时通过 `recover_pending_calls` 将中断的工具调用标记为 `"unknown"`，绝不自动重放可能有副作用的 Shell 命令。
3. **非破坏式上下文压缩（Non-destructive Single-Pass Compaction）**
   `src/context.rs` 保留完整原始对话与 `.wal`，仅在请求投影层（Projection）替换已消费的前缀，并配合 `history_search` / `history_read`（`src/history_tools.rs`）允许模型按需回查原始证据。
4. **跨平台进程树隔离**
   Unix 使用独立进程组 `process_group(0)` + `kill(-pgid, SIGKILL)`（`src/shell.rs`），Windows 使用 `CREATE_SUSPENDED` + `JobObject`（`src/winsys.rs`），消除了挂起前子进程逃逸的时间窗。
5. **多模态附件惰性投影与缓存**
   `src/attachments.rs` 在 WAL 中只存文件引用和私有区 Unicode 标记（`U+E000..U+F8FF`），仅在当前及上一轮用户回合按需 Base64 编码，并在同一任务的多轮工具调用间通过 `TaskImageCache` 复用编码结果。

---

## 二、 核心缺陷与高风险问题详单（按优先级排序）

### 【P0 · 严重】1. 上下文压缩与 Steering 破坏 Anthropic Messages API 协议不变量，导致压缩后请求必现 HTTP 400

- **涉及文件与行号**：
  - `src/context.rs:903-915` (`Compression::push_summary_messages`)
  - `src/context.rs:917-935` (`Compression::is_safe_boundary`)
  - `src/provider/anthropic.rs:48-129` (`payload_with_cache_at`)
  - `src/agent.rs:820-839` (`Agent::append_pending_steering`)
  - `src/provider.rs:1505-1507` (`ModelClient::checkpoint_completion`)
- **根因分析**：
  1. **压缩后首条非系统消息角色为 `assistant`**：
     `push_summary_messages` 将压缩摘要构造为 `Message::new("assistant", "Compacted conversation checkpoint S1 ...")` 插入在 `view[0]`（`system`）之后。而在 `src/provider/anthropic.rs:58-129` 中，`system` 消息被提取到顶层 `"system"` 字段，`view[1]`（即 `role: "assistant"` 的压缩摘要）直接成为 `messages[0]`。Anthropic `/v1/messages` 接口强制要求 `messages[0].role == "user"`，否则直接返回 `HTTP 400 invalid_request_error: messages: first message must use the "user" role`。**这意味着在 `anthropic` 协议下，一旦触发一次自动压缩或手动 `/compact`，该会话后续所有模型请求都会 100% 失败。**
  2. **`is_safe_boundary` 允许在 `user` 或 `tool` 消息后切分**：
     ```rust
     // src/context.rs:917-935
     fn is_safe_boundary(messages: &[Message], boundary: usize) -> bool {
         if boundary == 0 || boundary > messages.len() {
             return false;
         }
         let previous = &messages[boundary - 1];
         if previous.role == "assistant"
             && previous
                 .tool_calls
                 .as_ref()
                 .is_some_and(|calls| !calls.is_empty())
         {
             return false;
         }
         if previous.role == "tool" && boundary < messages.len() && messages[boundary].role == "tool"
         {
             return false;
         }
         true
     }
     ```
     该函数只排除了“带 `tool_calls` 的 `assistant` 后”和“两个并行 `tool` 之间”，却**允许在 `previous.role == "user"` 或 `previous.role == "tool"`（后接多步工具循环中的下一个 `assistant`）处切分**。一旦切在这些位置：
     - `messages[boundary]` 是 `role == "assistant"`，紧跟在同样是 `role == "assistant"` 的压缩摘要之后，产生连续两条 `assistant` 消息；
     - 若切在 `user` 后，触发该轮回复的 `user` 提示词被吞入摘要，保留后缀以孤立的 `assistant`（甚至带 `tool_calls`）开头。
  3. **`tool` 后接 `user` 在 Anthropic 下产生连续 `user` 消息**：
     在 `anthropic.rs:116-127` 中，`role == "tool"` 被编码为 `{"role": "user", "content": [{"type": "tool_result", ...}]}`。当用户在工具执行期间发送 `/steer`（`src/agent.rs:820` 在 `tool` 消息后追加 `role == "user"` 的 steering 消息），或 `checkpoint_completion`（`src/provider.rs:1507`）在 `source` 末尾追加 `Message::new("user", instruction)` 时，`anthropic.rs:62` 对 `"user"` 分支总是直接 `items.push({"role": "user", ...})`，没有与前一条 `"role": "user"` 合并，违反 Anthropic 角色严格交替约束。
- **修复方案**：
  - 将 `is_safe_boundary` 收紧为仅允许在**无未完成工具调用的完整回合边界**切分（即 `previous.role == "assistant"` 且无 `tool_calls`，且 `boundary == messages.len() || messages[boundary].role == "user"`）；
  - 在 `anthropic::payload_with_cache_at` 中确保相邻同 `role` 的 `items` 统一合并为单个 `content` 数组，并保证 `items[0]` 的 `role` 必为 `"user"`（或将压缩摘要投影为用户侧上下文块）。

---

### 【P1 · 高危】2. Checkpoint 压缩请求首轮 `max_tokens` 与窗口准入预算不一致，导致临近窗口上限时压缩请求自身触发 Context Overflow

- **涉及文件与行号**：
  - `src/context.rs:985-994` (`Compression::summary_source`)
  - `src/provider.rs:1511-1536` (`ModelClient::checkpoint_completion`)
  - `src/provider.rs:1388` (`ModelClient::text_generation_with_budget`)
- **根因分析**：
  - `Compression::summary_source` 在计算待压缩前缀 `view` 能否放进模型上下文窗口时，预留的输出空间是：
    ```rust
    let output = self
        .budget
        .summary_tokens()
        .saturating_mul(2)
        .saturating_add(512);
    if tokens.saturating_add(output) <= self.budget.window() {
        return Some((boundary, view, tokens));
    }
    ```
    例如在 `128,000` 窗口下，`summary_tokens()` 为 `4,096`，`summary_source` 仅预留 `8,704` tokens（即允许 `source_tokens` 高达 `119,296`）。
  - 然而在 `ModelClient::checkpoint_completion`（`src/provider.rs:1511-1532`）中，为了复用前景请求的 Prompt Cache Key，第 1 次尝试（`attempt == 0`）发送的 `max_tokens` 是 `cache_max_tokens = self.effective_output_tokens()`（最高可达 `MAX_TURN_OUTPUT_TOKENS = 32,768`）。
  - 当会话达到 `soft_limit`（例如 128k 窗口的 90% = `115,200` tokens）触发压缩，且 `source_tokens` 在 `95,233..=119,296` 之间时，`summary_source` 判定合法，但 `checkpoint_completion` 实际发出的请求是 `input(~105k) + max_tokens(32,768) > 128,000`。
  - 上游直接返回 HTTP 400 `context_length_exceeded`，而 `text_generation_with_budget` 只对 `Truncated` 和 `is_timeout` 重试，遇到 HTTP 400 直接报错并将 `failed_this_turn = true`，导致本轮压缩彻底失败。
  - 此外，`text_generation_with_budget`（`src/provider.rs:1388`）写的是 `let max_tokens = self.output_limit.map_or(budget, |limit| budget.min(limit));`，遗漏了 `.or(self.catalog_output_limit)`（对比 `effective_output_tokens` 第 890 行）。
- **修复方案**：
  - `summary_source` 预留的 `output` 需覆盖 `checkpoint_completion` 首轮发送的 `max_tokens`（或当 `source_tokens + cache_max_tokens > window` 时自动回退到 `retry_budget`）；
  - `text_generation_with_budget` 中使用 `self.output_limit.or(self.catalog_output_limit)`。

---

### 【P1 · 高危】3. 插件子系统安全边界缺失与运行时阻塞问题

- **涉及文件与行号**：
  - `src/plugins.rs:437-459`、`777-788`、`1966`、`2065`、`2225`、`2320-2332`、`2388-2415`、`2482-2488`
  - `src/plugins/model.rs:146`、`308`、`413`
  - `src/agent.rs:647`、`716`
- **根因分析**：
  1. **插件子进程完整继承宿主敏感环境变量（`src/plugins.rs:2320-2332`）**：
     与 `src/mcp.rs:97`（启动子进程前调用 `filter_child_environment` 剥离 `KROMA_API_KEY`、`OPENAI_API_KEY`、`AWS_*` 等宿主密钥）不同，`PluginWorker` 启动插件进程时**未做任何环境变量清理**。更危险的是，`kroma plugins check <dir>`（用于检验未安装的第三方插件目录）也会直接拉起插件进程并把宿主全部凭据暴露给被检插件。
  2. **项目级插件（`<workdir>/.kroma/plugins`）缺乏内容指纹绑定（`src/plugins.rs:777-788`）**：
     `.kroma/mcp.json` 将信任绑定到了文件 SHA-256 指纹（修改即失效），但 `plugins.toml` 中的 `enabled_projects` 仅记录 `<canonical_path>::<plugin_id>`。一旦用户启用过某项目的本地插件，后续 `git pull` 或切换分支篡改了 `.kroma/plugins/<id>/main.js` 或 `plugin.toml`，下次启动 Kroma 时会无提示直接执行新代码。
  3. **非流式插件工具调用实际无超时（`src/plugins.rs:1966, 2225`）**：
     `reviews/KROMA_REVIEW_PLAN_2026-10-31.md` 声称“单次工具调用 120 秒超时”，但源码中 `PluginToolAdapter::execute` 调用的是 `worker.invoke_async_tool(payload)`，其内部固定传入 `timeout = None`。若插件工具死锁，任务将无限期挂起。
  4. **在 Tokio 异步线程的 `Drop` 中同步 `sleep(600ms)` / 同步执行 `taskkill.exe`，且重复杀进程两次（`src/plugins.rs:2388-2415, 2482-2488`）**：
     - 当 `invoke_async_with_timeout` 因超时或用户 `Ctrl+C` 被取消时，`PluginCallGuard::drop` 在 **Tokio 异步工作线程（或 TUI 主事件循环线程）** 上同步调用 `terminate_plugin(pid)`：在 Unix 上直接 `std::thread::sleep(Duration::from_millis(600))` 阻塞异步执行器 600ms；在 Windows 上同步 `.status()` 等待 `taskkill.exe` 进程退出。
     - 紧接着，`spawn_blocking` 中的 `read_frame()` 因进程被杀返回 `Err`，触发 `*process = None`，在析构 `PluginProcess::drop` 时**第二次**对同一个已死 PID 调用 `terminate_plugin(self.child.id())`，再阻塞 600ms / 再跑一次 `taskkill.exe`。
  5. **`compaction_started` 事件无法被任何插件订阅（`src/plugins.rs:437-459` vs `2065`）**：
     `plugin_event_name` 将 `RuntimeEvent::CompactionStarted` 映射为 `"compaction_started"`，但 `PluginManifest::validate` 的白名单（437 行）漏掉了 `"compaction_started"`，任何在 `plugin.toml` 中声明 `events = ["compaction_started"]` 的插件都会因校验失败无法加载。
  6. **`before_model_request` Hook 破坏压缩水位线校准（`src/agent.rs:647, 716` vs `src/plugins/model.rs:413`）**：
     `Agent::run_inner` 在调用 `model.complete_with_task_images` 后，将 **Hook 执行前** 的 `budget_view` 与 **Hook 修改消息/工具后** 模型返回的 `completion.usage.input` 一起传给 `self.compression.observe_with_bytes`，导致 `Compression::measured` 的增量估算基准失真。同时 `ProcessModelBackend` 未调用 `attachments::content_with_cache`，导致多模态附件和 `\u{e000}` 标记原样暴露给外部模型插件。
- **修复方案**：
  - 将 `mcp::filter_child_environment` 提取为公共模块，在 `PluginWorker` 生成子进程时默认清理宿主密钥；
  - 将项目级插件的启用状态绑定到 `PluginWorker.entrypoints` 的 SHA-256 指纹；
  - 为插件工具支持可配置超时（默认上限 + `plugin.toml` 声明覆盖）；
  - `PluginCallGuard::drop` 通过后台线程异步执行 `terminate_plugin`，并在 `PluginProcess` 中记录 `terminated: bool` 避免二次 `terminate_plugin`；
  - 在 `PluginManifest::validate` 的事件列表补齐 `"compaction_started"`。

---

### 【P1 · 中高危】4. Shell 工具执行与 Windows 平台边界问题

- **涉及文件与行号**：
  - `src/shell.rs:878-905`、`src/tools.rs:321`
  - `src/winsys.rs:58-60`
  - `src/config.rs:41, 106`、`src/diagnostics_bundle.rs:286-298`、`src/prompt.rs:193`、`src/convenience.rs:40`
- **根因分析**：
  1. **`shell` 工具无超时控制（`src/shell.rs:878-900`）**：
     `ShellArgs` 仅有 `command: String`，无可选 `timeout_ms`/`timeout` 参数，也无默认上限。在 `--print`、`--json`、Subagent 或自动化评测中，只要模型执行了阻塞命令（如启动未后台化的服务、交互式命令或死锁测试），整个 Kroma 进程将永久挂起。
  2. **Windows 与 Unix 对后台子进程生命周期语义不一致（`src/winsys.rs:58-60` vs `src/shell.rs:902`）**：
     `ChildTreeGuard::spawn` 给 Job Object 设置了 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`。当命令正常结束时，虽然 `guard.disarm()` 让 `Drop` 不主动调用 `TerminateJobObject`，但 `guard` 离开作用域时会关闭 `OwnedHandle`（Job Object 的最后一个句柄），Windows 内核仍会因 `KILL_ON_JOB_CLOSE` **强制杀死该命令派生的所有后台进程**；而 Unix 下 `ProcessGroupGuard::disarm()` 则保留后台进程存活（但后台进程仍持有 `stdout_file`/`stderr_file` 句柄）。
  3. **Windows `\\?\` Verbatim 路径导致 `diagnostics_bundle` 脱敏失效及 Git Bash 路径异常**：
     - `Config::from_env_for_workdir` 对 `workdir` 调用了 `.canonicalize()`，在 Windows 下始终返回 `\\?\C:\Users\<username>\...`。
     - `src/diagnostics_bundle.rs:286` 的 `redact_path` 使用 `path.strip_prefix(home)`，其中 `USERPROFILE` 是 `C:\Users\<username>`（不含 `\\?\`）。在 Rust `std::path::Path` 中，`\\?\C:\...` 与 `C:\...` 的 `PrefixComponent` 不同，`strip_prefix` **永远返回 `Err`**。导致 `kroma diagnostics` 生成的 `summary.json` 在 Windows 下**完全未脱敏用户的家目录与用户名**，违背了生成包内 `README.txt` 的隐私承诺。
     - 同样，虽然 TUI 在 `src/tui/view.rs:582` 专门写了 `strip_verbatim_path` 剥离 `\\?\`，但 `src/prompt.rs:193`（系统提示词中的工作目录）和 `src/convenience.rs:40`（`@file` 展开后的绝对路径）未剥离 `\\?\`，在 Windows 默认使用 Git Bash（`Dialect::Bash`）时，模型直接引用该路径会导致 Bash 工具链把 `\` 当作转义符报错。
- **修复方案**：
  - 为 `BuiltinShell` 增加可选 `timeout_ms` 参数与可配置默认超时；
  - 提取全局 `strip_verbatim_path` 辅助函数，在 `Config` 规范化 `workdir`、`convenience::file_reference` 和 `diagnostics_bundle::redact_path` 中统一归一化路径。

---

### 【P2 · 中危】5. Desktop 宿主、TUI 与会话管理边界缺陷

1. **Desktop 点击“加载更早的完整历史”界面无反应（`desktop/src/App.svelte:38, 65-77, 267-274`）**
   - `history` 为 `[...olderMessages, ...(snapshot?.messages ?? [])].filter(m => m.role !== 'system')`，但页面实际渲染的是 `visibleHistory = history.slice(-historyCount)`（初始 `historyCount = 150`，而 `snapshot.messages` 最多有 `1000` 条）。
   - 当会话超过 1000 条消息时，顶部同时显示“加载更早的完整历史”（调用 `loadOlderHistory` 从后端拉取更早的 200 条放到 `olderMessages` 头部）和“显示更早的 150 条消息”。由于 `loadOlderHistory()` 没有增加 `historyCount`，`history.slice(-150)` 依然只截取最末尾 150 条消息，用户点击后画面没有任何变化。
2. **Desktop 插件设置界面将所有未配置的密钥字段误标为“已设置”（`src/plugins.rs:881`, `desktop/src/components/PluginsView.svelte:14, 61`）**
   - `PluginCatalog::summaries()` 把所有键名匹配 `is_secret_setting` 的字段都放入 `secret_settings`，即使其值为空字符串 `""`（未配置）。
   - `PluginsView.svelte` 对所有 `secret_settings` 一律显示 placeholder `"已设置；留空保持不变"`，导致用户打开 `web` 插件时看到 6 个未配置的搜索引擎 API Key 全部显示“已设置”；且因前端拦截了 `value === ''`，一旦设置了密钥后无法在 Desktop UI 中清空重置。
3. **Desktop `connect` 异步重连期间存在竞态条件（`desktop/src-tauri/src/lib.rs:235-286`）**
   - `connect` 在 `await app::default_runtime_with(...)`（最长耗时可达 15s MCP 连接 + 4s 模型探测）期间释放了 `Host` 锁，且未标记 `Host` 为 busy（`require_idle` 仅检查 `host.cancel.is_some()`）。在此期间用户或前端可并发触发第二次 `connect`、`switch_session` 或 `send_message`。
4. **Desktop Windows 外部链接打开方式不一致（`desktop/src-tauri/src/lib.rs:514-534`）**
   - `validate_external_url` 仅检查了 `starts_with("http://")` 和非控制字符，未做 URL 解析校验，且在 Windows 下调用 `rundll32 url.dll,FileProtocolHandler`（对含引号/空格或特殊后缀的 URL 存在历史安全缺陷），应与 `src/tui/links.rs:202` 统一，使用 `reqwest::Url::parse` + `ShellExecuteW`。
5. **源码文件 `src/tui/gitinfo.rs:125` 混入原始 `0x00` NUL 字节**
   - `src/tui/gitinfo.rs` 第 125 行的 `.split_terminator('\0')` 中，单引号内不是两个字符 `\0`，而是一个真实的 **`0x00` 二进制字节**，导致 `git diff`、`ripgrep` 等工具将该源码文件判定为二进制文件。
6. **TUI 启动与 `/new` 持续向系统临时目录泄漏 `.lock` 文件（`src/agent.rs:28-34, 212, 464`, `src/session.rs:248`）**
   - TUI 欢迎页模式（`defer_initial_save = true`）和每次执行 `/new`（`enter_welcome`）都会在 `std::env::temp_dir()` 下生成 `kroma-unsaved-<pid>-<nonce>.json`，并由 `SessionStore::open_at` 创建对应的 `.kroma-unsaved-<pid>-<nonce>.json.lock` 文件，退出或切换会话后从不清理。
7. **`Config::resolved_limits_without_probe` 与 `app::configured_model` 输出预算计算不一致（`src/config.rs:285-339` vs `src/app.rs:166-190`）**
   - 对于 `responses` 和 `anthropic` 协议，未显式配置 `max_output_tokens` 且未知模型硬上限时，实际运行时（`src/app.rs:169`）固定发送并预留 `4096` tokens；但 `kroma config show` 和 `kroma diagnostics` 调用的 `resolved_limits_without_probe` 未区分协议，按 `(window / 8).min(4096)` 计算并报告“未发送显式上限”，导致诊断报告与实际运行时行为不符。
8. **`SessionCatalog` 在自定义 `KROMA_SESSION_FILE` 非 `.json` 后缀时文件名推导错误（`src/sessions.rs:44, 65, 111, 145, 269`）**
   - `src/session.rs:204` 的 `journal_name` 通过追加 `.wal` 生成日志路径（如 `foo.session.wal`），但 `src/sessions.rs:269` 的 `list()` 却用 `path.with_extension("json.wal")`（变成 `foo.json.wal`），且 `last_path`/`usage_path`/`models_path`/`names_path` 均使用 `with_extension` 替换扩展名，当 `KROMA_SESSION_FILE` 不以 `.json` 结尾或同目录存在不同扩展名文件时会发生错位或冲突。
9. **MCP 子进程环境变量黑名单存在遗漏（`src/mcp.rs:60-85`）**
   - `is_host_secret` 采用前缀/后缀黑名单过滤环境变量，遗漏了不带下划线或不以 `_KEY`/`_TOKEN`/`_PASSWORD` 结尾的常见敏感变量（如 `PGPASSWORD`、`MYSQL_PWD`、`*_KEY_ID`、`*_COOKIE`、`KUBECONFIG` 等）。

---

## 三、 子系统成熟度评分

| 子系统 | 评分 (10分制) | 评价摘要 |
| :--- | :---: | :--- |
| **Session & WAL 持久化 (`session.rs`, `sessions.rs`)** | **9.0 / 10** | 增量日志、校验和、代际隔离（`generation`）与写失败熔断设计非常扎实；仅需修复临时 `.lock` 清理与 `with_extension` 后缀假设。 |
| **Agent 核心循环与工具调度 (`agent.rs`, `tools.rs`, `steering.rs`)** | **8.8 / 10** | 职责边界清晰、零冗余抽象、取消与中断恢复语义严谨；需修复 `before_model_request` Hook 后的 `observe_with_bytes` 视图同步。 |
| **多协议 Provider 适配 (`provider.rs`, `anthropic.rs`, `responses.rs`)** | **8.2 / 10** | SSE 流增量解析、跨模型 Reasoning 签名隔离、截断 Usage 保留做得很细致；但 Anthropic 下角色交替合并与压缩摘要首条消息角色是必须修复的 P0 缺陷。 |
| **上下文压缩引擎 (`context.rs`, `history_tools.rs`)** | **8.0 / 10** | 单次同步 Checkpoint + 原始历史检索工具方向正确；需修复 `is_safe_boundary` 切分条件及 `summary_source` 与 `checkpoint_completion` 的 `max_tokens` 预算冲突。 |
| **TUI 终端界面 (`src/tui/*`)** | **8.7 / 10** | 渲染缓存、Grapheme 光标处理、流式限流防饥饿（128 events/batch）、OSC 52 剪贴板回退都很成熟；需清除 `gitinfo.rs` 中的 `0x00` 字节。 |
| **MCP 客户端 (`src/mcp.rs`)** | **8.5 / 10** | SHA-256 项目信任指纹、无 TOCTOU 读取、进程树取消守卫完善；建议对 `trusted-mcp.json` 加文件锁并扩充敏感环境变量过滤。 |
| **插件系统 (`src/plugins/*`)** | **7.2 / 10** | 进程隔离和冷启动零开销设计良好，但存在宿主环境变量泄漏、项目插件无内容哈希信任绑定、`Drop` 阻塞异步线程、`compaction_started` 校验遗漏等问题。 |
| **Desktop 桌面端 (`desktop/*`, `desktop_host.rs`)** | **7.5 / 10** | 复用 Rust Core 架构干净，但前端历史分页切片逻辑、未配置密钥占位提示、`connect` 并发互斥及 Windows 外部链接打开方式需尽快修补。 |

---

## 四、 分阶段修复路线图与验收标准（Action Plan）

### Batch 1：协议正确性与上下文压缩阻断项修复（P0 / P1）
- [ ] **修复 Anthropic 压缩摘要与角色交替规则**：
  - 在 `src/context.rs::is_safe_boundary` 中，要求切分点前一条消息为无 `tool_calls` 的 `assistant` 且切分点处消息（若存在）为 `user`；
  - 在 `src/provider/anthropic.rs::payload_with_cache_at` 中保证 `messages[0].role == "user"`，并将相邻相同 `role` 的消息内容块合并到同一个 `content` 数组。
- [ ] **对齐 Checkpoint 输出预算与窗口容量检查**：
  - 在 `src/context.rs::summary_source` 与 `src/provider.rs::checkpoint_completion` 之间统一 `max_tokens` 预留，确保 `source_tokens + max_tokens <= window`；
  - 在 `src/provider.rs::text_generation_with_budget` 中纳入 `catalog_output_limit`。
- [ ] **修复 `compaction_started` 插件事件白名单**：
  - 在 `src/plugins.rs::PluginManifest::validate` 的允许事件列表中加入 `"compaction_started"`。

### Batch 2：插件/Shell 安全边界与 Windows 路径治理（P1）
- [ ] **插件进程环境隔离与项目级插件指纹绑定**：
  - 在 `PluginWorker` 启动子进程时过滤宿主敏感环境变量；
  - 将项目级插件启用记录绑定到插件脚本与 `plugin.toml` 的 SHA-256 指纹。
- [ ] **非阻塞进程清理与工具超时**：
  - 消除 `PluginCallGuard::drop` 在 Tokio 异步线程上的 600ms 同步 `sleep` 及 `PluginProcess::drop` 的重复 `terminate_plugin`；
  - 为 `BuiltinShell` 与非流式插件工具提供可配置超时机制。
- [ ] **Windows `\\?\` 路径归一化与隐私脱敏修复**：
  - 消除 `Config`、`prompt`、`convenience` 与 `diagnostics_bundle::redact_path` 中的 `\\?\` 前缀差异，确保 Windows 下 `kroma diagnostics` 正确将家目录脱敏为 `~`。

### Batch 3：Desktop / TUI / Session 体验与细节收敛（P2）
- [ ] **修复 Desktop 历史分页与插件密钥设置 UI**：
  - `desktop/src/App.svelte` 在 `loadOlderHistory` 时同步推进可视窗口或直接展示已加载分页；
  - 区分“已设置密钥”与“空密钥”，支持在 Desktop UI 中清空已保存的插件密钥；
  - 在 `desktop/src-tauri/src/lib.rs` 中为 `connect` 增加 `connecting` 互斥保护，并将 `open_external` 升级为严格 URL 校验 + `ShellExecuteW`。
- [ ] **清理源码 NUL 字节与临时锁文件**：
  - 将 `src/tui/gitinfo.rs:125` 中的 `0x00` 字节替换为 `'\0'` 转义；
  - 在退出或切换 TUI 欢迎页临时会话时清理 `temp_dir` 下的 `.kroma-unsaved-*.lock`。
  - 统一 `Config::resolved_limits_without_probe` 与 `app::configured_model` 对 `responses` / `anthropic` 默认输出上限的计算逻辑。
