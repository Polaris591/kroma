# 逐任务台账（T00–T30）

状态含义：**PASS** = 有真实可复现的门禁证据（测试/构建/变异验证）并通过；**BLOCKED** = 缺外部依赖，
已按要求记录缺失项后继续；**PARTIAL** = 可本地验证部分通过，剩余部分 BLOCKED。

| # | 任务 | 状态 | 关键证据（可复现） | 提交 |
| --- | --- | --- | --- | --- |
| T00 | 基线冻结 | PASS | 交付树修复 E0004 + 2 条 clippy；基线 `/opt/kroma-orig` 重建；patched 820 pass / baseline 817 pass；`T00_baseline.md` + `commands.json` | 7c8ea1b, 7973e36, 772e999 |
| T01 | 宿主掌握验证策略 | PASS | `gate8` 的 `t01_rpc_tournament_follows_the_host_verification_policy`；6 案例矩阵；变异验证 RPC 不再硬编码 | e255200 |
| T02 | 规范化锦标赛意图 | PASS | `t02_tournament_intent`（5 条，digest 稳定 + 一次性授权不可重放） | adcde0b |
| T03 | 宿主预算与窗口 | PASS | `t03_context_budget`（12 条）；窗口检查与压力信号双变异验证 | 247cb39 |
| T04 | 请求/重试预算生命周期 | PASS | `t04_budget_lifecycle`（8 条）；per-request 上限变异验证 | fdb7502 |
| T05 | 压缩身份迁出正文 | PASS | `t05_compaction_identity`（6 条）+ gate2 18 条；`is_host_stub_header` 变异验证 | 0e61ee7 |
| T06 | 安全打开与有界读取 | PASS | `t06_bounded_io`（7 条）+ `secure_fs` 21 条；O_NOFOLLOW / O_NONBLOCK 双变异 | 4573c61 |
| T07 | WAL 写入/恢复限制对齐 | PASS | `session::tests` 21 条（含 limit±1 对偶、ENOSPC 回滚、撕裂尾部、checkpoint/overlay 同预算） | 57161fc |
| T08 | Windows 写者所有权 | PASS（runtime BLOCKED） | `session_ownership` 8 条 + `cargo build --target x86_64-pc-windows-gnu` 链接成功；三个 windows 用例已编译但**无 Windows runner 未执行** | 664f544 |
| T09 | 执行环境与沙箱贯通 | PASS | `t09_execution_boundary`（8 条，真 bwrap）；静默回退变异验证；`execution_sandbox` 配置接线 | 72f9ad5 |
| T10 | 探测与容器取消生命周期 | PASS | `t10_container_lifecycle`（9 条，3 条真 docker）；心跳停止 + 名字清理 + 只读 oracle 挂载 | 54924d1 |
| T11 | Runner 装配与机制接线 | PASS | `t11_assembly`（5 条，五门 digest 相等 + 依赖闸门 + ledger 语义） | e7ac228 |
| T12 | 工具发现到调用闭环 | PASS | `t12_tool_discovery_loop`（4 条，严格 provider 4 轮 round-trip）；变异验证 | bbc609d |
| T13 | MCP 握手与目录分页 | PASS | `t13_mcp_handshake`（6 条）+ `mcp_config` 9 条；分页变异验证 | ccf9f3d |
| T14 | 缓存断点与账目 | PASS（实网 canary NOT_RUN） | `t14_cache_accounting`（4 条）+ `context_cache_stability` 4 条；断点前进变异验证 | 1f922e5 |
| T15 | 分层证据存储 | PASS | `t15_evidence_store`（5 条，9 MiB 淘汰后恢复）；淘汰写入变异验证 | fcc48eb |
| T16 | RepoGraph 预算 | PASS | `t16_repo_graph_budget`（3 条，热/冷同cap）；测试空转已修正后变异成立 | 3b1cc4d |
| T17 | 冻结实验配置与留出集 | PASS | `t17_experiment_manifest`（10 条）；空 holdout 变异验证；CLI `--experiment` dry-run | 9af3fec |
| T18 | 请求级成本与运行身份 | PASS | `t18_usage_cost`（5 条）+ `t18_provenance_identity`（7 条，含 wire request identity）；可执行位变异 | 7469578 |
| T19 | AHE 不可自改裁判 | PASS | `t19_ahe_isolation`（9 条，源树指纹前后一致）；基线原地跑变异验证 | 5ac80e1 |
| T20 | 能力文档与验证索引 | PASS | `t20_capability_manifest`（7 条，清单 vs 仓库证据/CLI 参数/版本一致） | 3b9384e |
| T21 | 真实发布信任根 | **PASS** | 生产密钥已在所有者指示下生成并装入两处入口（`kroma-release-2026`）；真签名 manifest 经生产根验证通过、篡改/冒充/测试密钥三向被拒；`test-release-trust-root` 13/13、`test-kroma-js-upgrade` 109/109 | 4ee0cd2 + 本轮 |
| T22 | 发布与供应链闭环 | **PARTIAL** | 21 项检查 + 38 处动作钉住 + `release-gate.js lock` + 真签名 manifest 经生产根验证（payload 篡改被拒）；缺口：需 CI runner 跑一次完整 release 工作流，产出 `artifacts[]` 形态的签名 manifest（本机磁盘不足以做 4 平台构建，不用单平台冒充） | 2411bd8 + 后续 |
| T23 | 快照身份与恢复不变量 | PASS | `t23_snapshot_identity`（5 条实测 A→B→A / 崩溃重放 / rename+chmod / 多次 undo）+ ADR-0001 | 0479d71 |
| T24 | 真实模型冻结基线 | **PASS（结论 Inconclusive）** | 所有者授权后真跑 28 个 run / 8.77M token（≈$1.45）：留出集 3 次重复 = 单代理 5/6 vs 锦标赛 6/6，配对差 +16.7% CI [0%,+33.3%]，按预注册规则判 Inconclusive，默认值未改；另修掉 3 个"拿实现细节当验收"的判定缺陷 | b10d23e + 本轮 |
| T25 | TaskState 与语义压缩 | PASS（EXP-01/02 BLOCKED） | `t25_task_state`（4 条，强制压缩 + WAL resume 后约束逐字不变） | b5c17f2 |
| T26 | 代码定位模型适配 | PARTIAL（EXP-03 BLOCKED） | `t26_localization`（2 条：5/5 vs 0/5 符号基线，变异 3/5）+ 语言矩阵 | 42e12df |
| T27 | 自适应执行与反事实验证 | PASS（EXP-04 BLOCKED） | `control_policy` 8 条 + `t27_control_policy` 3 条（探针只在副本、父工作区字节不变） | 1fd23e4 |
| T28 | 长任务与可撤销经验 | PASS（EXP-05 BLOCKED） | `handoff` 5 条（撤销下次生效、四类失效、交接工件无 transcript） | bfe07b8 |
| T29 | 受约束的 Harness 演化 | PASS（EXP-06 BLOCKED） | `t29_harness_evolution` 5 条（冻结验收标准、单任务不晋升、负面轮次保留） | 0d6ee5e |
| T30 | 封板与可追溯交付 | PASS | 见 `T30_sealing.md`：全量/clippy/fmt 结果、补丁与 tar、复现命令 | c85db07 + 封板提交 |

## BLOCKED 清单（缺失依赖）

| 项 | 缺什么 | 已交付什么 |
| --- | --- | --- |




| T25/T26/T27/T28/T29 的实验 | 同 T24 的预算 | 机制与离线指标全部就绪并有测试；默认开关保持不变 |

## 既有 flake（与本次改动无关，已在 `regressions.md` 记录）

`b05_execution_context::run_deadline_clamps_mcp_timeout`（§2.1）、
`b08c_git_metadata`（§2.2，需 `--test-threads=1`）、
`mcp::call_lifetime_tests::subsequent_call_fails_closed_when_poisoned`（§2.4）、
`session_ownership::tests::different_workspaces...`（§2.5）、
`b05 agent_runner_real_toolchain_clamps_run_deadline`（§2.1b，负载敏感）。
