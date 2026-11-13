//! Bilingual help for the actual TUI bindings, independent of model prompts.
use super::preferences::Language;

pub(super) fn text(language: Language) -> &'static str {
    language.text(
        "Enter sends; Ctrl+J, Alt+Enter or Shift+Enter inserts a newline (terminal support varies).
Terminal paste keeps multiline text editable up to 4 KiB; larger text becomes an attachment.
Ctrl+V / Shift+Insert reads the local text clipboard if forwarded by the terminal; use terminal paste over SSH.
Shift+arrows/Home/End selects draft text; Ctrl+Shift+Home/End selects to the draft boundary.
Typing or pasting replaces the selection; Ctrl+Z restores the whole replacement.
PgUp/PgDn or wheel scrolls; Alt+↑/↓ jumps to previous/next user turn.
Ctrl+End returns to latest. Drafts survive navigation and Esc.
Type / or /commands for the index; Tab completes without executing.
Ctrl+P or Ctrl+L opens models; type / for commands, then Tab cycles picker categories.
Ctrl+R searches previously sent prompts; Enter recalls for editing without sending, Esc keeps the draft.
File choices complete the current directory, not a project-wide search.
/compact [instructions] optionally focuses the manual checkpoint on what matters.
F2 changes theme; Shift+Tab cycles thinking; /think high selects directly.
/language [en|zh] is saved.
Ctrl+O expands/collapses all tool commands and captured output inline; Ctrl+T previews reasoning; Alt+T opens the full transcript.
/inspect browses calls; click [view] for a read-only snapshot above the input.
Alt+T or /transcript opens searchable full history; /help, /hotkeys or ? on an empty draft opens this panel.
Esc closes overlays first; while an Agent runs, Esc interrupts and preserves the draft.
Click a tool chevron for full Command/Output in detailed display.
Successful output collapses; running calls show a short live preview, and failures and unknown results stay visible.
Click ⧉ Copy on a code label to copy raw code (including tabs and blank lines).
Rose marks your messages, blue marks Kroma; green/yellow/red indicate status.
Context is total-window use (~ estimated, — unknown); /usage has cache and token details.
Context turns yellow at 70%, red at 90%; /usage has API-token breakdowns.
Git status and per-turn /diff summaries are best effort, never inferred as zero.
Ctrl+Left/Right or Alt+B/F moves by word; Ctrl+W or Alt+Backspace deletes a word.
Ctrl+A/E jumps to line start/end, Ctrl+U/K deletes to line start/end, Ctrl+D deletes forward.
Ctrl+Z / Ctrl+Y undoes/redoes draft edits; Ctrl+G opens the external editor.
Alt+V pastes local clipboard media/text when idle; short text stays editable. Path fallback closes with Esc.
With a selection Ctrl+C copies first; otherwise clears a draft (Ctrl+Z restores), cancels a task or quits when idle and empty.
Esc never clears a draft; modal Esc closes the modal, running-task Esc cancels.
While running: Enter steers; Alt+Enter (Ctrl+Q on Windows) queues a follow-up; Esc or Ctrl+C cancels.
When idle and the draft is empty: Ctrl+D, Ctrl+Q, Ctrl+C or /exit quits; Ctrl+Q with a draft preserves it.
/history [count] · /sessions · /resume ID · /fork N · /rewind N · /undo
/checkpoints · /restore-files N (Git worktrees; files only)
/theme [dark|light|mono|reload] · /think [auto|off|low|medium|high|xhigh|max]
/queue task · /skills · /tools
Not included: approvals, sidebars, default split panes, card walls, complex animations or a new UI runtime.",
        "Enter 发送；Ctrl+J、Alt+Enter 或 Shift+Enter 换行（部分组合依赖终端支持）。
普通多行粘贴在 4 KiB 内保持可编辑，超过才折叠为附件。
终端转交按键时，Ctrl+V / Shift+Insert 读取本机文字剪贴板；SSH 请用终端自带粘贴。
Shift+方向键/Home/End 选择草稿；Ctrl+Shift+Home/End 选到草稿起点/终点。
输入或粘贴会替换选区；Ctrl+Z 一次撤销整次替换。
PgUp/PgDn 或滚轮翻页；Alt+↑/↓ 跳到上一轮/下一轮用户消息。
Ctrl+End 返回最新；导航与 Esc 均保留草稿。
输入 / 或 /commands 查看命令索引；↑↓选择，Tab 补全，不会直接执行。
Ctrl+P 或 Ctrl+L 选择模型；输入 / 搜索命令，在选择器内 Tab 切换类别。
Ctrl+R 搜索已发送的输入历史，Enter 取回编辑但不发送，Esc 保留当前草稿。
文件候选补全当前目录下的路径，不是全项目搜索。
/compact 后可追加说明，指定本次手动压缩的重点。
F2 切换主题；Shift+Tab 切换思考强度；/think high 直接选择。
/language [zh|en] 设置会保存。
Ctrl+O 在对话内展开或折叠全部工具命令与输出；Ctrl+T 预览思考；Alt+T 查看全文记录。
/inspect 浏览调用；点击 [检视] 在输入台上方查看只读执行快照。
Alt+T 或 /transcript 查看可搜索的全文记录；/help、/hotkeys 或空草稿按 ? 打开帮助弹层。
Esc 优先关闭弹层；任务执行时 Esc 取消任务，保留草稿。
详细显示中，点击工具箭头查看完整命令与输出。
成功输出默认收起；运行中显示少量实时输出，失败、警告及未知结果直接可见。
点击代码标签的 ⧉ 复制，原样复制代码（含 Tab、缩进和空行）。
粉色表示你，蓝色表示 Kroma；绿/黄/红仅表达状态。
上下文使用总窗口：~ 为估算、— 为未知；/usage 查看缓存和用量详情。
上下文达到 70% 变黄、90% 变红；/usage 查看 API 用量明细。
Git 状态与本轮 /diff 改动摘要尽力采集；未知不显示为零。
Ctrl+左右方向键或 Alt+B/F 按词移动；Ctrl+W 或 Alt+Backspace 删除前一个词。
Ctrl+A/E 移动到行首/行尾；Ctrl+U/K 删至行首/行尾；Ctrl+D 向前删除字符。
Ctrl+Z / Ctrl+Y 撤销/重做草稿；Ctrl+G 使用外部编辑器。
空闲时 Alt+V 粘贴本机图片或文本，短文本保持可编辑；附件路径输入用 Esc 关闭。
有选区时 Ctrl+C 优先复制；否则清空草稿（Ctrl+Z 恢复）、取消任务，或在空闲且空草稿时退出。
Esc 不清空草稿；弹层中 Esc 关闭弹层，执行任务时 Esc 取消任务。
运行中 Enter 引导；Alt+Enter（Windows 可用 Ctrl+Q）排队后续任务；Esc 或 Ctrl+C 取消。
空闲且草稿为空时 Ctrl+Q、Ctrl+C、Ctrl+D 或 /exit 退出；草稿未清空时 Ctrl+Q 不退出。
/history [count] · /sessions · /resume ID · /fork N · /rewind N · /undo
/theme [dark|light|mono|reload] · /think [auto|off|low|medium|high|xhigh|max]
/queue task · /skills · /tools
明确不做：审批界面、侧栏、默认双栏、全屏卡片墙、复杂动画、新 UI runtime。",
    )
}
