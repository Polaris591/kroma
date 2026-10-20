# Kroma v4 源码直接修复报告 — 2026-10-19

> 本文件记录两轮修复。下面第一节是**第二轮**（独立深度 review 的 R01–R16，2026-10-19 修复候选版）；
> 其后「基线与交付」起的内容是**第一轮**（KRV4-*/NEW-* 项）的原始记录，保留原样，其中“未运行”的
> 表述只描述第一轮当时的执行环境。

## 第二轮：独立 review R01–R16 修复（修复候选版，未做跨平台验收）

输入是独立 review 报告的 16 项（P1 11 项、P2 5 项）。以下按 review 的编号给出修复与**本机实际执行**的验证；
没有运行过的项目在“未验证”里逐条列出，不把静态检查或 Linux 结果当作跨平台结论。

| 编号 | 修复要点 | 本机验证（Linux x86_64 / Rust 1.85.1 / bwrap 0.12.0） |
|---|---|---|
| R01 | `ExecutionPolicy` 贯穿 `KromaRuntimeConfig` → 会话工具运行时 → 子 worker / 修复赛 / durable 恢复；缺策略的恢复 fail-closed | 真实 `RpcDispatcher`：prompt→spawn_subagent、tournament 候选、tasks.control 恢复三路都无宿主外写入且 backend=bwrap；host 对照确实写入 |
| R02 | bench 的 agent、开发门禁、held-out verifier 与子运行共用同一执行策略；不可用后端开跑前拒绝 | release 真二进制 + 本机 mock：bwrap 下 agent/verifier 都无宿主外 marker，host 对照两者都写入；`--executor provider` 不可用后端 exit 2、0 请求 |
| R03 | codemode 不再把外部 ID 当路径；脚本经 stdin 传入、不落宿主文件 | review 探针：`../../outside` 不再产生宿主 `outside.py`；bash 溢出文件名同样安全化 |
| R04 | 区分 trusted owner ceiling 与 workspace preference，保留来源；workspace 不能铸造预算 | 真二进制：仅 workspace `max_tokens` 时 exit 2、0 请求；纯合并探针 `OWNER_ADMISSION=Err(NoOwnerEnvelope)` |
| R05 | 实验整体上限 = 每臂 × 计划臂数；每臂独立 broker 账本 | 真二进制两臂都派发；`experiment_envelope(30000,2,None)=60000`，声明上限不足时拒绝 |
| R06 | 过滤后的 registry 交给真实运行时；记录实际暴露工具 | 探针：声明 `read` 时实际请求工具仅 `["read"]`，被排除的 `write` 未执行 |
| R07 | `BrokeredProvider` 透明转发 `message_semantics` | 内层与包装后语义相同；Anthropic 缓存断点经 broker 仍可达 |
| R08 | 执行器持有不可被 future drop 绕过的进程组所有权；父等待清理契约 | drop future 后无活进程；子硬超时后父返回且子组无活进程（探针 `ALIVE=false`） |
| R09 | 任一侧 `verdict_unknown` 的配对不参与效果估计，判定为 inconclusive | 全 unknown 基线 + 真实 candidate：`decision=inconclusive`，不再 accept |
| R10 | 只运行 `profile.test=true` 的 harness，按精确 target 名排除沙箱套件 | 93 harness / 1174 passed / 0 failed；从不运行 `kroma` 或 `paired_smoke` |
| R11 | TLS smoke 自包含小额预算并隔离用户配置 | 干净 HOME 与投毒 `~/.kroma/config.toml`+`KROMA_MAX_TOKENS=1` 两次都 PASS |
| R12 | codemode 使用 guest 视图运行，正常模式在隔离下可用 | bwrap 下 `print(read('data.txt'))` 成功返回 workspace-data |
| R13 | `run_bounded` 正常退出也扫同组残留 | 关闭 stdout/stderr 的后台孙进程不再存活 |
| R14 | IPv6 唯一本地/链路本地与 IPv4-mapped 统一按私网拒绝 | 三个精确 allowlist 端点 `authorize=Err(PrivateAddress)` |
| R15 | codemode 严格解析参数并传递 `timeout_sec` | 声明 1s 实测 1003ms、结果 `Error{ToolTimeout}` |
| R16 | 保留可信安全下限，拆分结构校验与产品基线校验；最终树 fmt 干净 | `cargo fmt --check` 干净；t20 8/8；workspace 全量 0 failed |

**本轮本机实际运行的门禁**（全部基于本仓库最终字节，非旧缓存日志）：
`cargo fmt --all -- --check` 通过；`cargo check --workspace` 通过；`cargo clippy --workspace --all-targets -- -D warnings` 通过（0 warning）；
`scripts/run-workspace-tests.sh` 93 harness/1174 passed/0 failed；`scripts/run-sandbox-tests.sh` 5 套件 46 passed/0 failed（真实 bwrap）；
`cargo build --release -p kroma-cli` 通过（`kroma 1.5.0`）；`scripts/tls-smoke.sh` 干净与投毒 HOME 均 PASS；
`node --test` 114 pass/2 skip/0 fail；`npm run test:bench` 101 pass/2 skip/0 fail；6 个 Python gate、`verify-prereg.py`、`release-gate.js actions/lock/openssl/dag` 均 PASS；
review 的仓外 Rust 探针与真 CLI bench 探针在 `evidence/integration-probes/` 中改为显式断言后全部通过。

**未验证（不得据此声称）**：Windows / macOS / Android / musl 交叉构建与原生运行；GitHub Actions 实跑（只做了本地静态与 DAG 检查）；
真实付费 API 或生产密钥；container 后端（本机无可用镜像）；DNS 解析到私网的 connect 层校验（仅 URL 层门禁）；
隔离运行时位于工作区之外的 verifier 脚本无法被 guest 读取，会 fail-closed 记为失败而不是回退宿主。


## 基线与交付

- 输入：`kroma-v4-source (1).tar.gz`（包内 Kroma 1.5.0）
- 输出：`Kroma_v4_Direct_Fixed_2026-10-19.tar.gz`、`Kroma_v4_Direct_Fixes_2026-10-19.patch`
- 方式：直接修改完整源码、补充 Node/Rust/Python 回归与发布门禁，未修改生产签名根或 Cargo.lock。
- **交付状态：修复候选版；不是已跨平台验收、可签名的生产版。** Rust 编译与真实隔离在当前环境未运行。

## 完成的代码变更

| 复验项 | 具体修改 | 验证状态 |
|---|---|---|
| KRV4-04 Node 下载器 | `bin/kroma.js`、npm 镜像改用 `stream/promises.pipeline` 与 Transform 增量 SHA256，完整响应 size/hash 校验、慢 sink 背压、ENOSPC/EACCES/断流错误传播；`wx` 防目标覆盖，失败/延迟打开时清理 | **已运行** 7 个 adversarial 子测试 + 109 原升级检查 |
| KRV4-06 本地路径 | 接受本地绝对文件路径内合法空格，拒绝控制字符，安全读取 stat 异常；HTTPS 主机白名单不变 | **已运行** |
| NEW-01 手动发布失败 | `validate-dispatch` 运行脚本前固定版本 `actions/checkout`；在所有 tag 的 `quality` 加 Python 门禁；第二次手动签名继续走 `validate-dispatch` | **工作流静态 + 负例变异测试已运行**；未调用 GitHub Actions |
| KRV4-05/07 门禁 | 预注册 Python gate 同时进入 CI、tag quality、manual input validation；CI `sandbox-isolation` 不再 `continue-on-error`，缺 bwrap 是阻断，不是绿灯；强制 Node 故障测试 | **源码静态与相关 Python tests 已运行** |
| KRV4-02/NEW-02 | `sandbox.rs` 改为 fail-closed `try_from_std_child`，Windows Job attach 失败中止，超时强制 kill leader，bounded reap；`process_group.rs` std attach-failure reap 不再无界 wait | **源码修复 + 新 Rust 回归（未运行）** |
| NEW-03/截断结果 | stdout/stderr 共享一次绝对 drain deadline；容器 create/start/inspect/remove/probe 不再将截断当成功 | **源码修复 + 新 Rust 回归（未运行）** |
| KRV4-01 大工作区容量 | 保持 manifest 读写一致上限，从 1 MiB 调整到 8 MiB；10,000 文件读回测试（原 6,000），保留超大 manifest 拒写测试 | **源码修复 + Rust 回归（未运行）** |
| NEW-04 资源限额 | codemode `ulimit -t` 和 `ulimit -v` 从忽略失败改为 `&&`，资源约束建立失败时不会执行 Python | **源码修复（未原生测试）**；不等价于完整 Windows/macOS 内核资源隔离 |
| NEW-05 安全能力名单 | Rust 代码内固定 11 个必需安全能力 ID；双重删除两份 JSON 列表也失败；增加变异回归 | **源码修复 + Rust 回归（未运行）** |
| KRV4-08 供应链 | Zig pip 依赖锁定 `ziglang==0.13.0`，新增仓库与 npm MIT `LICENSE` | **静态检查已运行**；Zig 的实际构建兼容性待 CI 验证 |
| KRV4-09 探测工具缺失 | `scripts/goal-interface.py` 中 rustc/git/curl 缺失、超时、权限不足转为结构化 `BLOCKED` (exit 2) | **Python 语法已运行**；实际目标运行须在真实构建环境复验 |

**新增测试**：`scripts/test-updater-faults.js`、`scripts/test-workflow-contract.py`；Rust 测试在 `krv401_manifest_ceiling.rs`、`krv402_bounded_drain.rs`、`c0012_perf_gate.rs` 扩充。发布 workflow 和主 CI 都调用这些可运行的 Node/Python 回归。

## 本机已执行的验收

| 检查 | 实际结果 |
|---|---|
| `node --test` | 116 tests / **115 pass / 1 skip / 0 fail** |
| `node scripts/test-kroma-js-upgrade.js` | **109 checks pass** |
| `node --test scripts/test-updater-faults.js` | **8 tests pass**（一个父 test、7 个子测试） |
| `node scripts/test-release-trust-root.js` | **13 checks pass** |
| `node scripts/test-release-pipeline.js` | **24 checks pass** |
| `node scripts/test-provenance.js` | **9 checks pass** |
| `node scripts/test-release-attestation.js` | **24 checks pass** |
| `npm run test:bench` | 103 tests / **102 pass / 1 skip / 0 fail** |
| `python3 -m compileall -q scripts` | PASS |
| 4 个 Python test 脚本 | PASS（`test-trusted-gate.py` 4/4） |
| `python3 scripts/verify-prereg.py` | 结构检查 exit 0；实际外部 oracle 仍有环境缺口，**不可写 live PASS** |
| `python3 scripts/test-workflow-contract.py` | PASS，包含破坏 checkout/gate/sandbox 的负例变异 |
| `node scripts/release-gate.js actions/lock/openssl/dag` | 均 PASS |
| `cmp bin/kroma.js npm/kroma-cli/bin/kroma.js` | PASS |
| Rust 1.85.1 `cargo fmt/clippy/test` | **未运行**：执行环境无 Rust 工具链，网络 DNS 不可用 |
| GitHub Actions / Windows / macOS / bubblewrap 实跑 | **未运行** |

## 尚未解决的风险与后续严格门禁

1. **KRV4-03 性能**：快照仍在部分 bash/变更后全量递归扫描并哈希。8 MiB 只是扩大可接受规模，不是增量扫描或分片索引；禁止声称 P2 性能完全修复。应量测 P50/P95 文件扫描时间与 I/O，设计具有正确性证明的增量机制。
2. **快照容量仍有极限**：8 MiB 上限保留防非受控内存分配，超长路径的大仓库会显式拒绝记录。产品级大型仓库支持需要分片 manifest 或索引设计，不能任意取消限额。
3. **Windows Job attach 时窗**：`spawn` 到 Job attach 间存在原项目已经记录的继承/逃逸窗口；要彻底解决需 suspended spawn + attach before resume。当前修复保障 attach 失败 fail-closed，但不声称完全解决所有绕过。
4. **排空线程**：共享 drain deadline 保证调用返回时间受限（加显式 grace），但外部、未受控进程永远持有 pipe 时 std drain 线程仍可能等待系统 EOF；需要更统一的可取消 I/O 监督器。
5. **codemode 执行边界**：本次保证 `ulimit` 设置失败不会继续执行，但 Python 对宿主文件系统的实际访问控制需要单独的强 OS 沙箱、Windows/macOS 原生资源限制测试；不能把 PRELUDE 的路径约束误认为操作系统隔离。
6. **真实交付门禁**：安装 Rust 1.85.1 后至少运行：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`scripts/run-workspace-tests.sh`、`scripts/run-sandbox-tests.sh`；Windows/macOS 上验证 Job Object/资源约束，另执行 CI/Release 的两阶段候选验收。未完成，不得把本版本写为生产 GO。
7. **Zig 版本**：已固定为 0.13.0，必须在真实 musl/OpenSSL CI 构建成功后才可签发。

## 推荐下一步

先在 Rust/真实 CI 环境跑以上所有硬门禁，修复实际编译/fmt/clippy 反馈；然后推进有测量数据支持的增量快照（KRV4-03），再执行 20–30 个隔离自举开发任务。无需先做新的模块大重构。
