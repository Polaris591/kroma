//! Bilingual help for the actual TUI bindings, independent of model prompts.
use super::preferences::Language;

pub(super) fn text(language: Language) -> &'static str {
    language.text(
        "Enter sends; Shift+Enter, Alt+Enter or Ctrl+J inserts a newline.
PgUp/PgDn or wheel scrolls; Alt+↑/↓ jumps to previous/next user turn.
Ctrl+End returns to latest. Drafts survive navigation and Esc.
Type / or /commands for the index; Tab completes without executing.
Ctrl+P searches commands; Tab switches model/session/file categories.
File choices complete the current directory, not a project-wide search.
F2 changes theme; Shift+Tab cycles thinking; /think high selects directly.
/language [en|zh] and /layout [comfortable|wide] are saved settings.
Ctrl+O expands tool details; Ctrl+R expands every reasoning segment.
/inspect browses calls; click [view] for a read-only snapshot above the input.
Esc closes inspection and restores the draft; Ctrl+T opens searchable full history.
Click a group chevron for steps, a step chevron for Command/Output.
Successful groups collapse; failures, warnings and unknown results stay visible.
Long expanded groups show the latest 8 steps and always retain failures.
Click ⧉ Copy on a code label to copy raw code (including tabs and blank lines).
Rose marks your messages, blue marks Kroma; green/yellow/red indicate status.
Context is total-window use (~ estimated, — unknown); /usage has cache and token details.
Context turns yellow at 70%, red at 90%; /usage has API-token breakdowns.
Git status and per-turn /diff summaries are best effort, never inferred as zero.
Ctrl+Left/Right moves by word; Ctrl+W or Alt+Backspace deletes a word.
Ctrl+Z / Ctrl+Y undoes/redoes draft edits; Ctrl+G opens the external editor.
Alt+V pastes clipboard media/text; fallback attachment path closes with Esc.
With a selection Ctrl+C copies first; otherwise clears/cancels/quits.
Esc only closes popups/path input, never cancels an Agent task or clears a draft.
While running: Enter queues; /steer text redirects; only Ctrl+C cancels.
Ctrl+Q quits when idle; /exit also exits.
/history [count] · /sessions · /resume ID · /fork N · /rewind N · /undo
/theme [dark|light|mono|reload] · /think [auto|off|low|medium|high|xhigh|max]
/queue task · /skills · /tools
Not included: approvals, sidebars, default split panes, card walls, complex animations or a new UI runtime.",
        "Enter 发送；Shift+Enter、Alt+Enter 或 Ctrl+J 换行。
PgUp/PgDn 或滚轮翻页；Alt+↑/↓ 跳到上一轮/下一轮用户消息。
Ctrl+End 返回最新；导航与 Esc 均保留草稿。
输入 / 或 /commands 查看命令索引；↑↓选择，Tab 补全，不会直接执行。
Ctrl+P 搜索命令；Tab 切换模型、会话、文件类别。
文件候选补全当前目录下的路径，不是全项目搜索。
F2 切换主题；Shift+Tab 切换思考强度；/think high 直接选择。
/language [zh|en] 与 /layout [comfortable|wide] 设置会保存。
Ctrl+O 展开工具详情；Ctrl+R 展开每段思考摘要。
/inspect 浏览调用；点击 [检视] 在输入台上方查看只读执行快照。
Esc 关闭检视并保留草稿；Ctrl+T 查看可搜索的全文记录。
点击组头箭头展开步骤，点击步骤箭头查看执行命令和输出。
成功组默认折叠；失败、警告、中断原因无需展开即可见。
较长展开组显示最近 8 步，失败步骤始终保留。
点击代码标签的 ⧉ 复制，原样复制代码（含 Tab、缩进和空行）。
粉色表示你，蓝色表示 Kroma；绿/黄/红仅表达状态。
上下文使用总窗口：~ 为估算、— 为未知；/usage 查看缓存和用量详情。
上下文达到 70% 变黄、90% 变红；/usage 查看 API 用量明细。
Git 状态与本轮 /diff 改动摘要尽力采集；未知不显示为零。
Ctrl+左右方向键按词移动；Ctrl+W 或 Alt+Backspace 删除前一个词。
Ctrl+Z / Ctrl+Y 撤销/重做草稿；Ctrl+G 使用外部编辑器。
Alt+V 粘贴图片或文本；附件路径输入用 Esc 关闭。
有选区时 Ctrl+C 优先复制，否则清空草稿、取消任务或退出。
Esc 只关闭弹层或路径输入，不取消 Agent 任务、不清空草稿。
运行中 Enter 排队；/steer 文本发送引导；只有 Ctrl+C 取消任务。
空闲时 Ctrl+Q 退出；也可使用 /exit。
/history [count] · /sessions · /resume ID · /fork N · /rewind N · /undo
/theme [dark|light|mono|reload] · /think [auto|off|low|medium|high|xhigh|max]
/queue task · /skills · /tools
明确不做：审批界面、侧栏、默认双栏、全屏卡片墙、复杂动画、新 UI runtime。",
    )
}
