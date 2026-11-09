# 原生 Kroma / Pi 简单竞赛

**可复现性声明。** 文中证据目录 `D:/Kroma/experiments/...` 位于 git 仓库之外（仓库根为
`D:/Kroma/kroma`），未入库；clone 本仓库无法复算文中数字，只能按本文件的命令重新生成新的
实验目录。运行方式：标准库 Python 3.12，经 `uv run --no-project --python 3.12 python ...`
执行；Rust 侧使用仓库自带工具链（`cargo build --locked`）。文中未记录 uv 版本，因此通过率、
耗时与 Token 均为历史记录，不能仅凭本仓库重算。

此外，本文引用了本机绝对路径（如 `C:/Users/24524/.cargo/bin/kroma.exe`）与本地安装的 Pi 1.4.0，
这些既不在仓库中也不可复现；只能凭记录判断当时的版本，不能当作当前环境。

本次使用已安装的原生 CLI，不用 `tool_ablation` 示例、不用提示词消融二进制、不替换系统提示词，不修改 Agent Loop。

- Kroma：`C:/Users/24524/.cargo/bin/kroma.exe`，0.4.0 (3ae32d69060e-dirty)。这是已安装版本，并非声称等于当前仓库提交。
- Pi：本机托管安装的 1.4.0，通过原生 `pi-launcher.js` 入口执行。
- 相同代理端点、`deepseek-v4.1-flash`、High、同一凭据和相同模型上下文/最大输出配置。
- 保留两边各自默认提示词、内置工具、上下文策略与原生 API 适配；不把 Pi 的工具集裁剪成 Kroma 的工具集。
- Kroma 原生 `deepseek_thinking=high` 保留，Pi 使用原生 `--thinking high`，没有在中间层强行改写两边请求体。
- 排除用户插件、技能、MCP、全局 AGENTS.md、历史会话及自动更新/遥测；不修改用户已有设置文件。
- 记录解析、跨文件修复、多模块持久化队列三个任务，各跑一次，共六场；交替先后顺序、串行运行。
- 两边使用相同任务文本、原始 fixture 和绝对工作路径，完成后保存代码再重置 fixture。
- 独立功能验收，不接受模型自报；原有测试文件必须保持不变。
- Pi 的 `usage.input` 是未缓存输入，归一化总输入为 input + cacheRead + cacheWrite；Kroma 的 input 已包含缓存输入，不能再重复相加。未报告的数据保留未知。
- 费用未知，不采用模型配置中的零价格或其他端点公开价格冒充实际费用。
- 仅一次简单冒烟比较，没有重复实验与显著性检验，不构成总体性能排名。

本次目录：`D:/Kroma/experiments/native-kroma-pi-smoke-v1`。
日志：`D:/Kroma/experiments/native-kroma-pi-smoke-v1-driver.log`。
目录内的 metadata、plan、results、summary 及 trials 保存版本哈希、结果、原始事件、会话、stderr、原始任务和最终代码。

离线计量自检：

```powershell
uv run --no-project --python 3.12 python benchmarks/native_kroma_pi_smoke.py --self-test
```

新建一次比较（明确产生模型调用，不得复用已存在的实验目录）：

```powershell
uv run --no-project --python 3.12 python benchmarks/native_kroma_pi_smoke.py --run --lab D:/Kroma/experiments/native-kroma-pi-smoke-new
```

任务和验收复用先前冻结的 `prompt-philosophy-v1/oracles`；该目录只作为 fixture 来源，不使用其插桩程序或实验提示词。

## 本次实测结果

|指标|原生 Kroma|原生 Pi|
|---|---:|---:|
|功能验收|3/3|3/3|
|累计墙钟耗时（秒）|125.984|136.375|
|输入 Token（含缓存）|115,934|128,860|
|输出 Token|12,715|15,777|
|输入＋输出 Token|128,649|144,637|
|未缓存输入|9,438|9,820|
|模型计量请求数|21|22|
|工具调用数|22|28|

|任务|Kroma 耗时（秒）|Pi 耗时（秒）|
|---|---:|---:|---:|
|implement_records|16.906|18.375|
|cross_file_fix|21.609|15.578|
|python_complex|87.469|102.422|

本轮 Kroma 累计耗时低 7.62%，输入加输出 Token 低 11.05%。
Kroma 在记录解析和多模块任务上较快，Pi 在跨文件修复上较快。两边均通过全部独立验收，原测试文件未改动，任务文本及初始源文件/测试哈希一致。
预检验收会生成 __pycache__，其中 .pyc 随 fixture 创建时间变化；原始 input.json 如实保留这些差异。本次不是所有工作区文件逐字节相同的严格实验。
Pi 会话元数据确认选择相同模型和 High。两边保留原生请求构造差异，未声称请求体完全一致。
实际代理货币成本未知；不以模型配置中的零价格认定免费。
只有三个任务、各一次，且共享端点缓存与时间波动未被消除：只能报告这一次观测，不能证明普遍优势。
