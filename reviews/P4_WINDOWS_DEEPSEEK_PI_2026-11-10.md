# Kroma 第四批：Windows DeepSeek High 性能专项报告

**日期：** 2026-11-10（本机）  
**状态：** 真实性能对照已执行；指标修正已实现并通过回归；由于没有证明通用性能收益，本批未修改默认 Prompt、工具集、压缩算法或 Agent Loop。

## 1. 环境与公平性

- Kroma：0.4.0，本地第三批补丁工作树（基于 `9a2ad90`，包含未提交修改）；最终二进制 SHA-256 见各实验的 `results.json`。
- Pi：`@earendil-works/pi-coding-agent` 1.5.0；在 Windows 本机更新。
- 相同的 Chat Completions API 端点：`https://cobblemon.asia/v1`。
- 相同的实际模型 ID：`deepseek-v4.1-flash`；同一凭据，经本机不输出秘密的比较确认。
- 思考等级：High（`reasoning_effort=high`）；两边均不附加独立 `thinking.type` 字段。
- 测试目录相互隔离，顺序随机交叉；独立验收，不使用代理的自述作为成功依据。
- Pi 使用 `--no-extensions --no-skills --no-mcp --no-context-files`；Kroma 使用 `--no-mcp` 和隔离的空 `KROMA_PLUGIN_DIR`，不会更改用户日常启用状态。
- **注意**：工具集仍是各自原生的四个工具；这测试原生 Harness 表现，而不是精确相同工具语义。

## 2. 发现并纠正的基准失配

初始六项实验（**不是公平的基础 Harness 对比**）意外包含 Kroma 已启用的 Goal/Memory/Subagents/Web 插件，Pi 则关闭扩展。无真实 API 的本地 HTTP 捕获证实：
- Pi：4 个工具，工具 Schema 约 2,841 JSON 字节，整请求约 5,599 JSON 字节；
- 原配置 Kroma：12 个工具，Schema 约 13,789 字节，整请求约 18,419 字节；
- 隔离插件 Kroma：4 个工具，Schema 约 3,232 字节，整请求约 5,926 字节。

原配置实验：双方均 6/6 成功，但 Kroma 108.48 s、157,215 输入 Token，对比 Pi 78.64 s、55,039 输入 Token。**不能用此差异判断基础内核的速度。** 原始数据保留在 `D:/Kroma/experiments/kroma-round4-pi150/results.json`。

## 3. 公平配对：6 项 × 2 重复，24 次运行

来源：`D:/Kroma/experiments/kroma-round4-clean-pi150/results.json`。

| 指标 | Kroma | Pi 1.5.0 |
|---|---:|---:|
| 外部验收 | 12/12 | 12/12 |
| 合计耗时 | **157.56 s** | 162.42 s |
| 模型请求 | **50** | 53 |
| 工具调用 | 51 | **50** |
| 输入 Token 总量 | **103,179** | 109,886 |
| 未缓存输入 Token | 28,811 | **24,766** |
| 缓存读取 Token | 74,368 | **85,120** |
| 输出 Token | 6,462 | **5,443** |

- Kroma 12 对中有 6 对更快，配对耗时差中位数约 -0.18 s；总耗时优势仅约 **3%**。
- Kroma 未缓存输入 **高约 16.3%**、输出 **高约 18.7%**。
- 工具调用轨迹中没有发现同一命令字符串重复执行的明显高频现象（此指标不等同于发现所有语义冗余）。
- 任务都是较小的合成编码/检查/日志诊断场景；模型响应和网关缓存有波动，不应将这个小样本解释为普遍领先。

## 4. Rust 多文件独立回归：2 对、4 次运行

来源：`D:/Kroma/experiments/kroma-round4-rust-pi150/results.json`。测试夹具包含多文件公共导出、解析器逻辑错误，以及运行后才由监督进程追加的隐藏 Rust 回归测试。

| 指标 | Kroma | Pi 1.5.0 |
|---|---:|---:|
| 独立测试通过 | 2/2 | 2/2 |
| 合计耗时 | 42.48 s | **38.73 s** |
| 模型请求 | 10 | 10 |
| 工具调用 | **11** | 15 |
| 输入 Token | 27,102 | **23,758** |
| 输出 Token | 3,111 | **1,678** |

一对速度 Kroma 更快，一对 Pi 更快。此样本揭示 Rust 任务**可能存在生成长度成本**，但不足以断言是哪段 Prompt 或工具行为引起的。

## 5. 代码落地与验证

本批针对现有 `TaskStats` 做有限改动：
- 原 `shell_ms` 实际包含所有工具，现拆分为 `tool_ms`（全部工具）与 `shell_ms`（Shell 本身），兼容保留 `shell_ms`；
- 失败模型请求在已知发起时记录经过时间，单独输出 `failed_model_ms`，并计入总 `model_ms`，避免把失败等待漏算；
- 缺失的上游 Token Usage 仍显示为未知值，而非伪造零成本。
- 增加 `benchmarks/round4_windows.py`：Windows 本地随机配对、独立目录、配对验收、Usage/工具次数/重复操作统计、存档原始试验记录。
- 增加 `benchmarks/round4_payload_capture.py`：**仅连接本机假 HTTP 端点**，用无效假密钥核对请求参数和工具/Prompt 体积，不输出 Prompt 原文或密钥。

验证命令：`cargo fmt --all -- --check`、`cargo test --locked -q`、`cargo clippy --locked --all-targets -- -D warnings`、Desktop 的 `cargo check`、Python 脚本 self-test。全数通过；777 个 Rust 单元测试通过，11 个按设计跳过，插件集成 59/59 通过。

## 6. 决定与尚未开展的消融

**本批不修改默认策略。** 目前缺少证据证明大幅缩短 Prompt、强制并发工具或重写 Agent Loop 能降低成功交付成本；更改它们还可能损害长任务验证质量。

尤其以下实验尚未完成，不应当列作已验证的收益：
1. Shell + edit_file 对 Shell + apply_patch 的同端点独立消融；当前不存在已准备好的等价 apply_patch 生产工具；
2. 真实、连续长会话压缩/缓存对比；
3. 大型真实 Rust 多文件任务的重复试验和统计检验；
4. 正式的请求级成本账单（部分网关报告的成本字段为 0，不能当作免费）。

**下一步的必要证据**：先实现 *benchmark-only* Apply Patch 工具、与现有 edit_file 做单变量实验，任务通过率不下降才考虑生产变更；再扩大 Rust 任务规模和重复次数。未经验证不应为了“追求更快”引入额外抽象。

## 7. 现场记录目录

- `D:/Kroma/experiments/kroma-round4-pi150/` —— Kroma 原配置含插件，**不公平基线**。
- `D:/Kroma/experiments/kroma-round4-clean-pi150/` —— 基础 Harness 六类任务的 12 组配对。
- `D:/Kroma/experiments/kroma-round4-rust-pi150/` —— Rust 多文件两组配对。

所有实验产物在 Windows 本地实验目录；未将凭据写入本报告。原始日志只包含人造任务，但仍建议按内部实验数据管理。
