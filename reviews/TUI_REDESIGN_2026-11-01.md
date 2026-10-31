# Kroma TUI 改版总览（2026-11-01）

工作目录：`D:\Kroma\kroma-latest`。三个批次与附加任务 A 均已在工作树完成，**未 commit / push**。

| 交付 | 文档 |
|---|---|
| 批次 1：主题、装饰管线、对话区、工具卡、高亮、取消通知 | `reviews/TUI_REDESIGN_BATCH1_2026-11-01.md` |
| 批次 2：页眉、输入框、页脚、活动/完成行、首页、浮层、终端反馈 | `reviews/TUI_REDESIGN_BATCH2_2026-11-01.md` |
| 批次 3：轮次跳转、代码复制、文档、全文浮层 | `reviews/TUI_REDESIGN_BATCH3_2026-11-01.md` |
| 对话回退调研（无实现代码） | `reviews/TUI_REWIND_FEASIBILITY_2026-11-01.md` |
| Windows `nul` 文件 | `reviews/TUI_REDESIGN_WINDOWS_NUL_2026-11-01.md` |
| 改版前基线快照 | `reviews/tui-redesign/before/`（16 份） |
| 改版后金样 | `reviews/tui-redesign/final/`（3 预设 × 4 尺寸 × 20 场景 = 240 份） |

## 验证摘要

- `cargo fmt --all -- --check`：通过
- `cargo clippy --locked --all-targets -- -D warnings`：通过
- `cargo test --locked`：388 passed / 2 ignored（lib）+ 所有集成套件通过
- `node --test examples/plugins/web/*.test.js`：81 passed
- 80×24 流式 bench（手动 `--ignored`）：300 条 p95 10.5ms、3000 条 p95 10.7ms；改版前 10.66ms / 10.60ms → 无退化
- `Cargo.toml` / `Cargo.lock`：无改动（未新增依赖）

## 与 origin/main 合并（a2c7e05 / eeb2345 / f07dd4a）

推送前远端已有 3 个更新提交，已 rebase 到其上并解决 3 个文件冲突。合并取舍：

- 保留上游新增的双语 onboarding 提示与采样 logo 阈值（首页仍是 logo + 一行提示）。
- 不保留上游的独立 cwd 行：页眉固定"1 行内容 + 1 行间隔"，项目名在页眉右侧；完整路径（去 verbatim 前缀、~ 缩短）在首页与 `/help`。
- 终端标题合并为 `{●/✓/✕/⊘}Kroma — {会话}`：上游的会话标题与"变更时才写 OSC 0"的去抖保留在本设计的 `Feedback` 内。
- 未测量的上下文仍显示 `Context —`（回退上游的 `Ctx 0 / window`）：恢复历史会话后 0 会被误读为"已用 0 tokens"；相应上游断言改为 `—`。
- 金样读取把 CRLF 归一化为 LF，避免 Windows（`core.autocrlf=true`）checkout 后金样对比失败。

## 既有测试夹具失配（已修复，仅测试）

HEAD 上原本有 2 个失败，均与 TUI 改动无关，已用 `git archive HEAD` 干净副本逐个复现；上游在合并前也自行修复了它们：

- `agent::tests::batching_guidance_is_stable_for_new_and_resumed_sessions`：V7 指导语追加后，断言仍要求以 `EXECUTION_GUIDANCE` 结尾。上游已改为"以 `V7_GUIDANCE` 结尾 + 顺序"断言，合并保留上游更严格版本。
- `tests/context_compaction_e2e.rs::checkpoint_failure_below_hard_limit_does_not_abort_the_task`：V7 指导语让合成负载从"刚低于硬上限"变成超出（27808 > 27648）。上游把工具证据 48k→47k，本地把合成上下文 4000→3800；合并后仍满足"越过 90% 触发点、低于硬上限"的意图。

合并后全量 `cargo test --locked`：388 passed / 2 ignored；无 production prompt 行为改动。

## 明确不做（保留在文档中）

审批 UI、侧栏、默认双栏 IDE、全屏卡片墙、复杂动画系统、新 UI runtime、自动文件回滚。

## 部署记录（2026-11-01）

- 远端提交：\`35524fb\`（\`a2c7e05..35524fb main -> main\`）
- 构建：\`cargo build --release --locked\` → \`target/release/kroma.exe\`，\`--build-info\` 为 \`kroma 0.4.0 (35524fb9416a)\`
- 安装：\`C:\Users\24524\.cargo\bin\kroma.exe\`，SHA256 \`61B877FC26A70CF97284DDC8B64ABDCD6B3E6AB6FEB79FC5ED255D0EE01B3A1B\`
- 备份：\`C:\Users\24524\.kroma\backups\tui-20261101-035420\kroma.exe\`（含旧哈希与部署信息）
- 部署时 PID 26316 仍在运行旧镜像，未终止任何进程；旧镜像被重命名为 \`kroma-previous-20261101-035420.exe\`，新会话使用新二进制。
- 冒烟：\`--build-info\`、\`--help\` 均通过；\`where.exe kroma\` 指向 \`C:\Users\24524\.cargo\bin\kroma.exe\`
- 回滚：关闭 Kroma 后用备份覆盖安装路径即可；静态官网（\`D:\Kroma\site\`）未部署（无托管目标，本地预览 \`node site/serve.mjs\`）。
