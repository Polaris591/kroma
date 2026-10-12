# Harness v0.6.1 代码审计修复报告（FIX_REPORT_v0.6.1）

本文档记录针对 5 路专家代码审计（`/opt/harness/docs/CODE_REVIEW_FIX_LIST.md`）发现的全部 P0/P1/P2 缺陷与假测试的逐项修复详情及回归验证结果。

---

## 一、组 1（`chat_completions.rs` & `sse.rs`）

1. **HTTPS/TLS 连接与 HTTP Chunked SSE 流式解码（P0）**
   - 在 `crates/harness-core/src/provider/chat_completions.rs` 中基于系统 `libssl / libcrypto`（OpenSSL 3.5）与 Tokio `AsyncFd` 实现了非阻塞 `OpenSslStream` 与 `MaybeTlsStream`：远端 `https://` 请求执行完整 TLS 握手、SNI 设置与主机名证书校验，本地回环 `http://127.0.0.1` / `localhost` / `::1` 保持 `TcpStream`。
   - SSE 流式分支新增 `Transfer-Encoding: chunked` 增量解码状态机，并在解析 `chunk_size` 时对单块与累计解码字节施加 `MAX_SSE_RESPONSE_BYTES`（2 MiB）硬上限，拦截 `7fffffff` 等超大块防止 OOM。
   - 整次请求尝试受 `budget.request_timeout` 截止时间约束，状态行、响应头、错误体、非流式响应体、Chunked 块与 SSE 流式读取均受 `budget.idle_timeout` 与字节上限双重保护。
   - SSE 流在 `assembler.saw_any_business_chunk() == false`（未收到任何业务 chunk）前断开或超时归类为 `AttemptFailure::Retryable`，收到业务 chunk 后断连归为 `AttemptFailure::Fatal`。

2. **`ReasoningBlock` 序列化兼容、多块签名隔离、缺失 `index` 槽位分配与 `finalize()` 严格校验（P1）**
   - `serialize_openai_request_body` 移除私有字段 `"reasoning_blocks"`，输出标准 `"reasoning_content"` 字符串，并在存在签名或加密块时输出兼容的 `"thinking"` 数组。
   - `SseStreamAssembler` 改为按块索引维护 `BTreeMap<usize, ReasoningBlock>`，防止多个 thinking 块的 `signature` 拼接损坏。
   - `tool_calls` 增量聚合在缺失 `index` 但携带新 `id` 时自动分配新槽位，避免串接进槽位 `0`。
   - `SseStreamAssembler::finalize` 在未收到 `data: [DONE]\n\n` 或残留未派发半截事件缓冲时直接返回 `Err(ProviderError::IncompleteStream)`，绝不强行派发半截缓冲。

3. **标准端口 `Host` 头与 URL 查询/片段拦截（P2）**
   - `http:80` 与 `https:443` 省略 `Host` 请求头的 `:port` 后缀。
   - `validate_base_url` 显式拦截含 `?` 或 `#` 的非法基础 URL。

---

## 二、组 2（`agent_loop.rs`, `context_projector.rs`, `loop_guard.rs`）

1. **`tool_call_id` 1:1 闭合与只读批次预检（P0）**
   - `crates/harness-core/src/agent_loop.rs`：`logger.log(EventPayload::ToolBatchReceived)` 在 `--strict-log` 失败退出前先调用 `Self::synthesize_remaining_results` 闭合已推入 `transcript` 的全部 `tool_calls`。
   - 并发只读子任务 `handle.await` 遇到 `JoinError`（取消或 panic）时向 `pre_resolved[local_i]` 填入合成错误 `ToolResultMessage`，杜绝 `None` 空洞。
   - 只读子批次预检触发 `max_tool_calls` 时立即合成剩余 `NotExecutedBudgetExhausted` 结果并返回 `MaxToolCalls`，不再 `spawn` 多余任务。

2. **`ContextProjector` 固定基线前缀保护、双视图内存同步释放与配对校验（P1/P2）**
   - `crates/harness-core/src/context_projector.rs`：`run_step_down_compaction` 仅扫描 `self.last_pinned_baseline_end.max(1)..tail_start`，并用 `self.last_pinned_baseline_end.max(idx + 1)` 单调推进基线，严禁二次篡改已发送给模型的固定基线前缀。
   - 废除独立的 `enforce_canonical_raw_memory_cap`，统一在 `run_step_down_compaction` 中同步替换 `canonical` 与 `projected_items` 的 `Arc<str>`。
   - `compute_working_set_pins` 改为 `turn_counter.saturating_sub(mod_turn) < WORKING_SET_PIN_TURNS`。
   - `notify_file_read` 使用 `self.compacted_file_paths.remove(rel_path)` 避免重复累加 `compaction_reread_count`。
   - `verify_tool_call_pairing` 移除 `expected_ids.is_empty()` 短路，精准拦截孤立 `ToolResultMessage`。

3. **`LoopGuard` 第 6 次乒乓拦截、编辑熔断解锁与路径归一化（P1）**
   - `crates/harness-core/src/loop_guard.rs`：删除第 5 次调用误杀正常 `read` 的 `is_fifth_ping_pong`，仅在完成 3 个完整周期（第 6 次调用 `[A,B,A,B,A]+B`）时触发 `PingPongCycleBlocked`。
   - `record_file_tool_outcome` 仅当 `streak >= MAX_EDIT_FAILURE_STREAK`（3 次）后才允许通过 `("read", true)` 解锁文件编辑熔断（`edit/write` 成功始终清零）。
   - 新增 `clean_relative_path` 对工具参数中的 `"path"` 与 `relative_path` 归一化，防止 `./` 绕过签名窗口与文件熔断。

---

## 三、组 3（`workspace.rs`, `read.rs`, `edit.rs`, `write.rs`）

1. **`InodeMutationLock` 统一锁键与空闲锁回收 + 软链目标敏感路径拦截（P0/P1）**
   - `crates/harness-core/src/workspace.rs`：`InodeMutationLock` 统一按规范化父目录 `(parent_dev, parent_ino, file_name)` 加锁，彻底解决新建文件与 `rename` 原子替换换新 Inode 导致的锁漂移；通过 RAII `InodeLockGuard` 在 `Arc::strong_count == 1` 时自动从 `HashMap` 回收空闲锁。
   - `resolve_existing_path` 与 `resolve_write_target` 对规范化路径及软链解析后的目标路径强制调用 `SensitivePathGuard::is_sensitive`，拦截指向 `.env` 等凭据的工作区内软链。

2. **`edit.rs` 双数行无尾换行 CRLF 判定与 Tier 1 换行切分统一（P1）**
   - `crates/harness-core/src/tools/edit.rs`：`dominant_eol` 判定修复为 `crlf_count >= lf_count && crlf_count > 0`；`normalize_crlf` 与 `find_tier1_exact_matches` 仅将 `\r\n` 归一化为 `\n`，与 `parse_raw_lines` 保持一致。

3. **`write.rs` RAII `TempFileGuard` 与原子 `if_not_exists` 提交（P1/P2）**
   - `crates/harness-core/src/tools/write.rs`：新增 `TempFileGuard` 确保未提交临时文件在 Drop 时自动清理；`no_clobber=true`（`if_not_exists=true`）时同样先写临时文件再通过 `fs::hard_link` + `fs::remove_file` 原子提交，避免写入中途失败残留半截目标文件。

4. **`read.rs` 8MiB 扫描上限退出与流式行截取（P2）**
   - `crates/harness-core/src/tools/read.rs`：简化 `scanned_bytes >= MAX_READ_SCAN_BYTES` 探测退出逻辑，改用流式行迭代按 `offset..offset+limit` 截取，避免全量 `.collect::<Vec<&str>>()` 内存放大，并在触发 8MiB 上限时保留续读提示。

---

## 四、组 4（`output_buffer.rs`, `bash.rs`, `ls.rs`, `find.rs`, `grep.rs`）

1. **`HeadTailBuffer` 头部封口 `head_sealed`、行对齐与落盘边界对齐（P0/P1）**
   - `crates/harness-core/src/tool/output_buffer.rs`：新增 `head_sealed: bool`，一旦首次切分入 Tail 立即封口 `head_buf`，防止后续短 ASCII 行插队到多字节中文行之前；移除 `push_to_tail` 中恒假的 `.filter(|&p| p < front.len())`；`init_spill_file` 与 `write_to_spill` 在 4MiB 边界按 `floor_char_boundary` 对齐。

2. **`bash.rs` 单次 `read` 200ms 管道排空超时、末行刷新与洪泛覆盖（P1）**
   - `crates/harness-core/src/tools/bash.rs`：通过 `child_exited_rx` 将 200ms `PIPE_DRAIN_TIMEOUT` 施加在子进程退出后的单次 `reader.read` 调用上（只要管道有数据可读即持续排空，仅当单次 `read` 阻塞满 200ms 才判定孙进程占住写端退出），退出循环后调用 `collector.finish()` 刷出未带 `\n` 的末行；排空后若 `total_observed_bytes > flood_cap` 则覆盖为 `StopTrigger::OutputFloodCap`。

3. **`find.rs / grep.rs / ls.rs` Glob `[]`/`{}`、`.gitignore` 反选、跳过目录防穿透与截断计数（P1）**
   - `crates/harness-core/src/tools/find.rs` & `ls.rs`：`compile_glob_to_regex` 支持字符类 `[...]` 与花括号 `{a,b}` 展开；`.gitignore` 支持子目录规则、`/` 锚定、目录限定与 `!` 反选；校验起始 `current_dir` 不穿透 `.git/target/node_modules`；敏感目录跳过时计入 `skipped_sensitive`；32KiB 截断提示输出实际展示条数 `shown`。

---

## 五、组 5（`runtime.rs`, `policy.rs`, `events.rs`, `main.rs`, `approval_ui.rs`, `golden_smoke.rs`）

1. **异步可取消审批、审批 TOCTOU 闭环、真实 Diff 预览、CLI 参数与日志降级告警（P0/P1/P2）**
   - `crates/harness-core/src/tool/runtime.rs`：使用 `tokio::task::spawn_blocking` + `tokio::select!` 监听 `cancel_rx` 执行交互式审批；审批前生成真实 Unified Diff / 命令预览；对比 `pre_approval_sha: Option<String>` 与 `now_sha: Option<String>`（覆盖新建文件与删除文件场景）并将校验后的 `expected_sha256` / `if_not_exists` 注入下游工具在持锁后二次核验。
   - `crates/harness-core/src/tool/policy.rs`：`PolicyMode::from_flags` 先校验 `--policy` 取值合法性，拦截 `--policy typo --allow-write`。
   - `crates/harness-cli/src/main.rs`：支持任意位置的 `--version | -V`；收到第二次 `ctrl_c()` 时立即 `std::process::exit(130)`。
   - `crates/harness-core/src/events.rs`：`EventLogger` 仅在首次降级时打印 `stderr` 告警，并对内存事件列表设上限。

2. **16 个端到端黄金场景全量实测化（P2）**
   - `crates/harness-core/tests/golden_smoke.rs`：彻底移除第 8–16 场景的 `_ =>` 假测试分支，将全部 16 个场景编写为独立端到端测试并补充严格断言。

---

## 六、回归测试验证

执行 `cargo test --workspace`，全套单元测试与集成测试 100% 通过且零编译警告：
- `harness` (bin unit tests): 1 passed
- `context_cache_stability`: 4 passed
- `edit_diagnostics`: 2 passed
- `filesystem_security`: 3 passed
- `gate0_config_and_events`: 3 passed
- `golden_smoke`: 1 passed (all 16 distinct end-to-end scenarios verified)
- `provider_contract`: 4 passed
- `shell_supervisor`: 4 passed
- `sse_contract`: 4 passed
- `tool_contracts`: 1 passed
