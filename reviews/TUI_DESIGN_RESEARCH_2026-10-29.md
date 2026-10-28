# Kroma TUI 深度设计研究与改版任务（2026-10-29）

**范围**：Windows `D:\Kroma\src_kroma\kroma-new-0a5500b`，main `cc4c903`。只研究和制定任务；不修改生产代码。本轮 `cargo test --locked --offline tui:: -q`：32/32 通过。

## 1. 设计总纲

**选单栏流式任务记录 + 固定输入区（Calm Transcript）**，不是仪表盘或多面板 IDE。终端最重要的是保持任务线索、阅读结果和快速输入；把所有低频操作收进搜索式选择器。默认信息密度由 *任务结果 > 工具动作与错误 > 工作状态 > 推理细节 > 设置/快捷键* 决定。

- 默认简洁：推理与成功工具输出折叠，**失败原因必须可见**；用户随时展开详情。
- 视觉语义优先：文字 + 符号共同表示状态，不能只靠颜色。不要满屏边框、标签、动画或提示。
- 输入不可误丢：Esc 不清空普通草稿，Ctrl+C 保留现有清空/取消规则，撤销可用。
- 内容可读：英语标识符、代码、中文、组合字符/Emoji、Markdown 表格、终端不同宽度应正确显示。
- 已有主题系统保留；先做好信息层级和功能，再做调色。
- 充分复用现有 Ratatui、pulldown-cmark、Crossterm、ThemeStore、事件流、附件模型；不另起 UI runtime。

## 2. 源码事实与风险

| 位置 | 已确认的当前行为 | 用户影响 | 优先级 |
|---|---|---|---|
| `src/tui/view.rs:405,1045-1073` | `reasoning_expanded=true`，每个思考块重复显示 Ctrl+R 提示 | 大量模型推理压过答案，历史难扫读 | P0 |
| `src/tui/view.rs:981-1044` | 完成的工具默认一行命令，失败时只显示符号；`Ctrl+O` 展开所有工具 | 默认状态看不清命令为什么失败；240 字命令可折行多行 | P0 |
| `src/tui/mod.rs:327-335` | 正常输入中的 `Esc` 调用 `discard_draft()` | 误触损失输入内容/附件 | P0 |
| `src/tui/view.rs:1142-1172,1330-1378` | 终端宽度低于 85 列隐藏模型名；底部提示与状态占两行 | 80 列等常见窗口中核心模型不可见，辅助提示重复 | P0 |
| `src/tui/view.rs:632-635,1360-1372` | `notification` 一直保持到下次覆盖，无清除路径 | 临时错误/主题提示长时间盖住快捷提示 | P0 |
| `src/tui/view.rs:852-868,1205-1235` | 滚动相对底部计行；流式文本改变总高度时无阅读锚点 | 读较早日志时，新内容可能推走正在看的行 | P1 |
| `src/tui/editor.rs:97-111,311-423` | 光标/退格按 Unicode `char`（码点）而非字素簇处理 | 组合重音和 ZWJ Emoji 可能被拆开；需 Windows IME 真机回归 | P1 |
| `src/tui/view.rs:718-724,1178-1203` | 所有 UI 事件使历史缓存失效，下一次绘制重排/Markdown 解析整个可见尾部 | 高频流式输出与长会话潜在渲染延迟；未定量复现 | P1 |
| `src/tui/mod.rs:41-49,458-477` | 强制 AlternateScreen + 鼠标捕获；右键通过 OSC52 复制 | Windows Terminal 原生选择/复制可能与应用鼠标捕获冲突，OSC52 支持需真机验证 | P1 |
| `src/tui/mod.rs:280-319,840-914` | 只有 Tab 补全或 /model 文本列表，选择会话也以命令为主 | 命令、模型、会话的发现性差 | P1 |
| `src/tui/markdown.rs:180-260` | 基础 Markdown 样式、代码块单色，无语言高亮；有表格宽度降级 | 复杂代码、Diff 不够好扫读；无需马上引入大型高亮框架 | P2 |
| `src/tui/theme.rs:46-98,230-255` | dark/light 的文字前景对比通过 4.5 测试；mono 采用 Reset 背景 + White 前景但无 contrast 检查 | 亮色终端的 mono 有文字不可见风险 | P1 |
| `src/tui/view.rs:928-946` | 空白首页有多行宣传式文案/示例 | 占用注意力；首屏应极简 | P2 |

**证据分级**：Esc 清空、模型隐藏、通知不消退、思考默认展开、全局工具展开是代码级确定行为；滚动体验、跨终端鼠标兼容、帧耗时属于待专门复现/量化风险，不能标作已复现故障。

## 3. 同类 Harness 的可借鉴设计

- **Pi**：全屏/普通终端历史双模式；容器和组件按宽度渲染，带真实焦点及 IME 光标；修改后按需 invalidation；大多数内容默认折叠；全屏兼容鼠标/滚动。**借**默认收敛信息、可靠编辑和缓存，不复制其整个 TUI 包。
- **Claude Code**：Ctrl+O 查详细工具/消息，Ctrl+R 搜索输入历史，Ctrl+V 或 Windows Alt+V 图片 chip；详细追踪和任务结果区分。**借**“主记录/详情查看”分层，少量一致快捷键。
- **OpenCode**：Ctrl+P 命令面板、`@file` 模糊选择、模型/会话选择器、自适应 Diff 布局、鼠标捕获可关。**借**同一个搜索式选择器复用三种数据源和附件提及；默认不增加永久侧栏。
- **Gemini CLI**：minimal/full 两种细节密度、inline thinking 默认 off、屏幕阅读器纯文本模式。**借**有意义的紧凑模式，而非增加多页设置项。
- **Codex CLI**：Markdown 代码/Diff 有语义高亮、主题预览/选择、外部编辑器。**借**简洁的代码和 Diff 呈现，避免以大量视觉装饰替代可读性。

## 4. 推荐布局：Calm Transcript / 静默任务流

下例是**规划示意**，不是当前程序截图：

```text
◆ Kroma  · 项目名 / 会话名                             ● 运行中
────────────────────────────────────────────────────────────────

▎ 你
  深入分析并修复这次测试失败。

  ◇ 思考中 …                        [Ctrl+R 展开]
  ✓ Bash · rg -n 'retry' src
  ✕ Bash · cargo test --locked
    ↳ 失败：E0599 · src/provider.rs:128
  ◌ Bash · cargo test --locked --offline -q
    ↳ test session_recovery ... ok

◆ Kroma
  已定位到接口错误，正在验证修复。


────────────────────────────────────────────────────────────────
› 继续描述任务或输入 / 命令…

DeepSeek V4.1 · Think High · Ctx 42k/128k (33%) · /help
```

**布局规则**：
1. 主区只放用户消息、Agent 回复、可扫描的工具状态；两个段落间适量空行，不为每条消息画矩形。
2. 工具默认成功一行、失败两行；运行工具临时展示尾部一行；更多细节显式展开。命令和错误都按实际可用宽度截断/换行，避免 240 字串冲垮主区。
3. 思考内容独立于最终答案，默认只占一行；展开后仍实时流式显示 Provider 可见推理，不虚构内部私有推理。
4. 顶栏显示项目/会话与运行状态；底栏显示当前模型、Think、Context 和帮助入口，不重复同一信息。80 列时仍保留模型；狭窄时优先隐去快捷键和冗长会话 ID。
5. 组合色彩：中性色承载 80% 内容；accent 仅用于活动焦点/重要标题；失败红、成功绿、运行中中性/琥珀。支持 mono、低色终端；语义以图标与文字共同表达。
6. 内容区建议 86–96 列阅读宽度，不额外引入侧栏。代码/表格在宽窗口可酌情放宽，但不能牺牲整页可读性。
7. 空首页只留一句简短提示，不再摆营销文案与整段教程。

## 5. 分批实施（严格按收益推进）

### 第 1 批：最小可见重做（P0）

1. `reasoning_expanded` 默认 false；思考状态和展开动作仅在活动思考块或底部显示一次。
2. `Esc` 在无覆盖层的常规编辑状态不丢草稿；`Ctrl+C` 继续沿用用户既定的清空、取消、退出语义。
3. 工具完成后：成功默认单行；失败默认命令 + 简短错误原因（退出码、stderr 首个有效行）；运行中尾部最多一行；Ctrl+O 可查看详情。
4. 80 列始终显示模型短名称、运行状态、Context 和 Think；会话显示友好名称而不是难读的部分 ID。
5. 通知在下一次有效交互/新任务时消退；关键失败转为可回看的事件记录，避免通知一直占用底栏。

### 第 2 批：可靠输入与长任务（P1）

1. 编辑器字素簇（grapheme cluster）光标/删除，中文 IME、组合字符、ZWJ Emoji、附件 chip 混排测试；保留现有 Alt+V、历史、撤销。
2. 流式追加时如用户在底部自动跟随；一旦向上阅读，保持原来的顶部内容不动；提供明确的“回到底部”入口。
3. 首先测量长会话 300/3000 条消息的 `history_text`、Markdown/宽度换行、`Text.clone()` 的帧耗时；如出现实测性能退化，再缓存已完成条目及更改区域，而非另建渲染系统。
4. Windows Terminal / VS Code 终端实测鼠标选择、右键复制、OSC52；本地 Windows 可复用已有 `arboard`，远程仍走 OSC52/终端原生机制。若需禁用捕获，则作为后续普通模式研究，不先维护两套界面。
5. 同一轻量搜索组件用于 `/` 命令、`@` 文件、模型和会话选择。Tab 补全与现有斜杠命令继续兼容。

### 第 3 批：视觉精修（P2）

1. 统一代码块缩进、语言标签、长路径折行与错误定位；Diff 采用按宽度单列显示，保留原文可复制。
2. 主题语义化（背景、正文、次级、活动、成功、错误、代码），保留 dark/light/mono；修复 mono 在亮背景下的可读性。
3. 如果已有编辑/审查场景确有需要，再加 **按需打开** 的 Diff/全文详情弹层。不要默认常驻双栏、不做复杂动画、不创建一个独立 UI 插件运行时。

## 6. 验收标准与研究实验

- **尺寸**：40×16、80×24、100×30、160×40 的 Ratatui TestBackend 金样快照；无文字重叠/关键状态丢失；实测 Windows Terminal 与 VS Code 集成终端。
- **输入**：中文 IME 输入、删除 `e + ◌́` 与 `👩‍💻`、长多行、撤销、附件引用顺序、Esc 不误丢、Ctrl+C 四种分支均有回归。
- **工具**：单一失败的首个错误行默认可见；连续 50 次工具调用不会把日志刷成无法扫读的屏幕；隐藏和展开不影响原始会话。
- **阅读**：用户滚动离底后，连续 100 个流式事件不能推走当前阅读锚点；到达底部后恢复自动跟随。
- **性能预算（尚非测量结果）**：80×24 的典型流式会话 UI 绘制 p95 **≤40ms**；输入确认无明显卡顿；高负载时允许合并帧，不丢模型输出。
- **无额外模型成本**：TUI 变更不增加 Agent Loop 的模型请求次数、不改变请求 Prompt/工具 schema/压缩策略，也不污染 WAL。
- **设计测试**：同一任务录屏比对旧版与新版本；盲评“10 秒内能否找到最终结果/失败原因/当前模型”，收集至少 3 个不同熟悉度用户反馈；不可只看静态截图打分。

**明确不做**：移动端专项、全屏彩色卡片墙、默认双栏 IDE、复杂动画帧系统、声音/通知中心、审批 UI、重新发明 Markdown/主题/插件内核。

## 7. 来源

- Pi Terminal UI: https://pi.dev/docs/latest/tui
- Pi terminal usage/fullscreen: https://pi.dev/docs/latest/usage
- Claude Code interactive mode: https://code.claude.com/docs/en/interactive-mode
- OpenCode TUI: https://docs.opencode.ai/docs/tui/
- Gemini CLI settings: https://geminicli.com/docs/cli/settings/
- Codex CLI customization: https://learn.chatgpt.com/docs/cli-customization
- Windows Terminal selection and right-click: https://learn.microsoft.com/en-us/windows/terminal/customize-settings/interaction
- Unicode grapheme boundaries: https://www.unicode.org/reports/tr29/
- Ratatui snapshots and PTY coverage: https://www.ratatui.rs/recipes/testing/snapshots/
- WCAG 2.2: https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum

**可信度说明**：上述差距基于源码核对与公开文档。未在本轮录制/截取新的 Windows 交互式 TUI 真实画面；现有 TestBackend 回归 32/32 通过，因此美观定稿仍应以改版前后的真实终端截图、录屏及操作实测为准。
