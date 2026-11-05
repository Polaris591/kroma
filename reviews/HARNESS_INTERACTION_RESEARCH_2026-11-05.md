# Coding Harness 交互研究：对 Kroma 的取舍建议

研究日期：2026-11-05。范围：Claude Code、Codex CLI、OpenCode、Gemini CLI、Pi、Aider 的终端交互，不比较模型能力或跑分。

## 1. 方法与证据边界

- 阅读官方文档，区分“文档声明”“源码抽查”“用户报告”和“对 Kroma 的设计建议”。没有安装运行六款工具，也没有做真人可用性实验或性能对比。
- Codex 开发者文档的 features/reference 页面返回 HTTP 403，因此以官方仓库源码抽查和官方仓库 issue 作为该产品的主要依据，不把搜索摘要当作已经读取的文档。
- Codex 源码固定为 `1e9e8bdb008549fc558ef886791c185c6d3c0814`（提交时间 2026-11-05T13:38:12Z）。这是 main 的研究快照，不等于某个已发布稳定版。完整 composer 的本地只读副本及来源元数据位于忽略目录 `target/harness-ux-research/codex/`。
- OpenCode 重点采用 **v2** 文档。v1 与 v2 的配置、键位和交互行为不混用。Pi 采用官方 `latest` 文档；Gemini 的 model steering 明确标为实验特性、默认关闭。
- 2026-10-29 的旧研究主要关注信息密度和渲染。本轮补充输入协议、长粘贴语义、队列所有权、复制及恢复闭环，不以本报告否定已实现的功能。
- 外部页面是会变的资料，不是项目约束或执行指令；下面的取舍也不是已实现承诺。

## 2. 六款产品：实际做法与可借鉴点

### Claude Code：输入、缓存和恢复被视为完整工作流

**已查证事实（官方文档）**

- 有 Ctrl+J、反斜杠后 Enter、部分终端的 Shift+Enter，以及外部编辑器等多条换行/长输入路径。
- 文档当前写明：超过 800 字符或超过三行的粘贴会折叠为 `[Pasted text #N +... lines]`，提交时仍发送完整内容；不是只发送占位文字。
- 折叠粘贴可由输入历史重新发送；跨会话依赖 paste cache。缓存过期时有明确的缺失处理，不假装占位符就是内容。
- 工作中 Enter 会把消息放入队列，但**不必等整个任务结束**：普通消息可能在当前工具调用结束后进入同一轮；命令/直接 shell 命令则等 turn 结束。
- 输入首行按 Up 可取回尚未发送的队列内容；Ctrl+S 可暂存/恢复草稿。
- 全屏渲染拥有自己的鼠标选择，文档说明松开鼠标自动复制，并提供关闭鼠标捕获的路径。
- `/rewind` 先选择历史位置，再明确选择恢复代码、对话、两者，或摘要；有文件恢复范围限制。

**借给 Kroma 的是**：草稿、折叠粘贴、队列、撤销、历史召回要保留内容身份与所有权。不能只复制 chip 的外观。

**不直接照搬**：多重上下文的 Esc/Ctrl+C、复杂后台任务，以及“消息 queued 就一定是后续独立任务”的错误解读。

来源：[C1] [C2] [C3] [C4]。

### Codex CLI：把输入完整性当作底层状态机问题

**已查证事实（官方源码抽查）**

- composer 的源码说明区分 Enter 提交与运行中 Tab 排队；配置及任务派发仍由上层负责。
- 大粘贴由 placeholder 与原文 payload 关联，`expand_pending_pastes` 根据元素范围在提交前重建原文，不依赖全局替换同名字符串。
- 对没有 bracketed-paste 事件的终端，专门有 `PasteBurst` 状态机，识别 Char/Enter/Tab 的密集输入流。非 ASCII/IME 有不暂扣首字的路径。
- 当前源码说明明确要求粘贴中的 Tab 在菜单/快捷键分发前归入粘贴；缓冲区在焦点、鼠标、提交等转换时必须正确 flush。
- 提交/排队只接受新按下事件，吞掉按键重复，避免一个长按在界面切换后意外再次提交。

**用户报告，不能当作当前版本故障结论**

- #13044 抱怨 Enter 引导比排队影响更大，容易误触；维护者后来以可配置键位支持结束该 issue。
- #44490 报告旧版本在 Windows Terminal 中把粘贴的 Tab 缩进当成排队快捷键。这是具体版本报告，issue 已关闭，当前 main 已有相关处理设计；本轮没有复现或验证某个发行版修复效果。

**借给 Kroma 的是**：先识别文本输入/粘贴，再分发会产生动作的快捷键。仅处理 `Event::Paste` 或增加 Alt+V，不能证明原生 Windows 的普通终端粘贴已经完整可靠。

**不直接照搬**：固定的启发式时间阈值。IME、快速键入、慢粘贴、SSH 都可能改变误判边界，需要事件测试和实际终端验证。

来源：[D1] [D2] [D3] [D4]。

### OpenCode v2：命令面板是完整的动作入口

**已查证事实（v2 官方文档）**

- 工作中 Enter 引导当前 session；默认 Ctrl+X 后 Enter 将提示加入后续队列。
- Ctrl+P 展示当前视图的动作，`/` 提供命令搜索；不是只能靠记忆 leader 组合键。
- 键位表有队列管理、取回/撤销队列项、删除队列项、草稿 stash 等命名动作。
- 支持 Shift+Enter/Ctrl+Enter/Alt+Enter/Ctrl+J 换行、方向键选区及外部编辑器。
- `/undo` 会把回退的提示放回 composer，用户可以编辑后继续；`/redo` 提供返回路径。

**借给 Kroma 的是**：复制、展开、停止、队列编辑等也应可从动作菜单发现；快捷键只是加速器。

**不直接照搬**：大量 leader 组合、会话 tabs、侧栏、终端面板或共享后台服务。它们适合 OpenCode 的产品规模，不是 Kroma 输入问题的必要答案。

来源：[O1] [O2] [O3]。

### Gemini CLI：让折叠、复制和鼠标模式有显式开关

**已查证事实（官方文档）**

- Tab 的命名动作是排队等待当前任务完成。
- Enter 引导要求开启实验 model steering；特性文档说默认关闭。因此不能用该文档推导所有默认运行中 Enter 的行为。
- 大粘贴显示 placeholder，可在其上 Ctrl+O 展开/折叠；alternate buffer 中还支持双击。
- F9 在 alternate buffer 中切换 copy mode；Ctrl+S 控制鼠标模式；空输入 `?` 显示上下文快捷键。
- 多种换行路径及 Ctrl+G 外部编辑器。Rewind 先选交互点，再区分 conversation/code/both；不把手工或 shell 改动纳入 AI 编辑回退保证。

**借给 Kroma 的是**：折叠状态应可检查；复制模式应容易退出；运行状态和焦点不同，提示可用动作也应不同。

**不直接照搬**：实验开关及键位组合本身。与 Kroma 现有绑定冲突时，应优先保持兼容并明确冲突。

来源：[G1] [G2] [G3]。

### Pi：更接近 Kroma 的自主、轻量交互哲学

**已查证事实（官方文档）**

- 无逐工具审批流程；强调自主执行，不能把这种便利理解成文件系统隔离保证。
- 工作中 Enter = steering：在当前 response/tool calls 之后、下一次响应之前交付。Alt+Enter = follow-up：当前任务结束后才交付。
- Alt+Up 可取回队列消息；中断时队列可恢复到编辑器。
- Windows/WSL 默认用 Ctrl+Q 作为 follow-up，因为 Windows Terminal 会拦截 Alt+Enter。**Kroma 的 Ctrl+Q 已是退出键，不能照搬。**
- `/hotkeys` 显示当前键位；Ctrl+O 展开工具、Ctrl+T 控制 thinking；`/copy` 或 Ctrl+X 复制最近回答；Ctrl+G 用外部编辑器。
- `/tree`、`/fork`、`/clone` 区分分支意图，历史提示可以回到编辑器；这不是文件回滚承诺。
- 终端设置文档专门处理修饰键丢失、系统拦截、IME 光标和不同终端的替代路径。

**借给 Kroma 的是**：少量明确的意图、可见的交付边界、可撤回的待发送消息，以及针对真实终端的降级说明。

**不直接照搬**：终端相关键位或把 session tree 一步升级成新的存储/执行架构。

来源：[P1] [P2] [P3]。

### Aider：不依赖复杂 TUI 的可移植兜底

**已查证事实（官方文档）**

- 支持直接多行粘贴、`{ ... }`/带标签的多行输入，以及 Meta+Enter 换行。
- `/multiline-mode` 可对调 Enter 与 Meta+Enter 的换行/发送职责。
- `/editor` 或 Ctrl+X Ctrl+E；`/paste` 从剪贴板取内容；`/copy` 复制最近回答，`/copy-context` 复制上下文。
- 中断保留已有的部分回复；`/undo` 针对 Aider 创建的最近 Git 提交，不是任意操作的事务回滚。

**借给 Kroma 的是**：终端不转发组合键时，还有可输入的命令和外部编辑器；这些路径应该清晰可发现。

**不直接照搬**：为了获得 `/undo` 而引入自动 Git 提交策略，或为兼容某个组合键改写默认工作流。

来源：[A1]。

## 3. 跨产品结论：不是选一个“最好看的”照着抄

### 3.1 折叠粘贴不等于变成日志附件

需要分开设计三件事：

1. **显示**：输入框不被几千行撑满，可以显示摘要/占位符。
2. **编辑与所有权**：原文还在，能查看、删除、撤销、复制、历史召回，不能只留下一个已经失效的文件名。
3. **模型投影**：提交时到底发送原文，还是要求模型读取附件？引用外部文本的信任/来源标记是否保留？

Claude/Codex 的占位符有原文 backing，并不等价于 Kroma 的自动日志附件。Gemini 文档也直接提供展开入口。它们各自的信任处理、上下文限制和可编辑能力不能因此被视为完全相同。

**建议判断**：普通文本粘贴优先按“可恢复的文本输入”设计；显式附加日志仍有价值。展示折叠与超长输入预算分别决定，不因为视口只有八行就把内容语义改成文件，也不承诺无限原文进入模型上下文。现有 4 KiB 策略是否改动需另行确认，不在本轮研究中偷偷调整。

### 3.2 “排队”必须解释交付边界

- Claude 的 queued 普通消息可能进入同一轮；Pi 的 follow-up 等整个当前任务完成。
- Codex/OpenCode/Pi 都能表达“影响当前任务”和“另起后续任务”，但键位并不一致。
- Gemini 的 steering 还取决于实验设置。

因此没有一套所有人都必须采用的 Enter 默认值。Kroma 的 **Enter → 后续任务，显式 `/steer` → 当前任务引导** 更保守，完全可以保留。真正要补的是跨端一致的意图、明确的“尚未交付/已交付”状态、可编辑队列，而不是跟风换键。

### 3.3 全屏与原生复制有真实取舍

Alternate screen、鼠标捕获和应用内滚动会改变原生终端选区、搜索与 scrollback。成熟产品选择不同：内置 copy mode、鼠标捕获开关、独立 transcript/export、普通终端模式。没有一个鼠标实现能替代所有终端原生行为。

建议先明确 Kroma 的原生选择逃生口、应用内复制成功反馈，以及最近回答/代码块的复制入口，再考虑更多鼠标拖动逻辑。复制内容不能混入状态标记、折叠装饰或行号。

### 3.4 恢复的对象要显式区分

“取回草稿”“撤回未交付消息”“停止当前任务”“回到对话历史”“恢复文件”不是同一件事。

借鉴各产品的预览/选择界面，但保留 Kroma `/undo`/`/rewind` 与 `/restore-files` 的语义边界。取消不等于回滚外部副作用，历史分支也不等于撤销 shell、数据库或第三方请求。

## 4. 对照当前 Kroma：保留什么、缺什么

以下是当前工作树的实现核对，不是六款竞品的行为推断。

| 范围 | 当前情况 | 设计上的下一步 |
| --- | --- | --- |
| 多行编辑 | 已有 Ctrl+J、外部编辑器、选择替换、撤销；短文本粘贴不再按行数强制附件化 | 保留，不再从头造编辑器 |
| 长粘贴 | `src/tui/attachments.rs::long_paste` 以 UTF-8 **字节数 > 4 KiB** 判定；`src/tui/mod.rs::attach_text` 转日志附件；运行中大粘贴可见地拒绝 | 将展示折叠与模型投影分开评估；超过阈值不能只剩难以查看的摘要 |
| 附件投影 | `src/attachments.rs` 中大日志最终会呈现附件读取提示，可由模型使用 shell 按需读取完整内容 | 这是明确的附件语义，不应宣传成“只是把原文折叠显示” |
| Windows 输入 | 有 bracketed paste 与 Ctrl+V/Shift+Insert 文本剪贴板路径；Alt+V 是空闲态媒体/文本路径；后两者仍依赖终端键传递 | 原始 Char/Enter/Tab 粘贴突发还需要专项验证，不能把替代快捷键当作已彻底解决 |
| 运行中输入 | TUI `running_input`：Enter 加 follow-up，`/steer` 引导；Desktop `App.svelte` 运行中提交调用 `api.steer` | 先统一意图和说明；不默认改成“所有端 Enter 都引导” |
| 队列 | `src/steering.rs` 有追加、列出、取下一条、全部清空；TUI 有 `/pending`、`/queue list`、`/queue clear` | 缺用户可操作的逐项取回/编辑/删除闭环；改动应限在 host/inbox/UI 边界 |
| 选择与取消 | Ctrl+C 有选区优先复制、清理草稿/取消任务等上下文行为；Ctrl+Q 退出 | 不简单宣称所有复用都错；让当前会执行的动作可见，停止操作始终可找到 |
| 发现与反馈 | 已有帮助、命令选择器、工具展开；但 Plain 的空输入提示弱，工具成功行的展开热区小，通知很快随下一输入清除 | 增加上下文动作入口与关键失败的可回看性，不把已有功能误报成不存在 |
| 恢复 | 对话撤回与文件恢复已分开 | 补范围预览和入口说明，而非把两者混为一个万能 undo |

另有文档漂移：`reviews/tui-plain-study/README.md` 仍描述运行中 Enter 引导，与当前 TUI 不一致。本轮不覆盖旧评审记录；未来实现/文档整理时应标注其历史语境。

## 5. 推荐优先级与实施边界

### P0：先保证输入不误触发、运行意图不含糊

1. 在 UI 输入层验证真实 Windows 粘贴。bracketed paste 优先；缺失协议时，先设计有测试支撑的归一化/分类，再让快捷键分发消费事件。覆盖 Enter、Tab、中文/IME、重复按键，不盲抄竞争产品的毫秒数。
2. 对齐 TUI/Desktop 的“当前任务引导”和“后续任务”。可以继续保留 TUI 的保守 Enter 默认；提交前后都显示交付意图。实际交付应由真实事件确认，不能因为用户按了键就宣布模型已接收。
3. 明确失败时草稿/粘贴 payload 的所有权：不能丢失、静默截断或自动重发。

这层不要求更换 Agent Loop、模型调度、超时、并发或审批策略。

### P1：可逆的长输入与队列管理

1. 明确区分“普通文本粘贴”和“显式日志附件”；如采用折叠文本，先规定查看、编辑、撤销、删除、复制、历史召回和缺失 payload 行为，再确定长度/预算策略。
2. 队列显示条目摘要、类型和状态，支持逐条取回编辑/删除。取回必须原子地移除未消费项；已有草稿要先保留或明确合并，不能静默覆盖。
3. 中断后说明哪些输入仍未交付、哪些已经交付。仅恢复未消费内容；不能把已消费 steering 假装成未发送草稿，也不能在取消后自动重复执行任务。

### P2：少量可发现的动作，而非更多常驻 UI

1. 扩展现有命令/动作入口，纳入队列、展开工具、复制、停止、恢复范围等；帮助和提示尽量复用同一动作定义与当前绑定。
2. 给全屏复制提供明确模式或鼠标捕获逃生口；补“复制最近回答/代码块”的低成本入口及成功/失败反馈。
3. 关键失败保留在可回看的记录中；恢复前展示对话与文件的作用范围。

不为此引入侧栏框架、守护服务、逐工具审批、自动 Git 提交，也不重写 Agent Loop。视觉润色排在这些行为闭环之后。

## 6. 一个小的交互草图（提案，不是现有行为）

```text
空闲且输入聚焦：  Enter 发送 · Ctrl+J 换行 · 动作菜单
运行且输入聚焦：  Enter 后续任务 · /steer 引导当前任务 · 后续 2 条

后续任务
  #1  尚未领取   修复解析边界……    [取回编辑] [删除]
  #2  尚未领取   再补回归测试……    [取回编辑] [删除]
```

- 复用现有单列视图与面板；不要求常驻侧栏或永久增加状态栏高度。
- 引导入口可从动作菜单发现，具体快捷键以后结合终端兼容性再定。
- “尚未领取”“已交付”的转换必须有可靠状态依据。做不到确认时就保守表述，不伪造回执。
- 取回后只进入编辑器，等待用户显式发送；取消、编辑器保存和历史选择均不隐式重新发送。
- 保存 Kroma 现有的高自主执行定位；输入意图透明不等于加审批链。

## 7. 未来验证任务（本轮没有执行）

先测实际使用的 **原生 Windows Terminal 和 VS Code 终端**，再补 WSL、Linux+tmux、macOS 终端。每项记录 OS、终端版本、键盘协议、鼠标捕获状态、是否 bracketed paste，不把本地 clipboard 的可用性外推到 SSH。

| 场景 | 验收重点 |
| --- | --- |
| 原生 Windows 粘贴含 CRLF、空行、真实 Tab、中文、emoji 的 50 行文本 | 不自动提交、不创建额外队列项、不打开帮助/补全菜单；内容不丢失 |
| 同一文本分别通过 bracketed paste、普通终端粘贴、显式剪贴板、外部编辑器输入 | 除用户明确选择附件外，文本内容和提交意图一致 |
| UTF-8 恰好 4096 字节、4097 字节、多字节字符跨阈值、超预算输入 | 行为与明确的产品策略一致；不把字符数当字节数，不切断 UTF-8，不静默截断 |
| 快速键入、IME 候选确认、慢速粘贴、按住 Enter/Tab | 不把正常输入误认成粘贴；不因按键重复产生多次任务 |
| 当前任务运行时输入 follow-up 和 steering | UI 明确区分交付边界；TUI/Desktop 不把同一提示暗中解释成不同意图 |
| 排队后取回、已有草稿时取回、取回时后台恰好领取该项 | 不覆盖草稿、不重复执行、不丢失内容；已消费项不可伪装成待发送 |
| 中断、工具失败、恢复会话、删除折叠粘贴后撤销 | 草稿与 payload 保持一致；无孤立占位符；恢复不自动重复执行 |
| 选中输入后 Ctrl+C、无选区时停止、重绘/缩放时复制回答 | 操作可预测；复制只含目标原文；停止入口不被选区困住 |
| 全屏鼠标选择、关闭捕获、远程终端复制失败 | 有明确退出/降级路径和失败反馈，不把“已尝试”说成“已复制” |
| 对话撤回与文件恢复 | 预览明确范围；不声称可撤销未被跟踪的 shell/外部副作用 |

实现后先用确定性事件测试覆盖输入状态，再做实际终端手测。计时/IME 兼容性不能仅靠合成 `Event::Paste` 测试验收。若后续要判断“更好用”，可做短时任务观察：首次多行输入、修改待发送消息、停止任务、复制回答、恢复草稿，记录误发送、找不到入口和内容丢失，而不是只问界面是否好看。

## 8. 最终取舍

**保留 Kroma 的单列、克制、高自主性；从 Pi 借清晰的交付意图，从 Codex 借输入完整性，从 Claude 借草稿/队列恢复，从 Gemini 借折叠与复制模式，从 OpenCode 借动作发现，从 Aider 借终端无关的编辑兜底。**

优先顺序：输入可靠性与跨端意图 → 可逆粘贴/队列 → 复制和动作发现 → 恢复预览 → 视觉润色。研究本身不修改应用代码、运行参数或 Agent Loop，也不宣称这些建议已实现或经过真人验证。

## 9. 官方来源与局限

以下标记也用于正文引用。网页内容以本次读取为准；Codex 源码使用固定提交。

- [C1] Claude Code — Interactive mode：多行、长粘贴、队列交付与取回、草稿。
- [C2] Claude Code — Terminal config：终端键传递、选择/复制及模式设置。
- [C3] Claude Code — Fullscreen：alternate screen 与应用内鼠标选择。
- [C4] Claude Code — Checkpointing：恢复对象与文件覆盖范围。
- [D1] OpenAI Codex — `chat_composer.rs` 固定提交：大粘贴 payload、提交恢复和按键处理。
- [D2] OpenAI Codex — `paste_burst.rs` 同一提交：非 bracketed paste 状态机。
- [D3] OpenAI Codex issue #13044：Enter/Tab 默认的定性反馈，已关闭；不是统计研究。
- [D4] OpenAI Codex issue #44490：Windows 粘贴 Tab 的历史版本报告，已关闭；非当前版本复现结果。
- [O1] OpenCode v2 — TUI：工作中提示、队列、编辑器和交互入口。
- [O2] OpenCode v2 — Keybinds：命名动作、队列、草稿和选择操作。
- [O3] OpenCode v2 — CLI：整体 CLI/服务组织背景；不据此建议复制架构。
- [G1] Gemini CLI — Keyboard shortcuts：折叠粘贴、复制模式、鼠标、编辑器。
- [G2] Gemini CLI — Model steering：实验设置与 steering/follow-up 区别。
- [G3] Gemini CLI — Rewind：范围选择与回退限制。
- [P1] Pi — Usage：输入、steering/follow-up、复制、工具视图、权限定位。
- [P2] Pi — Sessions：历史树、分支与提示恢复。
- [P3] Pi — Terminal setup：Windows/WSL 键位替代与终端协议问题。
- [A1] Aider — Commands：多行、editor、paste/copy、interrupt、Git undo。

[C1]: https://code.claude.com/docs/en/interactive-mode
[C2]: https://code.claude.com/docs/en/terminal-config
[C3]: https://code.claude.com/docs/en/fullscreen
[C4]: https://code.claude.com/docs/en/checkpointing
[D1]: https://raw.githubusercontent.com/openai/codex/1e9e8bdb008549fc558ef886791c185c6d3c0814/codex-rs/tui/src/bottom_pane/chat_composer.rs
[D2]: https://raw.githubusercontent.com/openai/codex/1e9e8bdb008549fc558ef886791c185c6d3c0814/codex-rs/tui/src/bottom_pane/paste_burst.rs
[D3]: https://github.com/openai/codex/issues/13044
[D4]: https://github.com/openai/codex/issues/44490
[O1]: https://opencode.ai/v2/docs/cli/tui
[O2]: https://opencode.ai/v2/docs/cli/keybinds
[O3]: https://opencode.ai/v2/docs/cli
[G1]: https://geminicli.com/docs/reference/keyboard-shortcuts/
[G2]: https://geminicli.com/docs/cli/model-steering/
[G3]: https://geminicli.com/docs/cli/rewind/
[P1]: https://pi.dev/docs/latest/usage
[P2]: https://pi.dev/docs/latest/sessions
[P3]: https://pi.dev/docs/latest/terminal-setup
[A1]: https://aider.chat/docs/usage/commands.html
