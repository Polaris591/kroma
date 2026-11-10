# Kroma 源码审查用事实报告

> 基准：`Polaris591/kroma`，main，`f64ab03dccf6bc0df1765d86bb66d3c4f2be6e93`（2026-11-09），工作区 `D:\Kroma\kroma`。审查前 `git status --porcelain` 为空。测量命令包括 `git ls-files`、`wc -l`、`rg -n`、Node 逐文件 LF 计数及以下列出的测试。Windows 工作区没有 `tree`、`cloc` 命令，目录树根据跟踪文件路径生成。除特别注明外，行号对应此提交源码。

## 1. 概览

Rust 2021 AI 编码代理，CLI/TUI 主入口 `src/main.rs:649-671`、核心异步循环 `src/agent.rs:1738-2632`、Shell 执行器 `src/shell.rs:902-1028`；桌面客户端 Tauri 2 + Svelte 5（`desktop/src-tauri/Cargo.toml:1-26`、`desktop/package.json:1-30`）。Rust 1.85 起（`Cargo.toml:1-8`）。可运行 `cargo run -- --help`、`cargo run -- --print TASK`（`src/cli.rs:319-354`）；桌面使用 `npm run dev`（`desktop/package.json:6-11`）。

## 2. 规模与布局

`git ls-files` 实测 1,100 文件：1,093 非二进制、7 二进制；非二进制 LF 合计 180,582；Rust 149 个，TS/TSX 54 个。注意 LF 数与 `wc -l` 的规则一致，二进制内部 LF 不当作代码行。目录树（三层）及全部文件行数**见附录 A**。

**最多行 10 文件：**

| 文件 | 行数 |
|---|---:|
| `desktop/src-tauri/Cargo.lock` | 5969 |
| `src/plugins.rs` | 5567 |
| `src/provider.rs` | 5023 |
| `tests/plugins_e2e.rs` | 4427 |
| `src/context.rs` | 4194 |
| `src/tui/mod.rs` | 4032 |
| `src/tui/view.rs` | 3984 |
| `src/tui/view_tests.rs` | 3686 |
| `src/agent.rs` | 3528 |
| `src/mcp.rs` | 2966 |

**最长 10 个 Rust 函数（Node fn+大括号扫描，边界未经 AST 验证）：**

| 函数 | 路径:起止行 | 行数 |
|---|---|---:|
| draw | `src/tui/view.rs:2798-3770` | 973 |
| run_inner | `src/agent.rs:1738-2632` | 895 |
| run_with_resume | `src/tui/mod.rs:1785-2639` | 855 |
| history_document | `src/tui/view.rs:2205-2796` | 592 |
| run_legacy | `src/main.rs:53-433` | 381 |
| render_document_with | `src/tui/markdown.rs:208-582` | 375 |
| input_event | `src/tui/mod.rs:731-1099` | 369 |
| builtin_with_warnings | `src/commands.rs:286-633` | 348 |
| event | `src/tui/view.rs:1329-1644` | 316 |
| append_plain_record | `src/tui/tool_records.rs:414-711` | 298 |

**第三方直接依赖及用途：** 根 `Cargo.toml:9-31`：`anyhow` 错误传播；`arboard` 剪贴板；`base64` 字符编码；`png` PNG；`image` 图片；`futures-util` 异步流；`crossterm` 终端控制；`pulldown-cmark` Markdown；`ratatui` TUI；`toml` TOML；`unicode-width` 字宽；`unicode-segmentation` 字素；`libc` Unix 系统调用；`reqwest` HTTP；`serde` 序列化；`serde_json` JSON；`semver` 版本；`serde_yaml` YAML；`tokio` 异步运行时；`windows-sys` Windows API。Desktop Cargo：`tauri` UI 宿主、`tauri-plugin-dialog` 对话框、`tauri-plugin-notification` 通知、`anyhow`、`serde`、`serde_json`、`toml`、`tokio`、`tauri-build`（`desktop/src-tauri/Cargo.toml:17-26`）。Desktop npm：`@lucide/svelte` 图标、`@tauri-apps/api` 桥接、`@tauri-apps/plugin-dialog`、`@tauri-apps/plugin-notification`、`dompurify` HTML、`marked` Markdown；开发依赖 `@sveltejs/vite-plugin-svelte,@tauri-apps/cli,@tsconfig/svelte,@types/dompurify,@types/node,svelte,svelte-check,typescript,vite,vitest`（`desktop/package.json:12-31`）。以上用途据名称及局部代码归类，逐项是否被产物链接：未验证。

## 3. 核心循环

静态调用链：`main` `src/main.rs:649` → `dispatch` `:673` → `run_legacy` `:53` → `drive_task` `:488` → `Agent::run` `src/agent.rs:1515` → `run_with_attachments` `:1523` → `run_inner` `:1738` → `ToolRegistry::execute_with_warnings` `src/tools.rs:257` → `BuiltinShell::execute` `:445` → `execute_inner` `:511` → `shell::execute_for_session_with_progress` `src/shell.rs:598` → `execute_internal` `:902` → `spawn` `:935-941`，合计 13 节点，12 条调用边。省略 provider request 中间环节及 trait 分派。核心循环总计 895 行（`src/agent.rs:1738-2632`），仅保留工具执行主干；省略流式输出、上下文预算、模型重试、控制、持久化：

```rust
// src/agent.rs:2495-2515
                let pending_tool = self.tools.execute_with_warnings(
                    &call,
                    ToolContext {
                        model_connection: active_model.as_ref(),
                        user_message: current_user_text,
                        user_turn: Some(current_user_turn),
                        user_images,
                        previous_user_images,
                        workdir: &self.workdir,
                        archive: self.session.archive_path(),
                        on_output: self
                            .events
                            .as_ref()
                            .map(|_| &output_event as &crate::shell::OutputCallback<'_>),
                        ask: self.asker.as_deref(),
                    },
                );
```

## 4. 工具

默认 `shell,history_search,history_read,edit_file`（`src/app.rs:115-121`）；有 Skills 时 `load_skill`（`:121-123`）；MCP 通过 `mcp_search,mcp_call` 发现和调用，插件可替换或注册工具（`src/mcp.rs:2663-2827`，`src/plugins.rs:2015-2048`）。实测启用插件：`goal 0.2.0`、`project-memory 0.3.0`、`subagents 2.0.0`、`web 4.4.1`；声明工具 `goal_checkpoint`（`examples/plugins/goal/plugin.toml:20-25`），`memory_remember,memory_search,memory_read,memory_forget`（`examples/plugins/project-memory/plugin/plugin.toml:25-113`），`subagent`（`examples/plugins/subagents/plugin.toml:20-56`），`web_search,web_fetch`（`examples/plugins/web/plugin.toml:36-99`）。完整运行时注册的 MCP 动态定义、各插件完整 JSON schema：未通过真实模型请求验证；原文以这些 manifest 为准。

内建 shell 的模型 schema 原文：

```rust
// src/tools.rs:484-503
pub fn definitions() -> Vec<serde_json::Value> {
    // The dialect is resolved from the host, so the schema always describes the
    // shell the command will really run in.
    let description = shell::tool_description();
    vec![json!({
        "type": "function",
        "function": {
            "name": "shell",
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to execute"}
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }
    })]
}
```

其余模型 schema 源文件原文位置：`history_search src/history_tools.rs:92-103`，`history_read src/history_tools.rs:185-194`，`edit_file src/file_edit.rs:40-58`，`load_skill src/skills.rs:194-207`，`mcp_search src/mcp.rs:2668-2679`，`mcp_call src/mcp.rs:2812-2821`，`ask src/tools.rs:562-602`；按 300 行摘录限制未在此全文复制。Shell description 原文的不同方言字符串：`src/shell.rs:880-899`。

PATH 上的自定义 `kroma-memory`（`examples/plugins/project-memory/plugin/plugin.toml:7`），实测 `--help` 输出：

```text
kroma-memory 0.3.0
Local, workdir-isolated project memory plugin for Kroma.
Usage:
  kroma-memory            Run the JSONL plugin worker on stdin/stdout
  kroma-memory --help     Show this help
  kroma-memory --version  Show the version
Worker requests use Kroma plugin API 2:
  ping, tool (memory_search, memory_read, memory_remember, memory_forget),
  command (memory), request_context, task_summary and ui panel.
Storage is private local SQLite; Git worktrees share a common-dir identity; no network access.
```

## 5. 系统提示词

默认 base（最终运行时会被工作路径、`EXECUTION_GUIDANCE`、`V7_GUIDANCE`、`AGENTS.md` 和插件 prompt 增补，完整动态字符串未抓取：`src/prompt.rs:10,16-20,61-68,236-288`）：

```rust
// src/prompt.rs:242-253
        let tool_description = if self.custom_tools {
            "Your available tools are provided in the request tool definitions."
        } else {
            "Your only tool is shell. Use it to inspect and work with files when needed."
        };
        vec![Message::new(
            "system",
            format!(
                "You are Kroma, an AI coding agent. {tool_description} The starting working directory is {}. Be accurate about command results and do not claim a command ran unless a tool result confirms it. Avoid destructive commands unless explicitly requested. Reply concisely in the user's language.",
                crate::path_display::for_shell(workdir)
            ),
        )]
```

Checkpoint 的系统指令另位于 `src/context.rs:32-78`，不是每次 agent 请求都附带。

## 6. 命令执行实现

```rust
// src/shell.rs:922-936
    let mut cmd = Command::new(&shell.program);
    shell.configure(&mut cmd);
    cmd.arg(shell.command_text(command))
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }
```

```rust
// src/shell.rs:974-992
    let status = match shell_deadline() {
        Some(limit) => match tokio::time::timeout(limit, wait).await {
            Ok(status) => status,
            Err(_) => {
                // The wait future is dropped before touching the child again.
                #[cfg(any(unix, windows))]
                guard.terminate();
                let _ = child.start_kill();
                let _ = child.wait().await;
                // The child is reaped: never signal this group ID again, it may
                // already belong to an unrelated process.
                #[cfg(any(unix, windows))]
                guard.disarm();
                bail!(
                    "shell command exceeded KROMA_SHELL_TIMEOUT_SECS ({}s)",
                    limit.as_secs()
                );
            }
        },
```

每条命令新进程、`current_dir(workdir)`，shell cwd/env 不跨调用保留；文件改动会保留（`src/shell.rs:902-943`）。stdin 连空设备，TTY 未分配（`:922-929`）。`KROMA_SHELL_TIMEOUT_SECS` 未设/0 无绝对超时；另有 `KROMA_TOOL_IDLE_SECS` 输出空闲监控（`src/shell.rs:35-49`，`src/agent.rs:37-77,2474-2520`）。Unix 进程组 SIGKILL，Windows Job Object 终止树（`src/shell.rs:365-395`，`src/winsys.rs:105-172`）。stdout/stderr 先文件捕获；超过 64 KiB 或 2000 行只显示 `PREVIEW ONLY`、头尾及文件路径，归档可从路径分段读，后续清理可能删除（`src/shell.rs:24-33,521-582,228-334`）。退出码 `exit_code: Option<i32>`，异常/信号可为空；启动错误/超时回传 error JSON 无退出码（`src/shell.rs:400-412,999-1028`、`src/tools.rs:445-453`）。Ctrl-C 通过 drop Future、`agent.cancel()` 补 unknown 结果、写取消消息（`src/main.rs:486-556`、`src/agent.rs:1419-1504`、`src/session.rs:732-776`）。

## 7. 消息日志与状态

`Message` 字段 `role,content,reasoning_content,tool_calls,tool_call_id,provider_items`（`src/protocol.rs:5-27`）。持久化 JSON v3 snapshot + 带 checksum 的逐条 WAL；256 条或 16 MiB 后写 snapshot 并截断 WAL（`src/session.rs:17-32,89-165,509-704`）。

**已定位写入/更改位置**：`src/agent.rs:450-468` 清空，`:892-919` 注记 push/pop，`:1122-1165` 分叉，`:1243-1290` 回退，`:1331-1387` 新会话/载入，`:1419-1504` 中断恢复，`:1761-1780` 用户 push，`:2060-2093` 中断注记，`:2170-2190` assistant push，`:2250-2272` 控制注记，`:2535-2610` tool push/改写待提交结果，`:2634-2655` 失败注记，`:2656-2691` steering；`src/session.rs:89-165,417-490,491-704,732-776` 恢复/覆盖存档；`src/prompt.rs:256-288` 与 `src/context.rs:748-915` 修改请求投影。其他调用方的完全性未验证。

`static` 全局状态文本检索：`src/main.rs:25,35`、`src/shell.rs:48`、`src/session.rs:17`、`src/sessions.rs:13`、`src/profiles.rs:16`、`src/file_edit.rs:20`、`src/private_file.rs:11`、`src/mcp.rs:582`、`src/diagnostics.rs:5`、`src/winsys.rs:52,342`、`src/task_control.rs:743`、`src/checkpoints.rs:353`（含测试静态局部变量）。

## 8. 上下文管理

预算阈值 `src/context.rs:302-445`，Compression 状态 `:582-680`，投影截短工具结果 `:748-915`，token/usage 校准与缓存 watermark `:928-1133`，checkpoint `:1202-1543`，prepare `:1544-1660`，finish-task 压缩 `src/agent.rs:2292-2343`，cache trace `src/cache_trace.rs:1-160`；原始历史与模型投影分开（`src/context.rs:1-5`、`src/prompt.rs:256-288`）。`src/token_estimate.rs` 未通读。

## 9. 权限与沙箱

默认父代理 Shell 无逐命令审批、无命令白名单（执行调用链 `src/app.rs:115-160`、`src/tools.rs:445-453`）；子代理有 tools allow/deny、shell deny 和 turn/token 限制（`src/child_policy.rs:1-104`）。MCP project 配置具有信任校验（`src/mcp.rs:29-53,674-715`）；插件环境清理默认不开启（`src/plugins.rs:107-130`）；`edit_file` 路径仅限 workdir（`src/file_edit.rs:44-55`）。`current_dir`、进程组不是容器/chroot/用户隔离（`src/shell.rs:922-936`）。未对所有跨平台隔离配置做全仓验证。

## 10. 错误处理

Rust `Result`/`?`、`match Err`。上游无流式内容时：输出截断可重试一次，context overflow 可先恢复后重试；未成功时保存中断 note（`src/agent.rs:2010-2095`）。工具参数/启动/超时异常给模型 JSON error（`src/tools.rs:445-453`）；before hook 失败阻止执行，after hook 失败回退本次结果修改并发 warnings（`src/tools.rs:276-360`）。成功任务收尾压缩失败仅 runtime_log（`src/agent.rs:2296-2343`）；捕获文件消失转空输出（`src/shell.rs:521-532`）；输出写入错误 `let _ = output.text(text)`（`src/main.rs:444-451`）。所有错误分支穷尽：未验证。

## 11. 配置面

源码模式 `rg -o KROMA_[A-Z][A-Z0-9_]* | sort -u` 命中 **52 个不同文字**（含测试/构建，仅字符串计数，不等于有效 env 键总数）。详列：`KROMA_API_KEY,KROMA_API_PROTOCOL,KROMA_BASE_URL,KROMA_BUILD_REVISION,KROMA_CACHE_TRACE,KROMA_CHECKPOINT_CHILD_HELPER,KROMA_CHECKPOINT_DESCENDANT,KROMA_CHILD_DEPTH,KROMA_CHILD_POLICY_FILE,KROMA_CHILD_SKIP_TASK_SUMMARY,KROMA_CONFIG_FILE,KROMA_CONTEXT_WINDOW,KROMA_DEEPSEEK_THINKING,KROMA_GIT_TIMEOUT_SECS,KROMA_JOB_OWNER_HELPER,KROMA_LANG,KROMA_MAX_OUTPUT_TOKENS,KROMA_MCP_CALL_SECS,KROMA_MCP_CONFIG,KROMA_MCP_EAGER,KROMA_MCP_SERVERS,KROMA_MCP_STDERR,KROMA_MODEL,KROMA_MODEL_CATALOG_MODEL,KROMA_NO_CHECKPOINTS,KROMA_NO_COLOR,KROMA_NO_MCP,KROMA_OFFICIAL_PLUGIN_DIR,KROMA_OUTPUT_RESERVE_TOKENS,KROMA_PLUGIN_DIR,KROMA_PLUGIN_ISOLATE_ENV,KROMA_PLUGIN_TOOL_TIMEOUT_SECS,KROMA_PLUGIN_TRY,KROMA_PROFILE,KROMA_PROFILE_FRAMES,KROMA_PROVIDER_PROFILE,KROMA_QUIET,KROMA_REASONING_EFFORT,KROMA_SESSION_FILE,KROMA_SHELL,KROMA_SHELL_TIMEOUT_SECS,KROMA_SNAPSHOT_DIR,KROMA_TEST_EMPTY_KEY_FALLBACK,KROMA_TEST_KEY,KROMA_TOOL_IDLE_SECS,KROMA_TRUST_PROJECT_MCP,KROMA_TRUST_TEST_SECRET_,KROMA_TUI_SESSION,KROMA_TUI_STYLE,KROMA_UPDATE_SNAPSHOTS,KROMA_VERBOSE,KROMA_WORKDIR`。

CLI 参数：`--print/-p,--json,--continue/-c,--session,--new,--no-mcp,--mcp,--quiet/-q,--verbose/-v,--cwd,--config-file,--language,--no-color,--no-tui/--cli,--help/-h,--version/-V,--build-info`；子命令 `setup,config,plugins,diagnostics,trust-project,recover-context,help,version,update`（`src/cli.rs:9-35,135-385`）。Profile 文件 `~/.kroma/config.json` 有 `active,profiles` 及每个 profile 的 `base_url,model,api_key,api_key_env,deepseek_thinking,api_protocol,reasoning_effort,context_window,max_output_tokens,thinking_levels,catalog_model,native_web_search`（`src/profiles.rs:19-52,81-100`）。MCP JSON `mcpServers` 下 `command,args,env,cwd,url,headers,protocol_version`（`src/mcp.rs:645-692`）；plugin TOML 字段 `src/plugins.rs:133-169,255-280`；child policy `src/child_policy.rs:17-27`。动态 plugin settings 全量未测量。

## 12. 测试

实际运行：`cargo test --lib --locked --offline` → **765 passed，0 failed，10 ignored**；`cargo test --tests --locked --offline -q` → 库相同且所有执行的集成测试通过，另有 1 ignored；`cd desktop && npm test -- --run` → **23 文件，132 passed**；`cd desktop && npm run check` → **0 errors、0 warnings**。代理和命令执行相关单测 `src/agent.rs:3135-3528`、`src/shell.rs:1239-1538`，集成测试 `tests/*.rs`。真实模型请求、Tauri release、Linux/macOS、ignored 测试未执行。

## 13. 不变量核对表

| 项 | 判定 | 文件:行号及源码原文 |
|---|---|---|
| a 每个调用恰有结果（含崩溃） | 部分 | `src/session.rs:739-776`：`messages.push(Message::tool(call.id, serde_json::json!({ "error": detail }).to_string()));`，崩溃重启全链路未验证 |
| b 截断明示、全文可取 | 部分 | `src/shell.rs:540-577`：`PREVIEW ONLY; full output stored at {}`；清理 `:228-334` |
| c 全带 exit_code | 未保证 | `src/shell.rs:400-412`：`exit_code: Option<i32>`；`src/tools.rs:452`：`ToolResult::error(json!({"error": error.to_string()}).to_string())` |
| d stdin 永不阻塞 | 已保证（builtin shell） | `src/shell.rs:926`：`.stdin(Stdio::null())`；插件另计 |
| e 超时杀进程组并告知 | 部分 | `src/shell.rs:35-49`：默认无 wall timeout；`:980-989`：`guard.terminate(); ... bail!("shell command exceeded ...")` |
| f 消息日志只追加 | 未保证 | `src/agent.rs:1273-1275`：`self.session.reset(&shortened, &next_compression.state())?; self.messages = shortened;`，`src/session.rs:585-704` |
| g 命令不改写/重试 | 未保证 | `src/tools.rs:282-337`：`hook.before_with_context(&mut effective, ctx)`；`src/shell.rs:692-710`：`redirect::rewrite(command)`、PowerShell wrapper；`src/agent.rs:2025-2060` 模型请求重试 |

## 14. 坏味道清单（事实）

跨文件相同八行窗口的静态扫描共 **37 个匹配窗口**（含重叠/测试，非独立函数数）；例 `src/provider/anthropic.rs:63-68` ↔ `src/provider/responses.rs:69-74`，`src/external.rs:54-76` ↔ `src/tui/links.rs:238-260`。长文件/函数见 §2。只有一个实现的抽象、无调用函数、无引用配置、注释掉的代码块：未完整测量。特定模型分支 `src/provider.rs:1513-1516,1963-1967,2353-2355` 含 `contains("deepseek")`。TODO/FIXME/HACK 源码字符串检索 **1 条**：`src/tui/tool_record_tests.rs:400` 的测试字符串 `"rg -n TODO src"`；注释模式匹配 **0 条**，故无前十条注释。并行实现路径：Shell 截短 `src/shell.rs:521-582`、context 投影缩短 `src/context.rs:819-915`、TUI 呈现 `src/tui/tool_records.rs:414-711`；WAL/JSON 快照 `src/session.rs:509-704`。

## 15. 意外发现

`src/main.rs:455-460` 的换行字符串跨多行，原文：

```rust
    if saw_text && !ends_with_newline {
        if let Some(output) = output {
            let _ = output.text(
                "
",
            );
```

`src/shell.rs:521-532` 捕获文件找不到时 `return Ok((String::new(), None, 0));`；实机命中情况未验证。`src/provider.rs:1963-1967` 针对 DeepSeek checkpoint 设置 `reasoning_effort = "low"`，和普通请求配置不同。`src/shell.rs:692-710` 对 Windows Bash 和 PowerShell 命令有文本转换。

## 16. 覆盖范围

自动字节读取 1,100 个 Git 文件仅用于行数和路径，不表示逐行审查。重点读取 `src/main.rs,agent.rs,shell.rs,tools.rs,context.rs,prompt.rs,session.rs,protocol.rs,config.rs,cli.rs,profiles.rs,app.rs,mcp.rs,plugins.rs,child_policy.rs,winsys.rs,provider.rs,history_tools.rs,file_edit.rs,skills.rs` 的局部片段，不保证全文阅读。**我没有逐行读过的文件/目录**：`reviews/`（649 文件）、`benchmarks/`（81）、`desktop/` 除 manifest 和测试（108）、`examples/` 除四个插件 manifest（62）、`tests/`（28）、`docs/`（18）、`npm/`（13）、`.github/`（11）、`scripts/`（7）、`assets/`（2）和 `src/` 中上述部分以外代码。精确每一个路径见附录 A。未追踪的 `.git,target,node_modules` 等目录未计数/未读取。

## 附录 A：完整文件清单（三层目录树 + 每个文件行数）

`[B]` 表示二进制，其 LF 字节个数不是代码行数。此附录的文件路径与行数将由实际扫描数据插入。


```text
    18     .env.example
    14     .gitattributes
    64     .github/ISSUE_TEMPLATE/bug_report.yml
     8     .github/ISSUE_TEMPLATE/config.yml
    29     .github/ISSUE_TEMPLATE/feature_request.yml
    32     .github/pull_request_template.md
    40     .github/workflows/core.yml
    82     .github/workflows/desktop.yml
    46     .github/workflows/docs.yml
   117     .github/workflows/npm.yml
    45     .github/workflows/project-memory-release.yml
    32     .github/workflows/project-memory.yml
   143     .github/workflows/release.yml
    24     .gitignore
    62     CHANGELOG.md
    65     CONTRIBUTING.md
  2360     Cargo.lock
    31     Cargo.toml
    21     LICENSE
   242     PLUGIN_ROADMAP.md
   184     README.md
   130     README.zh-CN.md
    64     SECURITY.md
     1     assets/kroma-mark-on-dark.svg
     1     assets/kroma-mark-on-light.svg
    35     benchmarks/ARCHITECTURE_P0_BASELINE.md
    35     benchmarks/EXPERIMENT_DIRECTIVE.md
    69     benchmarks/NATIVE_KROMA_PI_SMOKE.md
    51     benchmarks/P10_APEC_CONSOLIDATION_REPORT.md
    66     benchmarks/P10_SIMPLIFICATION_REPORT.md
   103     benchmarks/P11_ARCHITECTURE_FOUNDATION_REPORT.md
    76     benchmarks/P12_ARCHITECTURE_REVIEW_REPAIRS.md
    75     benchmarks/P2_REVIEW.md
   115     benchmarks/P3_REVIEW.md
    65     benchmarks/P4_EXECUTION_REVIEW.md
   104     benchmarks/P4_REPORT.md
    56     benchmarks/P5_PROMPT_TOOL_REPORT.md
   109     benchmarks/P6_REPORT.md
    69     benchmarks/P6_RUST_PROMPT_REPORT.md
   176     benchmarks/P7_FINAL_REPORT.md
    95     benchmarks/P8_REPORT.md
    64     benchmarks/P9_APEC_AB_REPORT.md
    84     benchmarks/P9_BATCH1_REPORT.md
    48     benchmarks/P9_BATCH2_REPORT.md
    78     benchmarks/PROMPT_ABLATION_REPORT.md
    42     benchmarks/README.md
   129     benchmarks/REAL_TASK_RELIABILITY_REVIEW.md
   319     benchmarks/agent_task_suite.py
   108     benchmarks/architecture_contract.json
    44     benchmarks/architecture_contract.py
   166     benchmarks/astra_cancel_smoke.py
   188     benchmarks/astra_tui_profiles_smoke.py
   159     benchmarks/audit_pi_kroma_traces.py
   120     benchmarks/cli_automation_smoke.py
   418     benchmarks/engineering_compare.py
   222     benchmarks/fix2_targeted_probe.py
   602     benchmarks/fusion_ablation.py
   186     benchmarks/fusion_final.py
   207     benchmarks/fusion_stress.py
   120     benchmarks/graphite_windows_smoke.cjs
   123     benchmarks/mock_profile_smoke.py
   226     benchmarks/native_kroma_pi_smoke.py
   210     benchmarks/p0p1_fair.py
   107     benchmarks/p10_config_regression.py
   128     benchmarks/p11_architecture_baseline.py
   151     benchmarks/p1_continuous.py
   349     benchmarks/p3_real_dev.py
   133     benchmarks/p4_pi_compare.py
   254     benchmarks/p4_tool_ablation.py
   238     benchmarks/p5_prompt_tool_factorial.py
   341     benchmarks/p6_rust_prompt.py
   405     benchmarks/p7_abf_full.py
    93     benchmarks/p8_live_regression.py
   384     benchmarks/p9_apec_ab_live.rs
   134     benchmarks/p9_batch1_legacy_regression.py
   104     benchmarks/p9_batch1_regression.py
   148     benchmarks/p9_batch2_regression.py
   161     benchmarks/pi_kroma_complexity_matrix.py
   194     benchmarks/pi_kroma_extended.py
   288     benchmarks/pi_kroma_fix_crossover.py
    97     benchmarks/pi_kroma_full_compare.py
    69     benchmarks/pi_kroma_harness_ablation.py
    62     benchmarks/pi_kroma_randomized_crossover.py
     1     benchmarks/ponytail/A.txt
     1     benchmarks/ponytail/B.txt
     1     benchmarks/ponytail/C.txt
    88     benchmarks/ponytail/README.md
    84     benchmarks/ponytail/RESULTS.md
   266     benchmarks/ponytail_tasks.py
   272     benchmarks/prompt_ablation.py
     4     benchmarks/prompt_philosophy/B.txt
     4     benchmarks/prompt_philosophy/C.txt
   100     benchmarks/prompt_philosophy/README.md
    66     benchmarks/prompt_philosophy/RESULTS.md
   589     benchmarks/prompt_philosophy_ablation.py
    82     benchmarks/read_batching/README.md
     1     benchmarks/read_batching/append.txt
    32     benchmarks/test_evidence_loss_repro.py
   123     benchmarks/test_ponytail_ablation.py
   290     benchmarks/test_ponytail_tasks.py
   212     benchmarks/test_prompt_philosophy_ablation.py
   245     benchmarks/tool_ablation_live.py
    85     benchmarks/tool_batch_ablation.py
   345     benchmarks/tool_batch_followup.py
   309     benchmarks/tui_pty_smoke.py
   164     benchmarks/v7_pi_smoke_20261031.py
    62     build.rs
     6     desktop/.gitignore
    11     desktop/EXTENSION_PLAN.md
    20     desktop/PORTABLE-README.txt
   146     desktop/README.md
    10     desktop/index.html
  2285     desktop/package-lock.json
    33     desktop/package.json
   135     desktop/scripts/install-local.ps1
    49     desktop/scripts/package-portable.ps1
     4     desktop/src-tauri/.cargo/config.toml
  5969     desktop/src-tauri/Cargo.lock
    28     desktop/src-tauri/Cargo.toml
     3     desktop/src-tauri/build.rs
    17     desktop/src-tauri/capabilities/default.json
     5     desktop/src-tauri/icons/.gitignore
    18 [B] desktop/src-tauri/icons/128x128.png
    25 [B] desktop/src-tauri/icons/128x128@2x.png
     3 [B] desktop/src-tauri/icons/32x32.png
     7 [B] desktop/src-tauri/icons/64x64.png
   376 [B] desktop/src-tauri/icons/icon.icns
    41 [B] desktop/src-tauri/icons/icon.ico
    40 [B] desktop/src-tauri/icons/icon.png
  2227     desktop/src-tauri/src/lib.rs
     5     desktop/src-tauri/src/main.rs
   272     desktop/src-tauri/src/workspace_review.rs
    12     desktop/src-tauri/tauri.bundle.conf.json
    32     desktop/src-tauri/tauri.conf.json
   787     desktop/src/App.svelte
   924     desktop/src/app.css
    32     desktop/src/components/AbilitiesDialog.svelte
    70     desktop/src/components/AddModelDialog.svelte
    25     desktop/src/components/AppBanner.svelte
   287     desktop/src/components/ChatTranscript.svelte
   479     desktop/src/components/ComposerPanel.svelte
   131     desktop/src/components/ContextMeter.svelte
    27     desktop/src/components/DeleteConfirmDialog.svelte
    82     desktop/src/components/LiveTimeline.svelte
    53     desktop/src/components/Markdown.svelte
    71     desktop/src/components/MessageView.svelte
    59     desktop/src/components/ModelPicker.svelte
   153     desktop/src/components/PluginsView.svelte
    37     desktop/src/components/RewindDialog.svelte
   394     desktop/src/components/SessionSidebar.svelte
   129     desktop/src/components/SettingsView.svelte
    42     desktop/src/components/StorageDialog.svelte
    27     desktop/src/components/StreamingMarkdown.svelte
    30     desktop/src/components/ThinkingPicker.svelte
    28     desktop/src/components/ToolDetail.svelte
    70     desktop/src/components/Topbar.svelte
   155     desktop/src/components/TranscriptTurn.svelte
    96     desktop/src/components/TurnChanges.svelte
   127     desktop/src/components/WorkspaceChanges.svelte
   163     desktop/src/lib/api.ts
   158     desktop/src/lib/chat-ui.test.ts
   139     desktop/src/lib/chat-ui.ts
    48     desktop/src/lib/checkpoints.test.ts
    34     desktop/src/lib/checkpoints.ts
    33     desktop/src/lib/citations.test.ts
   104     desktop/src/lib/citations.ts
    49     desktop/src/lib/composer.test.ts
    43     desktop/src/lib/composer.ts
    76     desktop/src/lib/desktop-host.ts
    41     desktop/src/lib/draft-cache.test.ts
    46     desktop/src/lib/draft-cache.ts
    41     desktop/src/lib/drafts.test.ts
    34     desktop/src/lib/drafts.ts
    37     desktop/src/lib/file-suggest.test.ts
    32     desktop/src/lib/file-suggest.ts
    48     desktop/src/lib/followup-session.test.ts
    33     desktop/src/lib/followup-session.ts
    80     desktop/src/lib/followups.test.ts
    97     desktop/src/lib/followups.ts
    22     desktop/src/lib/format.test.ts
    13     desktop/src/lib/format.ts
    88     desktop/src/lib/goal.test.ts
   139     desktop/src/lib/goal.ts
    21     desktop/src/lib/latest.test.ts
    15     desktop/src/lib/latest.ts
   165     desktop/src/lib/live-events.test.ts
   197     desktop/src/lib/live-events.ts
    49     desktop/src/lib/notify.test.ts
    28     desktop/src/lib/notify.ts
    57     desktop/src/lib/plugin-ui.test.ts
    70     desktop/src/lib/plugin-ui.ts
    41     desktop/src/lib/selection.test.ts
    34     desktop/src/lib/selection.ts
   145     desktop/src/lib/session-commands.test.ts
   284     desktop/src/lib/session-commands.ts
    17     desktop/src/lib/session-export.test.ts
    18     desktop/src/lib/session-export.ts
    45     desktop/src/lib/session-groups.test.ts
    26     desktop/src/lib/session-groups.ts
   224     desktop/src/lib/snapshot.test.ts
   131     desktop/src/lib/snapshot.ts
    39     desktop/src/lib/steering.test.ts
    32     desktop/src/lib/steering.ts
    11     desktop/src/lib/streaming.test.ts
     7     desktop/src/lib/streaming.ts
    34     desktop/src/lib/task-notify.ts
   107     desktop/src/lib/transcript.test.ts
   130     desktop/src/lib/transcript.ts
    92     desktop/src/lib/turn-actions.ts
    17     desktop/src/lib/ui-types.ts
     4     desktop/src/main.ts
     2     desktop/src/vite-env.d.ts
     3     desktop/svelte.config.js
    21     desktop/tsconfig.json
     9     desktop/vite.config.ts
    42     docs/README.md
    80     docs/architecture.md
  1070     docs/design-notes.md
   731     docs/goal-plugin-design.md
   129     docs/lifecycle.md
    65     docs/maintainer-operations.md
   355     docs/model-catalog.md
   333     docs/npm-distribution.md
   102     docs/performance.md
  1198     docs/project-memory-history.md
    88     docs/project-memory.md
    95     docs/promotion-linux-do.md
   143     docs/quickstart.md
   127     docs/reference/cli.md
   118     docs/reference/configuration.md
   153     docs/releasing.md
   111     docs/security.md
   457     docs/subagents.md
   581     examples/plugins/README.md
    27     examples/plugins/ask/plugin.toml
    23     examples/plugins/context-demo/main.js
     9     examples/plugins/context-demo/plugin.toml
    94     examples/plugins/goal/README.md
   274     examples/plugins/goal/goal.test.js
   598     examples/plugins/goal/main.js
   140     examples/plugins/goal/plugin.toml
   241     examples/plugins/goal/protocol.test.js
    28     examples/plugins/hello/main.js
    35     examples/plugins/hello/plugin.toml
    69     examples/plugins/image-gen/README.md
   235     examples/plugins/image-gen/image.js
   194     examples/plugins/image-gen/image.test.js
    47     examples/plugins/image-gen/main.js
    50     examples/plugins/image-gen/plugin.toml
    87     examples/plugins/image-gen/providers.js
    23     examples/plugins/memory/main.js
    56     examples/plugins/memory/memory.js
    22     examples/plugins/memory/memory.test.js
    19     examples/plugins/memory/plugin.toml
    11     examples/plugins/model-hook/main.js
     6     examples/plugins/model-hook/plugin.toml
    11     examples/plugins/model-http/plugin.toml
    19     examples/plugins/model-process/main.js
    12     examples/plugins/model-process/plugin.toml
    83     examples/plugins/model-stream/main.js
    15     examples/plugins/model-stream/plugin.toml
     1     examples/plugins/project-memory/.gitignore
    14     examples/plugins/project-memory/CONTRACT.md
   513     examples/plugins/project-memory/Cargo.lock
    23     examples/plugins/project-memory/Cargo.toml
   129     examples/plugins/project-memory/README.md
   116     examples/plugins/project-memory/plugin/plugin.toml
     5     examples/plugins/project-memory/src/lib.rs
    54     examples/plugins/project-memory/src/main.rs
   312     examples/plugins/project-memory/src/model.rs
  2520     examples/plugins/project-memory/src/protocol.rs
  1295     examples/plugins/project-memory/src/scope.rs
  2207     examples/plugins/project-memory/src/store.rs
  1437     examples/plugins/project-memory/src/store_read.rs
     9     examples/plugins/status/plugin.toml
   167     examples/plugins/subagents/README.md
    73     examples/plugins/subagents/main.js
   112     examples/plugins/subagents/main.test.js
    56     examples/plugins/subagents/plugin.toml
   924     examples/plugins/subagents/subagent.js
   404     examples/plugins/subagents/subagent.test.js
    24     examples/plugins/transform/main.js
    20     examples/plugins/transform/plugin.toml
   237     examples/plugins/web/README.md
    33     examples/plugins/web/cache.js
    85     examples/plugins/web/evaluation.md
   120     examples/plugins/web/http.js
    60     examples/plugins/web/http.test.js
    46     examples/plugins/web/main.js
   104     examples/plugins/web/main.test.js
   170     examples/plugins/web/page.js
   100     examples/plugins/web/plugin.toml
   737     examples/plugins/web/web.js
   739     examples/plugins/web/web.test.js
   203     examples/tool_ablation.rs
    97     npm/README.md
    85     npm/launcher.cjs
    11     npm/package.json
    27     npm/platforms.cjs
    95     npm/release.cjs
   165     npm/smoke.cjs
   126     npm/stage.cjs
   153     npm/test/launcher.test.cjs
    40     npm/test/platforms.test.cjs
    21     npm/test/registry.cjs
    88     npm/test/release.test.cjs
    97     npm/test/update.test.cjs
   154     npm/update.cjs
    90     reviews/ASTRA_XHIGH_CLOSURE_2026-10-28.md
    62     reviews/AUDIT_AND_OFFICIAL_PLUGINS_2026-11-06.md
    60     reviews/BATCH_1_2_FIXES_2026-11-05.md
    92     reviews/BATCH_3_4_PERFORMANCE_ARCHITECTURE_2026-11-05.md
    89     reviews/COMPACTION_CACHE_DIAGNOSIS_2026-11-05.md
    93     reviews/COMPRESSION_CACHE_DEEP_AUDIT_2026-11-05.md
   278     reviews/HARNESS_INTERACTION_RESEARCH_2026-11-05.md
   466     reviews/HARNESS_UX_FULL_COMPARISON_2026-11-05.md
    68     reviews/HTTP_524_RETRY_2026-11-04.md
    44     reviews/KROMA-E1-CORE-EFFICIENCY-2026-10-26.md
    23     reviews/KROMA-PHASE2-PERFORMANCE-2026-10-26.md
   162     reviews/KROMA-RUST-FULL-REVIEW-2026-10-26.md
   297     reviews/KROMA_COMPLEX_ENGINEERING_ROOT_CAUSE_2026-10-28.md
   229     reviews/KROMA_DEEP_REVIEW_2026-11-01.md
   178     reviews/KROMA_DEEP_REVIEW_VERIFICATION_2026-11-01.md
    91     reviews/KROMA_HARNESS_REPEATED_ABLATION_2026-10-28.md
   210     reviews/KROMA_PI_P0_RUST_TRACE_FORENSICS_2026-10-28.md
    77     reviews/KROMA_PI_RANDOMIZED_CROSSOVER_2026-10-28.md
   126     reviews/KROMA_PI_TOOL_ABLATION_2026-10-28.md
   174     reviews/KROMA_REVIEW_PLAN_2026-10-31.md
   142     reviews/MODEL_CATALOG_2026-11-04.md
    56     reviews/PLUGIN_ARCHITECTURE_FINAL_AUDIT_2026-11-06.md
    27     reviews/README.md
    43     reviews/RELEASE_PREPARATION_2026-11-05.md
    30     reviews/REVIEW_FIXES_2026-10-29.md
    78     reviews/THREE_OFFICIAL_PLUGINS_FINAL_DESIGN_ACCEPTANCE_2026-11-06.md
   130     reviews/TUI_DESIGN_RESEARCH_2026-10-29.md
    35     reviews/TUI_FULL_WIDTH_LINKS_2026-10-29.md
   258     reviews/TUI_GRAPHITE_PLAN.md
    35     reviews/TUI_P0_IMPLEMENTATION_2026-10-29.md
    40     reviews/TUI_P1_P2_DELIVERY_2026-10-29.md
    55     reviews/TUI_REDESIGN_2026-11-01.md
   926     reviews/TUI_REDESIGN_BATCH1_2026-11-01.md
   319     reviews/TUI_REDESIGN_BATCH2_2026-11-01.md
    46     reviews/TUI_REDESIGN_BATCH3_2026-11-01.md
    57     reviews/TUI_REDESIGN_WINDOWS_NUL_2026-11-01.md
    83     reviews/TUI_REWIND_FEASIBILITY_2026-11-01.md
   156     reviews/TUI_USABILITY_AND_LANGUAGE.md
    28     reviews/WINDOWS_P0_P1_2026-10-29.md
    65     reviews/batch-3-4/after.txt
    74     reviews/batch-3-4/baseline-initial.txt
    25     reviews/batch-3-4/before-checkpoints-tui.txt
    21     reviews/batch-3-4/checkpoint-path-assertion-failed.txt
    28     reviews/batch-3-4/clippy-initial-layout-failure.txt
     2     reviews/batch-3-4/clippy.txt
    11     reviews/batch-3-4/context-ablation.txt
   897     reviews/batch-3-4/core-tests.txt
     3     reviews/batch-3-4/desktop-check.txt
     3     reviews/batch-3-4/desktop-test-build.txt
    11     reviews/batch-3-4/desktop-tests.txt
     1     reviews/batch-3-4/document-check.txt
    16     reviews/batch-3-4/environment.json
    13     reviews/batch-3-4/tui-after.txt
   211     reviews/catalog-expansion/chinese-families.json
  1699     reviews/catalog-expansion/curated.json
   122     reviews/catalog-expansion/domestic-missing.json
   158     reviews/catalog-expansion/legacy-popular.json
   261     reviews/catalog-expansion/openai-google-claude.json
    26     reviews/catalog-expansion/other-vendors.json
   244     reviews/deep-review/continuation-fmt-output.txt
    42     reviews/deep-review/continuation-probe-output.txt
  2359     reviews/deep-review/continuation-probe/Cargo.lock
    15     reviews/deep-review/continuation-probe/Cargo.toml
   192     reviews/deep-review/continuation_probe.rs
    10     reviews/deep-review/desktop-probe-output.txt
    28     reviews/deep-review/desktop-race.cjs
    13     reviews/deep-review/plugin-probe-output.txt
    10     reviews/deep-review/plugin-scan.cjs
   390     reviews/deep-review/repair-plan.md
   208     reviews/deep-review/report_verification.rs
   262     reviews/deep-review/review_probe.rs
    20     reviews/deep-review/rust-probe-output.txt
    24     reviews/deep-review/verification-original-rust-output.txt
    13     reviews/deep-review/verification-plugin-scan-output.txt
    19     reviews/deep-review/verification-rust-output.txt
   601     reviews/deep-review/verification-tests-output.txt
     2     reviews/deep-review/verification-ui-output.txt
    39     reviews/deep-review/verification-ui.cjs
     2     reviews/release-prep/clippy.txt
   908     reviews/release-prep/core-tests.txt
    41     reviews/release-prep/credential-scan.json
     3     reviews/release-prep/desktop-check.txt
     1     reviews/release-prep/document-check.txt
    10     reviews/release-prep/install-smoke.txt
   164     reviews/release-prep/npm-tests.txt
     3     reviews/release-prep/release-build.txt
     6     reviews/release-prep/stage.txt
     2     reviews/tui-display-modes/.gitignore
     8     reviews/tui-graphite/.gitignore
    29     reviews/tui-graphite/final/dark-cancelled-100x30.txt
    39     reviews/tui-graphite/final/dark-cancelled-160x40.txt
    15     reviews/tui-graphite/final/dark-cancelled-40x16.txt
    23     reviews/tui-graphite/final/dark-cancelled-80x24.txt
    29     reviews/tui-graphite/final/dark-code-100x30.txt
    39     reviews/tui-graphite/final/dark-code-160x40.txt
    15     reviews/tui-graphite/final/dark-code-40x16.txt
    23     reviews/tui-graphite/final/dark-code-80x24.txt
    29     reviews/tui-graphite/final/dark-context75-100x30.txt
    39     reviews/tui-graphite/final/dark-context75-160x40.txt
    15     reviews/tui-graphite/final/dark-context75-40x16.txt
    23     reviews/tui-graphite/final/dark-context75-80x24.txt
    29     reviews/tui-graphite/final/dark-context95-100x30.txt
    39     reviews/tui-graphite/final/dark-context95-160x40.txt
    15     reviews/tui-graphite/final/dark-context95-40x16.txt
    23     reviews/tui-graphite/final/dark-context95-80x24.txt
    29     reviews/tui-graphite/final/dark-conversation-100x30.txt
    39     reviews/tui-graphite/final/dark-conversation-160x40.txt
    15     reviews/tui-graphite/final/dark-conversation-40x16.txt
    23     reviews/tui-graphite/final/dark-conversation-80x24.txt
    29     reviews/tui-graphite/final/dark-diff-100x30.txt
    39     reviews/tui-graphite/final/dark-diff-160x40.txt
    15     reviews/tui-graphite/final/dark-diff-40x16.txt
    23     reviews/tui-graphite/final/dark-diff-80x24.txt
    29     reviews/tui-graphite/final/dark-failure-100x30.txt
    39     reviews/tui-graphite/final/dark-failure-160x40.txt
    15     reviews/tui-graphite/final/dark-failure-40x16.txt
    23     reviews/tui-graphite/final/dark-failure-80x24.txt
    29     reviews/tui-graphite/final/dark-home-100x30.txt
    39     reviews/tui-graphite/final/dark-home-160x40.txt
    15     reviews/tui-graphite/final/dark-home-40x16.txt
    23     reviews/tui-graphite/final/dark-home-80x24.txt
    29     reviews/tui-graphite/final/dark-home-recent-100x30.txt
    39     reviews/tui-graphite/final/dark-home-recent-160x40.txt
    15     reviews/tui-graphite/final/dark-home-recent-40x16.txt
    23     reviews/tui-graphite/final/dark-home-recent-80x24.txt
    29     reviews/tui-graphite/final/dark-idle-100x30.txt
    39     reviews/tui-graphite/final/dark-idle-160x40.txt
    15     reviews/tui-graphite/final/dark-idle-40x16.txt
    23     reviews/tui-graphite/final/dark-idle-80x24.txt
    29     reviews/tui-graphite/final/dark-inspector-100x30.txt
    39     reviews/tui-graphite/final/dark-inspector-160x40.txt
    15     reviews/tui-graphite/final/dark-inspector-40x16.txt
    23     reviews/tui-graphite/final/dark-inspector-80x24.txt
    29     reviews/tui-graphite/final/dark-multi-turn-100x30.txt
    39     reviews/tui-graphite/final/dark-multi-turn-160x40.txt
    15     reviews/tui-graphite/final/dark-multi-turn-40x16.txt
    23     reviews/tui-graphite/final/dark-multi-turn-80x24.txt
    29     reviews/tui-graphite/final/dark-panel-100x30.txt
    39     reviews/tui-graphite/final/dark-panel-160x40.txt
    15     reviews/tui-graphite/final/dark-panel-40x16.txt
    23     reviews/tui-graphite/final/dark-panel-80x24.txt
    29     reviews/tui-graphite/final/dark-picker-100x30.txt
    39     reviews/tui-graphite/final/dark-picker-160x40.txt
    15     reviews/tui-graphite/final/dark-picker-40x16.txt
    23     reviews/tui-graphite/final/dark-picker-80x24.txt
    29     reviews/tui-graphite/final/dark-picker-files-100x30.txt
    39     reviews/tui-graphite/final/dark-picker-files-160x40.txt
    15     reviews/tui-graphite/final/dark-picker-files-40x16.txt
    23     reviews/tui-graphite/final/dark-picker-files-80x24.txt
    29     reviews/tui-graphite/final/dark-picker-models-100x30.txt
    39     reviews/tui-graphite/final/dark-picker-models-160x40.txt
    15     reviews/tui-graphite/final/dark-picker-models-40x16.txt
    23     reviews/tui-graphite/final/dark-picker-models-80x24.txt
    29     reviews/tui-graphite/final/dark-picker-sessions-100x30.txt
    39     reviews/tui-graphite/final/dark-picker-sessions-160x40.txt
    15     reviews/tui-graphite/final/dark-picker-sessions-40x16.txt
    23     reviews/tui-graphite/final/dark-picker-sessions-80x24.txt
    29     reviews/tui-graphite/final/dark-reasoning-collapsed-100x30.txt
    39     reviews/tui-graphite/final/dark-reasoning-collapsed-160x40.txt
    15     reviews/tui-graphite/final/dark-reasoning-collapsed-40x16.txt
    23     reviews/tui-graphite/final/dark-reasoning-collapsed-80x24.txt
    29     reviews/tui-graphite/final/dark-reasoning-expanded-100x30.txt
    39     reviews/tui-graphite/final/dark-reasoning-expanded-160x40.txt
    15     reviews/tui-graphite/final/dark-reasoning-expanded-40x16.txt
    23     reviews/tui-graphite/final/dark-reasoning-expanded-80x24.txt
    29     reviews/tui-graphite/final/dark-running-100x30.txt
    39     reviews/tui-graphite/final/dark-running-160x40.txt
    15     reviews/tui-graphite/final/dark-running-40x16.txt
    23     reviews/tui-graphite/final/dark-running-80x24.txt
    29     reviews/tui-graphite/final/dark-single-tool-100x30.txt
    39     reviews/tui-graphite/final/dark-single-tool-160x40.txt
    15     reviews/tui-graphite/final/dark-single-tool-40x16.txt
    23     reviews/tui-graphite/final/dark-single-tool-80x24.txt
    29     reviews/tui-graphite/final/dark-tools-failed-100x30.txt
    39     reviews/tui-graphite/final/dark-tools-failed-160x40.txt
    15     reviews/tui-graphite/final/dark-tools-failed-40x16.txt
    23     reviews/tui-graphite/final/dark-tools-failed-80x24.txt
    29     reviews/tui-graphite/final/dark-tools-interrupted-100x30.txt
    39     reviews/tui-graphite/final/dark-tools-interrupted-160x40.txt
    15     reviews/tui-graphite/final/dark-tools-interrupted-40x16.txt
    23     reviews/tui-graphite/final/dark-tools-interrupted-80x24.txt
    29     reviews/tui-graphite/final/dark-tools-running-100x30.txt
    39     reviews/tui-graphite/final/dark-tools-running-160x40.txt
    15     reviews/tui-graphite/final/dark-tools-running-40x16.txt
    23     reviews/tui-graphite/final/dark-tools-running-80x24.txt
    29     reviews/tui-graphite/final/dark-tools-success-100x30.txt
    39     reviews/tui-graphite/final/dark-tools-success-160x40.txt
    15     reviews/tui-graphite/final/dark-tools-success-40x16.txt
    23     reviews/tui-graphite/final/dark-tools-success-80x24.txt
    29     reviews/tui-graphite/final/light-cancelled-100x30.txt
    39     reviews/tui-graphite/final/light-cancelled-160x40.txt
    15     reviews/tui-graphite/final/light-cancelled-40x16.txt
    23     reviews/tui-graphite/final/light-cancelled-80x24.txt
    29     reviews/tui-graphite/final/light-code-100x30.txt
    39     reviews/tui-graphite/final/light-code-160x40.txt
    15     reviews/tui-graphite/final/light-code-40x16.txt
    23     reviews/tui-graphite/final/light-code-80x24.txt
    29     reviews/tui-graphite/final/light-context75-100x30.txt
    39     reviews/tui-graphite/final/light-context75-160x40.txt
    15     reviews/tui-graphite/final/light-context75-40x16.txt
    23     reviews/tui-graphite/final/light-context75-80x24.txt
    29     reviews/tui-graphite/final/light-context95-100x30.txt
    39     reviews/tui-graphite/final/light-context95-160x40.txt
    15     reviews/tui-graphite/final/light-context95-40x16.txt
    23     reviews/tui-graphite/final/light-context95-80x24.txt
    29     reviews/tui-graphite/final/light-conversation-100x30.txt
    39     reviews/tui-graphite/final/light-conversation-160x40.txt
    15     reviews/tui-graphite/final/light-conversation-40x16.txt
    23     reviews/tui-graphite/final/light-conversation-80x24.txt
    29     reviews/tui-graphite/final/light-diff-100x30.txt
    39     reviews/tui-graphite/final/light-diff-160x40.txt
    15     reviews/tui-graphite/final/light-diff-40x16.txt
    23     reviews/tui-graphite/final/light-diff-80x24.txt
    29     reviews/tui-graphite/final/light-failure-100x30.txt
    39     reviews/tui-graphite/final/light-failure-160x40.txt
    15     reviews/tui-graphite/final/light-failure-40x16.txt
    23     reviews/tui-graphite/final/light-failure-80x24.txt
    29     reviews/tui-graphite/final/light-home-100x30.txt
    39     reviews/tui-graphite/final/light-home-160x40.txt
    15     reviews/tui-graphite/final/light-home-40x16.txt
    23     reviews/tui-graphite/final/light-home-80x24.txt
    29     reviews/tui-graphite/final/light-home-recent-100x30.txt
    39     reviews/tui-graphite/final/light-home-recent-160x40.txt
    15     reviews/tui-graphite/final/light-home-recent-40x16.txt
    23     reviews/tui-graphite/final/light-home-recent-80x24.txt
    29     reviews/tui-graphite/final/light-idle-100x30.txt
    39     reviews/tui-graphite/final/light-idle-160x40.txt
    15     reviews/tui-graphite/final/light-idle-40x16.txt
    23     reviews/tui-graphite/final/light-idle-80x24.txt
    29     reviews/tui-graphite/final/light-inspector-100x30.txt
    39     reviews/tui-graphite/final/light-inspector-160x40.txt
    15     reviews/tui-graphite/final/light-inspector-40x16.txt
    23     reviews/tui-graphite/final/light-inspector-80x24.txt
    29     reviews/tui-graphite/final/light-multi-turn-100x30.txt
    39     reviews/tui-graphite/final/light-multi-turn-160x40.txt
    15     reviews/tui-graphite/final/light-multi-turn-40x16.txt
    23     reviews/tui-graphite/final/light-multi-turn-80x24.txt
    29     reviews/tui-graphite/final/light-panel-100x30.txt
    39     reviews/tui-graphite/final/light-panel-160x40.txt
    15     reviews/tui-graphite/final/light-panel-40x16.txt
    23     reviews/tui-graphite/final/light-panel-80x24.txt
    29     reviews/tui-graphite/final/light-picker-100x30.txt
    39     reviews/tui-graphite/final/light-picker-160x40.txt
    15     reviews/tui-graphite/final/light-picker-40x16.txt
    23     reviews/tui-graphite/final/light-picker-80x24.txt
    29     reviews/tui-graphite/final/light-picker-files-100x30.txt
    39     reviews/tui-graphite/final/light-picker-files-160x40.txt
    15     reviews/tui-graphite/final/light-picker-files-40x16.txt
    23     reviews/tui-graphite/final/light-picker-files-80x24.txt
    29     reviews/tui-graphite/final/light-picker-models-100x30.txt
    39     reviews/tui-graphite/final/light-picker-models-160x40.txt
    15     reviews/tui-graphite/final/light-picker-models-40x16.txt
    23     reviews/tui-graphite/final/light-picker-models-80x24.txt
    29     reviews/tui-graphite/final/light-picker-sessions-100x30.txt
    39     reviews/tui-graphite/final/light-picker-sessions-160x40.txt
    15     reviews/tui-graphite/final/light-picker-sessions-40x16.txt
    23     reviews/tui-graphite/final/light-picker-sessions-80x24.txt
    29     reviews/tui-graphite/final/light-reasoning-collapsed-100x30.txt
    39     reviews/tui-graphite/final/light-reasoning-collapsed-160x40.txt
    15     reviews/tui-graphite/final/light-reasoning-collapsed-40x16.txt
    23     reviews/tui-graphite/final/light-reasoning-collapsed-80x24.txt
    29     reviews/tui-graphite/final/light-reasoning-expanded-100x30.txt
    39     reviews/tui-graphite/final/light-reasoning-expanded-160x40.txt
    15     reviews/tui-graphite/final/light-reasoning-expanded-40x16.txt
    23     reviews/tui-graphite/final/light-reasoning-expanded-80x24.txt
    29     reviews/tui-graphite/final/light-running-100x30.txt
    39     reviews/tui-graphite/final/light-running-160x40.txt
    15     reviews/tui-graphite/final/light-running-40x16.txt
    23     reviews/tui-graphite/final/light-running-80x24.txt
    29     reviews/tui-graphite/final/light-single-tool-100x30.txt
    39     reviews/tui-graphite/final/light-single-tool-160x40.txt
    15     reviews/tui-graphite/final/light-single-tool-40x16.txt
    23     reviews/tui-graphite/final/light-single-tool-80x24.txt
    29     reviews/tui-graphite/final/light-tools-failed-100x30.txt
    39     reviews/tui-graphite/final/light-tools-failed-160x40.txt
    15     reviews/tui-graphite/final/light-tools-failed-40x16.txt
    23     reviews/tui-graphite/final/light-tools-failed-80x24.txt
    29     reviews/tui-graphite/final/light-tools-interrupted-100x30.txt
    39     reviews/tui-graphite/final/light-tools-interrupted-160x40.txt
    15     reviews/tui-graphite/final/light-tools-interrupted-40x16.txt
    23     reviews/tui-graphite/final/light-tools-interrupted-80x24.txt
    29     reviews/tui-graphite/final/light-tools-running-100x30.txt
    39     reviews/tui-graphite/final/light-tools-running-160x40.txt
    15     reviews/tui-graphite/final/light-tools-running-40x16.txt
    23     reviews/tui-graphite/final/light-tools-running-80x24.txt
    29     reviews/tui-graphite/final/light-tools-success-100x30.txt
    39     reviews/tui-graphite/final/light-tools-success-160x40.txt
    15     reviews/tui-graphite/final/light-tools-success-40x16.txt
    23     reviews/tui-graphite/final/light-tools-success-80x24.txt
    29     reviews/tui-graphite/final/mono-cancelled-100x30.txt
    39     reviews/tui-graphite/final/mono-cancelled-160x40.txt
    15     reviews/tui-graphite/final/mono-cancelled-40x16.txt
    23     reviews/tui-graphite/final/mono-cancelled-80x24.txt
    29     reviews/tui-graphite/final/mono-code-100x30.txt
    39     reviews/tui-graphite/final/mono-code-160x40.txt
    15     reviews/tui-graphite/final/mono-code-40x16.txt
    23     reviews/tui-graphite/final/mono-code-80x24.txt
    29     reviews/tui-graphite/final/mono-context75-100x30.txt
    39     reviews/tui-graphite/final/mono-context75-160x40.txt
    15     reviews/tui-graphite/final/mono-context75-40x16.txt
    23     reviews/tui-graphite/final/mono-context75-80x24.txt
    29     reviews/tui-graphite/final/mono-context95-100x30.txt
    39     reviews/tui-graphite/final/mono-context95-160x40.txt
    15     reviews/tui-graphite/final/mono-context95-40x16.txt
    23     reviews/tui-graphite/final/mono-context95-80x24.txt
    29     reviews/tui-graphite/final/mono-conversation-100x30.txt
    39     reviews/tui-graphite/final/mono-conversation-160x40.txt
    15     reviews/tui-graphite/final/mono-conversation-40x16.txt
    23     reviews/tui-graphite/final/mono-conversation-80x24.txt
    29     reviews/tui-graphite/final/mono-diff-100x30.txt
    39     reviews/tui-graphite/final/mono-diff-160x40.txt
    15     reviews/tui-graphite/final/mono-diff-40x16.txt
    23     reviews/tui-graphite/final/mono-diff-80x24.txt
    29     reviews/tui-graphite/final/mono-failure-100x30.txt
    39     reviews/tui-graphite/final/mono-failure-160x40.txt
    15     reviews/tui-graphite/final/mono-failure-40x16.txt
    23     reviews/tui-graphite/final/mono-failure-80x24.txt
    29     reviews/tui-graphite/final/mono-home-100x30.txt
    39     reviews/tui-graphite/final/mono-home-160x40.txt
    15     reviews/tui-graphite/final/mono-home-40x16.txt
    23     reviews/tui-graphite/final/mono-home-80x24.txt
    29     reviews/tui-graphite/final/mono-home-recent-100x30.txt
    39     reviews/tui-graphite/final/mono-home-recent-160x40.txt
    15     reviews/tui-graphite/final/mono-home-recent-40x16.txt
    23     reviews/tui-graphite/final/mono-home-recent-80x24.txt
    29     reviews/tui-graphite/final/mono-idle-100x30.txt
    39     reviews/tui-graphite/final/mono-idle-160x40.txt
    15     reviews/tui-graphite/final/mono-idle-40x16.txt
    23     reviews/tui-graphite/final/mono-idle-80x24.txt
    29     reviews/tui-graphite/final/mono-inspector-100x30.txt
    39     reviews/tui-graphite/final/mono-inspector-160x40.txt
    15     reviews/tui-graphite/final/mono-inspector-40x16.txt
    23     reviews/tui-graphite/final/mono-inspector-80x24.txt
    29     reviews/tui-graphite/final/mono-multi-turn-100x30.txt
    39     reviews/tui-graphite/final/mono-multi-turn-160x40.txt
    15     reviews/tui-graphite/final/mono-multi-turn-40x16.txt
    23     reviews/tui-graphite/final/mono-multi-turn-80x24.txt
    29     reviews/tui-graphite/final/mono-panel-100x30.txt
    39     reviews/tui-graphite/final/mono-panel-160x40.txt
    15     reviews/tui-graphite/final/mono-panel-40x16.txt
    23     reviews/tui-graphite/final/mono-panel-80x24.txt
    29     reviews/tui-graphite/final/mono-picker-100x30.txt
    39     reviews/tui-graphite/final/mono-picker-160x40.txt
    15     reviews/tui-graphite/final/mono-picker-40x16.txt
    23     reviews/tui-graphite/final/mono-picker-80x24.txt
    29     reviews/tui-graphite/final/mono-picker-files-100x30.txt
    39     reviews/tui-graphite/final/mono-picker-files-160x40.txt
    15     reviews/tui-graphite/final/mono-picker-files-40x16.txt
    23     reviews/tui-graphite/final/mono-picker-files-80x24.txt
    29     reviews/tui-graphite/final/mono-picker-models-100x30.txt
    39     reviews/tui-graphite/final/mono-picker-models-160x40.txt
    15     reviews/tui-graphite/final/mono-picker-models-40x16.txt
    23     reviews/tui-graphite/final/mono-picker-models-80x24.txt
    29     reviews/tui-graphite/final/mono-picker-sessions-100x30.txt
    39     reviews/tui-graphite/final/mono-picker-sessions-160x40.txt
    15     reviews/tui-graphite/final/mono-picker-sessions-40x16.txt
    23     reviews/tui-graphite/final/mono-picker-sessions-80x24.txt
    29     reviews/tui-graphite/final/mono-reasoning-collapsed-100x30.txt
    39     reviews/tui-graphite/final/mono-reasoning-collapsed-160x40.txt
    15     reviews/tui-graphite/final/mono-reasoning-collapsed-40x16.txt
    23     reviews/tui-graphite/final/mono-reasoning-collapsed-80x24.txt
    29     reviews/tui-graphite/final/mono-reasoning-expanded-100x30.txt
    39     reviews/tui-graphite/final/mono-reasoning-expanded-160x40.txt
    15     reviews/tui-graphite/final/mono-reasoning-expanded-40x16.txt
    23     reviews/tui-graphite/final/mono-reasoning-expanded-80x24.txt
    29     reviews/tui-graphite/final/mono-running-100x30.txt
    39     reviews/tui-graphite/final/mono-running-160x40.txt
    15     reviews/tui-graphite/final/mono-running-40x16.txt
    23     reviews/tui-graphite/final/mono-running-80x24.txt
    29     reviews/tui-graphite/final/mono-single-tool-100x30.txt
    39     reviews/tui-graphite/final/mono-single-tool-160x40.txt
    15     reviews/tui-graphite/final/mono-single-tool-40x16.txt
    23     reviews/tui-graphite/final/mono-single-tool-80x24.txt
    29     reviews/tui-graphite/final/mono-tools-failed-100x30.txt
    39     reviews/tui-graphite/final/mono-tools-failed-160x40.txt
    15     reviews/tui-graphite/final/mono-tools-failed-40x16.txt
    23     reviews/tui-graphite/final/mono-tools-failed-80x24.txt
    29     reviews/tui-graphite/final/mono-tools-interrupted-100x30.txt
    39     reviews/tui-graphite/final/mono-tools-interrupted-160x40.txt
    15     reviews/tui-graphite/final/mono-tools-interrupted-40x16.txt
    23     reviews/tui-graphite/final/mono-tools-interrupted-80x24.txt
    29     reviews/tui-graphite/final/mono-tools-running-100x30.txt
    39     reviews/tui-graphite/final/mono-tools-running-160x40.txt
    15     reviews/tui-graphite/final/mono-tools-running-40x16.txt
    23     reviews/tui-graphite/final/mono-tools-running-80x24.txt
    29     reviews/tui-graphite/final/mono-tools-success-100x30.txt
    39     reviews/tui-graphite/final/mono-tools-success-160x40.txt
    15     reviews/tui-graphite/final/mono-tools-success-40x16.txt
    23     reviews/tui-graphite/final/mono-tools-success-80x24.txt
    13     reviews/tui-graphite/performance.txt
     5     reviews/tui-plain-runtime/.gitignore
    88     reviews/tui-plain-study/README.md
    29     reviews/tui-redesign/before/cancelled-100x30.txt
    39     reviews/tui-redesign/before/cancelled-160x40.txt
    15     reviews/tui-redesign/before/cancelled-40x16.txt
    23     reviews/tui-redesign/before/cancelled-80x24.txt
    29     reviews/tui-redesign/before/conversation-100x30.txt
    39     reviews/tui-redesign/before/conversation-160x40.txt
    15     reviews/tui-redesign/before/conversation-40x16.txt
    23     reviews/tui-redesign/before/conversation-80x24.txt
    29     reviews/tui-redesign/before/failure-100x30.txt
    39     reviews/tui-redesign/before/failure-160x40.txt
    15     reviews/tui-redesign/before/failure-40x16.txt
    23     reviews/tui-redesign/before/failure-80x24.txt
    29     reviews/tui-redesign/before/home-100x30.txt
    39     reviews/tui-redesign/before/home-160x40.txt
    15     reviews/tui-redesign/before/home-40x16.txt
    23     reviews/tui-redesign/before/home-80x24.txt
    29     reviews/tui-redesign/final/dark-cancelled-100x30.txt
    39     reviews/tui-redesign/final/dark-cancelled-160x40.txt
    15     reviews/tui-redesign/final/dark-cancelled-40x16.txt
    23     reviews/tui-redesign/final/dark-cancelled-80x24.txt
    29     reviews/tui-redesign/final/dark-code-100x30.txt
    39     reviews/tui-redesign/final/dark-code-160x40.txt
    15     reviews/tui-redesign/final/dark-code-40x16.txt
    23     reviews/tui-redesign/final/dark-code-80x24.txt
    29     reviews/tui-redesign/final/dark-context75-100x30.txt
    39     reviews/tui-redesign/final/dark-context75-160x40.txt
    15     reviews/tui-redesign/final/dark-context75-40x16.txt
    23     reviews/tui-redesign/final/dark-context75-80x24.txt
    29     reviews/tui-redesign/final/dark-context95-100x30.txt
    39     reviews/tui-redesign/final/dark-context95-160x40.txt
    15     reviews/tui-redesign/final/dark-context95-40x16.txt
    23     reviews/tui-redesign/final/dark-context95-80x24.txt
    29     reviews/tui-redesign/final/dark-conversation-100x30.txt
    39     reviews/tui-redesign/final/dark-conversation-160x40.txt
    15     reviews/tui-redesign/final/dark-conversation-40x16.txt
    23     reviews/tui-redesign/final/dark-conversation-80x24.txt
    29     reviews/tui-redesign/final/dark-diff-100x30.txt
    39     reviews/tui-redesign/final/dark-diff-160x40.txt
    15     reviews/tui-redesign/final/dark-diff-40x16.txt
    23     reviews/tui-redesign/final/dark-diff-80x24.txt
    29     reviews/tui-redesign/final/dark-failure-100x30.txt
    39     reviews/tui-redesign/final/dark-failure-160x40.txt
    15     reviews/tui-redesign/final/dark-failure-40x16.txt
    23     reviews/tui-redesign/final/dark-failure-80x24.txt
    29     reviews/tui-redesign/final/dark-home-100x30.txt
    39     reviews/tui-redesign/final/dark-home-160x40.txt
    15     reviews/tui-redesign/final/dark-home-40x16.txt
    23     reviews/tui-redesign/final/dark-home-80x24.txt
    29     reviews/tui-redesign/final/dark-home-recent-100x30.txt
    39     reviews/tui-redesign/final/dark-home-recent-160x40.txt
    15     reviews/tui-redesign/final/dark-home-recent-40x16.txt
    23     reviews/tui-redesign/final/dark-home-recent-80x24.txt
    29     reviews/tui-redesign/final/dark-idle-100x30.txt
    39     reviews/tui-redesign/final/dark-idle-160x40.txt
    15     reviews/tui-redesign/final/dark-idle-40x16.txt
    23     reviews/tui-redesign/final/dark-idle-80x24.txt
    29     reviews/tui-redesign/final/dark-panel-100x30.txt
    39     reviews/tui-redesign/final/dark-panel-160x40.txt
    15     reviews/tui-redesign/final/dark-panel-40x16.txt
    23     reviews/tui-redesign/final/dark-panel-80x24.txt
    29     reviews/tui-redesign/final/dark-picker-100x30.txt
    39     reviews/tui-redesign/final/dark-picker-160x40.txt
    15     reviews/tui-redesign/final/dark-picker-40x16.txt
    23     reviews/tui-redesign/final/dark-picker-80x24.txt
    29     reviews/tui-redesign/final/dark-reasoning-collapsed-100x30.txt
    39     reviews/tui-redesign/final/dark-reasoning-collapsed-160x40.txt
    15     reviews/tui-redesign/final/dark-reasoning-collapsed-40x16.txt
    23     reviews/tui-redesign/final/dark-reasoning-collapsed-80x24.txt
    29     reviews/tui-redesign/final/dark-reasoning-expanded-100x30.txt
    39     reviews/tui-redesign/final/dark-reasoning-expanded-160x40.txt
    15     reviews/tui-redesign/final/dark-reasoning-expanded-40x16.txt
    23     reviews/tui-redesign/final/dark-reasoning-expanded-80x24.txt
    29     reviews/tui-redesign/final/dark-running-100x30.txt
    39     reviews/tui-redesign/final/dark-running-160x40.txt
    15     reviews/tui-redesign/final/dark-running-40x16.txt
    23     reviews/tui-redesign/final/dark-running-80x24.txt
    29     reviews/tui-redesign/final/dark-single-tool-100x30.txt
    39     reviews/tui-redesign/final/dark-single-tool-160x40.txt
    15     reviews/tui-redesign/final/dark-single-tool-40x16.txt
    23     reviews/tui-redesign/final/dark-single-tool-80x24.txt
    29     reviews/tui-redesign/final/dark-tools-failed-100x30.txt
    39     reviews/tui-redesign/final/dark-tools-failed-160x40.txt
    15     reviews/tui-redesign/final/dark-tools-failed-40x16.txt
    23     reviews/tui-redesign/final/dark-tools-failed-80x24.txt
    29     reviews/tui-redesign/final/dark-tools-interrupted-100x30.txt
    39     reviews/tui-redesign/final/dark-tools-interrupted-160x40.txt
    15     reviews/tui-redesign/final/dark-tools-interrupted-40x16.txt
    23     reviews/tui-redesign/final/dark-tools-interrupted-80x24.txt
    29     reviews/tui-redesign/final/dark-tools-running-100x30.txt
    39     reviews/tui-redesign/final/dark-tools-running-160x40.txt
    15     reviews/tui-redesign/final/dark-tools-running-40x16.txt
    23     reviews/tui-redesign/final/dark-tools-running-80x24.txt
    29     reviews/tui-redesign/final/dark-tools-success-100x30.txt
    39     reviews/tui-redesign/final/dark-tools-success-160x40.txt
    15     reviews/tui-redesign/final/dark-tools-success-40x16.txt
    23     reviews/tui-redesign/final/dark-tools-success-80x24.txt
    29     reviews/tui-redesign/final/light-cancelled-100x30.txt
    39     reviews/tui-redesign/final/light-cancelled-160x40.txt
    15     reviews/tui-redesign/final/light-cancelled-40x16.txt
    23     reviews/tui-redesign/final/light-cancelled-80x24.txt
    29     reviews/tui-redesign/final/light-code-100x30.txt
    39     reviews/tui-redesign/final/light-code-160x40.txt
    15     reviews/tui-redesign/final/light-code-40x16.txt
    23     reviews/tui-redesign/final/light-code-80x24.txt
    29     reviews/tui-redesign/final/light-context75-100x30.txt
    39     reviews/tui-redesign/final/light-context75-160x40.txt
    15     reviews/tui-redesign/final/light-context75-40x16.txt
    23     reviews/tui-redesign/final/light-context75-80x24.txt
    29     reviews/tui-redesign/final/light-context95-100x30.txt
    39     reviews/tui-redesign/final/light-context95-160x40.txt
    15     reviews/tui-redesign/final/light-context95-40x16.txt
    23     reviews/tui-redesign/final/light-context95-80x24.txt
    29     reviews/tui-redesign/final/light-conversation-100x30.txt
    39     reviews/tui-redesign/final/light-conversation-160x40.txt
    15     reviews/tui-redesign/final/light-conversation-40x16.txt
    23     reviews/tui-redesign/final/light-conversation-80x24.txt
    29     reviews/tui-redesign/final/light-diff-100x30.txt
    39     reviews/tui-redesign/final/light-diff-160x40.txt
    15     reviews/tui-redesign/final/light-diff-40x16.txt
    23     reviews/tui-redesign/final/light-diff-80x24.txt
    29     reviews/tui-redesign/final/light-failure-100x30.txt
    39     reviews/tui-redesign/final/light-failure-160x40.txt
    15     reviews/tui-redesign/final/light-failure-40x16.txt
    23     reviews/tui-redesign/final/light-failure-80x24.txt
    29     reviews/tui-redesign/final/light-home-100x30.txt
    39     reviews/tui-redesign/final/light-home-160x40.txt
    15     reviews/tui-redesign/final/light-home-40x16.txt
    23     reviews/tui-redesign/final/light-home-80x24.txt
    29     reviews/tui-redesign/final/light-home-recent-100x30.txt
    39     reviews/tui-redesign/final/light-home-recent-160x40.txt
    15     reviews/tui-redesign/final/light-home-recent-40x16.txt
    23     reviews/tui-redesign/final/light-home-recent-80x24.txt
    29     reviews/tui-redesign/final/light-idle-100x30.txt
    39     reviews/tui-redesign/final/light-idle-160x40.txt
    15     reviews/tui-redesign/final/light-idle-40x16.txt
    23     reviews/tui-redesign/final/light-idle-80x24.txt
    29     reviews/tui-redesign/final/light-panel-100x30.txt
    39     reviews/tui-redesign/final/light-panel-160x40.txt
    15     reviews/tui-redesign/final/light-panel-40x16.txt
    23     reviews/tui-redesign/final/light-panel-80x24.txt
    29     reviews/tui-redesign/final/light-picker-100x30.txt
    39     reviews/tui-redesign/final/light-picker-160x40.txt
    15     reviews/tui-redesign/final/light-picker-40x16.txt
    23     reviews/tui-redesign/final/light-picker-80x24.txt
    29     reviews/tui-redesign/final/light-reasoning-collapsed-100x30.txt
    39     reviews/tui-redesign/final/light-reasoning-collapsed-160x40.txt
    15     reviews/tui-redesign/final/light-reasoning-collapsed-40x16.txt
    23     reviews/tui-redesign/final/light-reasoning-collapsed-80x24.txt
    29     reviews/tui-redesign/final/light-reasoning-expanded-100x30.txt
    39     reviews/tui-redesign/final/light-reasoning-expanded-160x40.txt
    15     reviews/tui-redesign/final/light-reasoning-expanded-40x16.txt
    23     reviews/tui-redesign/final/light-reasoning-expanded-80x24.txt
    29     reviews/tui-redesign/final/light-running-100x30.txt
    39     reviews/tui-redesign/final/light-running-160x40.txt
    15     reviews/tui-redesign/final/light-running-40x16.txt
    23     reviews/tui-redesign/final/light-running-80x24.txt
    29     reviews/tui-redesign/final/light-single-tool-100x30.txt
    39     reviews/tui-redesign/final/light-single-tool-160x40.txt
    15     reviews/tui-redesign/final/light-single-tool-40x16.txt
    23     reviews/tui-redesign/final/light-single-tool-80x24.txt
    29     reviews/tui-redesign/final/light-tools-failed-100x30.txt
    39     reviews/tui-redesign/final/light-tools-failed-160x40.txt
    15     reviews/tui-redesign/final/light-tools-failed-40x16.txt
    23     reviews/tui-redesign/final/light-tools-failed-80x24.txt
    29     reviews/tui-redesign/final/light-tools-interrupted-100x30.txt
    39     reviews/tui-redesign/final/light-tools-interrupted-160x40.txt
    15     reviews/tui-redesign/final/light-tools-interrupted-40x16.txt
    23     reviews/tui-redesign/final/light-tools-interrupted-80x24.txt
    29     reviews/tui-redesign/final/light-tools-running-100x30.txt
    39     reviews/tui-redesign/final/light-tools-running-160x40.txt
    15     reviews/tui-redesign/final/light-tools-running-40x16.txt
    23     reviews/tui-redesign/final/light-tools-running-80x24.txt
    29     reviews/tui-redesign/final/light-tools-success-100x30.txt
    39     reviews/tui-redesign/final/light-tools-success-160x40.txt
    15     reviews/tui-redesign/final/light-tools-success-40x16.txt
    23     reviews/tui-redesign/final/light-tools-success-80x24.txt
    29     reviews/tui-redesign/final/mono-cancelled-100x30.txt
    39     reviews/tui-redesign/final/mono-cancelled-160x40.txt
    15     reviews/tui-redesign/final/mono-cancelled-40x16.txt
    23     reviews/tui-redesign/final/mono-cancelled-80x24.txt
    29     reviews/tui-redesign/final/mono-code-100x30.txt
    39     reviews/tui-redesign/final/mono-code-160x40.txt
    15     reviews/tui-redesign/final/mono-code-40x16.txt
    23     reviews/tui-redesign/final/mono-code-80x24.txt
    29     reviews/tui-redesign/final/mono-context75-100x30.txt
    39     reviews/tui-redesign/final/mono-context75-160x40.txt
    15     reviews/tui-redesign/final/mono-context75-40x16.txt
    23     reviews/tui-redesign/final/mono-context75-80x24.txt
    29     reviews/tui-redesign/final/mono-context95-100x30.txt
    39     reviews/tui-redesign/final/mono-context95-160x40.txt
    15     reviews/tui-redesign/final/mono-context95-40x16.txt
    23     reviews/tui-redesign/final/mono-context95-80x24.txt
    29     reviews/tui-redesign/final/mono-conversation-100x30.txt
    39     reviews/tui-redesign/final/mono-conversation-160x40.txt
    15     reviews/tui-redesign/final/mono-conversation-40x16.txt
    23     reviews/tui-redesign/final/mono-conversation-80x24.txt
    29     reviews/tui-redesign/final/mono-diff-100x30.txt
    39     reviews/tui-redesign/final/mono-diff-160x40.txt
    15     reviews/tui-redesign/final/mono-diff-40x16.txt
    23     reviews/tui-redesign/final/mono-diff-80x24.txt
    29     reviews/tui-redesign/final/mono-failure-100x30.txt
    39     reviews/tui-redesign/final/mono-failure-160x40.txt
    15     reviews/tui-redesign/final/mono-failure-40x16.txt
    23     reviews/tui-redesign/final/mono-failure-80x24.txt
    29     reviews/tui-redesign/final/mono-home-100x30.txt
    39     reviews/tui-redesign/final/mono-home-160x40.txt
    15     reviews/tui-redesign/final/mono-home-40x16.txt
    23     reviews/tui-redesign/final/mono-home-80x24.txt
    29     reviews/tui-redesign/final/mono-home-recent-100x30.txt
    39     reviews/tui-redesign/final/mono-home-recent-160x40.txt
    15     reviews/tui-redesign/final/mono-home-recent-40x16.txt
    23     reviews/tui-redesign/final/mono-home-recent-80x24.txt
    29     reviews/tui-redesign/final/mono-idle-100x30.txt
    39     reviews/tui-redesign/final/mono-idle-160x40.txt
    15     reviews/tui-redesign/final/mono-idle-40x16.txt
    23     reviews/tui-redesign/final/mono-idle-80x24.txt
    29     reviews/tui-redesign/final/mono-panel-100x30.txt
    39     reviews/tui-redesign/final/mono-panel-160x40.txt
    15     reviews/tui-redesign/final/mono-panel-40x16.txt
    23     reviews/tui-redesign/final/mono-panel-80x24.txt
    29     reviews/tui-redesign/final/mono-picker-100x30.txt
    39     reviews/tui-redesign/final/mono-picker-160x40.txt
    15     reviews/tui-redesign/final/mono-picker-40x16.txt
    23     reviews/tui-redesign/final/mono-picker-80x24.txt
    29     reviews/tui-redesign/final/mono-reasoning-collapsed-100x30.txt
    39     reviews/tui-redesign/final/mono-reasoning-collapsed-160x40.txt
    15     reviews/tui-redesign/final/mono-reasoning-collapsed-40x16.txt
    23     reviews/tui-redesign/final/mono-reasoning-collapsed-80x24.txt
    29     reviews/tui-redesign/final/mono-reasoning-expanded-100x30.txt
    39     reviews/tui-redesign/final/mono-reasoning-expanded-160x40.txt
    15     reviews/tui-redesign/final/mono-reasoning-expanded-40x16.txt
    23     reviews/tui-redesign/final/mono-reasoning-expanded-80x24.txt
    29     reviews/tui-redesign/final/mono-running-100x30.txt
    39     reviews/tui-redesign/final/mono-running-160x40.txt
    15     reviews/tui-redesign/final/mono-running-40x16.txt
    23     reviews/tui-redesign/final/mono-running-80x24.txt
    29     reviews/tui-redesign/final/mono-single-tool-100x30.txt
    39     reviews/tui-redesign/final/mono-single-tool-160x40.txt
    15     reviews/tui-redesign/final/mono-single-tool-40x16.txt
    23     reviews/tui-redesign/final/mono-single-tool-80x24.txt
    29     reviews/tui-redesign/final/mono-tools-failed-100x30.txt
    39     reviews/tui-redesign/final/mono-tools-failed-160x40.txt
    15     reviews/tui-redesign/final/mono-tools-failed-40x16.txt
    23     reviews/tui-redesign/final/mono-tools-failed-80x24.txt
    29     reviews/tui-redesign/final/mono-tools-interrupted-100x30.txt
    39     reviews/tui-redesign/final/mono-tools-interrupted-160x40.txt
    15     reviews/tui-redesign/final/mono-tools-interrupted-40x16.txt
    23     reviews/tui-redesign/final/mono-tools-interrupted-80x24.txt
    29     reviews/tui-redesign/final/mono-tools-running-100x30.txt
    39     reviews/tui-redesign/final/mono-tools-running-160x40.txt
    15     reviews/tui-redesign/final/mono-tools-running-40x16.txt
    23     reviews/tui-redesign/final/mono-tools-running-80x24.txt
    29     reviews/tui-redesign/final/mono-tools-success-100x30.txt
    39     reviews/tui-redesign/final/mono-tools-success-160x40.txt
    15     reviews/tui-redesign/final/mono-tools-success-40x16.txt
    23     reviews/tui-redesign/final/mono-tools-success-80x24.txt
    86     reviews/tui-semantic-colors/README.md
    28     run_agent_bench.py
     7     rust-toolchain.toml
   115     scripts/check_doc_links.py
    16     scripts/crop.ps1
    33     scripts/install-windows.ps1
     5     scripts/preview-plain.ps1
    22     scripts/scan-cols.ps1
    21     scripts/scan-row.ps1
    24     scripts/scan-rows.ps1
  3528     src/agent.rs
   567     src/agent_repair_tests.rs
   519     src/app.rs
    25     src/atomic_file.rs
   760     src/attachments.rs
   373     src/cache_trace.rs
    48     src/catalog_file.rs
  1733     src/checkpoints.rs
   181     src/child_policy.rs
   620     src/cli.rs
   125     src/cli_events.rs
   733     src/commands.rs
   467     src/config.rs
  4194     src/context.rs
    49     src/context_report.rs
   303     src/context_state.rs
   394     src/convenience.rs
   423     src/desktop_host.rs
    31     src/diagnostics.rs
   413     src/diagnostics_bundle.rs
   124     src/events.rs
   111     src/external.rs
   490     src/file_edit.rs
    78     src/file_lock.rs
   571     src/history_tools.rs
   158     src/host_attachments.rs
    57     src/lib.rs
   921     src/main.rs
  2966     src/mcp.rs
   256     src/model.rs
  1099     src/model_catalog.rs
  1215     src/model_catalog/records.rs
   122     src/output.rs
   106     src/path_display.rs
    15     src/performance.rs
  5567     src/plugins.rs
   295     src/plugins/approval.rs
   474     src/plugins/context.rs
  1314     src/plugins/model.rs
   288     src/plugins/stream.rs
   379     src/plugins/task_control.rs
     8     src/ports.rs
    83     src/private_file.rs
  1044     src/profiles.rs
   648     src/prompt.rs
   111     src/protocol.rs
  5023     src/provider.rs
  1099     src/provider/anthropic.rs
   955     src/provider/responses.rs
  1432     src/session.rs
   978     src/sessions.rs
   279     src/setup.rs
  1554     src/shell.rs
   161     src/shell_redirect.rs
   328     src/skills.rs
   202     src/steering.rs
  1954     src/task_control.rs
   206     src/token_estimate.rs
  1347     src/tools.rs
   607     src/tui/ask.rs
   405     src/tui/attachments.rs
   383     src/tui/chrome.rs
   184     src/tui/command_catalog.rs
   310     src/tui/draft.rs
   955     src/tui/editor.rs
   393     src/tui/event_queue.rs
   275     src/tui/event_queue_tests.rs
   263     src/tui/gitinfo.rs
   284     src/tui/graphite_tests.rs
    83     src/tui/help.rs
   458     src/tui/highlight.rs
   619     src/tui/input_tests.rs
   114     src/tui/inspector.rs
   306     src/tui/links.rs
   172     src/tui/logo.rs
   947     src/tui/markdown.rs
  1001     src/tui/message_tests.rs
   113     src/tui/messages.rs
  4032     src/tui/mod.rs
    94     src/tui/navigation.rs
   158     src/tui/palette.rs
   117     src/tui/paste.rs
   571     src/tui/picker.rs
   183     src/tui/plain.rs
   748     src/tui/plain_tests.rs
    58     src/tui/plugin_panel.rs
   121     src/tui/preferences.rs
   314     src/tui/redesign_snapshots.rs
   512     src/tui/redesign_tests.rs
   268     src/tui/runtime_events.rs
   167     src/tui/semantic.rs
   635     src/tui/semantic_tests.rs
   457     src/tui/status.rs
   171     src/tui/terminal_feedback.rs
   422     src/tui/theme.rs
   254     src/tui/title.rs
   451     src/tui/tool_display.rs
   957     src/tui/tool_record_tests.rs
   854     src/tui/tool_records.rs
   564     src/tui/transcript.rs
   307     src/tui/transcript_panel.rs
  3984     src/tui/view.rs
  3686     src/tui/view_tests.rs
   139     src/update.rs
   515     src/usage.rs
   559     src/winsys.rs
   649     tests/architecture_backends.rs
   489     tests/architecture_repair_regressions.rs
    77     tests/attachment_import.rs
    21     tests/cli_broken_pipe.rs
   274     tests/cli_checkpoints.rs
   337     tests/cli_global_options.rs
   216     tests/cli_one_shot_sessions.rs
   142     tests/cli_steering_input.rs
    55     tests/cli_update.rs
   638     tests/context_budget_regressions.rs
   625     tests/context_compaction_e2e.rs
    60     tests/fixtures/plugins/timeline-note/note.js
    50     tests/fixtures/plugins/timeline-note/plugin.toml
   145     tests/image_gen_e2e.rs
   434     tests/instruction_steering.rs
   293     tests/interrupt_contract.rs
   589     tests/lifecycle_contract.rs
   571     tests/mcp_skills_e2e.rs
   298     tests/model_catalog_e2e.rs
   615     tests/model_protocol_mock_e2e.rs
  4427     tests/plugins_e2e.rs
    68     tests/plugins_performance.rs
   489     tests/project_memory_e2e.rs
    30     tests/release_workflow.rs
   255     tests/runtime_customization.rs
   304     tests/session_branches.rs
   133     tests/shell_roundtrip_e2e.rs
  1534     tests/task_control_e2e.rs
```


### 三层目录树（自动生成）

```text
.
- .env.example (undefined LF)
- .gitattributes (undefined LF)
+ .github/
  + .github/ISSUE_TEMPLATE/
    - .github/ISSUE_TEMPLATE/bug_report.yml (undefined LF)
    - .github/ISSUE_TEMPLATE/config.yml (undefined LF)
    - .github/ISSUE_TEMPLATE/feature_request.yml (undefined LF)
  - .github/pull_request_template.md (undefined LF)
  + .github/workflows/
    - .github/workflows/core.yml (undefined LF)
    - .github/workflows/desktop.yml (undefined LF)
    - .github/workflows/docs.yml (undefined LF)
    - .github/workflows/npm.yml (undefined LF)
    - .github/workflows/project-memory-release.yml (undefined LF)
    - .github/workflows/project-memory.yml (undefined LF)
    - .github/workflows/release.yml (undefined LF)
- .gitignore (undefined LF)
+ assets/
  - assets/kroma-mark-on-dark.svg (undefined LF)
  - assets/kroma-mark-on-light.svg (undefined LF)
+ benchmarks/
  - benchmarks/agent_task_suite.py (undefined LF)
  - benchmarks/architecture_contract.json (undefined LF)
  - benchmarks/architecture_contract.py (undefined LF)
  - benchmarks/ARCHITECTURE_P0_BASELINE.md (undefined LF)
  - benchmarks/astra_cancel_smoke.py (undefined LF)
  - benchmarks/astra_tui_profiles_smoke.py (undefined LF)
  - benchmarks/audit_pi_kroma_traces.py (undefined LF)
  - benchmarks/cli_automation_smoke.py (undefined LF)
  - benchmarks/engineering_compare.py (undefined LF)
  - benchmarks/EXPERIMENT_DIRECTIVE.md (undefined LF)
  - benchmarks/fix2_targeted_probe.py (undefined LF)
  - benchmarks/fusion_ablation.py (undefined LF)
  - benchmarks/fusion_final.py (undefined LF)
  - benchmarks/fusion_stress.py (undefined LF)
  - benchmarks/graphite_windows_smoke.cjs (undefined LF)
  - benchmarks/mock_profile_smoke.py (undefined LF)
  - benchmarks/NATIVE_KROMA_PI_SMOKE.md (undefined LF)
  - benchmarks/native_kroma_pi_smoke.py (undefined LF)
  - benchmarks/p0p1_fair.py (undefined LF)
  - benchmarks/p1_continuous.py (undefined LF)
  - benchmarks/P10_APEC_CONSOLIDATION_REPORT.md (undefined LF)
  - benchmarks/p10_config_regression.py (undefined LF)
  - benchmarks/P10_SIMPLIFICATION_REPORT.md (undefined LF)
  - benchmarks/p11_architecture_baseline.py (undefined LF)
  - benchmarks/P11_ARCHITECTURE_FOUNDATION_REPORT.md (undefined LF)
  - benchmarks/P12_ARCHITECTURE_REVIEW_REPAIRS.md (undefined LF)
  - benchmarks/P2_REVIEW.md (undefined LF)
  - benchmarks/p3_real_dev.py (undefined LF)
  - benchmarks/P3_REVIEW.md (undefined LF)
  - benchmarks/P4_EXECUTION_REVIEW.md (undefined LF)
  - benchmarks/p4_pi_compare.py (undefined LF)
  - benchmarks/P4_REPORT.md (undefined LF)
  - benchmarks/p4_tool_ablation.py (undefined LF)
  - benchmarks/p5_prompt_tool_factorial.py (undefined LF)
  - benchmarks/P5_PROMPT_TOOL_REPORT.md (undefined LF)
  - benchmarks/P6_REPORT.md (undefined LF)
  - benchmarks/P6_RUST_PROMPT_REPORT.md (undefined LF)
  - benchmarks/p6_rust_prompt.py (undefined LF)
  - benchmarks/p7_abf_full.py (undefined LF)
  - benchmarks/P7_FINAL_REPORT.md (undefined LF)
  - benchmarks/p8_live_regression.py (undefined LF)
  - benchmarks/P8_REPORT.md (undefined LF)
  - benchmarks/p9_apec_ab_live.rs (undefined LF)
  - benchmarks/P9_APEC_AB_REPORT.md (undefined LF)
  - benchmarks/p9_batch1_legacy_regression.py (undefined LF)
  - benchmarks/p9_batch1_regression.py (undefined LF)
  - benchmarks/P9_BATCH1_REPORT.md (undefined LF)
  - benchmarks/p9_batch2_regression.py (undefined LF)
  - benchmarks/P9_BATCH2_REPORT.md (undefined LF)
  - benchmarks/pi_kroma_complexity_matrix.py (undefined LF)
  - benchmarks/pi_kroma_extended.py (undefined LF)
  - benchmarks/pi_kroma_fix_crossover.py (undefined LF)
  - benchmarks/pi_kroma_full_compare.py (undefined LF)
  - benchmarks/pi_kroma_harness_ablation.py (undefined LF)
  - benchmarks/pi_kroma_randomized_crossover.py (undefined LF)
  + benchmarks/ponytail/
  - benchmarks/ponytail_tasks.py (undefined LF)
    - benchmarks/ponytail/A.txt (undefined LF)
    - benchmarks/ponytail/B.txt (undefined LF)
    - benchmarks/ponytail/C.txt (undefined LF)
    - benchmarks/ponytail/README.md (undefined LF)
    - benchmarks/ponytail/RESULTS.md (undefined LF)
  - benchmarks/PROMPT_ABLATION_REPORT.md (undefined LF)
  - benchmarks/prompt_ablation.py (undefined LF)
  + benchmarks/prompt_philosophy/
  - benchmarks/prompt_philosophy_ablation.py (undefined LF)
    - benchmarks/prompt_philosophy/B.txt (undefined LF)
    - benchmarks/prompt_philosophy/C.txt (undefined LF)
    - benchmarks/prompt_philosophy/README.md (undefined LF)
    - benchmarks/prompt_philosophy/RESULTS.md (undefined LF)
  + benchmarks/read_batching/
    - benchmarks/read_batching/append.txt (undefined LF)
    - benchmarks/read_batching/README.md (undefined LF)
  - benchmarks/README.md (undefined LF)
  - benchmarks/REAL_TASK_RELIABILITY_REVIEW.md (undefined LF)
  - benchmarks/test_evidence_loss_repro.py (undefined LF)
  - benchmarks/test_ponytail_ablation.py (undefined LF)
  - benchmarks/test_ponytail_tasks.py (undefined LF)
  - benchmarks/test_prompt_philosophy_ablation.py (undefined LF)
  - benchmarks/tool_ablation_live.py (undefined LF)
  - benchmarks/tool_batch_ablation.py (undefined LF)
  - benchmarks/tool_batch_followup.py (undefined LF)
  - benchmarks/tui_pty_smoke.py (undefined LF)
  - benchmarks/v7_pi_smoke_20261031.py (undefined LF)
- build.rs (undefined LF)
- Cargo.lock (undefined LF)
- Cargo.toml (undefined LF)
- CHANGELOG.md (undefined LF)
- CONTRIBUTING.md (undefined LF)
+ desktop/
  - desktop/.gitignore (undefined LF)
  - desktop/EXTENSION_PLAN.md (undefined LF)
  - desktop/index.html (undefined LF)
  - desktop/package-lock.json (undefined LF)
  - desktop/package.json (undefined LF)
  - desktop/PORTABLE-README.txt (undefined LF)
  - desktop/README.md (undefined LF)
  + desktop/scripts/
    - desktop/scripts/install-local.ps1 (undefined LF)
    - desktop/scripts/package-portable.ps1 (undefined LF)
  + desktop/src/
  + desktop/src-tauri/
    - desktop/src-tauri/build.rs (undefined LF)
    - desktop/src-tauri/Cargo.lock (undefined LF)
    - desktop/src-tauri/Cargo.toml (undefined LF)
    - desktop/src-tauri/tauri.bundle.conf.json (undefined LF)
    - desktop/src-tauri/tauri.conf.json (undefined LF)
    - desktop/src/app.css (undefined LF)
    - desktop/src/App.svelte (undefined LF)
    - desktop/src/main.ts (undefined LF)
    - desktop/src/vite-env.d.ts (undefined LF)
  - desktop/svelte.config.js (undefined LF)
  - desktop/tsconfig.json (undefined LF)
  - desktop/vite.config.ts (undefined LF)
+ docs/
  - docs/architecture.md (undefined LF)
  - docs/design-notes.md (undefined LF)
  - docs/goal-plugin-design.md (undefined LF)
  - docs/lifecycle.md (undefined LF)
  - docs/maintainer-operations.md (undefined LF)
  - docs/model-catalog.md (undefined LF)
  - docs/npm-distribution.md (undefined LF)
  - docs/performance.md (undefined LF)
  - docs/project-memory-history.md (undefined LF)
  - docs/project-memory.md (undefined LF)
  - docs/promotion-linux-do.md (undefined LF)
  - docs/quickstart.md (undefined LF)
  - docs/README.md (undefined LF)
  + docs/reference/
    - docs/reference/cli.md (undefined LF)
    - docs/reference/configuration.md (undefined LF)
  - docs/releasing.md (undefined LF)
  - docs/security.md (undefined LF)
  - docs/subagents.md (undefined LF)
+ examples/
  + examples/plugins/
    - examples/plugins/README.md (undefined LF)
  - examples/tool_ablation.rs (undefined LF)
- LICENSE (undefined LF)
+ npm/
  - npm/launcher.cjs (undefined LF)
  - npm/package.json (undefined LF)
  - npm/platforms.cjs (undefined LF)
  - npm/README.md (undefined LF)
  - npm/release.cjs (undefined LF)
  - npm/smoke.cjs (undefined LF)
  - npm/stage.cjs (undefined LF)
  + npm/test/
    - npm/test/launcher.test.cjs (undefined LF)
    - npm/test/platforms.test.cjs (undefined LF)
    - npm/test/registry.cjs (undefined LF)
    - npm/test/release.test.cjs (undefined LF)
    - npm/test/update.test.cjs (undefined LF)
  - npm/update.cjs (undefined LF)
- PLUGIN_ROADMAP.md (undefined LF)
- README.md (undefined LF)
- README.zh-CN.md (undefined LF)
+ reviews/
  - reviews/ASTRA_XHIGH_CLOSURE_2026-10-28.md (undefined LF)
  - reviews/AUDIT_AND_OFFICIAL_PLUGINS_2026-11-06.md (undefined LF)
  - reviews/BATCH_1_2_FIXES_2026-11-05.md (undefined LF)
  - reviews/BATCH_3_4_PERFORMANCE_ARCHITECTURE_2026-11-05.md (undefined LF)
  + reviews/batch-3-4/
    - reviews/batch-3-4/after.txt (undefined LF)
    - reviews/batch-3-4/baseline-initial.txt (undefined LF)
    - reviews/batch-3-4/before-checkpoints-tui.txt (undefined LF)
    - reviews/batch-3-4/checkpoint-path-assertion-failed.txt (undefined LF)
    - reviews/batch-3-4/clippy-initial-layout-failure.txt (undefined LF)
    - reviews/batch-3-4/clippy.txt (undefined LF)
    - reviews/batch-3-4/context-ablation.txt (undefined LF)
    - reviews/batch-3-4/core-tests.txt (undefined LF)
    - reviews/batch-3-4/desktop-check.txt (undefined LF)
    - reviews/batch-3-4/desktop-test-build.txt (undefined LF)
    - reviews/batch-3-4/desktop-tests.txt (undefined LF)
    - reviews/batch-3-4/document-check.txt (undefined LF)
    - reviews/batch-3-4/environment.json (undefined LF)
    - reviews/batch-3-4/tui-after.txt (undefined LF)
  + reviews/catalog-expansion/
    - reviews/catalog-expansion/chinese-families.json (undefined LF)
    - reviews/catalog-expansion/curated.json (undefined LF)
    - reviews/catalog-expansion/domestic-missing.json (undefined LF)
    - reviews/catalog-expansion/legacy-popular.json (undefined LF)
    - reviews/catalog-expansion/openai-google-claude.json (undefined LF)
    - reviews/catalog-expansion/other-vendors.json (undefined LF)
  - reviews/COMPACTION_CACHE_DIAGNOSIS_2026-11-05.md (undefined LF)
  - reviews/COMPRESSION_CACHE_DEEP_AUDIT_2026-11-05.md (undefined LF)
  + reviews/deep-review/
    - reviews/deep-review/continuation_probe.rs (undefined LF)
    - reviews/deep-review/continuation-fmt-output.txt (undefined LF)
    - reviews/deep-review/continuation-probe-output.txt (undefined LF)
    - reviews/deep-review/desktop-probe-output.txt (undefined LF)
    - reviews/deep-review/desktop-race.cjs (undefined LF)
    - reviews/deep-review/plugin-probe-output.txt (undefined LF)
    - reviews/deep-review/plugin-scan.cjs (undefined LF)
    - reviews/deep-review/repair-plan.md (undefined LF)
    - reviews/deep-review/report_verification.rs (undefined LF)
    - reviews/deep-review/review_probe.rs (undefined LF)
    - reviews/deep-review/rust-probe-output.txt (undefined LF)
    - reviews/deep-review/verification-original-rust-output.txt (undefined LF)
    - reviews/deep-review/verification-plugin-scan-output.txt (undefined LF)
    - reviews/deep-review/verification-rust-output.txt (undefined LF)
    - reviews/deep-review/verification-tests-output.txt (undefined LF)
    - reviews/deep-review/verification-ui-output.txt (undefined LF)
    - reviews/deep-review/verification-ui.cjs (undefined LF)
  - reviews/HARNESS_INTERACTION_RESEARCH_2026-11-05.md (undefined LF)
  - reviews/HARNESS_UX_FULL_COMPARISON_2026-11-05.md (undefined LF)
  - reviews/HTTP_524_RETRY_2026-11-04.md (undefined LF)
  - reviews/KROMA_COMPLEX_ENGINEERING_ROOT_CAUSE_2026-10-28.md (undefined LF)
  - reviews/KROMA_DEEP_REVIEW_2026-11-01.md (undefined LF)
  - reviews/KROMA_DEEP_REVIEW_VERIFICATION_2026-11-01.md (undefined LF)
  - reviews/KROMA_HARNESS_REPEATED_ABLATION_2026-10-28.md (undefined LF)
  - reviews/KROMA_PI_P0_RUST_TRACE_FORENSICS_2026-10-28.md (undefined LF)
  - reviews/KROMA_PI_RANDOMIZED_CROSSOVER_2026-10-28.md (undefined LF)
  - reviews/KROMA_PI_TOOL_ABLATION_2026-10-28.md (undefined LF)
  - reviews/KROMA_REVIEW_PLAN_2026-10-31.md (undefined LF)
  - reviews/KROMA-E1-CORE-EFFICIENCY-2026-10-26.md (undefined LF)
  - reviews/KROMA-PHASE2-PERFORMANCE-2026-10-26.md (undefined LF)
  - reviews/KROMA-RUST-FULL-REVIEW-2026-10-26.md (undefined LF)
  - reviews/MODEL_CATALOG_2026-11-04.md (undefined LF)
  - reviews/PLUGIN_ARCHITECTURE_FINAL_AUDIT_2026-11-06.md (undefined LF)
  - reviews/README.md (undefined LF)
  - reviews/RELEASE_PREPARATION_2026-11-05.md (undefined LF)
  + reviews/release-prep/
    - reviews/release-prep/clippy.txt (undefined LF)
    - reviews/release-prep/core-tests.txt (undefined LF)
    - reviews/release-prep/credential-scan.json (undefined LF)
    - reviews/release-prep/desktop-check.txt (undefined LF)
    - reviews/release-prep/document-check.txt (undefined LF)
    - reviews/release-prep/install-smoke.txt (undefined LF)
    - reviews/release-prep/npm-tests.txt (undefined LF)
    - reviews/release-prep/release-build.txt (undefined LF)
    - reviews/release-prep/stage.txt (undefined LF)
  - reviews/REVIEW_FIXES_2026-10-29.md (undefined LF)
  - reviews/THREE_OFFICIAL_PLUGINS_FINAL_DESIGN_ACCEPTANCE_2026-11-06.md (undefined LF)
  - reviews/TUI_DESIGN_RESEARCH_2026-10-29.md (undefined LF)
  - reviews/TUI_FULL_WIDTH_LINKS_2026-10-29.md (undefined LF)
  - reviews/TUI_GRAPHITE_PLAN.md (undefined LF)
  - reviews/TUI_P0_IMPLEMENTATION_2026-10-29.md (undefined LF)
  - reviews/TUI_P1_P2_DELIVERY_2026-10-29.md (undefined LF)
  - reviews/TUI_REDESIGN_2026-11-01.md (undefined LF)
  - reviews/TUI_REDESIGN_BATCH1_2026-11-01.md (undefined LF)
  - reviews/TUI_REDESIGN_BATCH2_2026-11-01.md (undefined LF)
  - reviews/TUI_REDESIGN_BATCH3_2026-11-01.md (undefined LF)
  - reviews/TUI_REDESIGN_WINDOWS_NUL_2026-11-01.md (undefined LF)
  - reviews/TUI_REWIND_FEASIBILITY_2026-11-01.md (undefined LF)
  - reviews/TUI_USABILITY_AND_LANGUAGE.md (undefined LF)
  + reviews/tui-display-modes/
    - reviews/tui-display-modes/.gitignore (undefined LF)
  + reviews/tui-graphite/
    - reviews/tui-graphite/.gitignore (undefined LF)
    - reviews/tui-graphite/performance.txt (undefined LF)
  + reviews/tui-plain-runtime/
    - reviews/tui-plain-runtime/.gitignore (undefined LF)
  + reviews/tui-plain-study/
    - reviews/tui-plain-study/README.md (undefined LF)
  + reviews/tui-redesign/
  + reviews/tui-semantic-colors/
    - reviews/tui-semantic-colors/README.md (undefined LF)
  - reviews/WINDOWS_P0_P1_2026-10-29.md (undefined LF)
- run_agent_bench.py (undefined LF)
- rust-toolchain.toml (undefined LF)
+ scripts/
  - scripts/check_doc_links.py (undefined LF)
  - scripts/crop.ps1 (undefined LF)
  - scripts/install-windows.ps1 (undefined LF)
  - scripts/preview-plain.ps1 (undefined LF)
  - scripts/scan-cols.ps1 (undefined LF)
  - scripts/scan-row.ps1 (undefined LF)
  - scripts/scan-rows.ps1 (undefined LF)
- SECURITY.md (undefined LF)
+ src/
  - src/agent_repair_tests.rs (undefined LF)
  - src/agent.rs (undefined LF)
  - src/app.rs (undefined LF)
  - src/atomic_file.rs (undefined LF)
  - src/attachments.rs (undefined LF)
  - src/cache_trace.rs (undefined LF)
  - src/catalog_file.rs (undefined LF)
  - src/checkpoints.rs (undefined LF)
  - src/child_policy.rs (undefined LF)
  - src/cli_events.rs (undefined LF)
  - src/cli.rs (undefined LF)
  - src/commands.rs (undefined LF)
  - src/config.rs (undefined LF)
  - src/context_report.rs (undefined LF)
  - src/context_state.rs (undefined LF)
  - src/context.rs (undefined LF)
  - src/convenience.rs (undefined LF)
  - src/desktop_host.rs (undefined LF)
  - src/diagnostics_bundle.rs (undefined LF)
  - src/diagnostics.rs (undefined LF)
  - src/events.rs (undefined LF)
  - src/external.rs (undefined LF)
  - src/file_edit.rs (undefined LF)
  - src/file_lock.rs (undefined LF)
  - src/history_tools.rs (undefined LF)
  - src/host_attachments.rs (undefined LF)
  - src/lib.rs (undefined LF)
  - src/main.rs (undefined LF)
  - src/mcp.rs (undefined LF)
  + src/model_catalog/
  - src/model_catalog.rs (undefined LF)
    - src/model_catalog/records.rs (undefined LF)
  - src/model.rs (undefined LF)
  - src/output.rs (undefined LF)
  - src/path_display.rs (undefined LF)
  - src/performance.rs (undefined LF)
  + src/plugins/
  - src/plugins.rs (undefined LF)
    - src/plugins/approval.rs (undefined LF)
    - src/plugins/context.rs (undefined LF)
    - src/plugins/model.rs (undefined LF)
    - src/plugins/stream.rs (undefined LF)
    - src/plugins/task_control.rs (undefined LF)
  - src/ports.rs (undefined LF)
  - src/private_file.rs (undefined LF)
  - src/profiles.rs (undefined LF)
  - src/prompt.rs (undefined LF)
  - src/protocol.rs (undefined LF)
  + src/provider/
  - src/provider.rs (undefined LF)
    - src/provider/anthropic.rs (undefined LF)
    - src/provider/responses.rs (undefined LF)
  - src/session.rs (undefined LF)
  - src/sessions.rs (undefined LF)
  - src/setup.rs (undefined LF)
  - src/shell_redirect.rs (undefined LF)
  - src/shell.rs (undefined LF)
  - src/skills.rs (undefined LF)
  - src/steering.rs (undefined LF)
  - src/task_control.rs (undefined LF)
  - src/token_estimate.rs (undefined LF)
  - src/tools.rs (undefined LF)
  + src/tui/
    - src/tui/ask.rs (undefined LF)
    - src/tui/attachments.rs (undefined LF)
    - src/tui/chrome.rs (undefined LF)
    - src/tui/command_catalog.rs (undefined LF)
    - src/tui/draft.rs (undefined LF)
    - src/tui/editor.rs (undefined LF)
    - src/tui/event_queue_tests.rs (undefined LF)
    - src/tui/event_queue.rs (undefined LF)
    - src/tui/gitinfo.rs (undefined LF)
    - src/tui/graphite_tests.rs (undefined LF)
    - src/tui/help.rs (undefined LF)
    - src/tui/highlight.rs (undefined LF)
    - src/tui/input_tests.rs (undefined LF)
    - src/tui/inspector.rs (undefined LF)
    - src/tui/links.rs (undefined LF)
    - src/tui/logo.rs (undefined LF)
    - src/tui/markdown.rs (undefined LF)
    - src/tui/message_tests.rs (undefined LF)
    - src/tui/messages.rs (undefined LF)
    - src/tui/mod.rs (undefined LF)
    - src/tui/navigation.rs (undefined LF)
    - src/tui/palette.rs (undefined LF)
    - src/tui/paste.rs (undefined LF)
    - src/tui/picker.rs (undefined LF)
    - src/tui/plain_tests.rs (undefined LF)
    - src/tui/plain.rs (undefined LF)
    - src/tui/plugin_panel.rs (undefined LF)
    - src/tui/preferences.rs (undefined LF)
    - src/tui/redesign_snapshots.rs (undefined LF)
    - src/tui/redesign_tests.rs (undefined LF)
    - src/tui/runtime_events.rs (undefined LF)
    - src/tui/semantic_tests.rs (undefined LF)
    - src/tui/semantic.rs (undefined LF)
    - src/tui/status.rs (undefined LF)
    - src/tui/terminal_feedback.rs (undefined LF)
    - src/tui/theme.rs (undefined LF)
    - src/tui/title.rs (undefined LF)
    - src/tui/tool_display.rs (undefined LF)
    - src/tui/tool_record_tests.rs (undefined LF)
    - src/tui/tool_records.rs (undefined LF)
    - src/tui/transcript_panel.rs (undefined LF)
    - src/tui/transcript.rs (undefined LF)
    - src/tui/view_tests.rs (undefined LF)
    - src/tui/view.rs (undefined LF)
  - src/update.rs (undefined LF)
  - src/usage.rs (undefined LF)
  - src/winsys.rs (undefined LF)
+ tests/
  - tests/architecture_backends.rs (undefined LF)
  - tests/architecture_repair_regressions.rs (undefined LF)
  - tests/attachment_import.rs (undefined LF)
  - tests/cli_broken_pipe.rs (undefined LF)
  - tests/cli_checkpoints.rs (undefined LF)
  - tests/cli_global_options.rs (undefined LF)
  - tests/cli_one_shot_sessions.rs (undefined LF)
  - tests/cli_steering_input.rs (undefined LF)
  - tests/cli_update.rs (undefined LF)
  - tests/context_budget_regressions.rs (undefined LF)
  - tests/context_compaction_e2e.rs (undefined LF)
  + tests/fixtures/
  - tests/image_gen_e2e.rs (undefined LF)
  - tests/instruction_steering.rs (undefined LF)
  - tests/interrupt_contract.rs (undefined LF)
  - tests/lifecycle_contract.rs (undefined LF)
  - tests/mcp_skills_e2e.rs (undefined LF)
  - tests/model_catalog_e2e.rs (undefined LF)
  - tests/model_protocol_mock_e2e.rs (undefined LF)
  - tests/plugins_e2e.rs (undefined LF)
  - tests/plugins_performance.rs (undefined LF)
  - tests/project_memory_e2e.rs (undefined LF)
  - tests/release_workflow.rs (undefined LF)
  - tests/runtime_customization.rs (undefined LF)
  - tests/session_branches.rs (undefined LF)
  - tests/shell_roundtrip_e2e.rs (undefined LF)
  - tests/task_control_e2e.rs (undefined LF)
```

