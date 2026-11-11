# UI Lab 示例插件

演示 Kroma 插件系统面向终端（TUI）与桌面端（Desktop）的完整 UI 扩展能力：

- **跨端通用积木**：
  - `ui.status` / `ui.notify` / `ui.composer`
  - `ui.widget`（常驻侧边栏挂件 + 可点击操作按钮 `ui_action`）
  - `ui.panel`（插件主动打开/更新面板）
- **终端自由画布**：`/uilab custom` 通过 `ui.custom` 渲染自定义交互列表并处理方向键。
- **桌面自定义网页**：`[ui.desktop]` 在安全隔离的沙盒插槽内渲染 `ui/index.html`，通过 `window.kroma` 与插件进程双向通信。

## 验证与启用

```bash
kroma plugins check examples/plugins/ui-lab
kroma plugins install examples/plugins/ui-lab
kroma plugins enable ui-lab
```
