# Kroma TUI 改版 · 批次 2 交付报告（2026-11-01）

范围：`D:\Kroma\kroma-latest`。已提交并推送到 `main`（`35524fb`），部署记录见 `reviews/TUI_REDESIGN_2026-11-01.md` 文末。批次 2：页眉（2.1）、圆角输入框与状态边框（2.2）、页脚与上下文条（2.3）、活动行/完成行（2.4）、首页（2.5）、浮层（2.6）、可选终端标题与任务栏进度（2.7），以及批次 1 的 1.1b（真彩色检测降级）。

## 1.1 与 origin/main 合并时的取舍

- 保留上游的双语 onboarding 提示与采样 logo 阈值；首页仍是 logo + 一行提示 + 完整路径 + 最近会话。
- 不保留上游的独立 cwd 行：页眉固定"1 行内容 + 1 行间隔"，项目名在页眉右侧；完整路径在首页与 `/help`。
- 终端标题为 `{●/✓/✕/⊘}Kroma — {会话}`，保留上游的"仅变更时写 OSC 0"去抖（在 `Feedback` 内）。
- 未测量的上下文显示 `Context —` 而不是 `0 / window`：恢复历史会话后 0 会被误读为已用 0 tokens。
- 金样读取把 CRLF 归一化为 LF，保证 Windows checkout（`core.autocrlf=true`）下仍可比较。

## 1. 实现要点

- **页眉（2.1）**：仍占 2 行（1 行内容 + 1 行间隔），插件 `header_rows` 语义不变。左侧 `◆ Kroma  {会话}`（`◆`=accent，`Kroma` 粗体 text，会话名 muted）；右侧右对齐 `{项目名}  {分支}{*}{状态}`；状态为 `● 运行中 12s`（accent_text）/`✓ 就绪`（muted）/`✕ 失败`（error）/`⊘ 已取消`（muted）。窄屏渐进：<70 去分支，<50 去项目名，状态最后省略。
- **gitinfo（2.1）**：新增 `src/tui/gitinfo.rs`。分支直接读 `.git/HEAD`（处理 worktree 的 `gitdir:` 文件，detached 显示 7 位 sha）；脏标记来自异步 `git status --porcelain -z`；`diff --numstat -z HEAD` 与 `ls-files --others --exclude-standard -z` 为每轮改动基线服务。全部 `tokio::process::Command`，1.5s 超时、`kill_on_drop`、Windows `CREATE_NO_WINDOW`；**`draw()` 里绝不执行 git**。结果经 `UiEvent::GitInfo{revision,session,...}` 回传，晚到或跨会话的响应被丢弃。
- **输入框（2.2）**：`BorderType::Rounded` + `surface` 背景；几何保持不变：左侧 1 边框+1 空格+`›`+1 空格 = 4，右侧 1+1 = 2，`editor_inner = (x+4, y+1, w-6, ...)`；唯一改动是被 `composer_max` 夹住时高度用 `height-2`，避免盖住底边框。边框颜色随状态：空闲 border、运行中 accent、附件路径 accent_text；顶边标题为 `引导任务 · /queue 排队` / `附件路径 · Esc 取消`；空草稿+空闲+宽度≥60 时底边右侧 `Enter 发送 · Shift+Enter 换行`（faint）。提示符 `›` 改 rose 粗体。
- **页脚（2.3）**：单行，分隔符 ` │ `（faint）。宽候选：`模型 │ 思考 high │ 上下文 ▰▰▱▱▱▱▱▱ 12% 62k/500k │ 缓存 92%`；上下文条 8 格，颜色 <70% muted、70–89% warning、≥90% error；用量未知显示 `—`（不编 0）；缓存改 faint，不再抢焦点；`Ctrl+P 命令` 仅在宽度≥100、有空位且无通知时出现。保留 0.4-F 的候选回退：80 列用 `模型 │ 思考 │ 上下文%` 候选，仍可见三者。通知着色 info=accent_text、warn=warning、error=error；`note(&str)` 仍为 info。
- **活动行（2.4）**：保留 `▲▲▲`（颜色 accent/text/user）。新增"本轮是否已收到模型输出"标记：首次等待 `Connecting/正在连接模型`，之后 `Waiting for model/等待模型响应`；工具阶段显示 `正在运行 {运行中步骤 summary} · {n}s`；同一阶段 ≥8s 追加 faint `Ctrl+C 中断`；活动行上方保留空行（原保留行 + 新空行 → 活动行不再贴住输出）。
- **完成行（2.4）**：`✓ 已完成 · 35.3s · 改动 3 个文件 +42 −7 · 新增 2 个文件 · /diff`。实现方式：任务开始时异步快照 `git diff --numstat` + 未跟踪集合，结束时再取一次并求差（逐文件增删变化 + 新增未跟踪文件数）。基线缺失、超时、非 git 仓库 → **整段不显示**；异步送达，晚到不阻塞完成行；不持久化，重启后的历史不显示。失败/取消不加摘要。
- **首页（2.5）**：保留 logo 与提示；增加一行 `模型 · 完整工作目录 · 分支`，以及最多 3 个最近会话（标题 + 相对时间）与 `/resume 查看全部`。高度不足先隐藏最近会话，再用紧凑 logo（`height>=17 && width>=34` 才画半块图）。无营销文案。
- **浮层（2.6）**：背景 `overlay`、圆角 `border`、标题 `accent_text`；选中行底色 `blend(accent, overlay, 0.35)` + text 粗体；描述列 muted；插件面板同样处理。
- **终端反馈（2.7，可选）**：`terminal_title()` 增加状态前缀（● / ✓ / ✕ / ⊘），控制字符过滤并截断 160 字符；仅当 `WT_SESSION` 存在时，运行中发 `OSC 9;4;3`，结束/退出/`UiState::drop` 发 `OSC 9;4;0`；其它终端不输出任何字节。
- **1.1b（可选）**：`terminal_feedback` 在 `COLORTERM` 不含 `truecolor/24bit` 且无 `WT_SESSION` 时，把渲染后 buffer 的 `Color::Rgb` 就近降到 `Color::Indexed`（立方体与灰阶取较近者），`Reset`/索引色原样保留。

## 2. 被修改的旧断言（逐个说明理由，意图均保留）

| 测试 | 旧断言 | 新断言 | 理由 |
|---|---|---|---|
| `view::short_chat_starts_below_header_and_live_status_follows_the_latest_message` | 活动行含 `Running tool` | 活动行含 `▲` 与 `Running Run Rust tests` | 2.4 要求工具阶段显示具体步骤，不再用泛化文案 |
| `view::status_keeps_model_thinking_and_context_visible_at_eighty_columns` | 80 列含 `Context 25%` | 80 列含 `Context ▰▰▱▱▱▱▱▱ 25%`（<80 仍含 `Context 25%`） | 宽候选加入上下文条；"80 列必见模型/思考/上下文"保留 |
| `view::context_status_uses_total_window_in_wide_and_narrow_terminals` | `Context 20%` | 110 列含 `Context ▰▰▱▱▱▱▱▱ 20% 100k/500k`；40 列仍含 `Context 20%` | 同上；总窗口而非预留输入预算的意图保留 |
| `view::footer_is_one_compact_row_without_repeated_command_hints` | `上下文0.1%`、`缓存87%`、`输入/输出1.5k/8` 同屏 | 上下文条+计数、`缓存87%` 同屏；输入/输出改用 `usage_variants()[1]` 断言 | 默认页脚不再抢上下文位置显示 API 用量；"单一紧凑行、不重复 /help"保留 |
| `view::footer_distinguishes_context_from_api_tokens_and_cached_ratio` | `In/out 8.0k / 500 · Cache 75%` 同屏 | `Cache 75%` 同屏 + `usage_variants()[1]` 含完整串 | 分离上下文与 API 用量的意图保留，位置改为 /usage 变体 |
| `view::background_title_and_apec_usage_cannot_replace_coding_cache_or_context` | 同屏 `In/out 1.0k / 80 · Cache 80%` | `Cache 80%` 同屏 + 变体断言 | 同上 |
| `view::bilingual_chrome_picker_descriptions_and_unknown_usage_render_at_multiple_sizes` | 各宽度都含 `未报告` | 保持 `未报告`（未报告的缓存比例、非 0）；未知上下文用 `Ctx —`/`—` | 未报告≠0 的意图保留；宽度不足时按候选回退 |
| `view::mixed_attachments_keep_numbered_order_after_history_restore` | 页脚含 `输入/输出1.5k/8` | 断言附件顺序 + `usage_variants()[1]` 含 `输入/输出 1.5k / 8` | 同上；测试主旨是附件顺序，未改动 |
| `view::completion_row_reports_the_outcome_and_how_long_the_turn_took` | 第一行含 `✓` 即完成行 | 选择同时含 `✓` 与 `Completed` 的行 | 页眉新增 `✓ 就绪`，原选择器会选到页眉；"完成行有标记+时长"保留 |
| `view::home_draws_original_brand_mark_and_clears_on_first_send` | 只检查 logo/会话切换 | 保持原断言（新增首页信息行不影响） | 未改动；首页新增内容在 logo 下方独立行 |
| `command_catalog` 中文非空 | — | 见批次 1 | 3.3 要求 |

未修改且通过：`searchable_picker_renders_over_history_without_obscuring_editor`、`plugin_panel_overlay_scrolls_and_keeps_composer_unchanged`、`composer_mouse_selection_is_visible_and_copies_unicode_exactly`、`long_draft_does_not_jump_when_drag_selection_starts`、`connecting_steering_stays_in_composer_not_activity_row`、`notification_dismisses_without_losing_important_transcript_events`。

## 3. 新增测试

- `gitinfo::tests::reads_branch_worktree_file_and_detached_head`（worktree `gitdir:` 文件、detached 短 sha）。
- `parses_nul_paths_renames_binary_and_missing_baseline`（含空格路径、rename、二进制 `-`、缺失基线返回 None）。
- `command_timeout_is_unknown_not_zero_changes`（超时 → unknown，不编 0）。
- `unborn_repository_keeps_branch_status_without_fabricated_diff_counts`（无提交仓库保留分支、但不伪造 diff）。
- `repository_snapshot_reports_only_the_turn_delta`（临时 git 仓库，无 git 时跳过；只取差值与新未跟踪文件）。
- `redesign_tests::context_thresholds_unknown_usage_and_notice_severity_are_distinct`（69/75/95% 三档颜色、8 格、`—`、通知三级颜色）。
- `waiting_phase_tracks_model_output_and_idle_hint_uses_last_output`（首次 Connecting → 之后 Waiting；8s 后出现 Ctrl+C，恢复输出后消失）。
- `late_git_results_are_scoped_to_session_and_turn_and_summary_is_success_only`（过期 revision 丢弃、完成行摘要、失败/取消不加摘要、load_history 清空）。
- `terminal_feedback::tests::*`（真彩色检测、索引降级、仅 WT 发 OSC、退出清除）。
- 金样矩阵新增 home-recent / context75 / context95 / picker / panel / running / idle 场景（240 份）。

## 4. 四尺寸快照（节选，dark）

**dark-home-80x24**

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

**dark-home-recent-160x40**

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
                                                                      Fix Unicode · 1h ago
                                                                   Review tool output · 2h ago
                                                                       Add tests · 2d ago
                                                                    /resume View all sessions

  ╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
  │ › Ask anything, or describe a coding task…                                                                                                               │
  ╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────── Enter send · Shift+Enter newline ╯
    test-model │ Think high │ Context —                                                                                                      Ctrl+P Commands
```

**dark-context75-80x24**

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
   test-model │ Think high │ Context ▰▰▰▰▰▰▱▱ 75% 375k/500k
```

**dark-context95-40x16**

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
  Think high │ Context 95%
```

**dark-picker-100x30**

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


              ╭ Commands · ↑↓ select · Tab complete · Esc close ─────────────────────╮
              │ › /                                                                  │
              │ › /clear  — Clear the current conversation                           │
              │   /commands  — Search all slash commands                             │
              │   /continue  — Resume the latest conversation                        │
              │   /diff  — Show workspace changes                                    │
              │   /editor  — Edit draft in external editor                           │
              │   /exit  — Exit Kroma                                                │
              │   /export <path>  — Export conversation to a file                    │
              │   /fork <checkpoint>  — Branch from a checkpoint                     │
              ╰──────────────────────────────────────────────────────────────────────╯
  ╭──────────────────────────────────────────────────────────────────────────────────────────────╮
  │ › Ask anything, or describe a coding task…                                                   │
  ╰──────────────────────────────────────────────────────────── Enter send · Shift+Enter newline ╯
    test-model │ Think high │ Context —
```

**dark-panel-80x24**

```text
 ◆ Kroma  untitled session                                     project  ✓ Ready
 ╭ Review · ↑↓/PgUp/PgDn · Esc Close ─────────────────────────────────────────╮
 │A scrollable plugin panel.                                                  │
 │The draft stays editable after closing.                                     │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 │                                                                            │
 ╰────────────────────────────────────────────────────────────────────────────╯
   test-model │ Think high │ Context —
```

**dark-running-100x30**

```text
  ◆ Kroma  untitled session                                                  project  ● Running 0s

    ▎
    ▎  Review the change. 请 检 查 代 码 。
    ▎  Preserve Unicode and indentation.
    ▎

    ◆  Updated the implementation.

       • Preserved input and links
       • Verified Unicode selection

     ▲▲▲  Waiting for model · 0s













  ╭ Steer task · /queue to enqueue ──────────────────────────────────────────────────────────────╮
  │ › Steer this task, or /queue another…                                                        │
  ╰──────────────────────────────────────────────────────────────────────────────────────────────╯
    test-model │ Think high │ Context —
```

**dark-idle-40x16**

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

四个尺寸（40×16 / 80×24 / 100×30 / 160×40）的全场景快照在 `reviews/tui-redesign/final/`，含 light 与 mono 两套。

## 5. 验证

- `cargo fmt --check`、`cargo clippy --locked --all-targets`：通过
- `cargo test --lib --locked --offline tui::`：174 passed / 1 ignored（microbench）；全量 `cargo test --locked` 388 passed / 2 ignored
- 80 列页脚仍含模型、思考、上下文（测试 `status_keeps_model_thinking_and_context_visible_at_eighty_columns`）
- 输入框原有的中文 IME 光标位置、鼠标选择、长草稿滚动、附件标签测试全部通过
- 80×24 流式 bench p95：300 条 10.5ms / 3000 条 10.7ms，与批次 1 持平，无退化

## 6. 风险与未完成项

- **git 摘要是"观测差值"而非语义归属**：同一轮里用户或后台进程改了文件，也会被计入；不是精确归属。基线在任务开始后异步获取，若首轮命令极快完成，基线可能缺失 → 摘要整段不显示（按约束宁可无数字）。
- 分支/dirty 只在启动、每轮结束与 `add_user` 时刷新；外部 `git checkout` 不会即时反映。
- 终端标题在部分终端可能被覆盖；OSC 9;4 仅 Windows Terminal，未在真机验证。
- 索引色降级是近似（灰阶/立方体取近），不追求感知均匀；降级只影响显示，不影响主题文件。
- 全量 `cargo test` 仍有一个与本次无关的既有失败（见批次 1 报告第 9 节，已用 HEAD 归档副本复现）。
