# 能力清单与证据导航（T20）

这份文档是 `docs/capabilities.json` 的人读版本。**清单是机器可校验的**：`crates/kroma-core/tests/t20_capability_manifest.rs`
会把每一项的声明与仓库本身对照——引用的测试必须真的在树里、文档里提到的 CLI 参数必须真的被解析、
版本号必须与 `Cargo.toml` / `package.json` / `npm/kroma-cli/package.json` 一致。

## 五列含义

| 列 | 含义 | 不能含混的地方 |
| --- | --- | --- |
| `implemented` | 代码存在 | 存在 ≠ 生效 |
| `wired` | 真正调用它的入口（`cli` / `rpc` / `bench` / `worker` / `resume`） | 空表示「库级能力，尚未接线」 |
| `contract_tested` | 有具名回归测试，且测试在树里 | 本审查「拟新增」的测试不计入 |
| `live_tested` | 对真实对象跑过（真容器运行时、真命名空间），不是只有 fixture | 需要可复现的现场记录 |
| `default_on` | 不配置就生效 | 未实现或未接线的不许为 true（清单校验会拒） |

## 当前能力一览

| 能力 | 任务 | implemented | wired | contract_tested | live_tested | default_on |
| --- | --- | --- | --- | --- | --- | --- |
| 宿主掌握验证策略 | T01 | ✓ | rpc | ✓ | – | ✓ |
| 规范化锦标赛意图 | T02 | ✓ | cli, rpc | ✓ | – | ✓ |
| 宿主预算上限 | T03 | ✓ | cli, rpc, bench | ✓ | – | ✓ |
| 请求预算生命周期 | T04 | ✓ | cli, rpc | ✓ | – | ✓ |
| 类型化压缩身份 | T05 | ✓ | cli, rpc, resume | ✓ | – | ✓ |
| 有界 I/O | T06 | ✓ | cli, rpc, bench, worker | ✓ | – | ✓ |
| WAL 容量对齐 | T07 | ✓ | cli, rpc, resume | ✓ | – | ✓ |
| Windows 写者所有权 | T08 | ✓ | cli, rpc | ✓ | **–（无 Windows runner）** | ✓ |
| 执行边界（沙箱） | T09 | ✓ | cli | ✓ | ✓ bwrap | –（opt-in） |
| 容器生命周期 | T10 | ✓ | cli | ✓ | ✓ docker | –（opt-in） |
| Runner 装配 | T11 | ✓ | cli, bench | ✓ | – | ✓ |
| 工具发现→激活 | T12 | ✓ | cli, rpc | ✓ | – | –（opt-in） |
| MCP 握手与分页 | T13 | ✓ | cli, rpc | ✓ | – | ✓ |
| 缓存断点与账目 | T14 | ✓ | cli, rpc | ✓ | –（canary NOT_RUN） | ✓ |
| 分层证据存储 | T15 | ✓ | cli, rpc, resume | ✓ | – | ✓ |
| RepoGraph 预算 | T16 | ✓ | cli, rpc | ✓ | – | ✓ |
| 实验 manifest | T17 | ✓ | cli（dry-run） | ✓ | – | –（opt-in） |
| 请求级成本账目 | T18 | ✓ | **无（库级）** | ✓ | – | – |
| 运行身份与来源 | T18 | ✓ | cli, bench | ✓ | – | ✓ |
| AHE 隔离实验 | T19 | ✓ | cli | ✓ | – | ✓ |

## 措辞纠正（相对旧文档）

- **CAS 快照 vs Git worktree**：`SnapshotStore` 是内容寻址的字节快照，负责 `--undo`；
  Git worktree 是并发候选的**文件隔离**手段。两者都不是 OS 安全边界——worktree 拦不住命令读 `$HOME`。
- **宿主 shell vs 沙箱**：默认 `bash` 在宿主上以你的账户权限运行；`execution_sandbox` 是**显式选择**，
  且当前只覆盖 `bash`（验证器 / 探针 / hook 尚未接线）。后端不可用时**拒绝执行**，不会静默回退宿主。
- **合成测试 vs 实网基准**：`kroma bench` 默认 `--executor synthetic`，不接触 provider；
  实网基准与缓存 canary 都需要额度，本阶段记为 **NOT_RUN**，没有用模拟比例冒充真实命中。
- **预测成立 vs 自动自愈**：AHE 把 `prediction_confirmed` / `experiment_valid` / `improvement_accepted` /
  `promotion_approved` 四级分开；「预测这个任务仍然失败」是成立的预测、不是修复；提升恒为人工决定。

## 证据导航

| 想要什么 | 去哪看 |
| --- | --- |
| 逐任务验收（做了什么、怎么验证、变异验证） | `docs/review/phase1_test_results/T??_acceptance.md` |
| 每条门禁命令与退出码 | `docs/review/phase1_test_results/commands.json` |
| 全量测试原始日志 | `docs/review/phase1_test_results/logs/test_t??.log` |
| 已知既有 flake（含复现条件） | `docs/review/phase1_test_results/regressions.md` |
| 环境与工具链 | `docs/review/phase1_test_results/environment.json` |
| 基线（未打补丁）对照 | `docs/review/phase1_test_results/T00_baseline.md` |

## 历史 Gate 报告：只作历史，不作当前证据

`docs/GATE*_CI_LOG.txt`、`docs/GATE2_EVAL_METRICS.json`、`docs/P0_*`、`docs/NEXT_GEN_BLUEPRINT_v1.0.md`
都是 **v1.5.0 之前的评审产物**，描述的是当时那棵树。它们不能用来满足当前发布门禁：
`CapabilityManifest::evidence_is_current(report_version, current_version)` 只在版本完全一致时返回 true，
而本清单声明的是 `1.5.0`（含 T00–T20 的改动）。

归档规则：旧报告保留原样、不改数字、不重写结论；新增证据一律进
`docs/review/phase1_test_results/`，并带 `commands.json` 里的命令与退出码。

## 版本一致性

`package_version` 必须同时等于：

- `Cargo.toml` 的 `[workspace.package].version`
- `package.json` 的 `version`
- `npm/kroma-cli/package.json` 的 `version`（存在时）

这条由 `package_versions_and_release_manifest_agree` 校验，改版本时三处必须一起改。

## 许可证

**已选定：MIT。** 条款见根目录 `LICENSE`（Copyright (c) 2026 Kroma contributors），与
`Cargo.toml` 的 `license` 字段一致。依赖与许可证清单见 `docs/nextgen/dependency_licenses.json`。

> **superseded（2026-10-21，KR-A06）**：本节此前写着「许可证文件与条款**不由本审查代为选择**：
> 需要项目所有者确认后再补」。`LICENSE` 落地后该表述已过时。原文保留于此作为审计轨迹，
> 不再代表当前状态；不删除历史结论是为了让漂移本身可追溯。
