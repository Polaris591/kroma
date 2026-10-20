# Kroma Benchmark Foundation v1 — 可执行基础层

状态：**FOUNDATION_V1_OFFLINE_VERIFIED / PROVIDER_RUN_NOT_PERFORMED**
适用基线：Kroma RC1 Hotfix（2026-10-18）；本分支为独立增量开发，**不是已签名的 RC1 正式发布包**。

## 0. 定位与非目标

Foundation v1 只完成三件事：

1. **Freeze**：把实验 Manifest v1、完整 BenchTask 任务/判题规则、运营者给定价目表以及实验决策门槛一次性冻结，计算原始字节 SHA-256、Rust Suite Fingerprint，并限制样本、模型、策略、矩阵、预算。
2. **Evaluate**：消费已有 `kroma bench --json` 输出，独立检查执行器类型、种子、矩阵完整性、预算、门禁、逐任务配对、任务指纹、计价证据；生成**剔除原始工具轨迹与模型答复**的统计报告。
3. **Smoke**：在**合成执行器**模式下，通过已有 Rust CLI 执行真正的「任务 → AgentBenchRunner → JSON → 证据处理」链条。不调用 API，不执行带付费模型的实验。

**不是**第二套 Agent Loop、另一个 Rust Runner、跨 Harness 自动比较器或可自行授权的真实模型计费系统。已有 `kroma-core/src/bench.rs` 仍是执行/独立评分权威。

特别说明：`kroma --experiment <manifest> bench` 目前是 dry-run，**不会**按 manifest 调用 AgentRunner；如果未修好装配接线，绝不可把 `EXP-00` 写成“真实可执行实验”。此版本明确**拒绝**不属于原生默认 `single_agent` / `adaptive_tournament_3` 的两臂配置，以免悄悄运行另一套策略。

## 1. 本次交付目录

| 文件 | 作用 |
|---|---|
| `scripts/bench-foundation.js` | 零依赖 Node CLI：freeze / evaluate / smoke |
| `scripts/test-bench-foundation.js` | 数据污染、预算绕过、成本、身份、篡改、报告脱敏等回归测试 |
| `crates/kroma-core/tests/bf01_foundation_v1_fingerprint.rs` | Rust / JS Suite SHA-256 跨语言合同（必须 cargo 验证） |
| `experiments/foundation-v1/{manifest,suite,prices}.json` | **模拟示例**，6 个小任务、2 个留出、2 次重复、2 个原生默认 Arm，不能当实际编码能力基准 |
| `docs/benchmark/FOUNDATION_V1.md` | 使用手册与安全边界 |
| `docs/benchmark/IMPLEMENTATION_STATUS.md` | 现状、未完成项、验收条件及外部工作最小集 |

## 2. 无模型、零费用的起步方法

仓库根目录执行：

```bash
# Node 20+；不需要 API Key
node --test scripts/test-bench-foundation.js

# 预注册冻结：240,000 是矩阵的“申明 Token 上限”，合成执行没有模型开销
node scripts/bench-foundation.js freeze \
  --manifest experiments/foundation-v1/manifest.json \
  --suite experiments/foundation-v1/suite.json \
  --prices experiments/foundation-v1/prices.json \
  --executor synthetic \
  --budget-tokens 10000 \
  --seed 12345 \
  --out /tmp/kroma-foundation-01

# 已有 Rust 可执行文件时直接做端到端烟雾测试：
node scripts/bench-foundation.js smoke \
  --lock /tmp/kroma-foundation-01/lock.json \
  --kroma ./target/release/kroma \
  --out /tmp/kroma-foundation-01-analysis

# 结果：metrics.json、report.md、results.redacted.json
```

`smoke` 用 `spawnSync` 的**argv 数组**调用已有的 Rust CLI，禁止 Shell 插值；使用独立临时 workspace，退出时清理；只执行 `--executor synthetic`。原始 Rust JSON 在私有临时目录处理后销毁，不保存完整模型答复/工具 Trace。路径参数必须指向可信的本地可执行文件。

已有 JSON 报告的离线导入：

```bash
node scripts/bench-foundation.js evaluate \
  --lock /tmp/kroma-foundation-01/lock.json \
  --report /secure/path/to/native-bench-report.json \
  --out /tmp/kroma-foundation-evidence
```

所有输出目录必须此前不存在，防止无提示覆盖。绝不把原始 Provider Report、密钥或环境变量上传到公共 CI Artifact。

## 3. 实验身份与验收约束

Freeze 前拒绝：

- 不支持的 Manifest 版本、未知 JSON 字段、错误 Arm / 模型 / 价目表版本；
- 不存在或重复的任务、隐藏在其他 JSON 里的不同任务、无 holdout 或 holdout 覆盖全样本；
- 空 `prompt`、空/真值恒成立的判题、初始状态已满足全部静态条件的无意义任务；
- 绝对路径、`..`、Windows 设备名、ADS、UNC 等跨平台危险相对路径；
- 预计矩阵 `arms × tasks × repeats × per_arm_tokens` 超出 Manifest Token 总上限；
- 候选策略不等于当前 Rust CLI 真正运行的 `adaptive_tournament_3`。

Evaluate 时拒绝：

- 报告 Executor 与 Lock 不一致，Suite Fingerprint 不一致，随机种子、重复次数或随机顺序不一致；
- 缺失/重复/额外任务结果、错误策略名称、错误 repetition；
- `resolved` 与独立门禁 `gate_status` 不符、Token 总量算术不符、预算越界却报告 `cap_ok`；
- regression-guard 失败次数不符或报告的 Arm 汇总与原始任务结果冲突；
- 可计价的单模型主 Agent 结果与运营者价目表重新计算的成本不同。

**原始输入哈希只证明字节一致，不证明身份真实性。** 不要把 Lock 文件和报告本身视作供应链签名，生产研究应把 Lock 的 SHA 写进不可回滚的版本记录或使用外部签名。

**Holdout 不可用于调参**。基线与候选配置必须在观察留出结果之前预注册。每次根据 holdout 调参，都需要另一个从未使用过的独立留出集。

## 4. 统计规则（不编造真实效果）

- `pass_rate` 为每个 Arm 的描述性成功比例；展示 Wilson 区间，但重复运行**不是**独立新任务。
- `(task_id, repetition)` 逐项配对；用 **task-cluster bootstrap** 重采样整个任务簇（保留该任务内所有重复），计算候选相对基线的 95% 区间。
- 没有独立留出、独立 holdout task 数少于 `min_clusters=5`、区间没有越过预注册的 `min_effect`、发生预算越界或执行未正常结束，绝不推荐晋升。
- 合成执行器的结果永远 `not_applicable_synthetic`。即使真实 Provider 在统计上胜出，当前 Bench 报告还不能证实评测器隔离和沙箱来源，因此仅返回 `needs_isolation_attestation` 而不是自动晋升。
- 定价是运营者根据自己给定费率的**非账单估算**。`usage_basis != provider_reported`、缺报模型、缺缓存分账、子代理或 Tournament 发生混合模型、无法从独立价目表校验时，价格为 **null（未知）**，不能显示为零；Cost per Success 只有所有对应 Arm 运行都被完整计价才有值。
- 存在确认问题：BenchArmResult 尚未逐请求序列化真实 Provider 成本/缓存写入及子模型身份，因此 Tournament 的成本通常会保持“未知”。不能靠重新填写一个汇总数字绕过。

## 5. 怎样真正开展真实模型比较

**本交付没有发起任何付费调用，也不提供自动调用 Provider 的命令。** 准备真实实验时，需要先完成以下工程合同：

1. 将 `--experiment` / 任务 Manifest 与**实际** Benchmark 策略参数接线；区分 Plan 与 Execute，明确拒绝未知参数。
2. 补齐 `--price-table` 到真实 ProviderBenchSettings，输出逐尝试的 reported model、输入/输出、cached read/write、retry、child run id；成本应按具体 attempt 计费。
3. 使用隔离的、模型不可写的外部裁判；独立验证执行时的沙箱能力和完成状态，fail-closed。对当前 Rust CLI 的 `ProviderBenchSettings.sandbox` 默认行为进行端到端测试。
4. 实现可在每个任务之前检查 token / 美元剩余额度的实验级停止条件；“声明 Token 上限”不等于“模型实际账单有严格上限”。
5. 冻结 50–100 个以上独立任务作为正式统计候选；少量任务只用来调试评测器。
6. 同模型、同 Provider、同推理参数、同权利/工作区/任务，允许交叉 Harness 对照；记录缓存、墙钟时间、每成功任务成本、安全/恢复。

以上完成且经所有者明确批准真实模型及成本上限后，再运行付费实验。`EXP-00` 当前只有任务叙述与验证命令，不包含完整、能独立运行的初始 BenchTask 文件与外部评分资产；**不要**把其 8 个任务和 48 个配置 Job 当作真实成功率样本。

## 6. 兼容及回滚

该补丁没有更改现有 Rust AgentLoop、Provider、执行权限、沙箱、WAL/Undo 或生产 Release Workflow；新增脚本和独立测试不会影响原有 `kroma bench` 默认行为。删除新增文件、回退本开发提交即可恢复 RC1 Hotfix 源码。RC1 历史发布清单保持原样，不会借新功能重新签名或冒充原始 RC1。

## 7. 运行中需要留意的问题

- Node 的跨语言 `serde_json` Fingerprint 对照已经在 Python 中独立算得一致，但**最终权威仍是新增 Rust Cargo 合同测试**。当前容器没有 Rust 工具链，尚未完成 `cargo fmt/clippy/test`。
- 如果升级 Rust BenchTask 结构/序列化字段，冻结输入的 Fingerprint 合同必须显式升级并增加迁移测试，不可静默改动旧 Lock。
- 使用真实 Provider 前，应将评测器及任务资产放到 Agent 不可写的位置；任何脚本/测试都不能以修改模型可写的测试文件来形成“独立”判决。
