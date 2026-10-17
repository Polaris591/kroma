### 代码审计报告（组 3：工作区边界、Inode 锁与 Read/Edit/Write 工具）

1. **[P0/P1] `InodeMutationLock` 在原子替换与新建文件时锁键漂移，导致并发互斥失效**
- **位置**：`workspace.rs:264-308` (`InodeMutationLock::compute_key` / `acquire`)、`write.rs:236-249` (`atomic_write_file`)
- **触发场景**：① 协程 A 对不存在的 `a.txt` 调用 `execute_write`，拿到 `NewFile(parent, "a.txt")` 锁并落盘生成 `ino=10`；此时协程 B 并发写/改 `a.txt`，`fs::metadata` 成功返回 `ExistingFile(dev, 10)`，两把锁 Key 不同，互斥直接失效。② 协程 A/B/C 并发 `execute_edit` 同一已存在文件（`ino=10`），A 完成 `atomic_write_file` 时通过 `rename` 将路径替换为临时文件的新 Inode（`ino=20`）；随后到达的 C 计算出 `ExistingFile(dev, 20)`，与刚从 `ino=10` 唤醒的 B 并发读写同一文件，造成更新丢失。此外 `self.locks` 从不清理旧 Inode Key，每次原子写都会永久泄漏一条 `HashMap` 记录。
- **修复方案**：改用规范化后的 `(parent_canonical_dev, parent_canonical_ino, file_name)` 作为统一锁键（或锁定后二次校验当前路径 `st_ino` 与持锁 Key 一致，不一致则重试），并在 `Arc::strong_count(&arc_mutex) == 1` 时从 `HashMap` 回收空闲锁。

2. **[P1] `WorkspaceCap` 仅在用户态拼接路径，`_root_dir_fd` 形同虚设，存在 TOCTOU 与符号链接绕过敏感路径检查**
- **位置**：`workspace.rs:38-53, 105-240` (`WorkspaceCap`)、`read.rs:62-76`、`sensitive_paths.rs:11-69`
- **触发场景**：① `_root_dir_fd` 从未使用，`resolve_existing_path` / `resolve_write_target` 在用户态逐级 `symlink_metadata` 后返回普通 `PathBuf`，后续 `File::open` / `fs::read` 之间若目录组件被并发替换为外部软链即可逃逸工作区。② `read.rs:62` 允许叶子软链（`forbid_leaf_symlink=false`），且 `resolve_existing_path` 未对 `canonical` 目标路径调用 `SensitivePathGuard::is_sensitive`，工作区内指向 `.env` 或 `.ssh/id_rsa` 的软链 `readme.txt` 可绕过上层路径名检查读取敏感凭据。
- **修复方案**：在 `resolve_existing_path` / `resolve_write_target` 对 `norm_rel` 及最终 `canonical` 路径强制调用 `SensitivePathGuard::is_sensitive`；基于 `_root_dir_fd` 使用 `openat2(..., RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`（或 `cap-std`）直接返回受限 `File` 句柄。

3. **[P1] `edit.rs` 独立 `\r` 换行与双数行无尾换行 CRLF 文件处理缺陷**
- **位置**：`edit.rs:198-203` (`execute_edit`)、`edit.rs:364-400` (`parse_raw_lines`)、`edit.rs:426-455` (`find_tier1_exact_matches`)
- **触发场景**：① 含 2 行且末尾无换行符的 CRLF 文件（`"a\r\nb"`），`file_lines.len() == 2`，`\r\n` 行数为 1，`1 * 2 > 2` 为 `false`，导致 `dominant_eol` 被误判为 `"\n"`，编辑后破坏原文件 CRLF 格式。② 文件含独立 `\r` 且行尾带空格（`"foo \rbar"`），Tier 1 将 `\r` 归一化为 `\n` 后因 `has_unmatched_trailing_ws_at_eol` 让位给 Tier 2，但 `parse_raw_lines` 仅按 `\n` 切行，导致 Tier 2 报 `MATCH_NOT_FOUND`。
- **修复方案**：
```rust
let crlf_count = file_lines.iter().filter(|l| l.eol == "\r\n").count();
let lf_count = file_lines.iter().filter(|l| l.eol == "\n").count();
let dominant_eol = if crlf_count > lf_count { "\r\n" } else { "\n" };
```
并在 `find_tier1_exact_matches` 中仅归一化 `\r\n`（去掉单独 `raw_bytes[i] == b'\r'` 转 `\n` 分支，与 `parse_raw_lines` 保持一致）。

4. **[P1/P2] `write.rs` 在 `if_not_exists=true` 写入失败或任务取消时残留损坏文件/临时文件**
- **位置**：`write.rs:214-258` (`atomic_write_file`)
- **触发场景**：① `no_clobber=true` 分支直接 `create_new` 打开目标路径写入，若 `write_all`/`sync_all` 因磁盘满失败，目标路径会残留半截损坏文件且未删除。② 常规分支无 RAII Drop Guard，若协程 panic/abort 会残留 `.harness-tmp-*`。
- **修复方案**：统一先写入同目录 `.harness-tmp-*` 并封装 `TempFileGuard`（`Drop` 时若未提交则 `remove_file`）；`no_clobber=true` 时用 ` nix::fcntl::renameat2(..., RENAME_NOREPLACE)` 或 `fs::hard_link` + `remove_file` 原子提交。

5. **[P2] `read.rs` 在 >8MiB 文件与持续增长文件上的流式截断及 `Vec<&str>` 内存放大**
- **位置**：`read.rs:125-213, 244-261` (`execute_read`)
- **触发场景**：① 文件初始 `st_size <= 4MiB`（`compute_hash=true`）但在读取中追加增长超过 8MiB 时，第 149、153 行两个 `break` 条件均为 `false`，导致循环无限读取且 `hit_scan_cap` 为 `false`（若 8MiB 边界截断中文 UTF-8 字符会误报 `BinaryOrInvalidUtf8`）。② 第 201 行对 8MiB 文本全量 `.split('\n').collect::<Vec<&str>>()`，若文件为 8MiB 连续 `\n`，单次调用额外分配约 134MiB 切片数组；且 >8MiB 文件读完前 8MiB 最后一行时 `more_lines_remain` 为 `false`，丢失截断续读提示。
- **修复方案**：将 `if scanned_bytes >= MAX_READ_SCAN_BYTES` 分支简化为只要后续仍有字节可读即设 `hit_scan_cap = true; break;`，并用流式行迭代器按 `offset..offset+limit` 截取，避免全量 `collect`。### 1. [P0] `--strict-log` 日志失败或并发任务 Panic 导致 `tool_call_id` 悬空（违反 1:1 闭合铁律）
- **位置**：`crates/harness-core/src/agent_loop.rs:319, 364-378`（`AgentRunner::run`）及 `537-583`（`AgentRunner::execute_tool_batch`）
- **触发场景**：
  1. 第 319 行已将含 `tool_calls` 的 `AssistantTurn` 推入 `transcript`，但第 364 行 `logger.log(ToolBatchReceived)` 在 `--strict-log` 下写盘失败时，第 369 行直接 `return self.finish_run(...)`，未调用 `synthesize_remaining_results`，导致全部 `tool_call_id` 悬空。
  2. 第 538 行 `if let Ok((local_i, msg, decision)) = handle.await` 静默忽略 `JoinError`（子任务 panic/取消），`pre_resolved[local_i]` 保持 `None`，第 548 行提交循环直接跳过该调用，破坏 1:1 配对并触发第 392 行 `debug_assert!` 崩溃。
  3. 第 470 行只读批次中途触发 `max_tool_calls` 时，仍继续 `spawn` 执行前序只读工具，浪费 I/O 后立即以 `MaxToolCalls` 终止。
- **修复方案**：
```rust
// 1) agent_loop.rs:364 ToolBatchReceived 失败时补齐合成结果
if let Err(e) = logger.log(EventPayload::ToolBatchReceived { .. }) {
    Self::synthesize_remaining_results(&tool_calls, ErrorCode::NotExecutedCancelled, "strict log failure", &mut transcript);
    return self.finish_run(TerminalReason::StrictLogFailure(e.to_string()), ...);
}
// 2) agent_loop.rs:537 处理 JoinError 兜底闭合
for ( (_, call, _, _), handle ) in tasks_to_run.iter().zip(join_handles) {
    match handle.await {
        Ok((local_i, msg, dec)) => { pre_resolved[local_i] = Some(msg); decisions[local_i] = Some(dec); }
        Err(e) => { /* 在 pre_resolved 填入 ToolOutcome::Error 合成结果，严禁留 None */ }
    }
}
```

### 2. [P1] `ContextProjector` 二次归档篡改已固定基线前缀 + 4MiB 内存上限假释放
- **位置**：`crates/harness-core/src/context_projector.rs:71-102`（`enforce_canonical_raw_memory_cap`）、`176-178, 250-305`（`run_step_down_compaction`）、`313-366`（`compute_working_set_pins`）
- **触发场景**：
  1. **基线前缀被二次篡改（击穿 KV Cache）**：第 1 次 Compaction 时某 `read`（如 `idx=2`）因最近 8 轮内有 `edit` 被 Working-Set Pinning 豁免，基线推进至 `last_pinned_baseline_end=9`。数轮后该文件超出 8 轮窗口并触发第 2 次 Compaction，`for idx in 1..tail_start`（第 269 行）从 `1` 重新扫描并原地将 `projected_items[2]` 替换为存根，篡改已发送的基线前缀；且第 299 行 `self.last_pinned_baseline_end = idx + 1` 未使用 `.max()` 会导致基线回退。
  2. **4MiB 内存从未释放且双视图分裂**：`projected_items` 与 `canonical` 各自持有 `Arc<str>` 强引用（计数为 2）。`enforce_canonical_raw_memory_cap` 仅替换 `canonical` 中的 `tr.content`（且序号误传 `idx + 1`），`projected_items` 仍持有原 64KiB `Arc<str>`，内存零释放且破坏未超水位线不改写原则。
  3. **回合计算偏差**：`agent_loop.rs:191` 先 `try_begin_turn()` 再调 `project()`，`effective_now.saturating_sub(mod_turn) <= 8` 实际豁免了 9 个回合；且第 65 行 `notify_file_read` 重读后未从 `compacted_file_paths` 移除路径，导致 `compaction_reread_count` 重复累加。
- **修复方案**：
```rust
// context_projector.rs:269 仅归档 last_pinned_baseline_end..tail_start 区间，且单调推进基线
let scan_start = self.last_pinned_baseline_end.max(1);
for idx in scan_start..tail_start { ... self.last_pinned_baseline_end = self.last_pinned_baseline_end.max(idx + 1); }
// 废除 project() 末尾独立的 enforce_canonical_raw_memory_cap，统一在 run_step_down_compaction 中同步替换双视图 Arc<str>
// compute_working_set_pins 使用已完成回合 turn_counter：turn_counter.saturating_sub(mod_turn) < WORKING_SET_PIN_TURNS
```

### 3. [P1] `LoopGuard` 乒乓检测在第 5 次调用误杀正常 `read` + 路径与失败计数绕过
- **位置**：`crates/harness-core/src/loop_guard.rs:37-47, 59-77, 100-127, 139-151`
- **触发场景**：
  1. **第 5 次调用误杀（仅 2 周期即拦截）**：标准流程 `read(A) -> edit(B,失败1) -> read(A) -> edit(B,失败2) -> read(A)` 中，第 5 次调用 `read(A)` 命中第 108 行 `is_fifth_ping_pong`（`[A,B,A,B]+A`），仅失败 2 次 `edit` 就直接拦截第 3 次正常的 `read` 自愈。
  2. **同文件连续 3 次编辑失败熔断绕过**：① 第 145 行 `("read", true)` 无条件清空 `per_file_edit_failures`，导致 `edit(参数1,败) -> read -> edit(参数2,败) -> read -> edit(参数3,败)` 永远记不到 3 次；② `compute_signature` 与 `relative_path` 未归一化路径，交替传入 `"src/a.rs"` 与 `"./src/a.rs"` 即可绕过签名窗口与文件熔断。
- **修复方案**：
```rust
// 1) loop_guard.rs:108 删除 is_fifth_ping_pong，仅保留完成第 3 周期（第 6 次调用 [A,B,A,B,A]+B）的 is_sixth_ping_pong
// 2) loop_guard.rs:145 仅当 streak >= MAX_EDIT_FAILURE_STREAK 时允许 ("read", true) 解锁熔断；("edit", true)|("write", true) 始终清零
// 3) 对 relative_path 及 JSON 中的 "path" 字段执行 clean_relative_path 归一化（剥离前导 "./" 与重复 "/"）
```

### 4. [P2] `verify_tool_call_pairing` 漏检孤立 `ToolResultMessage`（假测试断言）
- **位置**：`crates/harness-core/src/context_projector.rs:428-455`
- **触发场景**：当 `UserMessage` 或无工具调用的 `AssistantTurn`（`expected_ids` 为空）后紧跟孤立 `ToolResultMessage` 时，因条件写成 `if !expected_ids.is_empty() && expected_ids != seen_ids` 且末尾返回 `expected_ids.is_empty() || expected_ids == seen_ids`，函数恒返回 `true`。
- **修复方案**：删除三处 `expected_ids.is_empty()` 短路判断，统一改为 `if expected_ids != seen_ids { return false; }`，函数末尾改为 `expected_ids == seen_ids`。## Review

### 1. [P0] 缺失 TLS/HTTPS 栈且 SSE 流未解码 HTTP chunked 传输编码
- **位置**：`crates/harness-core/src/provider/chat_completions.rs:394-450, 542-586` (`execute_single_http_attempt`)
- **触发场景**：① `validate_base_url` 强制远端使用 `https://`（443 端口），但第 397 行直接用裸 `TcpStream::connect` 发送明文 HTTP/1.1 报文，连任何真实 HTTPS API 均握手失败；② 真实网关 SSE 响应均采用 `Transfer-Encoding: chunked`，但第 543 行仅在 `!stream` 分支调用 `read_chunked_body`，流式分支（560 行）直接将含十六进制 chunk 长度头（如 `1a4\r\n`）的原始字节喂给 `assembler.feed_bytes`，跨 chunk 分片时拼入 JSON 行内导致 `malformed JSON` 报错。
- **修复方案**：改用 `reqwest::Client::builder().redirect(Policy::none()).connect_timeout(budget.connect_timeout).build()`（启用 `rustls-tls` 与 `stream`），通过 `resp.bytes_stream()` 读取已由 HTTP 栈解包 chunked 的字节流喂入 `SseStreamAssembler`。

### 2. [P0] 非流式/Header/Chunked 读取无超时与内存上限导致 OOM 或挂死，且缺少单请求总超时
- **位置**：`crates/harness-core/src/provider/chat_completions.rs:479-554, 599-630` (`execute_single_http_attempt`, `read_chunked_body`)
- **触发场景**：① 第 481 行读 Header、516/528/548 行 `read_to_end` 完全未加 `idle_timeout` 与字节上限；② 第 617-621 行 `read_chunked_body` 解析 `chunk_size` 后直接 `vec![0u8; chunk_size]`，若服务端返回超大块（如 `7fffffff`）立即触发 OOM panic；③ 整次尝试未受 `budget.request_timeout`（180s）约束，且 SSE 在未收到任何业务 chunk 前断连也被标为 `Fatal` 而非按 §4.3 重试。
- **修复方案**：外层包裹 `tokio::time::timeout(budget.request_timeout, ...)`；所有 `reader` 包裹 `AsyncReadExt::take(MAX_SSE_RESPONSE_BYTES as u64)` 与 `idle_timeout`；SSE 异常时按 `assembler.saw_any_business_chunk()` 区分 `Fatal` 与 `Retryable`。

### 3. [P1] `ReasoningBlock` 回传注入非标字段触发 HTTP 400，且多块签名拼接损坏
- **位置**：`crates/harness-core/src/provider/chat_completions.rs:193-209` (`serialize_openai_request_body`)、`sse.rs:169-185` (`process_event_payload`)
- **触发场景**：第 205 行向 assistant 消息注入私有字段 `"reasoning_blocks": [...]`，OpenAI/DeepSeek/Claude 兼容端对未知字段直接拒绝并返回 HTTP 400；`sse.rs:175` 用 `push_str` 将多个 thinking 块的 `signature` 拼接为单字符串，破坏多块签名验证。
- **修复方案**：移除 wire 请求中的 `"reasoning_blocks"` 私有键，仅输出标准 `reasoning_content` 或 `thinking` 数组；`SseStreamAssembler` 按块索引维护 `Vec<ReasoningBlock>`。

### 4. [P1] SSE `tool_calls` 缺失 `index` 时错位拼接，且 `finalize` 违规解析半截事件
- **位置**：`crates/harness-core/src/provider/sse.rs:188-204, 214-226` (`process_event_payload`, `finalize`)
- **触发场景**：第 191 行 `unwrap_or(0)` 导致部分省略 `index` 的兼容流将多个工具的 `id`/`name`/`arguments` 串接进槽位 `0`（如 `name: "readedit"`）；第 215-225 行在流异常断开时无视 SSE 必须以 `\n\n` 结尾的规范，强行派发半截缓冲。
- **修复方案**：缺失 `index` 且携带新 `id` 时分配新槽位或报错 `InvalidToolBatch`；`finalize` 在未收到 `[DONE]` 时直接返回 `Err(ProviderError::IncompleteStream)`，删除残留半行强行派发逻辑。

### 5. [P2] 默认端口 `Host: host:443` 兼容性缺陷与 URL 查询参数未拦截
- **位置**：`crates/harness-core/src/provider/chat_completions.rs:58-82, 424` (`validate_base_url`, `execute_single_http_attempt`)
- **触发场景**：`Host: api.openai.com:443` 在部分严格 CDN/WAF 下报 400/421；`base_url` 含 `?query` 时会被拼成 `...?query/v1/chat/completions`。
- **修复方案**：标准端口（80/443）省略 `Host` 头 `:port`；`validate_base_url` 拦截 `?` 与 `#`。

- **Merge verdict**: BLOCK## Review
- Correct: `registry.rs:36-74`（`visible_definitions` / `lookup_for_dispatch`）与 `policy.rs:124-135`（`PolicyEngine::evaluate`）已在 Schema 暴露与 Runtime 分发双层拦截 `ReadOnly` 模式下的非只读工具。
- Finding: 见下方 P0/P1/P2 审计清单。
- Merge verdict: BLOCK

### 1. [P0] `approval_ui.rs:75` & `main.rs:209-214` (`request_approval` / `main`) — 审批阻塞 Tokio 线程且吞掉 Ctrl-C 导致进程死锁
- **触发场景**：终端交互模式触发审批提示 `[y/a/n]` 时用户按 `Ctrl-C`。`tokio::signal::ctrl_c()` 接管了进程 SIGINT，而主流程在异步上下文同步调用 `stdin.lock().read_line()` 阻塞且未监听 `cancel_rx`，导致 Ctrl-C 既无法中断 `read_line` 也无法退出进程。
- **修复方案**：在 `main.rs` 中收到第二次 `ctrl_c()` 时 `std::process::exit(130)`；在 `runtime.rs` 中用 `tokio::task::spawn_blocking` 执行 `prompt_approval` 并与 `cancel_rx.changed()` 做 `tokio::select!`，取消时立即返回 `NotExecutedCancelled`。

### 2. [P1] `runtime.rs:172-252` (` ToolRuntime::execute_call `) — 审批 TOCTOU 存在新建文件绕过、全量内存读取与锁空档
- **触发场景**：① 审批前目标文件不存在（`pre_approval_sha = None`），等待审批期间外部创建该文件或软链，第 222 行因 `before_sha` 为 `None` 跳过校验直接覆盖；② 审批前文件存在，审批期间被删除使得 `resolve_existing_path` 返回 `Err`，`if let Ok` 静默放行；③ 复核通过后到 `execute_edit`/`execute_write` 获取 `InodeMutationLock` 之间存在无锁空档，且 `fs::read` 无上限读取大文件可致 OOM。
- **修复方案**：
```rust
let now_sha = self.env.workspace().resolve_existing_path(rel, false)
    .ok().and_then(|(_, abs)| fs::read(abs).ok()).map(|b| sha256_hex16(&b));
if now_sha != pre_approval_sha {
    return (Self::into_message(call, ToolExecutionOutput::error(
        ErrorCode::StaleFile, "TOCTOU check failed".into(), ToolResultMeta::default()
    )), "toctou_stale".into());
}
```
并将校验后的 `expected_sha256` 注入 `execute_edit`/`execute_write` 在持锁后二次核验。

### 3. [P1] `runtime.rs:186-194` (`execute_call`) & `policy.rs:16-35` (`PolicyMode::from_flags`) — 审批预览未生成 Diff、非法 Policy 参数被忽略
- **触发场景**：① 交互审批 `edit/write/bash` 时，`diff_preview` 直接塞入原始 `raw_arguments` JSON，未按规范 §2.1 生成 Unified Diff 或提取命令；② 执行 `harness run --policy typo --allow-write` 时，`match (allow_write, allow_shell)` 抢先返回 `Ok(AllowWrite)`，吞掉非法 `--policy`。
- **修复方案**：`from_flags` 先校验 `matches!(base, None | Some("ask" | "read-only" | "allow-write" | "allow-shell"))`；审批前解析参数生成真实 Diff 预览。

### 4. [P2] `main.rs:32-104` (`parse_cli_args`) & `events.rs:200-238` (`EventLogger::log`) — CLI `--version` 位置受限、日志内存泄漏与降级刷屏
- **触发场景**：① `harness run --version` 报 `unrecognized argument`（仅在首个参数 peek 时识别 `-V`）；② `in_memory_events` 无界累积全部事件克隆，且降级后每次 `log()` 重复打印 `stderr` 告警。
- **修复方案**：在 `while` 循环内匹配 `"--version" | "-V"`；移除非测试环境的 `in_memory_events` 或设上限，仅在首次降级时打印 `stderr` 告警。

### 5. [P2] `tests/golden_smoke.rs:253-329` (`test_golden_smoke_16_end_to_end_scenarios`) — 场景 7 漏断言、场景 8–16 为恒真假测试
- **触发场景**：注释声称测试敏感路径恢复、Ping-Pong 恢复、行号前缀恢复等，但 `scenario_idx` 8..=16 全走同一个 `_ =>` 分支（仅重复执行 `write` + `read`），场景 7 也未断言 `ErrorCode::PolicyDenied` 及文件未落盘。
- **修复方案**：为场景 7 补充 `assert!(!ws.join("a.txt").exists())` 与错误码断言，将场景 8–16 拆分为真实触发 `SENSITIVE_PATH_BLOCKED`、`LoopGuard` 及行号前缀自愈的独立用例。### 1. [P0] `HeadTailBuffer::push_str` 多字节 UTF-8 跨边界导致 Head/Tail 乱序与落盘丢数据
- **位置**：`crates/harness-core/src/tool/output_buffer.rs:169-182` (`HeadTailBuffer::push_str`)、`224-256` (`init_spill_file`/`write_to_spill`)
- **触发场景**：当 `head_buf` 剩 1~2 字节（如 16382B）且收到以 3~4 字节 UTF-8 字符（如中文）开头的 `segment` 时，`floor_char_boundary(segment, remaining_head)` 返回 `0`。`head_buf` 未填满（仍为 16382B），该段却被推入 `tail_chunks`。后续若收到 ASCII 段（如 `"OK\n"`），因 `head_buf.len() < head_cap` 仍成立，新段会被塞进 `head_buf`（排在旧 `tail_chunks` 之前），造成输出与落盘流顺序错乱；同时 `head_buf` 差 2B 未满会导致 `total_observed_bytes == 65536` 时未触发 `spilled`，但 `tail_bytes (49154) > tail_cap (49152)` 已提前淘汰尾部块，导致 `init_spill_file` 漏写已淘汰数据。此外 `write_to_spill` 在 4MiB 边界直接按字节切片 `&bytes[..n]`，会截断多字节 UTF-8。
- **修复方案**：增加 `head_sealed: bool`；一旦发生 `split_idx < segment.len()` 切分入 Tail，立即置 `self.head_sealed = true`，后续全部进入 `push_to_tail`；触发落盘条件改为 `self.head_sealed && self.tail_bytes + segment.len() > self.tail_cap` 或总字节/行数超限；`write_to_spill` 使用 `floor_char_boundary` 对齐。

### 2. [P1] `HeadTailBuffer::push_to_tail` 行对齐条件恒假，始终在行中腰斩及丢尾行
- **位置**：`crates/harness-core/src/tool/output_buffer.rs:206-215` (`HeadTailBuffer::push_to_tail`)、`bash.rs:255-258` (`execute_bash`)
- **触发场景**：`StreamLineCollector` 遇到 `\n` 立即 flush，故 `tail_chunks` 中每个单行分片的 `\n` 均在末尾（`cut + pos + 1 == front.len()`）。第 210 行 `.filter(|&p| p < front.len())` 恒为 `false`，退回 `unwrap_or(cut)`，违反 §5.4“严格对齐换行符 `\n`”。此外 `bash.rs:256` 在 200ms 超时 `abort()` 读取任务时未调用 `collector.finish()`，丢失未带 `\n` 的末行缓冲。
- **修复方案**：移除 `.filter(|&p| p < front.len())`，当 `aligned_cut == front.len()` 时直接丢弃整块 `front`（不重新 `push_front("")`）；将 `StreamLineCollector` 放入 `Arc<Mutex<...>>`，在 `drain_res` 结束后显式调用 `finish`。

### 3. [P1] `execute_bash` 子进程秒退竞态：200ms 误杀活跃读取 & 16MiB 洪泛漏判为成功
- **位置**：`crates/harness-core/src/tools/bash.rs:196-260`, `277-318` (`execute_bash`)
- **触发场景**：①子进程快速输出数十 KiB~数 MiB 后立即退出（如 `cat big.log`），`child.wait()` 先赢下 `select!`。随后对整个任务施加总计 200ms 的 `PIPE_DRAIN_TIMEOUT`，若期间触发同步落盘 I/O 或调度延迟超过 200ms，正在活跃读取内核管道缓冲区的任务被直接 `abort()` 丢数据。②若子进程写满 >16MiB 后在管道排空前已退出，`trigger` 被记为 `NormalExit(0)`，排空阶段触发 `flood_notify` 无人接收，被截断的输出却返回 `exit_code=0` 成功。
- **修复方案**：将 200ms 超时加在子进程退出后的**单次 `reader.read(&mut chunk)` 调用**上（只要管道有数据可读即继续排空，仅当 `read` 阻塞满 200ms 才判定孙进程占住写端退出）；排空后检查 `buf.total_observed_bytes() > flood_cap`，若超限则覆盖 `trigger = StopTrigger::OutputFloodCap`。

### 4. [P1] `find.rs` / `grep.rs` / `ls.rs` 未按规格使用 `ignore::WalkBuilder` 且存在多处过滤绕过
- **位置**：`crates/harness-core/src/tools/find.rs:106-111, 135-152, 171-176, 193-255`、`ls.rs:117-121`
- **触发场景**：①未用 `ignore::WalkBuilder`（违反 §5.5）：`compile_glob_to_regex` 第 247 行把 `[]`、`{}` 转义成字面量，导致 `*.[oa]`、`*.{rs,toml}` 全部失效；`trimmed.trim_matches('/')` 抹掉了根锚定 `/foo` 与目录限定 `dir/`，且不读取子目录 `.gitignore`、忽略 `!` 反选规则。②显式传 `path: ".git"` 或 `"target"` 时，`walk_workspace_files` 未校验起始目录 `current_dir`，直接穿透内置跳过目录。③敏感目录（如 `.ssh/`）在 `find.rs:172` 因 `is_file() == false` 直接跳过但未累加 `skipped_sensitive`。④`ls.rs:119` 与 `find.rs:109` 按 32KiB 截断时错误打印 `limit` (200) 而非实际输出条数 `idx`。
- **修复方案**：按 §5.5 改用 `ignore::WalkBuilder`（设 `follow_links(false).git_global(false).git_exclude(false)`）；对起始 `current_dir` 执行相同的跳过/忽略校验；`ls/find` 截断提示改为实际输出计数 `shown`。