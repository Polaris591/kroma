# 第三批：性能实测；第四批：架构与文档整理

日期：2026-11-05。承接前两批的未提交修改，不覆盖已有 review/probe/script，未提交 Git commit。
第三批是可复现的性能实验，第四批是增量职责整理；不是 Agent Loop 重写。

## 第三批交付

新增 5 个默认忽略的本地测量，复用已有 TUI 测量：文件快照、上下文准备与假 checkpoint 等待、摘要输入规划、工具分派与实际 Shell 进程、自动标题排队与完成、长历史 TUI。无新增依赖，无付费提供商请求。

执行方法见 [performance](../docs/performance.md)。`PERF` JSON 包含场景、夹具、样本数、纳秒分位数及运行环境标记；完整机器信息见 [environment.json](batch-3-4/environment.json)。本机 Windows GNU、Ryzen 5 7500F、Rust 1.99.1；release、单测试线程。

### 保留的优化：同一 checkpoint source 只构建/投影一次

`Compression::summary_source` 在缩小候选边界时复用已构建的视图，仅逐步截短，避免为每个候选重建并投影全历史。不改 token 规则、安全边界、输出预算、阈值、重试、reasoning/provider provenance 或 WAL。

测试保留优化前参考实现，逐项比较最终边界、序列化后的完整输入和估算 tokens。同进程交替顺序、预热后各 21 个样本，201 条消息、132,534 字节原始 JSON、输入上限 1,800：

| 最终同进程对照 | p50 | p95 |
| --- | --- | --- |
| 旧参考实现 | 9.114 ms | 14.343 ms |
| 当前实现 | 3.827 ms | 4.541 ms |

该夹具 p50 耗时降低约 58%，约 2.38 倍速度；这是反复缩小前缀这一 CPU 路径的收益，不是整项任务加速比例。较早独立对照为 16.112 → 7.824 ms，同样支持保留该优化，但绝不混用不同运行的数字计算收益。

原始记录：[首次对照](batch-3-4/context-ablation.txt)、[最终 5 项测量](batch-3-4/after.txt)。

### 其他最终本机样本（不是前后优化对照）

| 场景 | p50 | p95 | 样本数 |
| --- | --- | --- | --- |
| 快照首次捕获，1,000 × 1 KiB | 1,095.004 ms | 不作分布解释 | 1 |
| 未变更快照 | 322.711 ms | 359.216 ms | 21 |
| 修改单文件快照 | 501.776 ms | 533.519 ms | 21 |
| 上下文准备，即时假 checkpoint | 0.262 ms | 0.360 ms | 7 |
| 上下文准备，30 ms 假 checkpoint | 31.710 ms | 46.819 ms | 7 |
| 工具直接 / registry / 空 hook | 49 / 87 / 262 ns | 62 / 110 / 331 ns | 各 31，单样本平均 1,000 次 |
| 实际 Shell `echo perf` | 51.332 ms | 55.585 ms | 11 |
| 标题同步 schedule | 4.4 µs | 26.5 µs | 11 |
| 标题完成，30 ms 假请求 | 46.758 ms | 47.937 ms | 11 |

快照计时包含根目录探测及可取消事务，但 store 预先初始化；首次捕获不是全新安装冷启动。假请求的调度/系统计时粒度会使 30 ms 延迟超时；不是 API 性能或账单数据。

| TUI TestBackend 场景 | p50 | p95 |
| --- | --- | --- |
| 300 条消息、流式窗口、80×24 | 1.902 ms | 2.468 ms |
| 3,000 条消息、流式窗口、80×24 | 1.879 ms | 2.628 ms |
| 3,000 条完整历史、强制重建 | 18.069 ms | 19.055 ms |
| 4,800 行展开输出、强制重建 | 9.959 ms | 13.904 ms |
| 900 行代码、强制重建 | 1.824 ms | 2.322 ms |

原始记录：[最终 TUI 样本](batch-3-4/tui-after.txt)。不同运行的流式窗口约 1.8–4.5 ms，说明负载影响明显。没有据此宣称架构抽取加速了渲染，也没有把强制 cache rebuild 当作每帧的正常行为。

**未采纳的策略变化**：不延后快照、不新增工具并行、不改压缩阈值/重试、不改变标题预算/超时、不强加 Shell 超时。工具 registry 开销远小于本机 Shell 启动；合成夹具不足以证明这些行为变化值得付出风险。未测实际终端/GPU、校准 RSS、真实提供商端到端成本或跨平台性能。

## 第四批交付

- 新 `tui/runtime_events.rs` 负责事件类型与 RuntimeEvent → UiEvent 适配；queue/title 不再依赖 renderer。
- 工具展示/截短函数原样归入已有 `tui/tool_display.rs`，历史重载与实时通知复用。保留入队前格式化和既有队列聚合/容量语义，未改成入队完整工具 JSON。
- 新 `plugins/approval.rs` 负责有界文件扫描、内容比较和 v3 授权指纹；worker 持有一个扫描值，不再拥有扫描算法。模块不依赖进程或 Host。指纹标签、字段顺序、mtime 处理不变。
- 新 [architecture](../docs/architecture.md) 明确 Host/core、WAL、通知、TUI 与插件职责；不增加框架，不按行数机械拆文件。
- 修正 README 的“两种 Host”“exact token accounting”和自动快照的过度承诺；区分观测 usage 与保守预算。
- design-notes 明确 TRC/APEC 基准属于旧阶段，不是当前 lazy single-pass 的证据；更新默认组装、snapshot profiling 与模块说明。
- lifecycle 区分前台 compaction 结束和独立标题/事件工作；记录 Warning 的提交顺序、取消快照清理和 Host reload 基线。

## 最终验证

| 检查 | 结果 |
| --- | --- |
| `cargo test --locked --tests` | 745 passed，10 ignored，0 failed；21 个测试 suite |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过 |
| Core / Desktop `cargo fmt --all -- --check` | 均通过 |
| `git diff --check` | 通过 |
| 新改文档本地链接 | 通过；含报告链接的最终检查另见会话验证 |
| release `perf_` | 5 passed |
| release 指定 TUI benchmark | 1 passed |
| Desktop `cargo check --locked --manifest-path desktop/src-tauri/Cargo.toml` | 通过 |
| Desktop 原生库单元测试 | 6 passed，使用下述 GNU manifest 方式 |

新增普通回归覆盖 planner 与旧实现等价、事件 accounting/reasoning 顺序及未知字段、失败工具展示与 Warning、v3 指纹字段布局和忽略 mtime。既有上下文投影、checkpoint cache-prefix、插件重载/授权/取消、WAL 与工具结果回归均通过。

Desktop 沿用前两批已确认的 Windows GNU 限制：标准 `cargo test --lib` 的库测试 EXE 缺少 Common Controls v6 资源。此次以 `cargo rustc --lib --profile test` 显式链接 Tauri 生成的 `libresource.a`，再运行最新测试 EXE，6 项通过；具体命令见 [前两批报告](BATCH_1_2_FIXES_2026-11-05.md)。没有保留临时 build.rs 资源改动，不能宣称标准 GNU 测试启动问题已解决。

本批未改 npm launcher 或 Desktop 前端，因此不重复其前两批已通过的检查；macOS/Linux 实机和真实提供商测试仍未执行。

## 记录完整性与修正

- 初次 5 项性能运行有 4 项通过，快照夹具错误地把 Windows canonical root 与非 canonical 路径直接比较，断言失败。修正测试路径比较后通过；`baseline-initial.txt` 和 `checkpoint-path-assertion-failed.txt` 是失败尝试，不能作为全套通过证据。
- 新事件适配测试最初误期望队列返回单条 Usage；实际队列按既有语义聚合为 UsageBatch。只修正测试，检查 last-request 的未知字段和交付顺序，不改变生产队列。
- 首次 Clippy 指出抽取函数放在已有 test module 后。将 test module 移至文件末尾后，Clippy 与该模块 2 项回归通过；没有用 allow 绕过检查。
- 正式原始结果：[core-tests](batch-3-4/core-tests.txt)、[clippy](batch-3-4/clippy.txt)、[Desktop check](batch-3-4/desktop-check.txt)、[Desktop unit tests](batch-3-4/desktop-tests.txt)。较早测量只用于交代波动，不替代最终结果。

本批保持 v3 授权格式，不再因职责抽取要求重新授权。前两批的 v2 → v3 兼容提示仍适用。所有改变均留在工作区，尚未提交。
