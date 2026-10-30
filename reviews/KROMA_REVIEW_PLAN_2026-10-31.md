# Kroma Review 与修复计划（2026-10-31）

## 结论

架构不需要重写。Agent Loop、WAL/原子写入、Tool/Model 分层、插件宿主的方向是成立的。
本轮按“增量修复”处理了安全边界、资源上限、桌面宿主和上下文策略中的高风险点，
原始历史始终保留，没有改成“压缩即删除”。

## 本轮已完成

### P0：项目级 MCP 信任边界

- `<workdir>/.kroma/mcp.json` 默认不加载、不执行。
- 新增 `kroma trust-project [--revoke]`，批准绑定“规范化项目路径 + 配置字节指纹”；
  修改配置后必须重新批准。
- `KROMA_TRUST_PROJECT_MCP=1` 只用于单次显式放行。
- 项目配置的 `${ENV}` 展开只允许非敏感 `MCP_*` 与少量路径变量；
  `*_KEY`、`*_TOKEN`、云厂商凭据、`KROMA_*` 等一律拒绝。
- MCP 子进程启动前清理宿主凭据环境变量。
- 修掉“先读一次做 hash、再读一次执行”的 TOCTOU：信任判断和解析使用同一份字节。
- 不可读、超大或未批准的项目配置只产生 warning，不再阻断启动。
- 回归测试：
  - `project_mcp_config_is_inert_until_explicitly_trusted`（Unix）
  - `project_mcp_config_is_inert_until_explicitly_trusted_windows`（Windows）
  - 未批准时 marker 文件不会出现；显式可信后才执行。

### P1：资源上限与超时

- Provider SSE：
  - 单个事件上限 2 MiB、pending 缓冲上限 4 MiB、整条流上限 64 MiB；
  - 错误响应体上限 64 KiB；
  - `/models` 元数据上限 256 KiB；
  - APEC 非流式响应上限 1 MiB；
  - SSE 事件字节数用增量计数，避免大量小行造成二次方扫描。
- MCP：
  - 配置 1 MiB、响应/tool result 8 MiB、图片 16 MiB；
  - stdio 单行、HTTP body、SSE 累计/单帧均有上限；
  - 所有 MCP RPC 120 秒超时，stdio/HTTP/SSE 都覆盖；
  - 图片在 base64 解码前就检查大小。
- 插件：
  - 单次工具调用 120 秒超时，超时后通过 guard 杀掉插件进程树；
  - 流式插件 120 秒无帧则中止并杀进程；
  - 原有 JSONL 2 MiB 帧上限保留。
- `web_fetch`：
  - 默认拒绝 loopback、RFC1918、link-local、metadata、IPv6 ULA 及 IPv4-mapped 私网地址；
  - DNS 解析加 5 秒上限；
  - 用户配置的搜索/Firecrawl 端点可显式放行本地地址；
  - `KROMA_WEB_ALLOW_PRIVATE=1` 为显式逃生口。
  - 仍不是完整网络沙箱：DNS 可能在检查与连接之间变化，代理也可能绕过。

### 文件权限与信息暴露

- `/editor` 草稿改用 `private_file`（Unix 0600，目录 0700）。
- `plugins.toml` 临时文件改用 `private_file`，避免 API key 以 umask 默认权限落盘。
- `PluginCatalog::summaries()` 和 `plugins config` 对 key/token/secret/password 等字段打码，
  不再把插件 API key 返回给桌面 WebView 或终端。
- README 修正了：
  - web 插件不再是“Bing RSS 默认”，而是 keyless auto；
  - profile 权限是 warning 而非 reject；
  - shell 大输出有上限并落盘，不再是“无限内存”；
  - MCP 项目配置需要信任、子进程会清理宿主凭据。

### 桌面宿主

- `bootstrap`、`open_project`、`save_profile`、session 操作、`plugin_action`
  改为 async command，避免直接在 Tauri 主线程做磁盘/插件扫描。
- 快照最多返回最近 1000 条消息，并带 `total_messages` / `history_truncated`；
  完整历史仍在会话文件中。
- Markdown 链接不再让 WebView 自己导航；只允许 HTTP(S) 交给系统浏览器。
- 新增 `open_external` command，校验协议、长度和控制字符。
- 删除未引用的 `desktop/src-tauri/src/host.rs`、`desktop/src/api.ts`、
  `Message.svelte`、`style.css`。

### 上下文压缩

- TRC 的失效成本改为按“实际投影后的 prompt 后缀”估算，而不是未压缩的 raw 后缀；
  避免把已经被 summary 覆盖的旧内容重复计入 invalidation cost。
- APEC summary 目标从 128–1024 tokens 提升到按可用输入缩放，最大 4096 tokens；
  同时保证不超过可用输入的 1/8。小窗口仍有下限保护。
- 保留原有安全边界：只在完整 assistant turn 边界切；最近消息和未消费工具结果受保护；
  原始 transcript 不删除。

### 工程与 CI

- 补齐并暂存了此前未跟踪的关键文件（`src/cache_trace.rs`、`examples/plugins/web/*`、
  desktop API/组件文件等）。
- CI 现在运行全部三个 web 插件测试文件，并增加 desktop UI 的
  `npm ci / check / test / build` job。
- 本地验证：
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets -- -D warnings`
  - `cargo test --locked`：309 + 各集成测试全部通过（9 ignored）
  - release build 成功
  - 插件性能：disabled/baseline = 0.997×
  - Node：81 passed
  - desktop：svelte-check 0 error/warning、vitest 3 passed、Tauri cargo test 2 passed

## 仍需处理（按优先级）

### P1

1. **MCP 信任指纹升级为密码学哈希**
   当前 FNV-1a 64-bit 足够防误改，但对抗性碰撞不合适。可引入 `sha2` 或现有依赖中的
   SHA-256，把 `fingerprint()` 换成 SHA-256；信任文件格式不必变。
2. **MCP 信任批准的可视化**
   现在只有 CLI/环境变量。桌面端应在检测到未批准项目配置时提示“查看配置并批准”，
   不能默认放行。
3. **桌面命令真正 `spawn_blocking`**
   目前改成 async 后已离开主线程，但仍在 async worker 上做同步磁盘扫描。
   若后续出现大项目卡顿，应把 `DesktopState` 改成 `Arc<Mutex<Host>>`，
   在 `spawn_blocking` 中取锁执行。
4. **快照内存仍会先 clone 全量消息**
   序列化已经截断到 1000 条，但 `agent.messages()` 仍会复制全部消息。
   需要给 Agent 增加 range/paging API，桌面端按页加载。

### P2

1. **压缩策略的预算拆分**
   目前 `output_reserve` 仍等于模型可请求的 `max_output_tokens`。当 profile 设置
   128k 输出时，会为输出预留 128k，压缩偏早。建议区分：
   - `max_output_capability`（模型能力）
   - `turn_output_budget`（实际每轮请求，例如 32k）
   - `safety_margin`
   并让 provider 的 `max_tokens` 也使用 `turn_output_budget`，否则只是改阈值不安全。
2. **预测式触发与候选 summary**
   当前仍是 soft 触发后台摘要、下一次请求边界应用。可以增加“下一请求大小预测”，
   在接近阈值前预生成候选，但只在真正需要空间时应用。
3. **历史检索工具**
   增加 `history_search` / `history_read`，让模型能按需取回被 APEC 覆盖的原始片段；
   这是长任务记忆从 4/10 提升的关键。
4. **缓存价格可配置**
   TRC 里 `saved_credit = saved * 0.3`、`invalidation_cost = suffix * 0.9` 仍是硬编码。
   在 CPA/Codex 等缓存价格未知时，保持“容量必要才压缩”，不要为了省 token 主动压缩。
5. **附件 GC / 引用计数**
   当前附件文件只增不减；需要按 session/WAL 引用清理孤儿文件，同时保留原始历史。
6. **`serde_yaml` 替换**
   `serde_yaml` 已停止维护；可换 `serde_yml`/`saphyr` 或专用 frontmatter 解析。
7. **LICENSE**
   仓库仍无 LICENSE 文件，发布前必须明确许可证。
8. **插件设置打码的 UI 语义**
   桌面端目前把打码值当普通字符串；用户直接点保存可能把 `••••` 写回。
   应增加 `is_secret`/`redacted` 标记，未修改时不提交该字段。

## 验收标准（下一阶段）

- 未批准的项目 MCP 配置：启动无副作用，marker/进程/网络请求均为 0。
- 批准后修改配置：下一次启动必须重新要求批准。
- 项目配置引用 `${AWS_SECRET_ACCESS_KEY}`、`${MCP_API_TOKEN}`、`${KROMA_API_KEY}`：全部拒绝。
- 1 个超大 SSE 事件、MCP 响应、插件帧：在有界内存内失败，不 OOM。
- MCP 卡死服务器：120 秒内返回错误，后续工具调用仍可用。
- `web_fetch` 访问 `127.0.0.1`、`[::1]`、`[::ffff:127.0.0.1]`、`169.254.169.254`：
  默认 `BLOCKED_ADDRESS`。
- 桌面 WebView 点击 `file:`/`javascript:` 链接：不导航、不执行；HTTP(S) 才交给系统浏览器。
- 压缩 summary 在 128k 上下文下目标约 4k tokens，且不超过可用输入 1/8。

## 2026-10-31 收尾更新

本轮已完成前述 P1/P2 中的：

- MCP 信任指纹改为纯 Rust SHA-256（不引入 `ring`，避免旧 Windows 的
  `bcryptprimitives!ProcessPrng` 入口点问题）。
- 桌面端 MCP 未批准提示、批准并重连。
- 桌面端重命令迁移到 `spawn_blocking`，快照增加 `history_page` 分页。
- `history_search` / `history_read` 接入默认运行时。
- 单轮输出预算 32k；TRC 缓存折扣/失效系数可通过
  `KROMA_CACHE_READ_DISCOUNT` / `KROMA_CACHE_INVALIDATION_FACTOR` 配置。
- 插件设置脱敏增加 `secret_settings` 标记，桌面端留空不会覆盖密钥。
- 添加 MIT `LICENSE`，Cargo 包声明 `license = "MIT"`。

仍保留为后续项：

- 附件 GC/引用计数需要跨 session 引用扫描，避免误删，未在本轮冒险实现。
- `serde_yaml` 仍是可用但停止维护的依赖；替换会改变 skill frontmatter 解析器，
  建议单独迁移并加兼容测试。
