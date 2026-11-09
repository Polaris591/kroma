# Ponytail 原则内化：嵌套消融竞赛

**可复现性声明。** 文中证据目录 `D:/Kroma/experiments/...` 位于 git 仓库之外（仓库根为
`D:/Kroma/kroma`），未入库；clone 本仓库无法复算文中数字，只能按本文件的命令重新生成新的
实验目录。运行方式：标准库 Python 3.12，经 `uv run --no-project --python 3.12 python ...`
执行；Rust 侧使用仓库自带工具链（`cargo build --locked`）。文中未记录 uv 版本，因此通过率、
耗时与 Token 均为历史记录，不能仅凭本仓库重算。

## 预先固定的设计

只替换内置 V7 的实现质量段，不引入 persona、模式切换、固定结尾或新的技能加载调用。

- A：改动前的生产质量段。
- B：A + 复用优先顺序（项目实现 → 标准库/平台能力及已有依赖 → 新代码）。
- C：B + 最小完整变更（调用方、接口、配置和测试；不削弱可读性、兼容、安全、验证，不静默缩减需求）。
- v2 比赛时 C 与内置提示词逐字一致，运行原生默认路径；A/B 通过隔离实验二进制的相同覆盖机制注入。
- 赛后默认选择 B，依据见 RESULTS.md；未来 setup 自动识别与当前生产逐字一致的组，走原生默认路径。
- 除这一段外，身份、执行流程、工具 schema、插件开关、模型、端点、thinking、工作路径均相同。

7 类任务，每类每组 3 次，共 63 场计分，另 3 场只读校准不计分。
保留原有算术修复、记录解析、跨文件修复、大日志诊断、多模块队列任务，新增：

- `reuse_existing`：复用项目 helper，隐藏验收实际调用行为，而非仅查文字。
- `complete_change`：扩展持续时间单位，覆盖解析、公共调度入口、间接调用和原有兼容行为。

固定种子 20261106，任务块打乱，三次重复使每组在每个任务的每个出场位置各出现一次。
所有组的任务指令明确原测试文件只读，新增测试另建文件，与哈希保护规则一致。
各场串行，超时看门狗统一 300 秒；超时计失败，不因失败删除场次或只挑成功重跑。
提示词用 LF 冻结并按原始字节核验，工具与基础身份哈希跨组一致。
实验源码快照包括构建所需未跟踪 Rust 模块，保留源文件哈希，不修改用户现有配置。

## 判定方式

第一优先级是外部功能验收、测试完整性、任务完成事件和实验控制审计。
只有完整配对才比较耗时、输入/输出/缓存 Token、模型请求、工具调用和错误。
补充审阅实际测试调用、无关文件修改和代码增减，行数小不作为功能不完整的补偿。
不根据未完成结果宣布冠军，不把汇总耗时较低说成每个任务都更快，不宣称统计显著。

限制：小样本合成 Python/日志任务，不代表真实大型 Rust/UI、插件或长上下文场景；
定向任务明确要求相关行为，可能出现通过率天花板；嵌套方案同时改变措辞和长度，
不是独立的纯长度实验，也不能估计两个原则的全部交互作用。
共享代理缓存无法清空；代理模型 ID 未独立核验；未知代理费率，货币成本记为 null。

## 离线验证

```powershell
uv run --no-project --python 3.12 python -m unittest discover -s benchmarks -p 'test_ponytail*.py' -v
uv run --no-project --python 3.12 python -m unittest discover -s benchmarks -p test_prompt_philosophy_ablation.py -v
cargo test --locked --lib prompt::tests
cargo test --locked --test context_budget_regressions
```

## 复现（run 会产生模型调用费用）

实验路径必须不存在；setup/build 不调用模型，run 才计费。下面以新路径为例：

```powershell
uv run --no-project --python 3.12 python benchmarks/prompt_philosophy_ablation.py setup --lab D:/Kroma/experiments/ponytail-ablation-new --quality-variants benchmarks/ponytail --repeats 3 --timeout 300 --tasks fix_arithmetic implement_records cross_file_fix diagnose_large_log python_complex reuse_existing complete_change
uv run --no-project --python 3.12 python D:/Kroma/experiments/ponytail-ablation-new/runner.py build --lab D:/Kroma/experiments/ponytail-ablation-new
uv run --no-project --python 3.12 python D:/Kroma/experiments/ponytail-ablation-new/runner.py run --lab D:/Kroma/experiments/ponytail-ablation-new
```

同一实验不要并发启动两个 run。结果保存在 `metadata.json`、`plan.json`、`results.json`、
`summary.json`、`controls.json` 及各场的原始提示词、工具定义、事件、stderr 和最终源码中。

## 预赛记录

`D:/Kroma/experiments/ponytail-ablation-v1` 在计分早期中止，全部 trace 保留。
原因是旧指令只禁止删除/削弱测试，但哈希保护拒绝任何测试文件变化，合理追加用例也被误判。
修正共同任务 guard 后，在全新 `ponytail-ablation-v2` 重启全部计划；v1 不用于比较或排名。

## 隔离与审计边界

本轮是受任务指令约束的本地隔离目录实验，不是操作系统安全沙箱，也不是严格盲测。
模型 shell 与宿主同权限，理论上可以越级访问相邻 oracle、先前场次或真实 provider 配置。
任务指令禁止跨项目、访问凭据、改配置、联网；最终审阅 trace 的明显越界行为，
但这种审阅不能证明不存在所有隐蔽访问。配置只在 run 开始时核验，不能强制阻止外部进程中途改配置。
共享实验 HOME 若被场次写入 AGENTS.md，会由基础提示词审计发现后停止；本轮不宣称解决跨场污染。
summary.json 的累计数含所有已记录尝试（含失败/超时），不能拿不完整/未匹配汇总排名；
最终分析另按完整配对和共同成功配对给出比较。

后续代码修复了 api_key_env 指向 KROMA_* 时被环境清理丢失的问题，仅保留 profile 明确指定的变量，
不把值写入元数据。当前 v2 使用内联凭据，冻结运行器不改动，不受此修复影响。

## 最终结果

完整报告见 `RESULTS.md`。63/63 通过；默认采用 B。原始实验与分析脚本位于
`D:/Kroma/experiments/ponytail-ablation-v2`，赛后选择单独记录在 `adoption.json`。
