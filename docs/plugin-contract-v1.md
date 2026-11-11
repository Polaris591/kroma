# 插件分发与契约 v1（修订稿）

目标：**开源之后，任何用户都能给自己写插件，也能把插件交给别人用；本体更新不弄坏老插件。**

## 关键判断：市场不用造

市场不是技术，是托管。只要满足一条——**插件就是一个文件夹**（现在就是这样）——分发就等于"把这个文件夹给别人"。

- 市场 = 一份清单，谁都能托管：官方一份、用户 fork 一份、公司内网一份。
- 不造服务器、账号、签名、评分、自动更新。
- 安装只需两种来源：**本地文件夹**（已有）、**一个网址或 git 地址**（要加）。

这不叫市场，叫"能装"，但它是市场的全部前提。

## 现状（核对过源码）

| 能力 | 现状 |
| --- | --- |
| 加工具、命令、快捷键、面板、设置、动画、配色 | 已有 |
| 开子进程干活 | 已有 |
| 看得见这一轮发生了什么（`events` 订阅：工具开始/结束、上下文、模型请求、会话切换） | 已有 |
| 问用户（工具声明 `host = "ask_user"`） | 已有 |
| 改流程（`before_tool_call` / `after_tool_result` / `before_model_request`） | 已有 |
| 持久化自己的数据（`plugin-data/`，请求里带 `data_dir` 和 `session_id`） | 已有；想按会话隔离就自己按 `session_id` 命名文件 |
| 干活途中问用户（`ui.ask` 宿主调用） | **已有**（终端；无界面场合直接回 ok:false） |
| 一键生成插件骨架（`kroma plugins new`） | **已有** |
| 从本地文件夹安装 | 已有 |
| 从网址/git 安装 | **已有**（git 仓库浅克隆后走同一安装流程） |
| 命令直接派活给 AI（`task`） | **已有** |
| AI 收工前把关（`before_turn_end`） | **已有** |
| 安装/自检时用人话告诉用户这插件要什么 | **已有**（`install`、`check` 都先打印一段人话） |
| 给作者看的说明书 | **已有**（本文件 + `docs/plugin-boundaries.md` + 各示例 README） |
| 写完自检 | **已有**（`kroma plugins check <目录>`；会启动一次带超时的 `ping`，不是纯静态解析） |
| 只加不改的书面承诺 | **已有**（见文末） |

结论：分发、说明、自检、承诺四样都已落地。下面四小节保留为当初的设计记录（判断依据和取舍），实现细节以各"新增"小节和源码为准。

## 四件事，按此顺序

### ① 从网址或 git 安装（2 天）

`kroma plugins install <目录|网址|git 地址>`。

- 只做下载/克隆到临时目录 → 校验清单 → 走现有的本地安装流程。
- 复用现有预算（条目数、总大小上限），不新造一套。
- 失败必须说清原因（网址不通、清单缺字段、超限），不猜。

**这条不做，"别人用我插件"就无从谈起。**

### ② 安装时用人话展示它能干什么（1 天）

装之前打印三五行：这个插件能跑命令、能读写文件、能问你问题、会在每次工具调用后插手。用户确认后才启用。

- 不新增机制，只是把已有声明翻译成人话。
- **诚实说明**：插件是独立程序，仍有你机器的权限；这不是沙箱。

### ③ 说明书（2 天）

`docs/plugin-boundaries.md` 扩成作者手册，不新建第二份会过期的文件：目录结构、清单每个字段、四个能跑的示例（最小工具、带设置、订阅事件、问用户）、常见错误。

示例同时作为测试夹具，防文档过期。

### ④ 静态自检 `kroma plugin check <目录>`（2 天）

> 实现与这里的一处偏差：自检在校验清单后**会启动一次带超时的 `ping`**，而不是完全不启动插件。
> 理由是没有真进程就查不出「清单看着对、程序其实不通」；它不写状态、不连模型、用的是临时 `data_dir`。

只读清单和文件，**不启动插件**：字段、版本、能力名、hook 名、工具参数格式、设置类型、示例脚本是否存在。

- 一项一行：通过 / 失败 + 错在哪、怎么改。
- `--json` 给 AI 读。
- 启动陌生代码不是检查工具该干的事。

## 收工把关：`before_turn_end`（新增，通用）

任何启用的插件都可以在清单里声明 `hooks = ["before_turn_end"]`，在模型答完、还没结束这一轮时被问一次：

```json
{"type":"hook","name":"before_turn_end",
 "arguments":{"final_text":"模型这次的最终答复",
              "tools":[{"name":"shell","arguments":"{\"command\":\"cargo test\"}"}]},
 "session_id":"…","archive":"…","workdir":"…"}
```

插件只回两种决定之一（其余内容一律当作"放行"）：

```json
{"decision":"finish"}
{"decision":"continue","feedback":"还没跑测试；先跑 cargo test 再收尾"}
```

- 多个插件按启用顺序依次询问，**第一个 `continue` 生效**，宿主把 `feedback` 作为一条复核意见继续这一轮。
- 插件超时、报错、返回无法解析的内容都**只放行，不会卡住任务**；用户按 Esc 仍可随时停下。
- 宿主会发 `turn_gate_continued` 事件（含插件名），界面据此说明是谁要求继续。
- 可运行的示例：`examples/plugins/turn-gate`（要求最终答复带 `[reviewed]` 标记）。

## 命令直接派活：`task`（新增，通用）

插件的 `[[commands]]` 处理完可以额外回一个 `task` 字段，宿主会把这段文字当作真实用户任务跑一轮：

```json
{"request_id":7,"protocol_version":2,
 "content":"已按模板生成请求",
 "task":"审查 src/agent.rs 的错误处理并给出修复建议"}
```

- `content` 照旧显示在屏幕上；`task` 是可选的，空字符串会被拒绝并报错。
- 宿主只把它当用户输入跑一次，**不会**再当命令解析，避免命令套命令。
- 老插件不写这个字段，行为完全不变。
- 端到端用例：`tests/plugins_e2e.rs` 的 `plugin_command_can_start_one_real_task`。

## 从网址安装（新增）

```
kroma plugins install <本地目录 | git 地址>
/plugins install <本地目录 | git 地址>
```

- 网址必须是 git 仓库地址（`https://`、`http://`、`ssh://`、`git://`、`git@`、`file://`）。
  用你机器上的 `git` 做一次浅克隆，然后**走和本地安装完全相同的流程**（清单校验、条目/体积上限）。
- 仓库根目录或**恰好一个**子目录里放了 `plugin.toml` 才安装；有多个则明确报出候选目录让你指定，不猜。
- 装完默认是**禁用**状态，需要你自己启用；安装前不会执行插件代码。
- 没有 git 或克隆失败时，错误信息会说明原因，不会静默失败。

## 实时状态栏：`ui.status`（新增，通用）

插件在自己跑一次工具/命令的过程中，可以先给宿主发一条**宿主调用**，把自己的状态行改掉；宿主办完回一句结果，插件再给最终答复：

```json
{"type":"host_call","request_id":7,"name":"ui.status",
 "arguments":{"text":"索引中 3/10"}}
```

宿主回：

```json
{"type":"host_call_result","request_id":7,"result":{"ok":true}}
```

- 文本超过 160 字符会被截断；传空字符串等于清除这一行，回到清单里 `[ui] status` 的静态文字。
- 终端界面和桌面端都会通过运行时 UI 总线实时刷新这条状态，不按全局共享，切换会话也不会串台。
- 一次请求最多 64 次宿主调用，超过就报错，避免插件把一轮对话卡住。
- 除了 `ui.status`，现已支持 `ui.ask`、`ui.notify`、`ui.composer`、`ui.widget`、`ui.panel`、`ui.desktop_emit` 与 `ui.custom` 等宿主调用（详见下文）。
- 端到端用例：`tests/plugins_e2e.rs` 的 `plugin_can_push_a_live_status_line_during_a_call`。

## 插件自己存状态（无需新能力）

插件要跨轮记住东西（比如目标），直接写宿主传给它的 `data_dir`：请求里带 `data_dir` 和 `session_id`，
按会话各存一个文件即可。宿主不解析、不迁移、不校验这些文件；换会话/`/fork`/`/rewind` 天然隔离。
参考实现：`examples/plugins/goal`（只用 `task` + `before_turn_end` + `ui.status`，核心没有任何目标代码），
端到端用例 `tests/goal_plugin_e2e.rs`。

## 中途问人：`ui.ask`（新增，通用）

插件跑命令、跑工具、守收工时想知道用户的选择，就发一次宿主调用：

```json
{"type":"host_call","request_id":8,"name":"ui.ask",
 "arguments":{"question":"用哪个数据库？","options":["Postgres","SQLite"]}}
```

宿主回（用户选了）：

```json
{"type":"host_call_result","request_id":8,
 "result":{"ok":true,"picked":["Postgres"],"typed":null}}
```

- 用户点了别的或关了窗口：`{"ok":false,"error":"…closed the question without answering"}`。**关掉不等于同意**，别拿它当批准。
- 终端里这个问题就长在**输入栏**上（问题 + 选项 + 直接打字那一行），不再弹到屏幕中间；桌面上是输入框上方的一条，键盘 ↑↓/Enter/Esc 同样可用。
- 没人看着的场合（`--print`、管道）会直接回 `ok:false`，插件应据此自己决定并说明，不要卡住。
- 选项至少两个；想问开放问题就用 `host="ask_user"` 的工具，让模型在对话里问。
- 端到端用例：`tests/plugins_e2e.rs` 的 `plugin_can_ask_the_person_during_a_call`；桌面端问答通道见 `desktop/src-tauri/src/lib.rs` 的 `DesktopAsker`。

## 一键起手：`kroma plugins new <id>`（新增）

在当前目录写出 `plugin.toml` + `main.js` + `README.md`，**不改就能过自检**（`kroma plugins check <id>`）。
`main.js` 里三个分支（命令、工具、ping）就是全部协议，照着改即可。

## 插件能改哪些界面（声明式 + 实时交互 + 自由扩展区）

### 第一层：跨端通用能力（写一份插件，终端与桌面通用）

| 想改什么 | 写在哪 | 效果 |
| --- | --- | --- |
| 一行状态 | `[ui] status` 或 `ui.status`（支持 `key`、`scope = "session"\|"global"`） | 终端与桌面底部实时刷新（按会话/实例隔离） |
| 实时通知 | `ui.notify`（`message`, `level`） | 终端通知行 / 桌面 Toast 实时提示 |
| 修改输入栏 | `ui.composer`（`text`, `mode = "replace"\|"append"`） | 直接预填或追加用户输入栏草稿 |
| 常驻挂件与按钮 | `ui.widget`（`id`, `placement`, `title`, `blocks`, `actions`, `clear`） | 在 `above_composer` / `below_composer` / `sidebar` / `header` / `footer` 实时渲染并接收 `ui_action` 点击事件；`placement = "transcript"` 把内容块插进消息流（随对话滚动，仅内容、不带按钮） |
| 主动弹出/刷新面板 | `ui.panel`（`action = "open"\|"update"\|"close"`, `title`, `blocks`, `actions`） | 无需等待用户敲 `/plugins view`，插件主动弹窗展示结构化内容 |
| 面板文字与按需面板 | `[ui] panel_title` / `panel_text` / `dynamic_panel = true` | `/plugins view <id>` 与插件页卡片查看 |
| 快捷键 / 名称 / 动画 / 颜色 / 布局 | `[[ui.shortcuts]]`、`[ui.tool_labels]`、`[ui] activity_frames`、`[ui.colors]`、`[ui.layout]` | 快捷键触发命令、改工具显示名、覆盖主题色等 |

### 第二层：两端各自的自由接管区

| 端 | 接管方式 | 说明 |
| --- | --- | --- |
| 终端（TUI） | `ui.custom` 宿主调用 | 插件申请弹层或全屏画布（`ui_render` / `ui_input` / `ui_close`），宿主按 `(width, height)` 接收样式行与光标坐标，并将按键/缩放实时转发给插件；`Ctrl+C` 随时安全退出；`--print`/桌面返回 `ok:false` |
| 桌面端（Desktop） | `[ui.desktop]`（`entry`, `slot = "sidebar"\|"tab"\|"above_composer"\|"panel"`, `title`, `height`） | 在沙盒化的独立网页容器内加载插件的本地 HTML/CSS/JS（防路径越界），注入 `window.kroma` 桥接 SDK（`request` 发 `ui_message`、`onEvent` 收 `ui.desktop_emit` 与会话上下文、`runTask`、`setComposer`、`notify`） |

- 完整可运行参考实现：`examples/plugins/ui-lab`（`kroma plugins check examples/plugins/ui-lab`）；端到端用例见 `tests/plugins_e2e.rs`（`ui_lab_widgets_actions_notify_composer_and_desktop_bundle_roundtrip` 与 `plugin_custom_tui_screen_renders_and_handles_input_roundtrip`）以及 `src/tui/view_tests.rs`。

### 面板里的结构化内容

动态面板照旧回 `content`，但内容可以是一段 JSON（数组，或 `{"blocks":[...]}`）。回普通文本的老插件行为不变。

```json
{"blocks":[
  {"type":"heading","text":"状态"},
  {"type":"text","text":"任意说明文字"},
  {"type":"list","items":["第一项","第二项"]},
  {"type":"keyvalue","rows":[{"key":"请求数","value":"3"}]},
  {"type":"progress","label":"进度","value":3,"max":10},
  {"type":"table","headers":["名称","结果"],"rows":[["单测","通过"]]},
  {"type":"divider"}
]}
```

- 宿主负责排版、换行和宽度；插件**不写 ANSI、不占输入、不碰任何控件**。桌面端拿到的是同一份块的 JSON（`plugin_panel_view`），画成真正的表格和进度条。
- 不认识的字块直接忽略（老版本宿主不会因此报错），字段有上限（200 块、表格 200 行、12 列、单段 2000 字），防止一次渲染被卡住。
- 示例：`examples/plugins/hello`（`/plugins view hello`）与 `examples/plugins/ui-lab`；端到端用例 `tests/plugins_e2e.rs` 的 `a_dynamic_panel_can_return_structured_blocks` 与 `ui_lab_widgets_actions_notify_composer_and_desktop_bundle_roundtrip`。

## 只加不改（承诺，写进文档）

- 已发布的字段名、消息名、语义永不改；要换就加新的。
- "老插件能用"指**行为不坏**，不只是"不崩"。
- 只冻结已经在用的东西（工具、命令、事件、设置、问用户、三个 hook）。清单里没人用的模型、上下文后端不进契约。

## 不做（这一轮）

- 会话级存储：收益没证明，且会和清理、分叉、回退三处打架。
- 插件主动发起一轮对话：没有真实插件需要。
- 沙箱、签名、市场服务器、自动更新、热重载。
- 那条"核心不许出现插件名"的自动检查：只能挡住最蠢的违规，给假安全感；改成文档约定。

## 验收

一个没用过 Kroma 的人配 AI，只读说明书，写一个插件，从网址安装，自检全绿，装上能用——核心零改动。

## 风险

| 风险 | 应对 |
| --- | --- |
| 陌生插件本身有恶意或缺陷 | 安装前展示它能干什么；文档写明"插件 = 你信任的程序" |
| 文档与代码脱节 | 示例即测试夹具，CI 跑 |
| 契约冻结过早 | 只冻在用的；新需求叠加，不删旧路 |
