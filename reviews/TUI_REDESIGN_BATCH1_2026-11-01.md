# Kroma TUI 改版 · 批次 1 交付报告（2026-11-01）

范围：`D:\Kroma\kroma-latest`。已提交并推送到 `main`（`35524fb`），部署记录见 `reviews/TUI_REDESIGN_2026-11-01.md` 文末。批次 1：主题令牌（1.1）、装饰管线与显式 prose（1.2）、用户消息块（1.3）、Kroma 回复（1.4）、工具分组摘要卡（1.5）、思考行（1.6）、代码块与轻量高亮（1.7）、系统通知（1.8）。

## 1. 开工前的基线

用 TestBackend 导出改版前的首页、一轮完整对话、失败、取消四个场景，尺寸 40×16 / 80×24 / 100×30 / 160×40，存于 `reviews/tui-redesign/before/`（16 份文本）。随后先落地 1.1 + 1.2，此时 147 个 `tui::` 测试全绿，16 份快照与基线**逐字符一致**（`diff -rq` 无差异），才继续视觉改动。

## 2. 与源码不符 / 需要说明的发现

1. **阅读列不是固定 100 列**：`reading_area` 只留 0/1/2 列外边距，prose 宽度由 `prose_width` 单独限制。保留该行为；装饰按整个阅读列宽铺背景。
2. **自动链接原本就依赖颜色**：`history_document` 用 `span.style.fg == Some(theme.text)`（且非 `theme.code`）判断 prose。已改为 `markdown::render_document` 返回显式 `prose` 行（扣除 inline code 行）。
3. **结构竖线原本是文本**：代码块与展开工具输出把 `│ ` 当 span 文本，复制会带上。现改为第 4 列装饰字形，原 span 替换为等宽空格，`selection_rows` 与复制结果不含竖线（旧断言调整见第 4 节）。
4. **`tool` 令牌没有用于组头 `◇`**：组头已有状态字形（◌/✕/⊘/!/✓），再加 `◇` 会重复。`tool` 保留以兼容 `theme.toml`，默认等于 `muted`，只参与插件颜色槽。
5. **取消通知是普通 assistant 消息**：已导出 `agent::TASK_CANCELLED_PREFIX`，TUI 在实时与 `load_history` 两条路径识别并本地化，持久化文本不变。
6. 旧的 `quiet_completed_tool`（连续 ≥10 条成功折叠）删除，由分组取代；`has_diagnostic` 的"退出成功但输出含警告"提示保留。

## 3. 实现要点

- **主题（1.1）**：`Theme` 新增 `surface/overlay/faint/accent_text/success/warning`，仍 `Copy + PartialEq + Eq`；dark/light 用 0.5 色板，mono 全部 `Reset` 并用 BOLD/DIM/ITALIC；`input_surface()` 返回 `surface`；新增 `Theme::blend(fg,bg,t)`；`▲▲▲` 改为 accent/text/user；对比度测试按 0.5 重写（dark/light × background/surface/overlay，`faint` 不测 overlay，`accent` ≥3.0）。`ThemeSettings` 新增可选键，旧 `theme.toml` 仍可解析。
- **装饰管线（1.2 / 0.4-A）**：`history_document` 追加 `decor: Vec<(source_row, Decor)>`；`Decor::{Surface, Gutter{column,glyph,color,first_only}, Group, TurnStart, CodeLabel}`；`draw` 先铺背景→画文本→叠加字形/标签，只处理可见行；换行后用 `source_lines` 映射 source row，`first_only` 只落首行。
- **用户块（1.3）**：删除 `▎ You` 标签；块首尾各一空行，整块铺 `surface`，第 2 列叠加 rose `▎`；正文显式 `text` 色；块间保留无装饰空行。
- **Kroma（1.4）**：删除 `◆ Kroma` 标签；首行第 2 列叠加 accent `◆`，正文仍在第 4 列；流式光标 `▍` 保留；失败回复沿用 error 高亮。
- **工具卡（1.5）**：相邻 `Kind::Tool` 即时分组（不存储），`Reasoning` 不打断并集中在卡前；组头 `{caret} {glyph} {caption} · {n} steps · {total}`，caption 优先级=运行中>失败>最后一步；含运行中步骤即展开并保留尾部输出预览；失败组展开失败步骤、保留 `↳ failure_summary`，其余折叠为可点击 `… Another N successful steps`；全成功只留组头；`expanded_tool_groups`（key=组内首个 call_id）加入缓存失效键；>12 步只显示最近 8 步并保留失败/警告；`Ctrl+O` 仍展开全部。
- **思考（1.6）**：折叠态 `◇ Reasoning collapsed · Ctrl+R`（muted、非粗体、带时长）；展开态正文 muted+ITALIC、左侧 faint 竖线（装饰，不进入复制）；展开内容亮度不高于正式回答。
- **代码（1.7）**：块背景 `surface`；`render_document` 返回 `CodeBlock{row,end,language,raw}`，语言标签与 `⧉ Copy` 在右上角以装饰绘制；新增 `src/tui/highlight.rs` 手写逐行词法（rust/python/js/ts/jsx/tsx/json/toml/yaml/bash/sh/zsh/powershell/diff），跨行只带块注释/多行字符串/三引号状态，>2000 字符回退单色，只用 `char_indices`；词元拼接恒等于原文（含 Tab/Unicode/随机串测试）；diff 用 success/error + `blend(...,0.15)` 底色，Hunk=accent_text，Meta=muted；mono 不上色只加粗/DIM。展开的 `git diff` 复用同一配色。
- **通知（1.8）**：取消消息渲染成一行 `⊘ Cancelled · incomplete commands may have had side effects`；tool result 的 `Session interrupted…` 显示为黄色 `⊘ Interrupted · result unknown (will not rerun automatically)`，不算失败；主动取消时只保留完成行（折叠 `CANCELLED` info 块与实时通知），`ERROR` 行为不变。

## 4. 被修改的旧断言（逐个说明理由，意图均保留）

| 测试 | 旧断言 | 新断言 | 理由 |
|---|---|---|---|
| `theme::builtin_text_and_status_colors_have_accessible_contrast` | 7 令牌在 background ≥4.5 | 新令牌 × 3 层表面矩阵，`accent` ≥3.0，`faint` 不测 overlay | 0.5 要求替换；旧断言缺少 surface/overlay，查不出"faint 画在 overlay 上" |
| `markdown::fenced_code_and_diff_have_language_labels_and_semantic_rows` | 文本中出现 `rust`/`diff` 行；`+ new`=user | 断言 `Document.blocks` 语言序列；Add=success、Hunk=accent_text | 语言标签改为装饰（复制不得含标签）；粉/蓝分工后 diff 用绿/红 |
| `markdown::code_blocks_preserve_consecutive_and_edge_blank_lines`、`tab_stops_measure_grapheme_clusters_not_scalar_widths` | 首行 `  text` | 首行为空行，标签在装饰层 | 同上；代码行与空白行断言原样保留 |
| `view::header_separation_and_role_body_columns_are_consistent` | 有 `You`/`◆ Kroma` 标签且正文对齐标签 | 无标签行；正文第 4 列、字形第 2 列；`selection_rows` 不含 `▎/◆` | 1.3/1.4 删除标签；列几何改由装饰位置验证 |
| `view::borrowed_viewport_renders_the_same_cells_as_original_paragraph` | 参考终端只画原 Text | 参考终端额外应用同一装饰层（背景/字形） | 装饰是真实绘制的一部分，不补会把正确背景判为差异 |
| `view::expanded_git_diff_uses_semantic_add_remove_colors` | `+new`=user、`@@`=accent | `+new`=success、`@@`=accent_text | 语义颜色重排 |
| `view::long_successful_tool_bursts_collapse_without_hiding_failures` | `Collapsed 14 successful tools` | 组头 `20 steps` + `Another 12 successful steps`，失败可见；展开后仍不显示全部成功步 | 分组取代"连续 ≥10 折叠"；"失败不被折叠隐藏"保留 |
| `view::one_reasoning_notice_per_user_turn_preserves_every_expanded_segment` | 未展开可见 `Check Node version` | 折叠组不可见；展开该组后可见 | 全成功组默认只留组头；"展开后所有片段都在"保留 |
| `view::reasoning_block_is_separated_from_the_answer_that_follows_it` | 空行后一行含 `Kroma` | 空行后一行含 `◆` 与答案 | 标签行删除 |
| `view::svg_task_layout_is_readable_across_narrow_and_wide_chinese_terminals` | 必须出现 `检查Node版本` | 出现 `bouncing-cat.svg` 与 `另有1步` | 成功步折叠为计数；失败 `exit49` 断言保留 |
| `view::single_tool_chevron_expands_only_its_own_output` | 两条成功工具各一行，点第 2 列展开单条 | 两条合成一组（组头可点）；单步卡仍按第 2 列展开 | 1.5 分组；"点击只影响该目标"保留 |
| `view::markdown_code_keeps_semantic_indentation_across_themes`、`code_whitespace_emoji_and_tabs_survive_the_render_pipeline`、`code_block_blank_lines_are_not_merged_or_trimmed`、`narrow_code_wrap_preserves_indent_and_continuation_gutter`、`expanded_tool_output_keeps_leading_indentation` | 复制行以 `    │ ` 开头 | 复制行以空格开头（竖线为装饰） | 0.3-6 要求装饰不进入复制；缩进/空行/Tab/续行对齐断言逐项保留 |
| `command_catalog::catalog_names_are_unique_and_descriptions_are_bilingual` | 仅英文非空 | 中文也非空 | 3.3 要求每条命令都有中英文描述 |

未修改且通过：`conversation_is_readable_without_competing_boxes`（只要求不出现满屏边框，本次改用背景分层，故无需放宽，仍可拦住未来的边框墙）、`only_existing_prose_files_or_valid_urls_are_clickable`、`markdown_and_plain_links_hit_the_correct_screen_cells`、`copied_code_selection_keeps_leading_indentation`。

## 5. 新增测试

- `highlight::tests::*`：每语言样例、跨行状态、随机串/Tab/Unicode 往返、Rust 生命周期不被当成未闭合字符字面量。
- `redesign_tests::raised_blocks_wrap_without_polluting_selection_or_links`：暗/亮/mono、CJK+Emoji 换行、链接仍可点、复制不含装饰。
- `completed_groups_collapse_and_failures_diagnostics_and_interruptions_stay_visible`：失败/中断/警告无需交互可见，中断为黄色 ⊘。
- `cancellation_is_localized_and_live_completion_has_no_duplicate_notice`。
- `selection_of_whole_code_and_tool_cards_excludes_decoration_gutters_and_labels`：框选用户块、代码块、工具卡后复制不含 `▎ ◆ │ ⧉ rust`，但保留缩进。
- `redesign_snapshots::calm_transcript_golden_matrix`：dark/light/mono × 4 尺寸 × 20 场景 = 240 份金样，位于 `reviews/tui-redesign/final/`。

## 6. 四尺寸文字快照（dark）

**home · dark · 40x16**

```text
◆ Kroma  new conversation        ✓ Ready



                K R O M A

 Start a conversation  ·  /resume for h



     test-model · D:/fixture/project

╭──────────────────────────────────────╮
│ › Ask anything, or describe a coding │
╰──────────────────────────────────────╯
  test-model │ Think high │ Context —
```

**home · dark · 80x24**

```text
 ◆ Kroma  new conversation                                     project  ✓ Ready







                                    K R O M A

                  Start a conversation  ·  /resume for history







                         test-model · D:/fixture/project

 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**home · dark · 100x30**

```text
  ◆ Kroma  new conversation                                                       project  ✓ Ready




                                                ▄  ▄
                                               █    █
                                              ██ ██ ██
                                            ▄██▀ ██ ▀██▄
                                           ▄███ ▄██▄ ███▄
                                          ▄████ ████ ████▄
                                         ▄████  ████  ████▄
                                        ██████ ▄████▄ ██████
                                       ██████▀ ██████ ▀██████
                                     ▄███████  ██████  ███████▄
                                    ▄████████ ████████ ████████▄

                                              K R O M A

                            Start a conversation  ·  /resume for history




                                   test-model · D:/fixture/project

  ╭──────────────────────────────────────────────────────────────────────────────────────────────╮
  │ › Ask anything, or describe a coding task…                                                   │
  ╰──────────────────────────────────────────────────────────── Enter send · Shift+Enter newline ╯
    test-model │ Think high │ Context —
```

**home · dark · 160x40**

```text
  ◆ Kroma  new conversation                                                                                                                   project  ✓ Ready







                                                                              ▄  ▄
                                                                             █    █
                                                                            ██ ██ ██
                                                                          ▄██▀ ██ ▀██▄
                                                                         ▄███ ▄██▄ ███▄
                                                                        ▄████ ████ ████▄
                                                                       ▄████  ████  ████▄
                                                                      ██████ ▄████▄ ██████
                                                                     ██████▀ ██████ ▀██████
                                                                   ▄███████  ██████  ███████▄
                                                                  ▄████████ ████████ ████████▄

                                                                            K R O M A

                                                          Start a conversation  ·  /resume for history







                                                                 test-model · D:/fixture/project





  ╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
  │ › Ask anything, or describe a coding task…                                                                                                               │
  ╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────── Enter send · Shift+Enter newline ╯
    test-model │ Think high │ Context —                                                                                                      Ctrl+P Commands
```

**conversation · dark · 40x16**

```text
◆ Kroma  untitled session        ✓ Ready

  ▎  Preserve Unicode and indentation. │
  ▎                                    │
                                       ┃
  ◆  Updated the implementation.       ┃
                                       ┃
     • Preserved input and links       ┃
     • Verified Unicode selection      ┃
                                       ┃
   ✓  Completed · 35.3s                ┃
                                       ┃
╭──────────────────────────────────────╮
│ › Ask anything, or describe a coding │
╰──────────────────────────────────────╯
  test-model │ Think high │ Context —
```

**conversation · dark · 80x24**

```text
 ◆ Kroma  untitled session                                     project  ✓ Ready

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

   ◆  Updated the implementation.

      • Preserved input and links
      • Verified Unicode selection

    ✓  Completed · 35.3s







 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**conversation · dark · 100x30**

```text
  ◆ Kroma  untitled session                                                       project  ✓ Ready

    ▎
    ▎  Review the change. 请 检 查 代 码 。
    ▎  Preserve Unicode and indentation.
    ▎

    ◆  Updated the implementation.

       • Preserved input and links
       • Verified Unicode selection

     ✓  Completed · 35.3s













  ╭──────────────────────────────────────────────────────────────────────────────────────────────╮
  │ › Ask anything, or describe a coding task…                                                   │
  ╰──────────────────────────────────────────────────────────── Enter send · Shift+Enter newline ╯
    test-model │ Think high │ Context —
```

**conversation · dark · 160x40**

```text
  ◆ Kroma  untitled session                                                                                                                   project  ✓ Ready

    ▎
    ▎  Review the change. 请 检 查 代 码 。
    ▎  Preserve Unicode and indentation.
    ▎

    ◆  Updated the implementation.

       • Preserved input and links
       • Verified Unicode selection

     ✓  Completed · 35.3s























  ╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
  │ › Ask anything, or describe a coding task…                                                                                                               │
  ╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────── Enter send · Shift+Enter newline ╯
    test-model │ Think high │ Context —                                                                                                      Ctrl+P Commands
```

**failure · dark · 40x16**

```text
◆ Kroma  untitled session       ✕ Failed

  ▎                                    │
                                       │
   ▸ ✕ Run Rust tests · 3 steps · 3.6s │
   ▸ ✕ Run Rust tests · 1.2s           ┃
     ↳ exit 101 · error[E0599]: method ┃
     …                                 ┃
     … Another 2 successful steps      ┃
                                       ┃
   ✕  Task failed · see error above ·  ┃
                                       ┃
╭──────────────────────────────────────╮
│ › Ask anything, or describe a coding │
╰──────────────────────────────────────╯
  test-model │ Think high │ Context —
```

**failure · dark · 80x24**

```text
 ◆ Kroma  untitled session                                    project  ✕ Failed

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

    ▸ ✕ Run Rust tests · 3 steps · 3.6s
    ▸ ✕ Run Rust tests · 1.2s
      ↳ exit 101 · error[E0599]: method not found
      … Another 2 successful steps

    ✕  Task failed · see error above · 35.3s







 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**failure · dark · 100x30**

```text
  ◆ Kroma  untitled session                                                      project  ✕ Failed

    ▎
    ▎  Review the change. 请 检 查 代 码 。
    ▎  Preserve Unicode and indentation.
    ▎

     ▸ ✕ Run Rust tests · 3 steps · 3.6s
     ▸ ✕ Run Rust tests · 1.2s
       ↳ exit 101 · error[E0599]: method not found
       … Another 2 successful steps

     ✕  Task failed · see error above · 35.3s













  ╭──────────────────────────────────────────────────────────────────────────────────────────────╮
  │ › Ask anything, or describe a coding task…                                                   │
  ╰──────────────────────────────────────────────────────────── Enter send · Shift+Enter newline ╯
    test-model │ Think high │ Context —
```

**failure · dark · 160x40**

```text
  ◆ Kroma  untitled session                                                                                                                  project  ✕ Failed

    ▎
    ▎  Review the change. 请 检 查 代 码 。
    ▎  Preserve Unicode and indentation.
    ▎

     ▸ ✕ Run Rust tests · 3 steps · 3.6s
     ▸ ✕ Run Rust tests · 1.2s
       ↳ exit 101 · error[E0599]: method not found
       … Another 2 successful steps

     ✕  Task failed · see error above · 35.3s























  ╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
  │ › Ask anything, or describe a coding task…                                                                                                               │
  ╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────── Enter send · Shift+Enter newline ╯
    test-model │ Think high │ Context —                                                                                                      Ctrl+P Commands
```

**cancelled · dark · 40x16**

```text
◆ Kroma  untitled session    ⊘ Cancelled

  ▎  Preserve Unicode and indentation. │
  ▎                                    │
                                       ┃
  ◆  Updated the implementation.       ┃
                                       ┃
     • Preserved input and links       ┃
     • Verified Unicode selection      ┃
                                       ┃
   ⊘  Cancelled · side effects are no  ┃
                                       ┃
╭──────────────────────────────────────╮
│ › Ask anything, or describe a coding │
╰──────────────────────────────────────╯
  test-model │ Think high │ Context —
```

**cancelled · dark · 80x24**

```text
 ◆ Kroma  untitled session                                 project  ⊘ Cancelled

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

   ◆  Updated the implementation.

      • Preserved input and links
      • Verified Unicode selection

    ⊘  Cancelled · side effects are not undone · 35.3s







 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**cancelled · dark · 100x30**

```text
  ◆ Kroma  untitled session                                                   project  ⊘ Cancelled

    ▎
    ▎  Review the change. 请 检 查 代 码 。
    ▎  Preserve Unicode and indentation.
    ▎

    ◆  Updated the implementation.

       • Preserved input and links
       • Verified Unicode selection

     ⊘  Cancelled · side effects are not undone · 35.3s













  ╭──────────────────────────────────────────────────────────────────────────────────────────────╮
  │ › Ask anything, or describe a coding task…                                                   │
  ╰──────────────────────────────────────────────────────────── Enter send · Shift+Enter newline ╯
    test-model │ Think high │ Context —
```

**cancelled · dark · 160x40**

```text
  ◆ Kroma  untitled session                                                                                                               project  ⊘ Cancelled

    ▎
    ▎  Review the change. 请 检 查 代 码 。
    ▎  Preserve Unicode and indentation.
    ▎

    ◆  Updated the implementation.

       • Preserved input and links
       • Verified Unicode selection

     ⊘  Cancelled · side effects are not undone · 35.3s























  ╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
  │ › Ask anything, or describe a coding task…                                                                                                               │
  ╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────── Enter send · Shift+Enter newline ╯
    test-model │ Think high │ Context —                                                                                                      Ctrl+P Commands
```

## 7. 其余重点场景

**dark-single-tool-80x24**

```text
 ◆ Kroma  untitled session                                     project  ✓ Ready

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

    ▸ ✓ Run · echo step-0 · 1.2s

    ✓  Completed · 35.3s










 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**dark-tools-success-80x24**

```text
 ◆ Kroma  untitled session                                     project  ✓ Ready

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

    ▸ ✓ Run · echo step-2 · 3 steps · 3.6s

    ✓  Completed · 35.3s










 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**dark-tools-failed-80x24**

```text
 ◆ Kroma  untitled session                                     project  ✓ Ready

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

    ▸ ✕ Run Rust tests · 3 steps · 3.6s
    ▸ ✕ Run Rust tests · 1.2s
      ↳ exit 101 · error[E0599]: method not found
      … Another 2 successful steps

    ✓  Completed · 35.3s







 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**dark-tools-running-80x24**

```text
 ◆ Kroma  untitled session                                project  ● Running 0s

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

    ▸ ◌ Run · echo step-2 · 3 steps · 2.4s
    ▸ ✓ Run · echo step-0 · 1.2s
    ▸ ✓ Run Rust tests · 1.2s
    ▸ ◌ Run · echo step-2
      ↳ test unicode_copy ... ok

    ▲▲▲  Running Run · echo step-2 · 0s






 ╭ Steer task · /queue to enqueue ────────────────────────────────────────────╮
 │ › Steer this task, or /queue another…                                      │
 ╰────────────────────────────────────────────────────────────────────────────╯
   test-model │ Think high │ Context —
```

**dark-tools-interrupted-80x24**

```text
 ◆ Kroma  untitled session                                     project  ✓ Ready

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

    ▸ ⊘ Run · echo step-2 · 3 steps · 3.6s
    ▸ ⊘ Run Rust tests · 1.2s
      ⊘ Interrupted · result unknown (will not rerun automatically)
      … Another 2 successful steps

    ✓  Completed · 35.3s







 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**dark-reasoning-collapsed-80x24**

```text
 ◆ Kroma  untitled session                                     project  ✓ Ready

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

    ◇  Reasoning collapsed · Ctrl+R · 0.0s

    ✓  Completed · 35.3s










 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**dark-reasoning-expanded-80x24**

```text
 ◆ Kroma  untitled session                                     project  ✓ Ready

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

    ◇  Reasoning · Ctrl+R to collapse · 0.0s

      │ Inspect the existing API, then verify the change.
      │
      │ Keep failures visible.

    ✓  Completed · 35.3s






 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**dark-code-80x24**

```text
 ◆ Kroma  untitled session                                     project  ✓ Ready

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

   ◆                                                            rust · ⧉ Copy
      │ // preserve source text
      │ fn greet(name: &str) {
      │     println!("Hello {name}");
      │     let count: usize = 42;
      │ }

    ✓  Completed · 35.3s





 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**light-code-80x24**

```text
 ◆ Kroma  untitled session                                     project  ✓ Ready

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

   ◆                                                            rust · ⧉ Copy
      │ // preserve source text
      │ fn greet(name: &str) {
      │     println!("Hello {name}");
      │     let count: usize = 42;
      │ }

    ✓  Completed · 35.3s





 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**mono-code-80x24**

```text
 ◆ Kroma  untitled session                                     project  ✓ Ready

   ▎
   ▎  Review the change. 请 检 查 代 码 。
   ▎  Preserve Unicode and indentation.
   ▎

   ◆                                                            rust · ⧉ Copy
      │ // preserve source text
      │ fn greet(name: &str) {
      │     println!("Hello {name}");
      │     let count: usize = 42;
      │ }

    ✓  Completed · 35.3s





 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything, or describe a coding task…                                 │
 ╰────────────────────────────────────────── Enter send · Shift+Enter newline ╯
   test-model │ Think high │ Context —
```

**dark-diff-100x30**

```text
  ◆ Kroma  untitled session                                                       project  ✓ Ready

    ▎
    ▎  Review the change. 请 检 查 代 码 。
    ▎  Preserve Unicode and indentation.
    ▎

    ◆                                                                              diff · ⧉ Copy
       │ diff --git a/main.rs b/main.rs
       │ @@ -1 +1 @@
       │ -old_value
       │ +new_value

     ✓  Completed · 35.3s












  ╭──────────────────────────────────────────────────────────────────────────────────────────────╮
  │ › Ask anything, or describe a coding task…                                                   │
  ╰──────────────────────────────────────────────────────────── Enter send · Shift+Enter newline ╯
    test-model │ Think high │ Context —
```

完整 240 份（三预设 × 四尺寸 × 二十场景）见 `reviews/tui-redesign/final/`；改版前基线见 `reviews/tui-redesign/before/`。

## 8. 验证

- `cargo fmt --check`：通过
- `cargo clippy --locked --all-targets`：通过，无 warning
- `cargo test --locked --offline tui::`：146 passed / 1 ignored（microbench）
- 80×24 流式 bench（手动 `--ignored`）：300 条 p95 10.5ms、3000 条 p95 10.7ms；改版前基线 10.66ms / 10.60ms → 无退化，远低于 40ms 预算
- `cargo test --lib --locked --offline tui::view::redesign_snapshots::calm_transcript_golden_matrix`：240 份金样全部匹配（重生成用 `KROMA_UPDATE_SNAPSHOTS=1`）
- `Cargo.toml` / `Cargo.lock` 的 `git diff` 为空（未新增依赖）

## 9. 风险与未完成项

- **合并 `origin/main` 时同步修复了两个既有测试夹具失配（仅测试，无 production 行为改动）**：
  1. `agent::tests::batching_guidance_is_stable_for_new_and_resumed_sessions` 在 HEAD 上可复现失败：`PromptComposer::default()` 先追加 `EXECUTION_GUIDANCE` 再追加 `V7_GUIDANCE`，断言却要求以 `EXECUTION_GUIDANCE` 结尾。上游已用同样的"以 `V7_GUIDANCE` 结尾 + 顺序"断言修复，合并保留上游更严格版本。
  2. `tests/context_compaction_e2e.rs::checkpoint_failure_below_hard_limit_does_not_abort_the_task` 的合成负载在 V7 指导语加入后超出硬上限（27808 > 27648 tokens），同样在 HEAD 复现。上游把工具证据 48k→47k，本地把合成上下文 4000→3800；合并后仍满足"越过 90% 触发点、低于硬上限"的意图，3 个用例全绿。
  合并后全量 `cargo test --locked` 为 388 passed / 2 ignored。
- 分组与代码块的装饰行号只在内存，随缓存失效重建；`/resume` 后分组结果一致，但手动展开的组只在同一会话内保留（与既有工具展开行为一致）。
- 高亮是启发式词法而非语义解析；未知语言、超长行退化为单色，符合 1.7。异常嵌套引号等极端输入仍保证逐字符不丢失，但不保证着色正确。
- 插件可注入颜色槽；插件自选色不参与内置对比度矩阵（沿用既有约束）。
