# Kroma 核心提示词三版本消融竞赛

**可复现性声明。** 文中证据目录 `D:/Kroma/experiments/...` 位于 git 仓库之外（仓库根为
`D:/Kroma/kroma`），未入库；clone 本仓库无法复算文中数字，只能按本文件的命令重新生成新的
实验目录。运行方式：标准库 Python 3.12，经 `uv run --no-project --python 3.12 python ...`
执行；Rust 侧使用仓库自带工具链（`cargo build --locked`）。文中未记录 uv 版本，因此通过率、
耗时与 Token 均为历史记录，不能仅凭本仓库重算。

## 研究问题

- A：当前生产 `EXECUTION_GUIDANCE + V7_GUIDANCE`，原样从 `src/prompt.rs` 提取。
- B：整理、合并措辞，但保留代码阅读顺序、独立检查合并、进度汇报时点等工作流要求。
- C：基于 B，只把检查方式/顺序与进度时点改为模型按需选择。
- B/C 的实现质量段、测试与退出条件段逐字相同。
- 身份、准确报告、破坏性操作边界、语言要求、工具定义与运行环境不随版本改变。

这里测的是核心指导文本，不是插件能力。三组都不加载用户插件、技能、MCP 和全局 AGENTS.md。没有迁移或删除生产插件规则，也没有修改生产 PromptComposer。B 同时改变措辞与长度，因此不能把收益单独归因于 Token 数减少。工具描述本身保留了合并检查等提示，C 不是“删除所有工作流提示”。

## 首轮预先固定的条件

- 源码：setup 时记录提交、实际源文件哈希和隔离插桩后的哈希。
- 模型 ID：`deepseek-v4.1-flash`，使用本地 `deepseek` profile。
- `reasoning_effort=high`、`deepseek_thinking=high`；端点、上下文和单次输出配置均保持原 profile 不变。
- 这是代理端配置的模型 ID，不声称已经独立核实代理背后的真实上游模型。
- 5 类任务：算术修复、记录解析、跨文件规范化修复、大日志定位、多模块持久化任务队列。
- 每类任务 3 次重复，每个版本 15 场，共 45 场计分；另有 3 场只读 API 校准，不计入排名。
- 固定种子 20261106，任务块打乱，每个任务的三个版本在三个位置各出场一次。
- 串行调用，不让不同版本争抢同一端点；不额外施加任务 Token 配额或默认时间截断。
- 所有场次使用相同绝对工作路径；结束后保留代码树，再从原始 fixture 创建下一场。
- 保留实际缓存命中统计，不假装能清空代理/上游缓存，也不宣称冷缓存测试。
- 没有已核验的代理费率，货币成本记为 null，而非使用官方定价冒充实际成本。

## 计分与解释

首要比较独立验收通过率；只有同任务、同重复的完整配对才用于性能结论。
通过要求：进程正常退出、任务完成事件、外部功能验收通过、原测试/只读输入未被篡改、提示词及工具控制审计一致。
验收代码保存在 fixture 外，不接受模型自己宣称通过；这不是操作系统级安全沙箱。

记录墙钟耗时、模型计量请求数、工具调用及错误、输入/输出/缓存/未缓存 Token 和压缩事件。
最终分析按任务分层，并对共同成功的配对比较耗时和 Token，避免复杂任务权重或失败提前退出扭曲结论。
不使用未经定义的综合分，也不根据中途不完整的汇总宣布冠军。
计量请求数不一定等于包含所有网络重试的 HTTP 次数，排查重试时应结合 stderr 中的 attempt 日志。

功能通过不等于完整工程质量：还需从 trace 核对是否实际执行了要求的测试、是否无关修改、汇报是否与证据一致。
首轮是合成 Python 任务，不代表大型 Rust 项目、长上下文压缩、会话恢复或插件负载下的效果；不能直接据此替换生产默认提示词。

## 文件与复现

运行依赖现有 Rust/Cargo、Git、Python 3.12；Python 脚本只使用标准库。
Windows 默认 python 可能指向商店占位符，本机可用 `uv run --no-project --python 3.12 python`。

离线检查（不产生模型调用）：

```powershell
uv run --no-project --python 3.12 python -m unittest discover -s benchmarks -p test_prompt_philosophy_ablation.py -v
```

创建新实验（路径必须不存在；只读已有 profile，不复制密钥）：

```powershell
uv run --no-project --python 3.12 python benchmarks/prompt_philosophy_ablation.py setup --lab D:/Kroma/experiments/prompt-philosophy-new --repeats 3
uv run --no-project --python 3.12 python benchmarks/prompt_philosophy_ablation.py build --lab D:/Kroma/experiments/prompt-philosophy-new
```

隔离副本的提示词回归检查：

```powershell
$env:CARGO_TARGET_DIR = 'D:/Kroma/experiments/prompt-philosophy-new/target'
Push-Location D:/Kroma/experiments/prompt-philosophy-new/runtime
cargo test --release --locked --lib prompt::tests
Pop-Location
```

显式启动付费模型调用：

```powershell
uv run --no-project --python 3.12 python benchmarks/prompt_philosophy_ablation.py run --lab D:/Kroma/experiments/prompt-philosophy-new
```

不要在同一个实验仍运行时再启动 run。断点恢复会跳过已完成场次，不会静默覆盖目录；有未完成 work 目录时会停下，需先检查中断原因。

本次实验目录：`D:/Kroma/experiments/prompt-philosophy-v1`。
关键文件：

- `metadata.json`、`plan.json`：冻结条件、哈希、全部出场顺序。
- `prompts/A.txt`、`B.txt`、`C.txt`：冻结的指导文本。
- `runner.py`：冻结的运行器快照。
- `runtime/`、`build.json`：隔离源码与统一二进制哈希。
- `competition.log`、`competition.pid`、`active.json`：运行日志、工作进程和当前场次。
- `results.json`：原始计分结果及独立校准结果。
- `summary.json`：当前汇总；未完成时不能用于排名。
- `trials/<任务>-<重复>-<版本>/`：实际 system 文本、工具定义、事件、计量、会话及最终源码。

只生成汇总，不调用模型：

```powershell
uv run --no-project --python 3.12 python benchmarks/prompt_philosophy_ablation.py report --lab D:/Kroma/experiments/prompt-philosophy-v1
```

setup 开发阶段曾有一次基线提取失败和一次缺失 build.rs 的构建失败，均发生在模型调用之前；失败目录/日志保留在实验父目录与实验目录中，不混入计分结果。
