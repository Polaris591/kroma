# Kroma 第五阶段：工具消融、长会话与复杂 Rust 竞赛

**2026-11-10，Windows 本机。** 所有任务使用 `deepseek-v4.1-flash` / High、同一 Cobblemon API 端点。所有实验在隔离目录完成；Kroma 插件目录隔离，Pi 使用 1.5.0。实验二进制、任务数据、运行时长及具体日志以各自 `results.json` 为准。结果为小样本观察，不代表显著的生产优势。

## 第一步：整改整合

前四批已整理为提交 `9381b24` 和 `33cdcb4`，连同原先的修复基线 `9a2ad90` 推送至 PR #13。原始 main 没有替换，且 PR 没有合并。

## 第二步：编辑工具消融

实验代码：`examples/tool_ablation.rs` 中的 `native_edit` 与 `native_patch`；脚本 `benchmarks/round5_edit_patch.py`。这是 **benchmark-only** 的 apply_patch 实现，不注册到默认产品工具。

单变量：保留同样的 Shell、历史工具、模型和任务指导，只替换编辑工具；每个场景随机交叉运行两次。四类任务：fix_arithmetic、implement_records、cross_file_fix、rust_multifile。

| 项目 | native_edit | native_patch |
|---|---:|---:|
| 外部验收 | 8/8 | 8/8 |
| 合计用时 | 232.35 s | 329.09 s |
| 模型请求次数 | 39 | 50 |
| 输入 Token | 88,779 | 131,295 |
| 输出 Token | 6,747 | 12,051 |

结论：**保留 edit_file，不将 apply_patch 用作默认工具。** patch 在个别运行更快，但跨任务总时长和 Token 明显更高；成功率相同。注意这比较的是本轮实现和 Schema，而非对所有可能的 apply_patch 实现的普遍否定。

实验记录：`D:/Kroma/experiments/kroma-round5-edit-patch/results.json`。

## 第三步：压缩/长会话

实验脚本：`benchmarks/round5_context.py`。单用户会话连续 13 轮；首轮种下隐藏字符串，最后一轮必须准确回答。16K 窗口强迫压缩，36K 窗口保留原始历史；除了上下文预算，其他设置一致。

| 项目 | 16K 压力窗口 | 36K 对照 |
|---|---:|---:|
| 独立应答通过 | 13/13 | 13/13 |
| 自动 checkpoint 触发 | 3 次 | 0 次 |
| 合计耗时 | 119.89 s | 95.81 s |
| 上游观察输入 Token | 85,158 | 112,491 |
| 观察缓存读取 Token | 62,464 | 97,664 |
| 最终首轮信息恢复 | 成功 | 成功 |

结论：**现有同步懒压缩可保持信息连续性，但触发时会有明显前台等待。** 压缩减少模型请求输入量，却不一定缩短任务总耗时。没有足够证据更换现有算法。

8K 极端预算预试在第 4 轮因为重复压缩请求/超时失败（日志为 90 秒及 context near hard limit）。这属于压力下的边界失败，不能声称正常上下文窗口会同样失败。数据：`D:/Kroma/experiments/kroma-round5-context/`、`.../kroma-round5-context-v2/`。

## 第四步：复杂 Rust 竞赛

当前主分支的真实项目夹具引入 WAL 时间排序回归，禁止修改注入测试。旧版本 fixture 因源码移动无法工作，已更新 `benchmarks/round4_windows.py`，由本次已提交 HEAD 生成复现仓库并冻结回归测试，保证输入和判分可追溯。

- Pi 1.5.0：148.25 秒通过，11 个模型请求、14 次工具调用。
- Kroma 原版本首次：170 秒达到实验超时，未通过；轨迹显示 20 次模型请求和大量源码搜索、反复测试。
- 已定位部分原因：Shell 子进程继承父进程 `KROMA_SESSION_FILE`，可能影响子进程中的嵌套 Kroma 测试。已在 `src/shell.rs` 中仅对 Shell 子进程移除该父会话环境变量。
- Kroma 修复后：**140.204 秒通过**，16 次模型请求、21 次工具调用，修复后单次运行仍比 Pi 多工具和模型请求。

结论：**修复了一个具体的跨进程执行边界，但没有证明 Kroma 已全面超过 Pi。** 本任务只是一个成对有效的复杂 Rust 运行；不同运行也受网关波动与 Cargo 测试成本影响。建议保留回归与再重复几次，不使用单次时长做产品宣传。

实验路径：`D:/Kroma/experiments/kroma-round5-complex-current/` 及 `.../kroma-round5-complex-session-fixed/`。

## 验证与剩余工作

- 完整 Windows `cargo test --locked -q`：777 单元测试通过，11 个按设计跳过；59 个插件集成测试通过；其他集成组均通过。
- `cargo clippy --locked --all-targets -- -D warnings`、`cargo fmt --all -- --check`、Desktop `cargo check` 均通过。
- PR #13 的工作流级结果仍有失败记录；连接器查询有的 job 成功但整体 workflow 失败，需要继续核查/重跑，**不能将 PR 当作已经 CI 全绿**。
- 当前编辑工具消融脚本与压缩脚本属于实验设施，不构成默认行为更改。
