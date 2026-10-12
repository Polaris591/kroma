# V08 总装验收与全局安装报告（Assembly / Expert Fix）

日期：2026-10-12
工作区：`/opt/harness`（Cargo workspace，无 git 元数据）
二进制：`/usr/local/bin/harness`（由 `/opt/harness/target/debug/harness` 复制安装，mode 755）
最终结果：`cargo test --workspace` → **49 passed / 0 failed / 0 ignored，exit 0，0 警告**

---

## 一、总装前的初始状态

前 4 个并行修复 Worker 完成后，直接运行 `cargo test --workspace` 得到：

- 编译：成功，0 警告（`cargo build --workspace --all-targets` 全量重编译干净）。
- 测试：**1 个失败**（`--no-fail-fast` 确认全量只有这一处）：

```
Running tests/context_cache_stability.rs
test test_watermark_step_down_compaction_working_set_pinning_and_outline_stub ... FAILED
thread '...' panicked at crates/harness-core/tests/context_cache_stability.rs:329:5:
assertion failed: baseline_len_after_comp1 > 4
```

其余 48 个测试全部通过，无编译错误。

---

## 二、修复项

### 修复 1（阻断性）：`context_cache_stability` 断言编码了已被评审否决的旧行为

**定位过程**

1. 失败断言位于 `crates/harness-core/tests/context_cache_stability.rs:327-329`：
   ```rust
   // Verify second compaction 10 turns later does NOT mutate the previously pinned baseline prefix
   let baseline_len_after_comp1 = proj.breakpoints[2].message_index;
   assert!(baseline_len_after_comp1 > 4);
   ```
2. 实际值：第 1 次压缩（turn 4）后 `PinnedBaselineEnd.message_index == 4`。逐项复算
   `run_step_down_compaction`：`tail_start == 6`，`src/hot.rs` 在 turn 2 被 `edit`，`current_turn=4`
   仍处于 8 轮 Working-Set Pinning 窗口内，其 `read`（idx 4）与 `edit`（idx 5）被豁免折叠；
   `can_advance_baseline` 在遇到该未压缩、仍被豁免且 > 1 KiB 的热文件时置 `false`，因此
   `last_pinned_baseline_end` 停在 4，而不是越过它推进到 6。
3. 交叉比对评审产物
   `…/outputs/18d93410-…/review-v07-cache-context.md`：
   - Finding [P1]（第 36-49 行）明确指出旧行为是缺陷：“`last_pinned_baseline_end` 在扫描时无条件
     越过被 Working-Set Pinning 豁免的节点，导致‘8 轮临时豁免’蜕变为‘永久不可压缩’”，
     并点名 `resumed_compaction_releases_stale_hot_file_and_keeps_recent_file_pinned` 只跑了一次
     压缩、掩盖了多轮压缩下的永久锁死漏洞。
   - 建议 1.3（第 111 行）给出的正是 `can_advance_baseline` 方案。
   - 结论：Worker 1 的源码改动是评审批准的修复；`assert!(baseline_len_after_comp1 > 4)` 是**过期的
     测试断言**，它断言的是评审要求修掉的旧行为（Worker 1 的交接说明也已显式标注该测试需要由总装
     Worker 更新）。因此不改源码、只把测试对齐到已评审语义，且**加强**而非削弱断言。

**修复内容（`crates/harness-core/tests/context_cache_stability.rs`）**

- 将 `assert!(baseline_len_after_comp1 > 4)` 改为 `assert_eq!(baseline_len_after_comp1, 4)`，并写明
  语义：仍被豁免的热文件必须留在冻结基线之外，否则 8 轮临时豁免会变成永久不可压缩前缀。
- 保留“第二次压缩不得改写冻结基线前缀”的断言（`proj.messages[0..baseline_len_after_comp1]` 与
  `proj3.messages[0..baseline_len_after_comp1]` 逐项相等）。
- **新增更强断言**：turn 15（距 turn 2 的 `edit` 已 13 轮，超出 8 轮窗口）第二次压缩后，
  `proj3.messages[4]`（`src/hot.rs` 的 read）必须以 `[Compacted: read(src/hot.rs` 开头 —— 即该文件
  在豁免窗口过期后确实被回收，防止“永久僵尸前缀 Token”缺陷回归。

### 修复 2（任务指定）：把 `PromptBuilder` 工作区上下文接上 `AgentRunner`

**问题**：`PromptBuilder::with_workspace_context`（Worker 1 新增）与 `AgentRunner::with_prompt_builder`
都已存在，但 `crates/harness-cli/src/main.rs` 从未调用，真实 CLI 运行始终使用
`PromptBuilder::default()`，`AGENTS.md` / `CLAUDE.md` 与顶层仓库清单从未注入静态前缀
（评审 review-v07-cache-context.md 建议 3.1 要求的正是这条注入）。

**修复内容（`crates/harness-cli/src/main.rs`）**

- 新增 `use harness_core::prompt::PromptBuilder;`。
- 在 `LocalHostEnv` 创建之后、`env` 被 move 进 `build_default_runtime` 之前捕获根目录并构造：
  ```rust
  // Freeze the workspace orientation block (AGENTS.md/CLAUDE.md + sorted top-level entries)
  // into the static system prefix before `env` is moved into the tool runtime.
  let prompt_builder = PromptBuilder::default().with_workspace_context(env.workspace().root());
  ```
- 在 runner 构造链上接入：`AgentRunner::new(...).with_prompt_builder(prompt_builder).with_session_journal(...)`。

**关于 `PromptBuilder::new()` 的说明（未做 API 变更）**：任务描述写作 `PromptBuilder::new()`，
但现有签名是 `PromptBuilder::new(system_prompt: impl Into<String>)`，无无参版本。
`PromptBuilder::default()` 语义上等价于 `PromptBuilder::new(DEFAULT_SYSTEM_PROMPT)`，因此采用
`PromptBuilder::default().with_workspace_context(...)`，不新增/不修改任何 `pub` 签名（避免为一句
“顺手接上”而做破坏性 API 改动）。若后续确实要无参 `new()`，需要主控明确批准后另行处理。

---

## 三、全量验证证据

### 3.1 全量重编译（0 警告）

```
$ cargo clean -p harness-core -p harness-cli && cargo build --workspace --all-targets
   Compiling harness-core v0.6.0 (/opt/harness/crates/harness-core)
   Compiling harness-cli v0.6.0 (/opt/harness/crates/harness-cli)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 19.49s
```
（输出中无任何 `warning:` 行）

### 3.2 `cargo test --workspace`（任务要求的主验收命令）

```
$ cargo test --workspace          # exit 0, grep -ci warning = 0
     Running unittests src/main.rs (harness-cli)            3 passed
     Running unittests src/lib.rs  (harness-core)          18 passed
     Running tests/context_cache_stability.rs               4 passed
     Running tests/edit_diagnostics.rs                      2 passed
     Running tests/filesystem_security.rs                   4 passed
     Running tests/gate0_config_and_events.rs               3 passed
     Running tests/golden_smoke.rs                          1 passed
     Running tests/provider_contract.rs                     5 passed
     Running tests/shell_supervisor.rs                      4 passed
     Running tests/sse_contract.rs                          4 passed
     Running tests/tool_contracts.rs                        1 passed
     Doc-tests harness_core                                 0 passed
合计：49 passed / 0 failed / 0 ignored / 0 measured，0 warning，exit 0
```
（先以 `cargo test --workspace --no-fail-fast` 跑过一遍，结论一致：仅 1 处失败，修复后 0 失败。）

### 3.3 安装与二进制验证

```
$ install -m 755 /opt/harness/target/debug/harness /usr/local/bin/harness
$ ls -la /usr/local/bin/harness
-rwxr-xr-x 1 root root 57897704 2026-10-12 16:06:09 +0800 /usr/local/bin/harness
$ md5sum target/debug/harness /usr/local/bin/harness
90ec775873a82ad907581b7f264a970a  target/debug/harness
90ec775873a82ad907581b7f264a970a  /usr/local/bin/harness     # 安装二进制与构建产物一致
$ /usr/local/bin/harness --help      # exit 0，完整打印 Usage/Options/Environment/Security Notice
$ /usr/local/bin/harness --version   # harness 0.6.0
```
`--help` 已列出 `--resume / --fork / --verify-cmd / --no-verify / --tool-filter` 等全部选项。

### 3.4 端到端验证：工作区上下文确实注入到静态前缀

用本机 mock OpenAI 兼容端点（`http://127.0.0.1:8731`，loopback 明文 HTTP 被 `validate_base_url` 允许），
工作区含 `AGENTS.md`、`Cargo.toml`、`src/`，以只读策略运行安装后的二进制：

```
$ HARNESS_API_KEY=… HARNESS_BASE_URL=http://127.0.0.1:8731 HARNESS_MODEL=mock-model \
  XDG_STATE_HOME=/tmp/harness-verify/state \
  /usr/local/bin/harness run --workspace /tmp/harness-verify/ws --task "say ack" \
  --no-stream --no-verify --policy read-only --quiet
ack
=== exit=0 ===
```
抓取到的请求体（`/tmp/harness-verify/request.json`）中 system 消息（665 字符）经断言确认包含：
- `Workspace guidance (AGENTS.md):` 与 `AGENTS.md` 正文 `Always run cargo fmt before finishing.`
- `Workspace top-level entries:` 且列表为 `AGENTS.md / Cargo.toml / src`（排序、确定性）
- 工具 schema 仅暴露只读工具 `['find','grep','ls','read']`（只读策略下写/执行类工具被隐藏，行为未被本次改动影响）

即：`PromptBuilder::with_workspace_context` → `AgentRunner::with_prompt_builder` 的接线在真实 CLI 路径上生效。

---

## 四、前 4 个 Worker 改动的总装结论

以下文件均由并行 Worker 修改，本次总装**未回退任何一处**；全部通过编译与全量测试：

- `crates/harness-core/src/session.rs`、`context_projector.rs`、`prompt.rs`（Worker 1）
- `crates/harness-core/src/completion_gate.rs`、`tools/bash.rs`、`tool/policy.rs`、`agent_loop.rs`
  （Worker 2，P1 阶段 + 本轮）
- `crates/harness-core/src/workspace.rs`、`sensitive_paths.rs`、`tools/read.rs`、`tools/edit.rs`、
  `tools/write.rs`（Worker 3）
- `crates/harness-core/src/provider/sse.rs`、`provider/chat_completions.rs`、`events.rs`（Worker 4）
- `crates/harness-cli/src/main.rs`（Worker 4 + 本次总装接线）

本次总装只改了 2 个源文件（`crates/harness-cli/src/main.rs`、
`crates/harness-core/tests/context_cache_stability.rs`）并重新安装二进制，未扩大改动范围。

---

## 五、残留风险 / 说明

1. **无 git 仓库**：`/opt/harness` 下没有 `.git`，`git status` 报 `not a repository`，因此不存在
   staged 文件，也无法提供 commit/diff 证据；所有改动以文件 mtime + 内容为准。
2. **测试语义调整的依据**：`PinnedBaselineEnd` 采用“冻结前缀的排他上界（长度）”语义。本次把
   该测试断言从 `> 4` 改为 `== 4` 是**跟随已评审结论**（review-v07-cache-context.md Finding P1 +
   建议 1.3）而非新决策；如主控希望改为“闭区间最后一条消息下标”（评审建议 2 的另一套语义），
   需要单独批准，因为它会同时影响 4 个断点的语义与断言。
3. **`with_workspace_context` 的边界**：注入块自身封顶 4096 B，但看不到
   `BudgetConfig::max_static_prefix_bytes`；真实运行中静态前缀会因此增长（本次 mock 验证为 665 字符
   system 消息）。若后续把 `max_static_prefix_bytes` 调低到接近 8 KiB，需要复核前缀预算。
4. **`can_advance_baseline` 的语义后果**：被豁免热文件在被回收前会停留在“非冻结区”，
   即从 `PinnedBaselineEnd` 之后开始的那段前缀可能在后续压缩时变化（KV-Cache 从该断点后失效）。
   这是评审明确接受的取舍（换取大热文件可回收）；`compaction_reread_count` 继续度量其后悔率。
5. **未运行 clippy**（任务只要求 0 警告 + 全量测试通过）；`cargo build --all-targets` 全量重编译已确认
   rustc 0 警告。

---

## 六、结论

- `cargo test --workspace`：**49/49 通过，0 失败，0 警告，exit 0**。
- `/usr/local/bin/harness` 已安装（755，与 `target/debug/harness` md5 一致），`--help` / `--version` 正常，
  并已通过 mock provider 端到端验证工作区上下文注入。
- 无未解决阻塞项。
