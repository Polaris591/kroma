# 独立读取合并提示：单句 A/B 消融

## 只改一个变量

- A：当前源码中的生产 EXECUTION_GUIDANCE + V7_GUIDANCE，原样提取。
- B：完整保留 A，只在末尾追加 append.txt 中的一句话。
- 新增句子的意思：相关实现与测试的读取彼此独立时，尽量一个模型回合取齐，但不要为了合并而扩大读取范围。
- A 本来已有合并独立检查的要求。因此本轮测的是“更具体的强调是否有额外收益”，不是新增批处理能力。
- 不修改生产提示词、工具定义、工具执行顺序、测试要求或 Agent Loop。
- B 指导文本比 A 多 169 字节；如果能少走无效回合，才可能减少最终 Token。不能因文字变化直接认定收益。

## 本轮冻结条件

- 模型配置：deepseek-v4.1-flash，reasoning_effort=high、deepseek_thinking=high，同端点与原 profile 预算。
- 源码：setup 时记录提交和实际文件哈希；不把工作区已有修改冒充干净提交。
- 同一个隔离构建的二进制，只有指导文本不同；审计实际 system 文本与工具 schema。
- 任务：算术修复、记录解析、跨文件修复、大日志定位、多模块持久化任务队列。
- 每题重复 4 次，每个版本 20 场，合计 40 场；另有 A/B 两场不计分接口校准。
- 每题 A/B 各两次先跑，任务块按固定种子打乱，所有模型调用串行。
- 使用相同绝对工作路径，每场从原始 fixture 开始，结束后保存最终代码。
- 关闭用户插件、技能、MCP 与全局指令，排除历史会话污染。
- 验收预检使用 Python -B，不在工作区生成带时间戳的字节码缓存。
- 不额外设置任务 Token 配额或默认时间截断。
- 记录真实共享端点缓存，不宣称能清空代理缓存或核实代理背后的上游身份。
- 没有经核验的代理费率，实际货币成本保留未知。

## 判定

先看独立验收通过率，再比较同任务、同重复的完整配对耗时、模型请求、工具调用、输入/输出及缓存 Token。
保留失败、返工和重试开销，不只挑获胜场次。按任务分层，尤其检查多模块任务有无退化。
模型自报成功不计分；原测试文件必须保持不变，退出状态与任务完成事件也必须正常。
通过功能验收不等于完整工程质量等价，最终仍需核对测试执行与无关修改。
没有收益就不采用；只有五类合成任务，不将小样本外推成所有项目上的普遍结论。

## 运行与证据

本次实验：D:/Kroma/experiments/read-batching-ab-v1。

- metadata.json：模型、源码/提示词/验收代码哈希与条件。
- plan.json：冻结的全部顺序；启动时核验计划哈希。
- prompts/A.txt、B.txt 与 append-guidance.txt：准确文本。
- build-driver.log、build.log：隔离构建日志。
- competition.log、competition.pid、active.json：运行进展。
- results.json：校准与逐场原始结果。
- summary.json：汇总；未配齐时不得直接排名。
- trials：实际提示词、工具定义、输入、事件、计量、会话与最终源码。
- runner.py、runtime、oracles：冻结运行器、隔离代码、外部验收。

离线检查，不产生模型调用：

```powershell
uv run --no-project --python 3.12 python -m unittest discover -s benchmarks -p test_prompt_philosophy_ablation.py -v
uv run --no-project --python 3.12 python benchmarks/agent_task_suite.py --self-test
```

创建新实验，目录必须不存在：

```powershell
uv run --no-project --python 3.12 python benchmarks/prompt_philosophy_ablation.py setup --lab D:/Kroma/experiments/read-batching-new --arms A,B --append-guidance benchmarks/read_batching/append.txt --repeats 4
uv run --no-project --python 3.12 python benchmarks/prompt_philosophy_ablation.py build --lab D:/Kroma/experiments/read-batching-new
```

在隔离源码中运行提示词回归后，再执行下面命令。run 会明确产生付费模型调用，不要对正在运行的实验重复启动：

```powershell
uv run --no-project --python 3.12 python benchmarks/prompt_philosophy_ablation.py run --lab D:/Kroma/experiments/read-batching-new
```

默认三版本实验仍可运行；这次没有改动上轮已冻结的实验目录和结果。

## 启动前修正与验证记录

- Windows 默认文本写入会将 LF 改成 CRLF；本轮已改为明确写入 LF，并让实际 system 审计按原始字节读取，不再隐式归一化。
- 额外用 Rust 公共 PromptComposer 接口验证 A 等于编译后的生产常量、B 只追加一句、基础身份不变、重复投影不叠加、持久历史不变及预算计算正确。
- 隔离快照的原单元测试因 TUI 的 MessageRole::Tools 缺少匹配分支而无法编译；没有擅自修改 TUI。公共接口集成验证通过，原单元测试记为受阻，不能宣称通过。
- 上一轮三版本实验未做原始换行字节强校验，因此其观测仍保留，但不能严格把全部差异归因于措辞；本轮消除了这个额外变量。所有修正均发生在本轮首次模型调用之前。
