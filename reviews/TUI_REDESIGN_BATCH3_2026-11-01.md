# Kroma TUI 改版 · 批次 3 交付报告（2026-11-01）

范围：`D:\Kroma\kroma-latest`，工作树内改动，未 commit / push。批次 3：按轮次跳转（3.1）、点击复制代码块（3.2）、帮助/文档同步（3.3）、对话回退调研（3.4）、可选全文记录浮层（3.5）。

## 1. 键位冲突核查

先读了 `editor.rs` 的全部按键与 `mod.rs` 的 `key_action` / 鼠标分支：

- 编辑器已用：Enter / Shift+Enter / Alt+Enter / Ctrl+J、Ctrl+Z/Y/A/E/U/K/W、Ctrl+←/→、Alt+Backspace、Tab、Home/End、↑/↓、Backspace/Delete。
- 全局已用：Ctrl+C（复制→清空/取消/退出）、Ctrl+Q、Ctrl+G、Ctrl+O、Ctrl+R、Ctrl+P、F2、Shift+Tab、PgUp/PgDn、Ctrl+End、Alt+V、Esc（只关弹层/通知/附件路径）。
- **Alt+↑ / Alt+↓ 未被占用**（Alt 只用于 Enter、Backspace、V 和插件 Alt+字母），因此作为轮次跳转键；实现里额外要求"无 picker、无插件面板、非附件路径模式"。
- 新增 **Ctrl+T** 打开全文浮层（原未占用）；浮层内 `/` 进入搜索、`n`/`N` 跳转，其余键仍由既有面板逻辑处理。

## 2. 实现要点

- **按轮次跳转（3.1）**：`src/tui/navigation.rs::jump_turn`。目标行不靠视觉行号硬编码：`Decor::TurnStart(entry_index)` 记录每个用户块的起始 source row，换行后用 `source_lines` 重映射；`draw` 在新的缓存里找到该 TurnStart 的视觉行并 `scroll_to_row`。历史窗口不足时自动扩大 `history_visible` 并清缓存。`navigation_status` 增加 `Turn 3/7 · Ctrl+End 返回最新`（中文 `第 3/7 轮 · …`）。浏览时使用既有"正在浏览历史"提示行；`Ctrl+End` 或任意滚动清除轮次态。
- **点击复制（3.2）**：代码块 `CodeLabel` 装饰在右上角画 `语言 · ⧉ Copy`；鼠标悬停该行标签时变 `accent_text`；`code_at(column,row)` 只在标签矩形内命中（仿 `toggle_tool_at` 的视觉行映射），命中后走既有复制路径并在页脚提示 `已复制代码块 · N 行`。`markdown::CodeBlock.raw` 保存 fence 内原文（含 Tab、空行、缩进），**不**包含 gutter、软换行、语言标签或展开后的 Tab。
- **全文浮层（3.5）**：`Ctrl+T` 复用插件面板的滚动浮层，把用户/助手/思考/工具（命令+输出）全部展开成纯文本；`/` 增量搜索（最多 256 字符），高亮全部匹配、当前匹配加粗下划线，`n`/`N` 前后跳转并把目标行滚入视口；Esc 关闭；期间草稿与选择不变。
- **文档（3.3）**：新增 `src/tui/help.rs`，`/help` 与 README 的 Terminal UI 章节同步新快捷键（Alt+↑/↓、Ctrl+T、/、n/N、⧉ Copy、上下文 70/90% 阈值、分组规则、非目标清单）；`command_catalog` 的 `/help` 描述更新为中英文"快捷键、轮次跳转与代码复制"；并新增"每条命令中英文描述都非空"的断言。
- **调研（3.4）**：见 `reviews/TUI_REWIND_FEASIBILITY_2026-11-01.md`，只有结论与建议，无实现代码。

## 3. 被修改的旧断言

批次 3 没有放松或删除旧断言；唯一相关改动是 `command_catalog::catalog_names_are_unique_and_descriptions_are_bilingual` 增加中文非空断言（见批次 1 表格）。所有既有快捷键测试（Esc 不清草稿、Ctrl+C 复制/清空/取消、Ctrl+O/Ctrl+R、选择与复制）保持通过。

## 4. 新增测试

- `redesign_tests::turn_navigation_survives_five_turns_reflow_and_preserves_the_draft`：5 轮、宽度 80/40/100/160、跳转后缩放再跳转；逐轮核对落在正确用户消息的 TurnStart 行；草稿全程保留；附件路径模式下不响应。
- `code_copy_hits_only_its_label_and_returns_original_tabs_blanks_and_unicode`：4 个宽度下只在标签矩形命中；返回原文与源码逐字符一致（Tab、空行、`你好🙂`）；相邻行与左侧区域不命中；`selection_rows` 不含 `Copy`/`rust` 标签。
- `full_transcript_search_expands_every_step_and_never_edits_the_draft`：20 步 + 思考都在浮层纯文本里；`/evidence` 命中 21 处；`n`/`N` 环绕；缩放后仍可搜索；草稿不变。
- `selection_of_whole_code_and_tool_cards_excludes_decoration_gutters_and_labels`（批次 1 新增，覆盖 3.2 的复制纯度）。

## 5. 验证

- `cargo fmt --check`、`cargo clippy --locked --all-targets`：通过
- `cargo test --lib --locked --offline tui::`：174 passed / 1 ignored；全量 `cargo test --locked` 388 passed / 2 ignored
- 80×24 流式 bench p95 10.5ms / 10.7ms（300 / 3000 条），与改版前持平
- 文档与 `/help` 中的快捷键与实现一致（Alt+↑/↓、Ctrl+T、`/`、`n/N`、⧉ Copy、Ctrl+O/Ctrl+R、Esc 语义）

## 6. 风险与未完成项

- `jump_turn` 的"当前轮"由视口顶部最近 TurnStart 推断；若用户停在两轮之间的长回答中部，第一次 Alt+↑ 会先对齐到本页已越过的轮次起点，再按一次才到上一轮。这是刻意选择（避免一次跳太远），但需要真实终端体验确认。
- 复制代码块的端到端剪贴板未在自动化测试中执行（CI 无桌面剪贴板）；测试验证的是"命中 + 返回原文"，真实写入沿用既有 `copy_selection`（含 OSC 52 回退）。
- 全文浮层把 Markdown 渲染成纯文本行，表格会退化为管道文本；这是 3.5 的可选功能，未追求富样式。
- 搜索只做大小写敏感的朴素子串匹配，未做正则/模糊；查询长度上限 256 字符，结果数量未设上限（单会话内存文本，量大时高亮循环会变慢，但只在浮层打开且搜索时发生）。
- HEAD 上两个既有测试夹具失配（prompt 断言与上下文预算负载）已按测试夹具修正，全量 `cargo test --locked` 全绿；详见批次 1 报告第 9 节。
