# Kroma 深度审查报告独立复核

复核对象：`reviews/KROMA_DEEP_REVIEW_2026-11-01.md` 及其 `deep-review/` 附带探针。  
代码基线：`9b125dc`，Kroma v0.4.0。  
方法：逐项对照当前实现、查阅协议官方文档、运行现有测试、复跑原探针并补充针对性探针。没有修改业务代码，也没有用真实 API Key 请求模型服务。

## 结论先行

**不能把原报告直接作为修复清单执行。大部分代码现象确实存在，但最严重的 Anthropic P0 是误报，若干 P1 混淆了明确的设计取舍与实现缺陷，附带探针中的几项重要真问题反而没进入正文。**

- **明确撤销**：“Anthropic 压缩后因 assistant 开头/连续角色而必现 HTTP 400”。两项协议前提均已过时。
- **明确撤销**：“进程模型不自动把图片变成 base64 是实现缺陷”。该插件协议明确规定接收本地图片引用，由插件自行读取。
- **保留**：Checkpoint 输出预算不一致、catalog 输出硬上限遗漏、插件事件白名单遗漏、同步取消清理、Windows 路径/后台进程问题、Desktop 分页/密钥显示、临时锁残留、诊断预算偏差和自定义会话后缀问题。
- **重新定性**：Shell 无默认超时是公开设计；插件无沙箱、继承环境及路径级持久信任属于需要明确告知和强化的信任模型，不能直接当成绕过已有沙箱或内容审批机制。
- **补进清单**：跨源重定向转发 Anthropic 密钥、历史分页跨会话串线、未启用插件的循环目录扫描、MCP 信任库并发丢更新/撤销复活、MCP 配置先全量分配后判限，以及部分流失败丢失已知 usage。

原报告的各子系统评分没有统一基准，不能当成已经验证的工程质量测量。

## 1. 最重要的误报：Anthropic P0

### 官方规则

官方发布说明在 **2024-10-08** 已明确说明：

> Consecutive user/assistant messages will be combined into a single message instead of erroring, and we no longer require the first input message to be a user message.

来源：
- https://platform.claude.com/docs/en/release-notes/api （2024-10-08 条目）
- https://platform.claude.com/docs/en/api/messages/create （messages 参数说明）

因此，下列原报告推导都不成立：

1. 压缩摘要是 assistant，成为第一条非 system 消息，所以必然 400；
2. 摘要与后续 assistant 相邻，所以违反严格交替；
3. tool_result 转成 user，后接 steering/checkpoint 的 user，所以必然 400。

`src/context.rs:908` 的摘要角色和 `src/provider/anthropic.rs:76,129,132` 的编码现象都是真的，但**从代码现象推导出的协议错误是假的**。特殊的旧版第三方兼容网关可能有自己的限制，不能据此宣称官方协议存在此 P0。

### 关于压缩边界

`src/context.rs:923` 确实允许在 user 或已消费的 tool 结果组之后切分；但是这不是上述角色限制的证据。现有逻辑禁止切开 assistant 的工具调用与结果，也禁止切开并行工具结果组，并受 `consumed_until` 限制。

原报告建议“只能在最终无工具调用的 assistant 后切分”，会使一个长时间、多步调用工具、尚未产生最终文字回复的任务缺少可用压缩点。不能为修复不存在的角色协议错误，直接把边界收紧到整个用户回合结束。

这不等于证明任意摘要都能完整保留语义；摘要质量与长工具循环的语义保真仍应通过专项任务评测验证。

## 2. 原报告逐项判定

下表“复现”表示运行针对性探针；“静态”表示由当前调用链直接确认，没有声称做了真实上游或完整 GUI 端到端验证。

### 2.1 压缩及插件

| 原报告项目 | 判定 | 依据与必要限定 |
| --- | --- | --- |
| 1：首条 assistant / 连续 assistant 或 user 必然导致 Anthropic 400 | **误报** | 官方早已放宽限制，见上一节。 |
| 1：允许在 user/tool 后切分就是不安全协议边界 | **结论不成立** | 不会仅因该角色排列违反官方协议；现有调用/结果组不可切分。完整用户回合边界是另一种设计，不是唯一合法方案。 |
| 2：Checkpoint 准入预留与首轮输出不一致 | **属实，有触发条件** | `context.rs:1006-1014` 只预留摘要目标的两倍加 512；`provider.rs:1651-1684` 首轮为缓存匹配使用前景输出参数。探针记录 source 估算 100282，准入通过，但 wire max_tokens=32768，两者合计超过 128000。 |
| 2：text_generation 未纳入 catalog 输出上限 | **属实，复现** | `provider.rs:1490` 只看 output_limit。探针 catalog cap=512、effective_turn_cap=512，但普通 text generation 发出 8192；Checkpoint 的截断重试也经过该路径。 |
| 3.1：插件及 plugins check 继承宿主密钥 | **事实属实；风险定性需限定** | `plugins.rs:2477` 的 Command 不清理环境；check 真的启动代码。用虚构的 KROMA_API_KEY/OPENAI_API_KEY 复现子进程可见。不是“未启用插件自动执行”，也不是绕过一个现有沙箱。 |
| 3.2：项目插件启用不绑定内容哈希 | **事实属实；属于信任模型强化项** | `plugins.rs:727-736` 绑定目录与 ID；修改 manifest 后仍 enabled，已复现。现行文档承诺的是路径级授权，不是修改即重新批准；文件指纹主要用于 worker 复用判定，不是持久内容授权。 |
| 3.3：非流式插件工具没有默认超时 | **属实，静态；不宜一律算高危** | `plugins.rs:2367-2368` 明确传 None，而 Hook/普通请求有 120 秒超时。仍可取消；适合提供可配置总时限，不能宣称一切插件调用均有 120 秒保护。 |
| 3.4：Drop 同步清理会阻塞，且可能清理两次 | **属实，静态** | `plugins.rs:2577-2599,2685`：取消 guard 同步 kill，Unix sleep 600ms；读请求失败又析构 PluginProcess，再 kill。Windows 同步等 taskkill。第二次清理通常在阻塞工作线程，不应写成“两次都必然阻塞 UI 线程”。 |
| 3.5：compaction_started 事件不在白名单 | **属实，复现** | `plugins.rs:437-459` 缺该项，`plugins.rs:2148` 却有映射；声明后直接校验失败。属于小而明确的功能缺陷，不是安全高危。 |
| 3.6 前半：Hook 后用量与 Hook 前视图用于校准 | **机制属实，后果有条件** | `agent.rs:1061,1222` 与 `plugins/model.rs:413-484`。动态改写消息/工具时测量可能不再对应下一次实际投影；固定、稳定变换不一定导致错误。Hook 后另有 validate_final_request，不能说可以直接绕过预算准入。 |
| 3.6 后半：进程模型没有自动物化图片引用 | **误报/设计约定** | `examples/plugins/README.md:248-253` 明确规定：messages[].provider_items 带 path/mime/mode，由进程插件读文件；协议不自动传图片字节/base64。收到中立附件标记不等于最终模型 API 会收到未处理标记，后者由插件负责。 |

**压缩预算例子还需更正：** 默认 native 输出通常是 4096，不是人人都会发 32768。128000 窗口且预留 32768 输出时，当前代码的 soft_limit 为 **92672**，不是原报告举的 115200；源码会随输出预留调整水位。该预算缺陷仍真实存在，但不能用默认 90% 的例子宣称常规会话必现。探针中的 HTTP 400 是本地模拟响应，只证明代码准入和 wire 参数不一致，不是假称调用真实 Anthropic 得到了 context overflow。

### 2.2 Shell 与 Windows

| 原报告项目 | 判定 | 依据与必要限定 |
| --- | --- | --- |
| 4.1：Shell 无默认超时 | **事实属实，但属于明确设计** | README:960-962、docs/lifecycle.md:86 明确写明无限时、可取消。`shell.rs:880` 还把 stdin 设为 null，并非所有交互式命令都会永久等待。自动化场景可选 deadline 值得增加，但不是违背现行契约的 P1。 |
| 4.2：Windows 命令成功后也会杀后台子进程 | **属实，Windows 实测** | `winsys.rs:61,99`：disarm 只改 armed，不清 KILL_ON_JOB_CLOSE。独立 PowerShell 对照组后台任务能完成；经 Kroma Shell 父命令 exit=0，但后台任务未完成。生命周期是否允许常驻后台进程需要统一产品语义，不宜未经设计就去掉取消/宿主崩溃保护。 |
| 4.3：Windows 诊断工作目录脱敏失败 | **属实，Windows 实测** | `diagnostics_bundle.rs:307` 直接 strip_prefix，不能匹配 verbatim path 与普通 HOME。探针 HOME 下工作目录输出 home_redacted=false、verbatim_prefix=true。不是“Windows 的所有路径一定完全无法脱敏”，而是这些表示不一致的路径失败。 |
| 4.3：系统提示和 @file 展示 verbatim 路径影响 Git Bash | **属实，Windows 实测** | `prompt.rs:208`、`convenience.rs:33`。真实目录的单引号包裹 verbatim 路径执行 ls 返回 2，去前缀且改正斜杠返回 0。主要是给模型/用户使用的路径表示问题。 |

**修复提醒：** 不要简单把 Config 中所有 Windows 原生路径都去掉 verbatim 前缀；这可能破坏长路径文件 I/O。应区分原生文件系统路径、展示/提示词中的 shell 友好路径，以及脱敏比较时的等价规范化。

### 2.3 Desktop、TUI、会话、MCP

| 原报告项目 | 判定 | 依据与必要限定 |
| --- | --- | --- |
| 5.1：加载旧历史后看起来没有变化 | **属实，复现真实函数逻辑** | `App.svelte:46,78-89`：加载 200 条后 historyCount 仍为 150，visibleHistory 完全不变。不是历史数据没加载，而是可见切片没推进。 |
| 5.2：空密钥显示已设置，无法从 UI 清空 | **属实，复现/静态** | `plugins.rs:875` 只按键名生成 secret_settings，空值也进入；`PluginsView.svelte:14-18,69` 把空值视为不更新并显示“已设置”。需要把“敏感字段”和“已有非空值”拆开，并给显式清除操作。 |
| 5.3：connect 期间缺少并发互斥 | **部分属实，影响被放大** | `src-tauri/src/lib.rs:188-244` 无 connecting/请求版本互斥；同时 IPC connect 可出现后完成请求覆盖前一请求。**但提交前会再次 require_idle 并检查 project generation；普通前端 doConnect 也有 busy/running 保护。** 不能说会无条件覆盖运行中任务；恢复 session ID 也是在提交时重新读取。 |
| 5.4：Desktop URL 校验弱、Windows 打开方式不同 | **实现差异属实；代码执行漏洞未证实** | `src-tauri/src/lib.rs:536-556` 仅前缀和控制字符校验；可加强为 URL 解析并与 TUI ShellExecuteW 统一。但这里用 Command.args，不是 shell 字符串拼接；报告没有给出可复现利用或具体漏洞依据，不能据此认定已存在命令注入/RCE。 |
| 5.5：gitinfo.rs 含真实 NUL | **属实，二进制检查** | 恰好 1 个 NUL，在第 125 行。会干扰文本工具；当前 Rust 编译/测试通过，属于源码卫生/工具兼容问题，不是编译阻断。 |
| 5.6：欢迎页及 /new 遗留临时 lock | **属实，复现** | `agent.rs:30,799`、`session.rs:267`。启动 1 个临时会话，再 enter_welcome 3 次并 drop，留下 4 个 lock。是零字节文件/目录项积累，不是仍持有 4 个 OS 锁或内存泄漏。 |
| 5.7：native 离线诊断与运行时默认输出预算不同 | **属实，复现** | `config.rs:258`、`app.rs:229-245`。未知 responses 模型、8192 窗口：离线 reserve=1024、effective=None；运行时 reserve=4096、effective=4096。128k 等大窗口可能恰好同为 4096，不能泛化为全部配置都有数值差异。 |
| 5.8：非 .json 默认会话后缀产生 WAL 路径/sidecar 问题 | **属实，复现** | `session.rs:218` 追加 .wal；`sessions.rs:307` 替换成 json.wal。foo.session.wal 的较新时间被 list 忽略；foo.session 与 foo.other 共享 foo.names.json，第二个默认会话会继承第一个标题。**SessionStore 自身恢复 WAL 仍走正确路径；不能夸大成原始会话必然丢失。** |
| 5.9：MCP 黑名单遗漏一些敏感变量 | **属实，静态；属隔离强化** | `mcp.rs:63-87` 没覆盖 PGPASSWORD、MYSQL_PWD、任意 *_COOKIE 等。AWS_*_KEY_ID 已被 AWS_ 前缀拦截，不能把所有 *_KEY_ID 一概称为泄漏。KUBECONFIG 通常是凭据文件路径，不等于凭据正文。可信本地 MCP 本来就有同用户文件权限，过滤并非沙箱。 |

## 3. 附带探针里已经存在、但汇总正文遗漏的重要问题

以下不是把原探针输出直接当成证明；本次也复跑了相应代码。

### A. Anthropic 密钥随跨源重定向转发 —— 建议优先修复

- 位置：`src/provider.rs:810-816,905-908`。
- HTTP Client 使用默认重定向策略；Anthropic 认证用自定义 x-api-key，不是 Authorization。
- 两个不同 loopback 端口构成不同 origin。来源服务返回 307 后，目标服务实际收到了虚构的 x-api-key；复跑输出 `REDIRECT credential_forwarded=true`。
- 这不证明任意模型输出都能控制 provider URL；前提是模型服务/代理发生跨源重定向。但把 provider 密钥发给未授权 origin 是真实的凭据边界缺陷，应拒绝跨源重定向或显式处理认证头。
- 不建议把日志中的 API Key 打印出来；现有探针仅检测预置虚构值。

### B. 加载历史与切换会话竞争，会把 A 的历史显示到 B

- 位置：`desktop/src/App.svelte:78-89,130-143`。
- loadOlderHistory 等待 API 期间，用户可以切换会话；回包没有检查请求 generation 或返回的 session_id。
- 本次对真实函数体的 VM 执行结果：`visibleSession=B` 且 `wrongSessionVisible=true`。
- 影响是**前端跨会话显示串线**，未证明原始 WAL 被写错。这比单纯点击后没反应更值得修复。

### C. 未启用的项目插件也能拖住插件发现

- 位置：`src/plugins.rs:2284-2321`。
- worker 定义指纹扫描跟随目录链接，没有 canonical visited 集合或总扫描节点/字节预算。pending.len()<128 只是当前待处理栈限制，不是总工作量限制。
- 复跑原探针：正常 `plugins list` 16ms；加两个指回插件目录的 Windows junction 后，**插件未启用**，仍超过 2 秒而被测试 watchdog 终止。
- 这证明启动/发现阶段可被循环/爆炸式扫描拖住，不等同于证明任意平台上绝对无限循环。应增加防循环和整体预算。

### D. MCP 信任库并发丢更新，撤销可被另一个写者复活

- 位置：`src/mcp.rs:267-310`。
- 原子替换文件只防止半写，不使 read-modify-write 原子化；缺少跨进程锁。
- 本次 24 个并发批准，最后仅保存 1 个；4 个调用发生错误，其他返回成功的调用也丢失更新。
- 并发批准另一项目与撤销已信任项目时，1/5/10/20ms 的四种测试延迟均复现 `revoked_project_trusted_again=true`。
- 不能只把它写成“建议增加文件锁”的美化项：这里有已经复现的状态正确性问题，包括撤销失效。

### E. MCP 的 1 MiB 限制在全量读入之后才检查

- 位置：`src/mcp.rs:222-228,252-258,272-275,436-451`。
- 32 MiB 未信任项目配置，最后虽被跳过，仍额外分配约 33555050 字节。
- 说明限制并不保护读取阶段内存。应在同一打开文件上有界读取到 limit+1 后拒绝，不能只添加一个有 TOCTOU 风险的 metadata 预检查。

### F. 部分流式失败仍丢失已报告 usage

- 位置：`src/provider.rs:634-647,1835-1868`，`src/provider/anthropic.rs:385`。
- finish() 的错误会包装 ReportedModelFailure，但解析 SSE 中 error 事件、网络错误/超时等提前返回路径不一定走 finish()。
- 发送有效 message_start，携带 input=123/output=5，再发 error 事件，reported_error_usage=None；相同前缀直接 EOF 则能保留 Some(Usage)。
- 因而“所有失败请求都保留可知 usage”的说法不准确。此问题主要影响统计与成本可见性，不是工具副作用重放。

### G. Unicode history_search 摘录偏移错误

- 位置：`src/history_tools.rs:36-48`。
- 在 to_lowercase 后的文本上获得字节偏移，却在原文本上切片；某些 Unicode 字符的小写映射会改变字节长度。
- 原探针中的 `İ` 重复 300 次后接 needle，匹配被找到，但 excerpt 为空。本次也复现。
- 属于检索体验/正确性问题，优先级低于前述安全边界和状态问题。

## 4. 实际验证结果及边界

- 根目录 `cargo test --all-targets --offline`：**499 passed，0 failed，4 ignored**。该命令不包含独立的 Tauri crate。
- Desktop `npm test`：**6 passed**。
- Desktop `npm run check`：**0 errors，0 warnings**。
- 独立 Tauri `cargo test --manifest-path src-tauri/Cargo.toml --offline`：测试程序构建完成，但启动返回 **0xc0000139 / STATUS_ENTRYPOINT_NOT_FOUND**，没有正常执行测试。具体 DLL/入口点原因本次未定位，**不能宣称其测试通过**。
- GUI 历史问题验证使用从真实 App.svelte 提取的函数体，不是 WebView 鼠标端到端测试。
- Windows 后台进程、路径/脱敏行为在本机实测；Unix 600ms 阻塞清理由 cfg(unix) 源码静态确认，没有冒称在 Unix 跑过。
- 压缩使用本地 HTTP mock，不涉及真实模型 tokenizer；不将模拟 HTTP 400 冒充真实上游故障。
- 现有测试通过不代表上述问题不存在：相关边界并没有被现有断言充分覆盖。

本次新增/保存的证据：

- `reviews/deep-review/report_verification.rs`：针对压缩预算、密钥继承、白名单、空密钥、路径级信任、离线预算、临时锁、Windows 路径/后台进程和自定义后缀的探针。
- `reviews/deep-review/verification-rust-output.txt`：上述探针输出。
- `reviews/deep-review/verification-original-rust-output.txt`：复跑原 Rust 探针输出。
- `reviews/deep-review/verification-ui.cjs` 与 `verification-ui-output.txt`：真实前端函数的分页与会话竞争复现。
- `reviews/deep-review/verification-plugin-scan-output.txt`：复跑原禁用插件循环扫描探针。
- `reviews/deep-review/verification-tests-output.txt`：根目录测试日志。

重跑 Rust 探针时可把对应 .rs 临时复制到 examples/ 后 `cargo run --offline --example <name>`，跑完删除临时副本。探针会隔离 HOME，使用虚构密钥和 loopback 地址；审阅后再执行。没有将其接入日常产品构建。

## 5. 建议修复顺序

1. **凭据/启动前边界**：provider 跨源重定向；MCP 有界读取和信任库锁；未启用插件指纹扫描防循环/总预算。
2. **请求正确性**：对齐 Checkpoint 首轮与重试预算，统一 catalog 硬上限；动态 model Hook 的测量失效/最终投影反馈。
3. **明确的用户体验和平台问题**：历史分页的可见窗口与会话 generation、Windows 诊断脱敏、插件密钥是否已配置/显式清除、Windows 后台生命周期。
4. **小型一致性修复**：补 compaction_started；离线 limits 与 runtime 共用计算；会话 sidecar 命名；只清理唯一的临时会话 lock；移除源码 NUL；完善失败 usage 包装和 Unicode 摘录。
5. **独立设计讨论**：Shell/插件工具可配置 deadline；插件环境最小权限与显式继承声明；项目插件路径信任是否升级为内容信任。不要把这三项未经讨论直接施加为全局行为变更。

特别注意：照搬 MCP 环境黑名单会破坏依赖环境凭据的 web/provider/subagents 插件；应设计明确的凭据/环境传递权限。正常持久会话的锁文件也不能通用地在 Drop 时删除，以免制造新旧 inode 两把独立锁的竞态。上述强化项必须配合兼容性和生命周期回归测试。
