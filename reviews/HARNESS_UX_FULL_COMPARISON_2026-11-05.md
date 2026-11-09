# 六款 Coding Harness 完整交互流程研究与 Kroma 对照

研究日期：2026-11-05。Kroma 基线：`7b4c8a6` 上的当前脏工作树，不等同于已发布版本。

> **先读结论：Kroma 终端版已经有不少高级能力，主要欠缺是输入可靠性、功能入口和完整操作流程；桌面版则还没有接齐终端版。下一步不应先加更多面板，而应先让同一操作在不同入口、不同状态下意思一致。**

## 1. 本轮查了什么，没查什么

这次不是上一份粘贴/队列研究的改写。覆盖六款终端型产品的公开核心流程：Claude Code、Codex CLI、OpenCode v2、Gemini CLI、Pi、Aider；同时核对 Kroma TUI、行式 CLI、JSONL 与 Desktop 的实际代码入口。

按使用过程覆盖：安装/更新 → 首次配置 → 选择项目和模型 → 输入/引用文件/附件 → 执行/看进度 → 提问/改方向/排队/停止 → 检查修改和测试 → 会话查找/分支/恢复 → 扩展/自动化 → 复制/导出/隐私/退出。具体拆为第 4 节的 40 项。

证据边界：

- 竞品结论来自官方文档或官方仓库源码，不是本轮安装、登录、运行后的体验。没有 GUI 实测、真实终端录屏、真人任务观察或性能跑分。
- Kroma 采用符号定位、关键分支阅读与两项只读子审计。子审计分别聚焦 TUI 与 Desktop；关键跨端语义和 `/undo` 进一步直接核对实现，不只相信帮助文字。
- Codex 官网 features/reference/slash-commands 无法读取（HTTP 403）；raw 地址的网页读取也有网络失败，改用 Node 下载官方固定提交源码做局部检查。**源码存在某入口，不代表所有发行版默认启用它，更不代表已经验证其端到端效果。**
- Codex 固定提交：`1e9e8bdb008549fc558ef886791c185c6d3c0814`。本地证据位于忽略目录 `target/harness-ux-research/codex/` 与 `target/harness-ux-research/full-2026-11-05/codex/`，公开引用使用固定源码 URL，不依赖本机缓存。
- OpenCode 采用 `opencode.ai/v2/docs/`。没有把 v1 的键位、权限默认值、配置格式或存储方式混入 v2。不同官方站点/年代页面可能仍有差异，以所引用页面为准。
- Pi 采用 `latest` 文档；不要把旧项目印象中的“只有四个工具、完全没有 MCP”等说法直接套到当前版本。扩展能力与默认内置能力分开计。
- Gemini 的“Plan Mode 功能默认启用”**不等于默认启动就在 Plan Mode**；model steering 仍在实验设置中，文档默认关闭。键位表与设置表存在上下文/版本差异时，不把单独一张表当成完整行为规范。
- 这里的“完整”是完整核心使用流程，不是穷尽所有插件、企业管理策略、云端收费功能、IDE 内全部界面或每一个系统/终端组合。没查到的不记成竞品“没有”。
- 本轮只新增研究材料；不修改应用代码、Agent Loop、超时、并发、配置或现有用户文件，不安装竞品，不提交或发布。

### 1.1 如何读判断

| 标记 | 含义 |
| --- | --- |
| 保留 | 已实现且符合 Kroma 定位，不应为了模仿重做 |
| 难找 | 有能力，但入口、提示或操作步骤不够顺手 |
| 待补 | 在所核对核心入口中未发现完整用户流程 |
| 不一致 | 同一意图在不同端/不同状态下处理不同 |
| 待实测 | 源码能指出风险，但尚未在实际环境复现 |
| 产品取舍 | 不同不等于缺陷，需单独决定是否改变定位 |

不做没有依据的“竞品 9 分、我们 6 分”，也不把功能数量当易用性。

## 2. 六款产品各自是怎样让用户工作的

### 2.1 Claude Code：围绕长时间连续工作组织交互

- **开始**：首次会话引导浏览器认证，之后可 `/login` 重新认证；启动信息展示模型、目录和版本。[C6]
- **写请求**：多种换行方式、外部编辑器、Vim、输入历史搜索、草稿 stash/恢复；长粘贴占位符保留原文 backing，历史重新发送不把占位符当正文。[C1] [C8]
- **补上下文**：`@` 文件/目录/MCP 资源具有不同含义；目录是列表，不等于递归塞入全部内容；文件自动包含也受大小/权限限制。[C4]
- **执行**：用户可看任务清单、详细工具记录、后台任务；部分只读/显示命令运行中立即可用，其他命令等当前 turn 结束。审批还可附加“为什么拒绝/应该怎么做”的说明。[C1] [C2] [C5]
- **中途插话**：普通 queued 消息可能在当前工具结束后进入同一轮，不能把它直接等同于 Pi 的 follow-up。可取回未发送项或暂存当前草稿。[C1]
- **看结果**：`/diff`、按回答/代码块复制、导出、全文查看/搜索；全屏同时提供原生 scrollback 和编辑器查看出口。[C2] [C7]
- **继续与恢复**：会话列表能搜索、预览、改名、扩大项目/分支范围；分支、后台复制、rewind 和文件恢复的范围分别说明。[C3] [C9]
- **代价与限制**：功能多、状态多，Esc/Ctrl+C/Tab 都不是永远单一用途；可用功能受版本、账户和渲染模式影响。

**对 Kroma 最有价值**：把“输入—执行—取回—继续”做成连贯流程，而不是仅多一个快捷键。**不建议照搬**：复杂云端入口、权限模式和后台会话体系。

### 2.2 Codex CLI：重视输入状态、上下文与可管理的任务入口

以下为固定提交源码抽查，不宣称整个稳定版均已实测：

- 登录是有状态的选择流程：ChatGPT、设备码、API key 等路径，错误和完成状态明确区分。[D4]
- Composer 分离编辑、历史、附件、补全和 paste burst；粘贴中的 Enter/Tab 要在会产生副作用的快捷键分发前识别。大粘贴以元素范围绑定 payload，在提交前重建，不做脆弱的同名字符串替换。[D1] [D2]
- 源码里存在 `/model`、`/permissions`、`/status`、`/diff`、`/copy`、`/export`、`/raw`、`/warnings`、`/keymap`、`/vim`、会话和 agent 等命令；并明确区分哪些可在任务运行中使用。这里只确认命令定义及策略，不据此保证所有设置组合中的完整效果。[D3]
- 审批选项区分一次、当前会话、命令前缀/未来主机等范围，拒绝与停止并不是同一选项。[D5]
- 多问题回答复用 composer：选项加备注、每题草稿、逐题前进、最后统一提交，未回答项有确认流程。[D7]
- 会话选择器有分页、搜索、范围筛选与按需加载的记录预览；不是把所有历史一次性塞进列表。[D6]

**对 Kroma 最有价值**：输入完整性、根据状态开放命令、保留警告、选择前预览。**不建议照搬**：时间阈值、默认键位或为了 UX 移植其后台服务架构。

### 2.3 OpenCode v2：以动作菜单和工作区管理为中心

- `/connect` 连接服务商和账户，API key、OAuth、设备码/授权链接等按方法引导；账户能激活、改名、删除。模型选择与连接账户是不同动作。[O6] [O7]
- 输入有 `@path#range`、shell mode、外部编辑器；工作中 Enter 引导，leader+Enter 排后续；队列有专门管理动作。[O1] [O2]
- Ctrl+P 展示当前视图动作；队列、草稿、复制、会话、模型、子代理、终端和 diff 使用有名字的动作体系。[O2]
- Diff viewer 支持按范围看修改、逐文件/逐 hunk 跳转、split/unified、标记已审阅；这是完整审阅流程，不只是打印 `git diff`。[O2] [O8]
- 有会话 tabs、父子会话导航、后台 shell 查看与通知；也有 `mini` 模式。设置可控制鼠标、粘贴摘要、阅读位置和提示详情。[O2] [O3] [O8]
- Build、Plan 与子代理区分主/子用途。**不要把 Plan 当成任意命令都无法写磁盘的通用沙箱**：v2 文档说明主要限制普通文件编辑，shell 仍受其权限规则控制。[O4] [O5]
- `/btw` 将旁支问题放在独立结果对话框，不加入后续主对话。[O1]

**对 Kroma 最有价值**：动作菜单、队列管理、审阅范围。**不建议照搬**：tabs/侧栏/共享服务整套架构；特别不能照抄 v1 的权限默认值来解释 v2。

### 2.4 Gemini CLI：设置、模式与用户问答比较完整

- Google 登录、API key、Vertex 的认证方法分开引导；`/settings` 支持搜索设置，说明作用域与部分设置需要重启。[G7] [G3]
- `@` 文件、目录、`!` shell、编辑器、历史搜索、Vim 等各有入口。文件引用可能注入内容；目录引用则交给模型用工具探索，不应一概理解成递归读取。[G1]
- 有折叠粘贴展开、复制模式、鼠标开关、Markdown 显示开关、快捷键帮助与按上下文复用的命名动作。[G2]
- Plan Mode 是可讨论、编辑计划文件、再批准实施的流程；`ask_user` 是选择/文本/确认对话框，执行会等待回答。[G4] [G6]
- shell 支持 PTY 时能切换焦点把输入送给命令，而不是送给模型；有后台 shell 查看/结束入口，缺 PTY 时功能会降级。[G2] [G8]
- 会话浏览器可预览、按内容搜索、删除；手工命名保存点与自动会话分组呈现。默认历史保留期限也在设置中明确显示。[G1] [G5]
- 设置表包含终端通知、screen reader 纯文本、兼容性警告、工具输出与错误详情选项。[G3]
- **文档限制必须保留**：steering 默认关闭；Plan 功能可用不等于启动模式；新旧键位条目有迁移，不能抄某个 Ctrl+X 就称为跨版本标准。[G2] [G3]

**对 Kroma 最有价值**：问答与模式状态、可搜索设置、兼容性降级、复制/错误详情入口。**不建议照搬**：所有实验开关、自动更新和策略引擎。

### 2.5 Pi：保持自主执行，同时让输入和会话可逆

- `/login` 先选服务商，`/model` 再搜可用模型；“本次切换”“保存成以后默认”“恢复会话时还原”分开处理。模型选择限制为已具备认证的服务商。[P5] [P7]
- 输入、工具输出、thinking、模型强度、当前目录和用量各有小入口；启动头部列出加载的指令和资源。[P1]
- Enter 引导当前任务，Alt+Enter 后续任务，Alt+Up 取回；Windows/WSL 使用另一组默认键，并解释操作系统拦截原因。[P1] [P8] [P9]
- 全屏和普通 scrollback 模式可切换；全屏有记录搜索与提示跳转。`/hotkeys` 展示当前有效绑定，`/reload` 重载资源。[P1] [P2] [P8]
- 会话选择器能搜索、排序、改名、删除；`/tree`、`/fork`、`/clone` 区分在原会话中换分支、从旧消息另开会话、复制当前分支。[P3]
- 没有逐工具审批，但有项目资源信任。两者都不构成沙箱。扩展可以提供问答、权限提示、检查点等，不能把扩展示例算成默认都已内置。[P4] [P6]
- 复制、HTML/JSONL 导出、上传分享、bug report 的入口与敏感内容提示分别说明。[P1] [P3]

**对 Kroma 最有价值**：清晰的意图、当前有效快捷键、资源可见、可逆的会话操作。**不建议照搬**：Windows 上 Ctrl+Q 排队（Kroma 已用它退出）或把整个扩展平台都搬进核心。

### 2.6 Aider：围绕文件、变更和验证来组织工作

- 通过 `/add`、`/drop`、`/read-only` 明确哪些文件是当前上下文，`/ls`、`/map`、`/tokens` 可检查；仓库 map 为其余代码提供受预算限制的概览。[A1] [A4]
- `ask`、`code`、`architect` 模式提示符不同；单条消息的模式与持续模式不同。Architect 是两个阶段/模型调用，不是更便宜的普通聊天模式。[A2]
- 多行直接粘贴之外，有显式多行模式、文本定界符、外部编辑器、Vim/Emacs 及历史搜索等兜底。[A1]
- `/run` 可选择是否把输出给模型，`/test` 在失败时送入错误，自动 lint/可配置自动测试让验证成为工作流的一部分，而不是只有口头“完成”。[A1] [A6]
- Git 自动提交、预先处理 dirty 文件和 `/undo` 配套；这些行为会碰用户仓库，不能只复制 undo 按钮而忽略副作用。[A3]
- `--watch-files` 通过 IDE 中的 AI!/AI? 注释触发操作，是不依赖整套 IDE 插件的集成路径；命令行也能单次执行任务。[A5] [A7]

**对 Kroma 最有价值**：上下文是否加入、只读还是可改、测试是否真正执行都应清楚。**不建议照搬**：自动替用户提交 dirty worktree，或把文件上下文管理方式移植成新的核心循环。

## 3. 我们已经有什么：先避免误诊

### 3.1 Kroma TUI 不是只有一个输入框

当前已有：

- 多行输入、选区替换、编辑撤销/重做、带附件的输入历史；清理草稿后 Ctrl+Z 可以连同附件恢复。[K3] [K4]
- `/` 命令搜索、Ctrl+P 分类选择器、`@` 路径补全、外部编辑器、图片/文件/日志附件、直接 `!` shell。[K2] [K5] [K6]
- 工具成功折叠、失败突出、全局/单工具展开、thinking 开合、执行检视器；滚动时保持阅读位置、有未读新动态提示。[K7] [K8]
- Ctrl+T 全文面板，面板内 `/` 搜索、n/N 跳转；主记录里 Alt+↑/↓ 跳轮次。**不能把“主视图没有 Ctrl+F”写成整个 TUI 没有搜索。**[K9] [K10]
- 选区复制、专用复制键、右键复制、代码块 Copy、Ctrl+点击链接；SSH 下有 OSC 52 路径。[K6]
- 已保存会话列表、模糊过滤、恢复/改名/导出、对话分支和回退；任务前私有文件快照与显式恢复。[K5] [K11] [K12]
- 模型 profile 切换、支持范围内的 thinking level、主题/语言/宽度设置、模型预算来源与用量查询。[K2] [K5] [K13]
- MCP 懒发现、Skills、插件状态/配置/重载、插件工具卡片和只读面板；已有子代理插件，不是“没有多代理”。[K14] [K15]

这些应保留。主要问题是入口覆盖、状态一致和用户能否走完操作，不是每一项都要从零实现。

### 3.2 Desktop 有基础能力，但不是 TUI 的等功能版本

已有项目选择、profile 表单与连接、会话列表/改名、拖放/选择附件、流式回答/工具卡片、原生文本选区和停止按钮；已有 alert/status 标签、焦点样式和 reduced-motion CSS。[K16] [K17] [K18]

但本轮未发现对应的完整 UI：命令面板、`@` 引用/输入历史、follow-up 队列、会话搜索、对话分支/回退/文件恢复、diff viewer、导出、消息/代码专用复制、模型强度快速切换。部分不只是“少按钮”，相应 Tauri 命令也未开放。[K16] [K19]

特别是 `queue_followup` 虽被注册，但当前**明确返回不支持**，没有消费者；不能把函数名存在当作桌面队列已经实现。[K19]

### 3.3 行式 CLI 和 JSONL 应按自己的场景保留

- 行式 CLI 可运行任务、接收运行中输入、显式排队、暂停/恢复后续队列；工作中普通文本当前默认引导。[K20]
- `--print` / `--json`、stdin 和退出码是脚本入口，不应被全屏交互依赖绑住。[K21]
- 大 stdin 当前超过 16 KiB 会保存后引用，和 TUI 的 4 KiB 粘贴阈值不同；这是不同入口的产品策略，不能让用户误以为所有入口都原文内联。[K21]
- JSONL 明确发任务、模型、工具、上下文、用量、警告与会话事件。Desktop 看不到某阶段，不一定是运行时没有发事件。[K22] [K23]

## 4. 按真实操作逐项对比（40 项）

表中“别人”列只举本轮已核实的代表做法，不表示六家都相同；“待补”也不自动等于最高优先级。

### 4.1 打开工具，知道自己在用什么

| 编号 / 操作 | 别人怎么做 | Kroma TUI / CLI | Kroma Desktop | 判断与取舍 |
| --- | --- | --- | --- | --- |
| J01 启动、首次配置、更新 | Claude/Pi 登录引导；更新方式随安装渠道变化；OpenCode 更新后可看 highlights | 缺模型的交互首启自动进入 setup；有服务商预设、隐藏密钥、可选小请求测试；显式 `update --check`/update，不启动后台检查 | 未配置时进入设置/欢迎入口，有 profile 表单；未见同等引导/测试入口 | **保留+补齐**。不是“我们没有 onboarding”；先改善普通人能否选对协议和模型，不为模仿开启后台网络更新。[C6] [P7] [O1] [K1] [K32] |
| J02 连接服务商/账户 | OpenCode /connect、Pi /login 区分认证和模型选择，说明凭证来源 | profile/env/custom endpoint，setup 可检测已有环境变量；TUI /model 选择已保存 profile | 保存配置、连接模型是两个动作，连接状态与编辑中配置容易混淆 | **难找/待补**。明确“已保存/已连接/下次使用”；OAuth 不是必须追平的基础能力。[O7] [P5] [K1] [K17] |
| J03 项目目录/信任 | Pi 明确工作目录及项目资源信任；OpenCode 说明 server 所在机器和环境变量 | --cwd；项目 MCP 与项目插件需显式信任/启用；不是每次 shell 都审批，目录不是沙箱 | 能选项目，有项目 MCP 批准并重连入口 | **保留**。把目录、信任对象、远程服务位置说明白，不把“信任项目”宣传成隔离保证。[P4] [O7] [K12] [K16] |
| J04 换模型/强度 | Pi 区分当前、保存默认、会话恢复；OpenCode 模型/agent/variant 分开 | /model 选 profile；/think、Shift+Tab；会话保存模型选择；/model-status 显示有效限制及来源 | 顶栏模型只读；设置有 reasoning 字段，无会话内快捷选择器 | **TUI 保留、Desktop 待补**。不支持的强度不能假装可用；当前模型和新编辑配置要分开显示。[P5] [O2] [K5] [K13] [K17] |
| J05 设置/自定义键位 | Gemini 可搜设置，Pi /hotkeys 查当前绑定，OpenCode 命名动作可重绑 | 已有主题、中英、宽度和插件快捷键；本轮未见面向全部内置动作的键位配置/统一设置搜索 | 有模型/插件表单；无全局快捷键设置 | **待补但分阶段**。先统一动作定义与现有提示，后做重绑；不是先造复杂配置框架。[G3] [P8] [O2] [K2] [K24] |
| J06 找到可用操作 | Claude 的 / 菜单、OpenCode Ctrl+P、Pi 当前会话命令列表 | /、/help、Ctrl+P 已有；Ctrl+P 仅空闲；有些复制/检视动作不在命令目录里 | 无对应动作搜索/命令面板 | **难找+不一致**。扩展已有菜单，区分当前可用和需等任务结束的动作，别再造第二个命令入口。[C2] [O2] [P2] [K2] [K6] [K29] |

### 4.2 写请求和提供材料

| 编号 / 操作 | 别人怎么做 | Kroma TUI / CLI | Kroma Desktop | 判断与取舍 |
| --- | --- | --- | --- | --- |
| J07 多行/选区/撤销 | Claude/Pi/Aider 都有多行、编辑和历史；终端修饰键有条件 | 已有多行、选区替换、撤销/重做、Unicode 编辑；组合键能否到达应用仍待终端验证 | textarea，Enter 发送、Shift+Enter 换行，有 isComposing 保护 | **保留**。跨端统一意图，不强求按键字面完全相同；不能把自动化事件测试当真实 IME 验收。[C1] [P8] [A1] [K3] [K16] |
| J08 长文本粘贴 | Claude/Codex 占位符有原文 backing；Gemini 可展开；OpenCode 可选 compact/full | >4 KiB UTF-8 字节转日志附件；短文本不按行数强制附件化；运行中大粘贴拒绝；原始 Windows paste burst 未系统实测 | 原生 textarea 文本输入，未见相同折叠预览流程 | **待补/待实测**。先保证不误发送，再分开决定展示折叠、编辑恢复、模型投影。[C8] [D1] [G2] [O8] [K6] [K35] [K36] |
| J09 引用文件/行范围 | OpenCode @path#range；Claude/Gemini 会按规则插入文件；Pi 有文件搜索 | @ 路径补全已有；当前是指定目录内前缀候选、最多 16 项；有效文件被转换为模型按需用 Shell 读的路径，不是全文注入 | 通过附件选文件；无对应 @ 搜索 | **保留语义、补提示**。不应把 @ 当成统一协议；明确引用还是已读入。递归模糊找文件/行范围可单独评估。[O1] [C4] [G1] [K5] [K25] |
| J10 图片/文件附件 | Claude 图片 chip、Pi 兼容终端粘贴、OpenCode 可预览图片 | Alt+V/路径导入、inline chip、最多 8 个、历史引用；运行中附件受限 | 文件选择/拖放、可删除 chip、最多 8 个；前端未见大小/类型预检、图片粘贴及预览 | **保留+补齐**。模型是否支持图片、文件过大/不可读最好发送前可见，不能只在清空输入后报错。[C1] [P1] [O8] [K4] [K6] [K16] |
| J11 草稿/历史召回 | Claude stash/清空草稿恢复、Pi 取回队列、Aider 历史搜索 | 已有输入历史及附件恢复；清空草稿后 Ctrl+Z 恢复；未见独立多草稿列表/跨进程未发送草稿持久化 | 页面内 prompt；发送前清空，异常路径未见自动恢复原始草稿；未见输入历史/持久草稿 | **TUI 不是缺失；Desktop 有风险待测**。失败不等于自动重发；回取队列不能覆盖另一份草稿。[C1] [P1] [A1] [K3] [K4] [K16] |
| J12 外部编辑器 | Claude/Pi/Aider/OpenCode 都有，保存后回到输入区再发送 | Ctrl+G、/editor 已有；可见草稿带附件时有约束；运行中不可借用终端编辑 | 未见对应按钮/命令 | **保留/低成本发现**。运行中不开放可以是合理取舍，但要说明原因；不把保存等同发送。[C1] [P1] [A1] [O1] [K6] |

### 4.3 看它做事，并在中途补充要求

| 编号 / 操作 | 别人怎么做 | Kroma TUI / CLI | Kroma Desktop | 判断与取舍 |
| --- | --- | --- | --- | --- |
| J13 进度/用量/上下文 | Pi 目录/模型/用量，Claude 任务清单，Gemini 工作/需操作状态 | 已区分 preparing/compacting/waiting/streaming 等；有用量/cache/context 与估计标记；未见完整货币费用/账户额度面板 | 有 running、流式文本、基础用量；compaction 等事件未在 switch 中消费 | **TUI 保留、Desktop 补齐**。先说正在做什么，缺失用量保留未知；费用不能从 token 数凭空换算。[P1] [G3] [K13] [K23] [K34] |
| J14 查看工具命令/输出 | Claude transcript、Pi 工具开合、Codex retained warnings、OpenCode shell viewer | Ctrl+O 全局、单工具 chevron、失败常显、/inspect 只读检视；成功热区小 | 历史工具 details；实时卡片保留输出尾部；有 output 时 result 被 UI 取值遮住，stdout/stderr 不分显示 | **难找+Desktop 待补**。命令、状态、退出码与完整输出路径应可找到，不能用“有输出”代替成功结果。[C1] [P1] [D3] [O2] [K7] [K8] [K16] [K18] |
| J15 查看/隐藏推理 | Pi thinking 开关；Claude/OpenCode/Gemini 依模式显示 | Ctrl+R；点一个推理段的 chevron 也会切换全局显示，不是单段独立展开 | 推理 details 与运行中 thinking 展示 | **保留+澄清**。全局按钮和单段按钮的外观/文案不应暗示不同作用域；推理不是实际执行证据。[P1] [O8] [G3] [K8] [K18] |
| J16 浏览/搜索记录 | Claude 全文视图搜索/原生 scrollback 出口，Pi 全屏搜索和提示跳转 | 阅读位置保持、未读计数、滚动条、分页/跳轮；Ctrl+T 面板可搜；主视图未见 Ctrl+F 搜索 | 原生滚动，加载更早历史，接近底部跟随；无专用记录搜索 | **TUI 有但难找；Desktop 待补**。先暴露现有搜索，别因为看不到 Ctrl+F 就另写搜索系统。[C7] [P8] [K8] [K9] [K10] [K16] |
| J17 复制/导出结果 | Claude 可挑回答/代码块并写文件；Pi/Aider /copy；OpenCode 消息/会话 copy | 选区/代码复制、右键、SSH OSC 52、/export 已有；未见复制整条消息/最近回答命令；全文面板无选区复制 | 原生选择；未见消息/代码 Copy、导出 UI/宿主命令 | **补入口而非重写渲染**。明确复制什么、是否成功，保留不依赖剪贴板的文件出口。[C2] [P1] [A1] [O2] [K6] [K9] [K11] [K19] |
| J18 工作中再发一句话 | Pi/OpenCode 明确 steering/follow-up；Claude 的 queue 边界不同；Gemini steering 是实验选项 | TUI 普通 Enter 后续任务、显式 /steer 当前引导；行式 CLI 普通文本默认引导 | 普通 Enter 默认引导，toast 反馈 | **不一致，优先级高**。先统一“当前/后续”意图，不必统一成竞品 Enter 默认；接收不等于当前模型已消费。[P1] [O1] [C1] [G3] [K20] [K28] [K16] |
| J19 待发送消息的管理 | Pi 可取回、OpenCode 队列管理/删除、Claude 首行 Up 取回 | 有数量、最多 4 条预览、列出/整队清空；无逐条取回/编辑/删除的用户入口 | 无 follow-up UI；宿主明确不支持 | **待补**。先支持未消费项的原子取回，保留已有草稿；取消后不能自动重复执行。[P1] [O2] [C1] [K28] [K19] |

### 4.4 停下来、做选择、控制执行范围

| 编号 / 操作 | 别人怎么做 | Kroma TUI / CLI | Kroma Desktop | 判断与取舍 |
| --- | --- | --- | --- | --- |
| J20 停止、中断、退出 | Pi Esc 中断，Aider 保留部分回答，Claude 多种退出/中断上下文 | Ctrl+C 选区优先复制，否则取消；Esc 关弹层而不取消；Ctrl+Q 仅空闲退出。运行时保留中断边界，不把取消当回滚 | 有停止按钮；任务结果 cancelled 未被单独展示，最终快照可能仍呈现中断记录 | **保留语义、补反馈/待实测**。不强制加停止确认来拖慢中断；明确选区时 Ctrl+C 会复制，退出需先停任务。[P1] [A1] [C1] [K6] [K16] [K12] |
| J21 先讨论、再实施 | Claude/Gemini Plan，Aider ask/code，OpenCode Plan agent | 能用文字要求“只研究”，但本轮未见内置、强制只读的 Plan/Ask 模式 | 同核心，无对应模式选择 | **产品取舍**。自然语言承诺不是只读保证；真做只读涉及工具策略，不应靠换输入框颜色冒充，也不纳入纯 UI 改造。[C4] [G4] [A2] [O5] [K12] |
| J22 操作审批/项目信任 | Codex/Claude 按请求和作用域审批；OpenCode v2 可配置 ask/deny；Pi 自主执行、项目信任分开 | 明确无逐命令审批；项目 MCP/plugin 信任已有、内容变化失效；不是沙箱 | 有项目 MCP 信任提醒/批准重连 | **保留定位**。这不是必须补的缺陷；但首次使用应让用户理解真实权限。[D5] [C5] [O4] [P4] [K12] [K16] |
| J23 模型问用户问题 | Gemini ask_user、Codex 多问题/备注/未回答确认；Pi 扩展可给交互表单 | 普通聊天可问答；未见核心的“等待回答”问卷/选项控件 | 未见对应表单和等待态 | **可选待补**。澄清问题不等于逐工具审批；若做，需明确跳过/取消/无交互模式结果，不可无限挂起。[G6] [D7] [P6] [K2] [K24] |
| J24 用户直接跑命令 | Pi ! 含上下文、!! 不含；Aider /run 可选择送入输出；Claude/OpenCode shell mode | 已有 ! 直接执行并在本地显示结果；所读入口未见把结果追加为对话消息的步骤，也没有选择纳入输出的流程 | 无独立 shell 输入入口；模型工具调用可见 | **保留+明确语义**。这是“用户直接执行”还是“要求模型执行”，输出是否进入对话，都应说明；先不扩展成终端模拟器。[P1] [A1] [O1] [K37] [K20] |
| J25 交互命令/后台进程 | Gemini PTY 焦点切换、OpenCode shell 查看/结束、Claude /tasks | shell 的 stdin=null，无 PTY 输入接管；成功命令可留下后台后代，但没有用户可管理的后台作业中心 | 无 PTY/后台 shell 控制台 | **产品取舍**。有后台进程不等于有后台任务管理；如以后要支持，应独立设计 stdin 所有权和退出清理，不能本轮偷偷改执行模型。[G8] [O2] [C1] [K33] [K12] |
| J26 子代理/并行工作 | Claude worktree/subagent，OpenCode 父子会话导航，Codex agent 入口 | 已有子代理插件，任务列表/活动/结果/证据目录走工具卡片；不是缺多代理；无完整的会话 tabs 或托管任务中心 | 通用工具卡片呈现，未见专门子会话操作入口 | **保留最小边界**。先让任务摘要和证据路径好找；单项 steering/停止、独立 worktree 是插件/协议能力，不是加按钮就有。[C4] [O2] [D3] [K15] [K38] |

### 4.5 检查修改，找回之前的工作

| 编号 / 操作 | 别人怎么做 | Kroma TUI / CLI | Kroma Desktop | 判断与取舍 |
| --- | --- | --- | --- | --- |
| J27 看修改 | OpenCode diff 有范围/文件/hunk/已审阅，Claude /diff，Aider 按消息 diff | /diff 已有：与 HEAD 对比并列未跟踪文件，长输出明确截断；不是逐轮归因的专门审阅器 | 未见 diff UI/宿主命令 | **TUI 有但流程薄**。先标清比较基线和被截断内容，后考虑文件跳转/上轮修改；不把全部 dirty worktree 说成都是 AI 改的。[O2] [O8] [C2] [A1] [K25] [K19] |
| J28 测试/验证结果 | Aider 自动 lint、可配置自动测试、失败反馈流程 | 模型可以执行 shell 测试，工具有命令/状态/输出；未见独立验证清单/自动测试设置 | 通用工具卡片；实时卡片可能不显示最终结果正文 | **保留执行事实、补结果可读性**。完成一轮不等于功能正确；展示实际命令、退出码和输出，不靠文本“测试通过”判断覆盖范围。[A6] [K7] [K16] |
| J29 找旧会话 | Claude 搜索/预览/分支范围；Pi 搜索排序；Gemini 内容搜索；Codex 懒预览分页 | 会话选择器过滤名称/ID，保留原顺序；本轮未见内容全文搜索和选前预览 | 列表和历史分页；无会话搜索 | **待补**。优先人能辨认的摘要、日期、预览；先不承诺跨项目内容索引。[C3] [P3] [G5] [D6] [K5] [K16] |
| J30 改名/整理会话 | Pi 会话列表改名/删除/排序，OpenCode pin/rename/delete，Codex archive/delete 命令 | /rename 已有；picker 内未见改名/导出/删除；未见用户删除/归档流程 | 当前标题可改名；无搜索/删除/归档；运行中改名控件与 host idle 限制不一致 | **难找+待补/待实测**。删除范围须区分会话、附件、快照；改名无需先记命令。[P3] [O2] [D3] [K11] [K16] [K19] |
| J31 对话分支/撤回 | Pi tree/fork/clone，Claude branch/background fork，OpenCode undo/redo | /history、/fork、/rewind、/undo 已有；旧未来保存为其他会话。实际 undo 是回退最近一轮/未完成尾部，不改文件 | 未见分支/回退 UI 或宿主命令 | **已有但难用，另有文案不符**。选历史内容比先查编号顺手；不能把所有产品的 fork/undo 当相同含义。[P3] [C3] [O1] [K11] [K27] |
| J32 恢复文件 | Claude/Gemini 先选位置与范围；Aider undo 和自己的 Git 提交配套 | 任务前私有 Git 快照、/checkpoints、/restore-files 已有；恢复会重写/删除范围内文件；无交互式恢复前预览 | 未见恢复 UI/宿主命令 | **保留隔离快照，补预览**。这不同于污染用户 Git 提交；非 Git/忽略文件/外部副作用边界必须可见。[C9] [G9] [A3] [K12] [K19] |
| J33 上下文压缩/长期指令 | Claude /context，Gemini /memory list/show/refresh，Pi 加载资源提示，Aider /tokens/add/drop | /compact、context/cache/预算来源、原始历史检索工具已有；AGENTS 按任务刷新并按显式路径加载嵌套规则；未见统一“本轮指令来源”查看器 | 有基础 context/cache 信息；compaction 事件未展示、无 memory 来源入口 | **保留轻量策略，补透明度**。区分发现、启用、本次实际加载；不为漂亮图表加入额外模型调用或新压缩算法。[C2] [G1] [P1] [A1] [K13] [K14] [K26] [K23] |

### 4.6 扩展、兼容、错误与自动化

| 编号 / 操作 | 别人怎么做 | Kroma TUI / CLI | Kroma Desktop | 判断与取舍 |
| --- | --- | --- | --- | --- |
| J34 MCP/Skills/插件 | Gemini 管理/重连/描述/schema；Pi reload 与命令发现；OpenCode 扩展/agent | 已有懒 MCP、/tools 警告、/skills 元数据、插件安装/启用/配置/只读面板/空闲重载；部分身份变更需重启 | 有插件列表/开关/配置及 MCP trust；plugin_warnings 未在主 UI 渲染 | **已有，补状态说明**。不能把发现/启用/进程运行/工具暴露混成一个绿点；重启原因应明确。[G1] [P2] [O5] [K14] [K24] [K16] |
| J35 IDE/多端 | Aider watch-files，Gemini IDE context，Claude CLI/Desktop/远程衔接，OpenCode editor context | 外部编辑器和 CLI/JSONL；未见专用 IDE companion/活动选区上下文入口 | 同运行时的另一 host，不是可切换的同功能镜像 | **产品取舍+一致性优先**。先对齐任务状态和输入意图，再考虑 IDE 集成；不为跨端照抄共享服务。[A5] [G1] [C2] [O8] [K16] [K22] |
| J36 错误/重试/诊断 | Codex warnings 命令，Gemini F12 详情，Pi debug/bug，Claude doctor/debug | 任务失败和工具失败可回看；warning/footer 通知随下一输入清除；有本地 diagnostics 和明确退出码 | error/notice 单槽；事件未全消费，resync_required 未处理；有最终 snapshot 对齐，严重度需实测 | **已有+关键缺口**。不能把暂时掉流说成持久数据丢失；保留原因、当前结果和下一步，不默认重放完成的命令。[D3] [G2] [P3] [C2] [K7] [K16] [K23] [K34] |
| J37 终端/无障碍/语言 | Pi 分终端解释修饰键/IME；Gemini screen reader 纯文本；OpenCode mouse/动画设置 | 已有中英/主题/宽度、颜色降级和 line CLI；鼠标捕获默认开启；未见通用键位重绑/输入诊断/专用 screen reader 模式 | 有 alert/status、焦点样式、reduced-motion；全局键盘导航、rename 标签/取消及流式播报仍有缺口 | **部分已有，待专项实测**。没测过不能称全面无障碍；终端 Shift 绕过捕获也不能当所有环境通用保证。[P9] [G3] [O8] [K6] [K17] [K18] [K30] |
| J38 完成/需操作提醒 | OpenCode 可配置通知/声音，Gemini 有可选通知，Pi program-status 协议 | 已有终端标题与 Windows Terminal 任务栏进度；未见独立系统通知/完成响铃 | 主要在窗口内显示运行/提示，未见系统通知 | **低成本可选**。不能说“完全没状态反馈”；通知应可关闭、不泄露任务内容，避免每次工具完成都吵。[O8] [G3] [P9] [K30] |
| J39 导出/分享/隐私 | Pi 分享先提醒敏感内容，Claude feedback 有确认，Gemini 可退出并删会话 | /export 本地 Markdown、diagnostics 本地允许列表、无遥测服务；记录/快照/工具输出仍可能含敏感数据 | 未见导出/分享/删除会话流程；密钥输入为 password | **保留本地优先**。不要因竞品有 share 就默认上传；删除与保留范围先说明，诊断脱敏不等于所有模型输出已脱敏。[P3] [C2] [G1] [K11] [K12] [K17] |
| J40 管道/自动化/非交互 | Claude -p、Codex exec、OpenCode run、Pi print/JSON/RPC、Aider --message、Gemini 非交互模式 | --print/--json、stdin、事件/退出码已有；长 stdin 有独立引用策略；不需后台服务器 | 面向交互操作，不作为脚本兼容替代 | **保留强项**。无 TTY 时不能悬空等待问答/审批；事件顺序、失败码、stdout/stderr 比更多 GUI 控件重要。[C4] [D8] [O3] [P4] [A7] [G5] [K21] [K22] |

## 5. 比“别人多几个按钮”更重要的发现

### 5.1 同一句话，三个入口的意思不同

正在执行任务时，输入“另外也把文档补上”并回车：

| 入口 | 当前实际行为 | 用户可能以为 |
| --- | --- | --- |
| TUI | 放进 follow-up，当前任务结束后另跑 | 立刻修改当前任务 |
| 行式 CLI | steering，在后续模型边界交付 | 排队，稍后另做 |
| Desktop | api.steer，在后续模型边界交付 | 和刚用过的 TUI 相同 |

**没有证据证明其中一种默认永远正确。问题是用户不知道自己选了哪一种。** TUI 已有“下次模型请求生效”的 steering 提示，不能说完全没交付说明；但默认输入意图仍跨端不同。[K20] [K28] [K16]

还发现两个更具体的静态分支问题：

- TUI 空闲时 `/queue run` 恢复队列；运行中进入 `/queue ` 前缀分支，把 **`run` 当成后续任务文字**。行式 CLI 运行中则有专门提示。
- 行式 CLI 运行中，未被专门拦截的 `/help`、`/model` 等可能落入默认 steering；TUI 会本地处理或说明不支持。Desktop 本来没有 TUI 的 slash-command 解析器，不应暗示这些字符串可跨端照搬。

以上是**代码路径核对，不是终端端到端复现**。优先目标应是明确“改当前任务 / 下一件事”的可见意图、状态适用范围和拒绝反馈，而不是先统一快捷键或重写队列。[K20] [K28]

### 5.2 Desktop 的主要问题不是外观，是“用户看到的是否真实”

运行时已经发送 context、compaction、usage、model_failed、task_finished、session、resync_required 等事件；前端 applyEvents 主要消费七类文本/推理/工具/模型完成/警告事件。bootstrap 的 running 也未见被主界面采纳。[K16] [K22] [K23]

因此存在这些**待实测的风险**：

1. 任务执行中重新打开界面，按钮和工作状态可能先显示为空闲。
2. 丢事件后，没有对应的重同步处理；最终 snapshot 可能补齐历史，但流式期间显示仍可能不一致。
3. 点击发送前已经清空 prompt/attached，send 报错后未见原草稿恢复。请求可能已被 host 接收并留在快照中，所以不能直接断言“消息永久丢失”；但**用户可继续编辑的草稿所有权不明确**。
4. 实时工具卡片用 `output || result || args`，有输出时可能遮住最终结果，取消也没有独立结果呈现。

先补接已有生命周期事实、失败草稿、重同步，再加更多面板。不是要求 Desktop 复刻 TUI 的键盘布局，也不是另建一套 Agent Loop。[K16] [K18] [K19]

### 5.3 已有动作需要被找到，而不是再写一套实现

Kroma 已有 command_catalog、命令补全、Ctrl+P、F1、工具/全文面板。但“复制一段代码”“搜索历史内容”“把对话回到这里”“恢复这一轮的文件”仍分散在快捷键、命令和不同面板里。

一个明确的文案漂移是 `/undo`：目录写“撤销上次回退 / Undo the last rewind”，实现却是撤回最近完成轮次或清理未完成尾部，**不是 redo**。[K2] [K27]

应优先扩展现有动作目录：同一动作的名称、帮助、状态适用性、鼠标/键盘入口共享定义；不是另起第二份命令表，也不是要求会话切换等危险动作在运行中强行开放。[K2] [K29]

### 5.4 长文本要分清“怎么看”和“送给模型什么”

Kroma 当前超过 4 KiB 的粘贴是按 UTF-8 **字节**附件化；短文本不再按行数自动附件化。文件引用 `@file` 主要转为规范路径和“需要时用 Shell 读取”的提示，并不是直接注入全文。[K35] [K25]

Claude 的路径引用、OpenCode 的注入/行范围、Aider 的编辑文件集、Pi 的长粘贴显示标记不是同一语义。不能只看都叫 `@` 或 `[pasted]` 就照抄。[C1] [O1] [A1] [P1]

建议把三件事分开：

- 编辑器保存用户原文，折叠只是显示方式；展开/取回后仍可编辑。
- 附件/文件引用说明真正的读取方式与限制，不悄悄改变请求含义。
- 模型上下文继续遵守现有预算，不承诺无限原文全部进入一次请求。

原生 Windows 的逐键粘贴、IME 和 Enter/Tab 误触发必须实测。Codex 的源码值得学的是**识别输入来源的先后顺序**，不是盲抄其计时常量。[D1] [D2]

### 5.5 “停止”“撤回对话”“恢复文件”“清空”是四件事

- 停止：取消当前执行，不撤销已经发生的文件/网络副作用。
- 撤回对话：`/undo`、`/rewind` 改历史路径，不自动恢复文件；被离开的未来会话保留。
- 恢复文件：私有 Git 快照，不改用户 HEAD/index，但可能覆盖文件、删除后建的范围内文件。
- `/clear`：清空当前会话，不只是清屏；`/new` 才是另开会话并保留旧会话的入口。

Kroma 这套能力并不少，隔离用户 Git 的快照设计也应保留。差的是**操作前能看懂作用范围**：本次改对话、文件还是两者；哪些路径不覆盖；恢复会删除什么。不要为了表面统一把它们绑成一次隐式破坏操作。[K11] [K12] [K27]

### 5.6 警告、复制、全屏逃生口决定长期使用是否累

不是所有错误都会消失：任务失败、工具失败和部分插件/Hook 错误已有记录。但 UiEvent::Warning 和普通 notice 主要是 footer 单槽，下一次输入即可清掉，缺少统一回看入口。[K7] [K34]

同样，不是“没有复制”：TUI 已有选区、专用复制键、代码块复制、SSH OSC 52 和 `/export`。缺少的是容易发现的“复制最近回答/整条消息”，以及鼠标捕获、原生复制、全文面板各自边界的解释。[K6] [K8] [K9]

全屏并非行业唯一答案。先提供清楚的终端兼容说明、已有导出/行式入口和可控鼠标策略；不要为了学某个产品，再维护一套完全重复的 transcript 引擎。[G3] [P9] [K21]

## 6. 建议先后顺序：这是一份后续任务清单，不是本轮实施承诺

| 顺序 | 要解决的用户问题 | 最小合理范围 | 不做什么 |
| --- | --- | --- | --- |
| P0-1 | 粘贴、IME、Enter/Tab 不误发 | 明确 paste 与键盘动作的优先级；终端矩阵验证；不改短文本语义 | 不按“看起来像多行”自动改成附件；不抄固定延时 |
| P0-2 | 工作中输入到底改当前还是排下一个 | 明确意图与交付边界；修正各 host 的命令路由、/queue run 状态含义 | 不强改成所有入口同一种 Enter 默认；不改并发/超时 |
| P0-3 | Desktop 显示真实任务状态，失败后输入仍可找回 | 接好已有生命周期/重同步；保留未确认草稿；不遮最终工具结果 | 不自动重发已接收请求；不新建 Agent Loop |
| P1-1 | 长粘贴和排队内容可以取回编辑 | 显示和 payload 分离；未消费队列逐项取回/删除；保护已有草稿 | 不取回正在消费的项；不在取消后自动重放 |
| P1-2 | 不记快捷键也能找到现成功能 | 基于现有动作目录统一名称/帮助/可用性；修正 undo 描述 | 不把所有 idle-only 操作都当成 bug |
| P1-3 | 漏看的警告、结果和回答能找回 | 可回看的 warning；复制最近回答/消息；工具结果优先级清楚 | 不持久化所有逐字进度；不把取消当成功 |
| P2-1 | 旧会话好找，分支和恢复不心慌 | 会话预览/过滤/整理、历史选点、恢复前范围预览 | 不把 dirty worktree 全归因给 AI；不破坏用户 Git |
| P2-2 | 知道模型看到了什么 | 展示现有预算/加载来源/插件信任和重启原因 | 不新增模型请求计算“透明度”；不伪造精确费用 |
| P2-3 | 长期使用更顺手 | 终端键位诊断、可选提醒、键盘/a11y 补齐、本地导出 | 不默认上传/后台查更新/建立守护服务 |

**另立产品决策，不能混在 UI 小改里：**强制只读 Plan、结构化问答协议、PTY/托管后台任务、远控/跨端接力、多 worktree 自动管理。若要做，先写权限、无交互模式、取消和恢复契约。**逐工具审批不是本轮建议，自动 Git 提交也不是。**

## 7. 将来实施时怎样验收（本轮没有执行这些产品测试）

### 7.1 按任务验收，不只拍一张漂亮截图

| 场景 | 必须能说明和核对的结果 |
| --- | --- |
| 首次配置/重连 | 新手知道当前目录、模型、权限；连接失败不吞草稿/密钥配置；模型请求只在用户明确发起时发生 |
| 连续粘贴 | 短/长文本、Tab、CRLF、中文、emoji、含 /、!、? 和 Enter 的片段不触发提交/命令/补全；按明确的换行归一化规则核对原文 |
| IME/键盘输入 | 中文候选确认不是发送；长按 Enter 不重复交付；真实 Tab 与粘贴 Tab 分开处理；修饰键不可达时有替代入口 |
| 长粘贴取回 | 折叠/展开/历史取回后内容可编辑；不把显示标记当模型正文；原有附件和草稿不被覆盖 |
| 工作中补充要求 | 每个 host 明确当前意图、何时交付；不支持的 slash 命令不悄悄当任务；/queue run 在空闲/工作中都不生成意外任务 |
| 队列管理 | 只取回未消费项，移除与恢复原子化；已有草稿先保留；失败/取消后不自动重放；预览、数量与实际发送对应 |
| 停止/退出 | 选区复制与取消路径可区分；停止有可用入口；不宣称回滚副作用；未完成回答有明确边界 |
| 工具/验证 | 展开后能分辨命令、运行/成功/失败/取消、输出和最终结果；验证必须可追到实际命令，不能把所有 exit=0 当全项目都测过 |
| 旧会话/分支 | 能在大量会话中辨认、预览、改名；切换后输入归属明确；对话撤回不被误认为文件恢复；旧未来仍可找回 |
| 文件恢复 | 先显示基线、将覆盖/删除/不覆盖的范围；含非 Git、忽略文件、已有 dirty 文件、新建文件场景；不改用户 HEAD/index |
| Desktop 中途重载 | 对已运行任务 bootstrap 正确；丢事件/重同步可恢复已有状态；最终结果不重复；旧任务事件不污染新项目/会话 |
| 发送失败 | 分别制造 host 接收前/接收后的失败；原文本和附件有找回途径；已接收请求不自动重复执行 |
| 警告/错误 | 离开 footer 后仍能找回重要原因；区分任务失败、工具失败和普通提示；诊断不泄露密钥/不自动上传 |
| 键盘/a11y | 不用鼠标能选择模型/会话/附件和关闭弹层；焦点可见且可恢复；读屏不逐 token 轰炸；字号/缩放/减少动画有效 |
| 自动化 | 无 TTY 时行为明确；JSONL stdout 可解析、错误/退出码可核对；超大 stdin、取消和残缺流不伪装成功；不悬空等人工回答 |
| 通知/扩展 | 通知可关、内容不泄密；信任变化和重启范围说明清楚；子代理完成状态能追到其结果/证据，进度故障不篡改结果 |

### 7.2 必须覆盖的环境矩阵

不是要求对所有系统笛卡尔积穷举；先按实际支持与风险分层，至少记录下列组合的输入、复制、滚动和退出结果：

| 维度 | 建议样本 | 本轮状态 |
| --- | --- | --- |
| Windows 原生 | Windows Terminal + PowerShell；Windows Terminal + Git Bash；不同剪贴板/逐键粘贴路径 | 未实测 |
| WSL/远程 | WSL；SSH；SSH + tmux；OSC 52 被允许/被过滤 | 未实测 |
| macOS/Linux | macOS 默认终端与一款现代终端；Linux 一款主流终端 | 未实测 |
| 输入法/键盘 | 中英文 IME、emoji、Alt/Meta、无法发送 Shift+Enter、长按重复键 | 未实测 |
| 小窗口/显示 | 窄宽度、频繁 resize、高 DPI、无真彩色、reduced motion | 未实测 |
| Desktop | 本平台 WebView 的键盘/拖放/重载/离线；其他支持平台分别验收 | 未实测 |
| 无障碍 | 键盘全流程；读屏；鼠标捕获/原生复制绕过；行式替代入口 | 未实测 |

**测试失败时先确认输入事件和当前状态，不先加延时、提高超时或改并发来掩盖。**

## 8. 本轮产物、验证与限制

- 应用代码、测试代码、Agent Loop、配置、超时和并发：未修改。未安装竞品、登录、更新、提交、发布或进行真实 provider 请求。
- 新增本报告；官方源码证据放在 target 下的忽略研究目录。没有覆盖旧的 `HARNESS_INTERACTION_RESEARCH_2026-11-05.md` 或现有用户审阅材料。
- 文档验证范围：UTF-8/NUL/行末空白/最终换行、代码 fence、引用唯一性与完整性、J01–J40 连续性、来源 URL 格式与 Codex 固定 SHA、本地引用路径/行号存在。
- 不是应用改动，未运行 cargo/npm 应用测试；第 7 节全部是**未来验收建议**，不是本轮通过的测试。
- 链接格式与本地路径检查不等于对所有网页重新做可用性扫描，更不等于竞品真实安装验收。官方 latest 文档会变化，结论以研究日期和所引用版本/源码为限。

## 9. 来源与本地核对入口

以下官方资料在本轮或上一轮连续研究中实际读取。Codex 采用官方仓库固定提交，其他文档是研究时的公开页面；未把搜索摘要当正文证据。Kroma 行号用于定位相关实现，单一链接不代表只读了这一行。

### 9.1 Claude Code

[C1]: https://code.claude.com/docs/en/interactive-mode "交互、输入、工作中消息和快捷键"
[C2]: https://code.claude.com/docs/en/commands "命令与动作入口"
[C3]: https://code.claude.com/docs/en/sessions "会话管理、查找与分支"
[C4]: https://code.claude.com/docs/en/common-workflows "文件引用、模式与集成流程"
[C5]: https://code.claude.com/docs/en/permissions "权限和拒绝范围"
[C6]: https://code.claude.com/docs/en/quickstart "首次登录和启动"
[C7]: https://code.claude.com/docs/en/fullscreen "全屏、滚动、查看与复制"
[C8]: https://code.claude.com/docs/en/terminal-config "终端键位、粘贴与换行"
[C9]: https://code.claude.com/docs/en/checkpointing "检查点、文件与对话恢复范围"

### 9.2 Codex CLI：固定提交源码

[D1]: https://raw.githubusercontent.com/openai/codex/1e9e8bdb008549fc558ef886791c185c6d3c0814/codex-rs/tui/src/bottom_pane/chat_composer.rs "输入、历史、payload 与动作顺序"
[D2]: https://raw.githubusercontent.com/openai/codex/1e9e8bdb008549fc558ef886791c185c6d3c0814/codex-rs/tui/src/bottom_pane/paste_burst.rs "逐键粘贴识别状态机"
[D3]: https://raw.githubusercontent.com/openai/codex/1e9e8bdb008549fc558ef886791c185c6d3c0814/codex-rs/tui/src/slash_command.rs "命令定义和运行态可用性"
[D4]: https://raw.githubusercontent.com/openai/codex/1e9e8bdb008549fc558ef886791c185c6d3c0814/codex-rs/tui/src/onboarding/auth.rs "登录选择与错误/完成状态"
[D5]: https://raw.githubusercontent.com/openai/codex/1e9e8bdb008549fc558ef886791c185c6d3c0814/codex-rs/tui/src/bottom_pane/approval_overlay.rs "审批范围、拒绝与取消"
[D6]: https://raw.githubusercontent.com/openai/codex/1e9e8bdb008549fc558ef886791c185c6d3c0814/codex-rs/tui/src/resume_picker.rs "分页、搜索与预览"
[D7]: https://raw.githubusercontent.com/openai/codex/1e9e8bdb008549fc558ef886791c185c6d3c0814/codex-rs/tui/src/bottom_pane/request_user_input/mod.rs "结构化问答流程"
[D8]: https://raw.githubusercontent.com/openai/codex/1e9e8bdb008549fc558ef886791c185c6d3c0814/codex-rs/cli/src/main.rs "Exec 非交互入口；未验证完整脚本契约"

### 9.3 OpenCode v2

[O1]: https://opencode.ai/v2/docs/cli/tui "输入、上下文引用、队列与旁支问题"
[O2]: https://opencode.ai/v2/docs/cli/keybinds "动作菜单、队列、diff、会话与终端动作"
[O3]: https://opencode.ai/v2/docs/cli "CLI、mini 与非交互入口"
[O4]: https://opencode.ai/v2/docs/permissions "v2 权限范围和默认行为"
[O5]: https://opencode.ai/v2/docs/agents "Build/Plan 与子代理"
[O6]: https://opencode.ai/v2/docs/ "快速开始与连接"
[O7]: https://opencode.ai/v2/docs/cli/providers "认证方法与账户管理"
[O8]: https://opencode.ai/v2/docs/cli/config "界面、diff、通知和终端设置"

### 9.4 Gemini CLI

[G1]: https://geminicli.com/docs/reference/commands/ "命令、上下文、会话、复制和退出"
[G2]: https://geminicli.com/docs/reference/keyboard-shortcuts/ "命名动作、输入、shell 焦点与错误详情"
[G3]: https://geminicli.com/docs/cli/settings/ "设置作用域、实验开关、可访问性和通知"
[G4]: https://geminicli.com/docs/cli/plan-mode/ "计划到实施流程"
[G5]: https://geminicli.com/docs/cli/session-management/ "会话预览、管理与非交互边界"
[G6]: https://geminicli.com/docs/tools/ask-user/ "选项、文本与确认问答"
[G7]: https://geminicli.com/docs/get-started/authentication/ "登录、API key 与 Vertex"
[G8]: https://geminicli.com/docs/tools/shell/ "PTY、后台命令及限制"
[G9]: https://geminicli.com/docs/cli/rewind/ "历史与文件恢复范围"

### 9.5 Pi

[P1]: https://pi.dev/docs/latest/usage "编辑、引导、后续任务、工具、复制与显示"
[P2]: https://pi.dev/docs/latest/slash-commands "动作入口与资源重载"
[P3]: https://pi.dev/docs/latest/sessions "搜索、会话树、分支、复制、导出与分享"
[P4]: https://pi.dev/docs/latest/security "自主执行、资源信任与非交互边界"
[P5]: https://pi.dev/docs/latest/models "认证、模型选择、默认与会话配置"
[P6]: https://pi.dev/docs/latest/extensions "扩展交互能力；不等于默认内置"
[P7]: https://pi.dev/docs/latest/quickstart "安装、认证与首次任务"
[P8]: https://pi.dev/docs/latest/keybindings "当前键位与平台差异"
[P9]: https://pi.dev/docs/latest/terminal-setup "终端协议、修饰键与兼容性"

### 9.6 Aider

[A1]: https://aider.chat/docs/usage/commands.html "文件集、编辑、运行、测试与撤回入口"
[A2]: https://aider.chat/docs/usage/modes.html "Ask/Code/Architect 与单次模式"
[A3]: https://aider.chat/docs/git.html "Git 自动提交、dirty 文件与 undo"
[A4]: https://aider.chat/docs/repomap.html "仓库 map 和上下文预算"
[A5]: https://aider.chat/docs/usage/watch.html "IDE 文件监听与 AI 注释"
[A6]: https://aider.chat/docs/usage/lint-test.html "自动 lint、可配置测试与失败反馈"
[A7]: https://aider.chat/docs/scripting.html "单次任务和脚本使用"

### 9.7 Kroma：当前工作树中的实现

[K1]: ../src/setup.rs#L133 "首次配置；启动 gate 另见 src/main.rs:625"
[K2]: ../src/tui/command_catalog.rs#L23 "已有动作目录及菜单文案"
[K3]: ../src/tui/editor.rs#L354 "编辑、选区与历史/附件状态"
[K4]: ../src/tui/draft.rs#L197 "清理草稿与撤销恢复"
[K5]: ../src/tui/mod.rs#L325 "模型、会话和分类选择器"
[K6]: ../src/tui/mod.rs#L633 "输入、快捷键、复制、粘贴与显示动作"
[K7]: ../src/tui/tool_records.rs#L350 "工具卡片、状态与输出"
[K8]: ../src/tui/view.rs#L1511 "记录、推理、阅读锚点与呈现"
[K9]: ../src/tui/transcript_panel.rs#L16 "全文面板与搜索"
[K10]: ../src/tui/navigation.rs#L6 "轮次/滚动导航"
[K11]: ../src/commands.rs#L216 "会话、历史、撤回和文件恢复命令"
[K12]: ../docs/security.md "权限、资源信任、私有快照与恢复边界；取消语义另见 docs/lifecycle.md"
[K13]: ../src/tui/status.rs#L120 "预算、用量与状态展示；完整计算见 src/usage.rs"
[K14]: ../src/skills.rs#L103 "Skills 元数据与加载；AGENTS/MCP 另见 src/prompt.rs 与 src/app.rs"
[K15]: ../examples/plugins/subagents/README.md#L118 "子代理进度、最终结果与证据；实现见 subagent.js:651–747，设计边界见 docs/subagents.md"
[K16]: ../desktop/src/App.svelte#L54 "bootstrap、事件消费、输入、任务与会话 UI"
[K17]: ../desktop/src/components/SettingsView.svelte#L66 "profile、插件与设置交互"
[K18]: ../desktop/src/components/MessageView.svelte#L23 "回答/工具渲染、输出与可访问性"
[K19]: ../desktop/src-tauri/src/lib.rs#L410 "桌面 host API 与明确未支持的 follow-up"
[K20]: ../src/main.rs#L470 "行式 CLI 运行中输入、命令路由与 steering"
[K21]: ../src/main.rs#L90 "print/JSON/stdin 入口及大小策略"
[K22]: ../src/cli_events.rs#L24 "运行时 JSONL 事件投影"
[K23]: ../src/desktop_host.rs#L119 "桌面 host、bootstrap、事件与任务状态"
[K24]: ../src/plugins.rs#L144 "插件能力、命令、只读面板与重载边界"
[K25]: ../src/convenience.rs#L18 "文件引用、路径补全和 diff 便利命令"
[K26]: ../src/prompt.rs#L52 "全局/项目/嵌套指令刷新与来源"
[K27]: ../src/agent.rs#L839 "实际 undo 行为，不是 undo rewind 的 redo"
[K28]: ../src/tui/mod.rs#L1151 "TUI 运行中命令、后续任务与 steering 分流"
[K29]: ../src/tui/help.rs#L6 "快捷键/命令帮助与发现入口"
[K30]: ../src/tui/terminal_feedback.rs#L81 "终端标题与 Windows Terminal 任务栏反馈"
[K32]: ../docs/reference/cli.md#L119 "CLI、显式更新和诊断入口"
[K33]: ../src/shell.rs#L900 "shell stdin=null 和进程执行边界"
[K34]: ../src/tui/status.rs#L292 "footer 提示绘制；输入清理见 tui/mod.rs:640–650"
[K35]: ../src/tui/attachments.rs#L280 "长粘贴阈值按字节判断"
[K36]: ../src/attachments.rs#L159 "附件投影；文件/图片等大小常量在文件开头"
[K37]: ../src/tui/mod.rs#L1427 "用户直接 shell 的执行/显示；行式入口见 main.rs:270–299"
[K38]: ../examples/plugins/subagents/subagent.js#L651 "实际 roster/liveRow/beat 与最终结果，不只设计设想"
