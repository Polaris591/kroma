# 对话回退（Rewind）可行性调研（2026-11-01）

**范围**：只读 `src/agent.rs`、`src/session.rs`、`src/sessions.rs`、`src/commands.rs`、`tests/session_branches.rs` 与 TUI 键位；**本文件不含实现代码**。结论基于当前源码静态阅读与既有测试，未在真机交互验证。

## 1. 结论摘要

- 核心（Agent）**已经支持"从更早一轮分叉"和"回退到更早一轮"**，语义是会话分支，不是文件快照：`/fork N` 生成新会话并切换过去；`/rewind N` 把当前会话截断到 #N，并把"未来"另存为一个兄弟会话；`/undo` 是"回退一轮"的简写（只回退对话）。
- 因此 TUI 侧**不需要新的分支机制**，只需要一个可发现、可预览、键位不冲突的入口，并明确提示"只改对话，不改文件"。
- 文件状态无法从对话自动恢复：agent 只有 Shell 工具，可改任意文件，核心也没有文件快照。用 `git stash create` 一类机制只能做**尽力而为**的只读快照，存在未跟踪/忽略/二进制/GC/并发写等明确风险；建议作为显式、可预览、默认关闭的附加能力，而不是回退的隐含保证。

## 2. 分支在核心里怎么表示

- `HistoryPoint { number, until, prompt, response }` 由 `history_points(messages)` 生成：从 #0（`until = 1`，"Start of conversation"）开始，每遇到**不带 tool_calls 的 assistant 消息**就产生一个点，`until = index + 1`，`prompt` 取该段最后一条 user 消息。也就是说**一个"检查点"= 一次完整结束的模型回答**；工具调用中间态不产生检查点。
- 取消也会产生检查点：`Agent::cancel()` 追加一条普通 assistant 文本（`TASK_CANCELLED_PREFIX …`），它没有 tool_calls，因此会被 `history_points` 视为一次结束。
- `fork(checkpoint)`：
  1. `session.ensure_writable()`；
  2. `number = checkpoint.unwrap_or(history.len()-1)`，`until = checkpoint_until(number)`（越界报 `no checkpoint #N`）；
  3. `catalog.new_path()` 生成新会话 id/路径；
  4. `context_factory.branch_state(...)` 基于 `messages[..until]` 复制上下文（压缩/外部上下文用 `fork_async` 变体）；
  5. `clone_session_captures(...)` 把前缀里引用的大输出捕获文件复制到新会话目录；
  6. 新会话 `reset(prefix)`、复制模型选择、`remember_active`、切换会话。
  源会话与其 WAL 不被修改。
- `rewind(checkpoint)`：先为**完整当前历史**创建 `backup` 兄弟会话（`branch_state` + `new_path`），再把当前会话替换为 `messages[..until]`；`until == messages.len()` 时报"已经在 #N，无需回退"。`undo()` 等价于回退最后一个检查点。
- 会话 id 形如 `s<nanos>-<serial>-<pid>`；每个分支是独立文件（`json` + `json.wal`），因此"分支"是**磁盘上的平行会话**，可在 `/sessions` 或 `/resume ID` 中互相切换；`tests/session_branches.rs` 已验证 fork 后两边历史独立、重启后仍可恢复、捕获文件随之复制。

## 3. 能否从"更早的一条用户消息"分叉

可以，但边界是**检查点**而不是任意消息：

- 用户消息与其后的完整回答共同构成一个检查点；`/fork <number>` 分叉到该回答末尾，继续对话即"另一条未来"。
- 若一轮里发生多次工具调用后仍未产出最终回答，**没有新检查点**，不能在这一轮中间分叉；这是核心现有的安全边界（避免把半截工具状态带入新分支）。
- 失败任务通常没有最终 assistant 回答 → 可能没有检查点；取消任务有（见上）。
- 外部上下文（`PersistedContext::External`）必须走 `_async` 变体；CLI 的 `/fork` `/rewind` 已使用 async 版本，TUI 若接入也必须使用 async 版本，且必须等当前任务结束后再调用（否则与正在运行的 Agent future 冲突）。
- 历史条数与 `until` 都来自持久化消息，因此重启、压缩后仍然一致；`session_branches` 覆盖了"fork 后原会话继续、再切回"的场景。

## 4. 入口与键位建议

现状键位约束：

- **Esc 只关闭弹层/通知/附件路径，永远不取消任务、不清空草稿**（`mod.rs` 的 `key_action` 与面板分支；测试 `escape_preserves_draft_and_dismisses_transient_feedback`、`completion_escape_preserves_draft_and_ctrl_c_still_cancels_active_task`）。
- Ctrl+C 是唯一的取消/清空/退出前缀；Ctrl+O/Ctrl+R 已被展开占用；Ctrl+P 是选择器；Alt+↑/↓ 是本次新增的轮次跳转；Alt+V 粘贴、Alt+Backspace 删词、Alt+Enter 换行；插件可用 Alt+字母。
- 选择器/面板打开时，按键被其独占。

建议（按优先级）：

1. **选择一个检查点**：复用既有搜索式浮层（`/history` 已有列表输出），做一个只读列表：`#3 · 12:04 · "修复失败测试" · 3 轮`，行动作两项——`Enter = fork 从这里继续`、`Ctrl+Shift+R = rewind 到这里`（或 `Tab` 切换动作）。入口用命令 `/rewind` 或 `Ctrl+Shift+H`（均未被占用；避免 `Ctrl+H`，它在许多终端等同 Backspace，也避免裸 Esc）；不新增裸 Esc 语义。
2. **就地回退一轮**：`Alt+←`（当前未占用，且与 Alt+↑/↓ 的"轮次"语义同族）；只在 **无弹层、无选择器、非附件路径、任务未运行** 时生效；执行前在页脚显示"将回退到第 N 轮，未来会保存为分支"，需要第二次确认（Enter 或再次 Alt+←），避免误触。
3. **不建议**把"双击 Esc"作为主入口：Esc 在终端里还承担取消 IME 组合、退出 bracketed paste、关闭通知等职责，部分终端/多路复用器会把 Esc 序列拆成两次按键；即便实现"输入框为空 + 空闲 + 300ms 内两次"，也会与用户"连按 Esc 逃出某种状态"的习惯冲突。若产品上仍想要，建议作为**可配置的次要入口**，并保留第一次 Esc 的原有"关闭通知"行为。

无论哪个入口，都必须：

- strip 掉当前选择/草稿的影响：回退只改会话，**不清空未发送草稿**（与 Esc/Ctrl+C 的既有语义一致），并在回退后调用 `load_history(agent.messages())` 重建 UI；
- 在页脚/通知里明确 `只回退对话，不改文件`，并给出备份会话 id（`/resume ID`）；
- 运行中禁用（不能与 Agent future 并发改 `messages`）；取消后要等 `cancel()` 落盘完成。

## 5. 文件状态：能不能自动回滚

**核心事实**：agent 只有 `shell` 工具，命令可改任意文件、可删除、可生成二进制；会话 WAL 只记录对话与工具结果，不记录文件系统。(`session.rs` 的持久化只有 messages/context/generation。)

可选方案与风险：

| 方案 | 能捕获 | 主要风险 |
|---|---|---|
| 每轮开始 `git stash create` | 已跟踪文件的修改（生成一个 commit 对象，不改变工作区） | 不包含未跟踪文件（`create` 不支持 `--include-untracked`）；不包含 ignored 文件；不记录"被删除的未跟踪文件"；对象不可达，`git gc --prune=now` 会清掉；恢复需要 `git stash apply`/`checkout`，可能冲突、属于破坏性操作；不是 git 仓库/大二进制/子模块时不可靠 |
| `git stash push -u` + 立即 `stash pop` | 同上 + 未跟踪 | 会真实移动工作区文件（哪怕立刻恢复），与并发写/文件锁/IDE 冲突；崩溃窗口会留下半恢复状态 |
| `git update-ref refs/kroma/rewind/<id> <stash-commit>` | 让快照对象不被 GC | 仍需解决未跟踪/ignored；refs 会污染仓库（需文档说明与清理命令） |
| `git diff > patch` / `git bundle` | 文本 diff | 二进制/权限/空目录不安全；应用补丁是破坏性操作 |
| 复制整个工作树（worktree/rsync） | 全部文件 | 成本与磁盘占用不可控；node_modules、target、媒体文件；符号链接/权限；大仓库不可行 |

**建议**：

- **第一版只做对话回退**（核心已具备），UI 明确写"文件不会回滚"。这也符合改版"不做审批 UI、不新建 runtime"的边界。
- 若确需文件保护，做**显式、可预览、默认关闭**的"为本轮创建 git 快照"：仅当目录是 git 仓库、且工作区干净度可判定时提供；用 `git stash create`（不改工作区）+ `update-ref refs/kroma/rewind/<session>-<n>` 保存对象；在回退确认页显示"将恢复 N 个文件 / 可能冲突"，并**永不自动执行**恢复；对未跟踪/ignored 文件在 UI 中显式标为"无法恢复"。
- 更稳妥的长期方案是把"文件快照"做成 Shell 工具之外的显式能力（例如受控的 `git commit-tree` 快照 + 只读比较），但那属于新的产品面，不在本次 TUI 改版范围。

## 6. 建议的落地顺序与测试

1. 只读检查点浮层 + `/history` 复用（无新机制）：测试选择器排序、键位独占、空历史、取消产生的检查点可见。
2. `fork` 接入（async 变体）：测试 fork 后源会话 WAL 不变、捕获文件复制、UI 历史重建、草稿保留。
3. `rewind`/`undo` 接入 + 二次确认：测试"已在 #N"报错、备份会话可 `/resume`、运行中禁用。
4. （可选）git 快照：临时仓库测试、无 git 时优雅缺失、超时、未跟踪文件标记、恢复前预览；不写默认路径。

不做：任意消息（工具调用中间态）分叉、自动文件回滚、隐式破坏性 `git checkout/reset`、把回退混入 Ctrl+C/Esc 的既有语义。
