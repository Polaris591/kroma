# Goal 插件（自带状态）

目标管理是**普通插件**：Kroma 核心不认识「目标」，只提供三个通用口子。

- `/goal ...` 命令返回一个 `task`，宿主把它当成你的下一句话立刻开跑。
- `before_turn_end` 钩子：AI 想收工时，插件可以回一句「还没完」和一段提示，宿主继续这一轮。
- `ui.status` 宿主调用：插件把自己那行状态文字改掉（底部状态栏/桌面快照都会显示）。

## 状态存在哪里

每个对话一个文件：`plugin-data/goal/goal-<session_id>.json`。
换对话、`/fork` 打开的是另一个会话文件；`/rewind` 和 `/clear` 会触发 `session_rewound` / `session_cleared` 事件，自动把旧活动目标取消并清掉状态栏，避免历史回退后目标串台。插件也不读别的对话。

## 用法

```
/goal <目标正文>      开始目标并直接开干
/goal 或 /goal status  看当前目标
/goal cancel          放弃目标（不删除已完成的工作）
Ctrl+C                停止当前这一轮（目标本身还在，说句话就能接着干）
```

模型用 `goal_checkpoint` 记录进展：`claim=working` 继续、`done` 结束、`blocked` 先停住不再催（目标保留）。

## 与旧版区别（重要）

旧版把目标状态机放在核心里（`src/task_control.rs`），核心会替插件核验命令回执、写入存档。
现在这一切都不在核心里：**插件只看得见宿主递给它的事实**（`before_turn_end` 里的本轮工具调用、
命令回执不再单独提供）。因此：

- 插件不会阻止你结束对话，也**不授予任何权限**；
- AI 说「做完了」时，插件只根据自己记录的 `goal_checkpoint` 判断，不再由宿主校验；
- 你随时可以 `/goal cancel` 或按 Esc 停手。

## 测试

```
cd examples/plugins/goal && node --test goal.test.js
```
