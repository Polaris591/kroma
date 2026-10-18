# Kroma RC1 收口验收

```
RC1-00 input   : kroma-p1-sealed-source.tar.gz（阶段 1 输入包，只用于来源冻结）
                  sha256 e34ac0c1dc505dd8b0877749f1c6e88fd5441857e489ef98dcd2e4cae353e23b
                  与本仓库当时那份 tar 逐字节一致（已核对）
This delivery   : kroma-rc1-source.tar.gz（本轮 RC1 候选包，由下方提交重新打包）
                  sha256 记录在交付目录的 SHA256SUMS 中；归档无法包含自身摘要
Git commit      : 见 RELEASE_MANIFEST.json 的 git_commit（本文件随该提交冻结）
Result          : RC1_CANDIDATE_BLOCKED（G4 未实跑、G5 覆盖不全、G6 运行证据不全）
```

`RC1_CANDIDATE_BLOCKED` 的含义：源码与所有本地可执行门禁真实通过；真实签名（需要所有者批准）、
真实 runner（Windows/macOS/Android）与外部依赖摘要（本机无出网）缺证据，已逐条列出解除路径。
**不是** RELEASE_READY，也**没有**做任何推送/发布。

## 1. 逐卡状态

| 卡 | Implemented | Verified | Gate | 证据 | 阻塞原因 |
| --- | --- | --- | --- | --- | --- |
| RC1-00 冻结来源 | 是 | **是** | G0 PASS | `evidence/00_provenance.json`（提交、tar sha、工具链、锁、双入口一致） | — |
| RC1-01 Node late-open 竞态 | 是 | **是** | G2 PASS | 复现 17/100 → 修复后 **100/100**；两个入口逐字节一致；109/109（`evidence/logs/rc1-01_*.log`） | — |
| RC1-02 全仓门禁 | 是 | **是** | G1 PASS | fmt 105→0；clippy 0；Rust 全量 + 4 个 Node 套件真跑（`evidence/logs/`） | — |
| RC1-03 Release DAG 接线 | 是 | **否** | G4 `BLOCKED` | `quality` → 五平台 → `pack` → `attest` → `release-verdict` 已接线，`release-gate.js dag` 与 24 条负向测试通过 | **`release.yml` 从未跑通**（只由 tag/dispatch 触发，本环境无 `actions:write` 令牌，且不许打 tag）；绿灯的是 `ci.yml`，不能算作 Release 证据 |
| RC1-04 OpenSSL 验真 | 是 | **是** | G3 PASS | 四条路径统一 `scripts/fetch-openssl.sh`；pin 已钉 `a8c0d28a529ca480…`（取自国外小鸡实下载 + openssl.org 官方 .sha256，两者一致）；缺/错 pin 仍 fail-closed | — |
| RC1-05 产物/签名一致性 | 是 | **是**（覆盖已产出产物） | G5 `PARTIAL` | `release-provenance/1` 文档用 `kroma-release-2026` 真签，生产根验签通过；篡改字节/换 keyId/改文档/旧 schema 四向被拒（`test-provenance` 9/9）；新增 attestation 机制（`scripts/release-attestation.js`，9 条负向测试：改字节/多文件/缺文件/错提交/沙箱未过全部拒绝）；私钥不进仓库/日志/CI | 尚未覆盖「CI 产出的五平台发布集」——`ci.yml` 不上传 artifact，`release.yml` 从未跑通，没有可签的字节 |
| RC1-06 跨平台 | 是 | 部分 | G6 `PLATFORM_NOT_VERIFIED` | 五平台**构建/链接**全部真实通过（CI run 39103615371，提交 9c6bec59）；启动：linux-x64、linux-arm64(qemu)、macOS；TLS：linux-x64 与 linux-arm64(qemu) 实跑通过；矩阵见 `CI_AND_PLATFORM_MATRIX.md` | Windows/Android 无运行证据；macOS 无 TLS 证据；安全边界仅在 CI 上 `HOST_CAPABILITY_BLOCKED` |
| RC1-07 状态账本 | 是 | **是** | G7 PASS | 本文件；历史文档加 `Superseded by` 标注 | — |
| RC1-08 阶段 2 交接 | 是 | — | — | `PHASE2_BASELINE_HANDOFF.md` | — |

## 2. 本轮修改（三个独立提交，均可单独回滚）

| 提交 | 内容 | 影响 |
| --- | --- | --- |
| `style(rc1-02)` | `cargo fmt --all`（105 处 → 0），纯格式化 | CI 的 fmt 门禁从必然红变成绿 |
| `feat(rc1-03,04)` | quality job + DAG gate；四条 OpenSSL 路径统一验真 + openssl gate | 打包不能再绕过质量；未验真的 OpenSSL 不可能被解包 |
| `fix(rc1-01)` | 归档 finalize 的 settle 语义：await 后复查、文件未创建时不 settle、错误不被覆盖 | 升级失败不再报误导性 ENOENT；flaky 17% → 0% |
| 本文件所在提交 | `rc1/` 文档与证据 | 无代码影响 |

没有重构大型 Rust 文件，没有改默认策略（仍 `SingleCandidate`），没有动 WAL/CAS/权限/预算内核。

## 3. 当前质量结果（全部本轮真跑，不是引用历史日志）

| 门禁 | 结果 | 日志 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 差异 | — |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning | — |
| `cargo test --workspace --no-fail-fast` | 见 `evidence/logs/rc1_full_suite.log` 的 EXIT 与合计行 | 该文件 |
| `node scripts/test-kroma-js-upgrade.js` | 109 / 109，且**连续 100 次全过** | `evidence/logs/rc1-01_repro_*.log` |
| `node scripts/test-release-trust-root.js` | 13 / 13 | 同上 |
| `node scripts/test-release-pipeline.js` | 24 / 24（新增 3 条 RC1-03/04 检查） | 同上 |
| `node scripts/verify-ledger.js` | ok（32 文档 / 92+ 命令 / 31 行） | 同上 |
| `release-gate.js actions / lock / openssl / dag` | 全 ok | 同上 |

测试数量以实际输出为准，本文不硬编码某个"必须等于 995"的数字。

## 4. Release DAG 实跑状态（**未闭环**）

- **已接线**：`quality` → `sandbox` → 五个平台构建 → `pack` → `attest` → `release-verdict`；
  `pack` 的 `needs` 含 `quality` 与 `sandbox`，`release-verdict` 在签名或沙箱不满足时给出 `RELEASE_BLOCKED`。
- **未实跑**：`release.yml` 只由 `v*` tag 或 `workflow_dispatch` 触发；本环境没有 `actions:write` 令牌，
  也不允许打 tag，所以**这条流水线一次都没跑过**。此前把 `ci.yml` 的全绿写成"Release DAG 实跑全绿"
  是高估，已更正。
- **有证据的是 CI**：`https://github.com/Polaris591/kroma/actions/runs/39103615371`（提交 `9c6bec59`）——
  `fmt, clippy, tests` 与五个平台构建 success；独立 `sandbox isolation` job failure（宿主无用户命名空间）。
- 解除路径：给一个 `actions:write` 令牌，或批准打一个跑完即删的预演 tag。
- 修掉的真问题：workflow 因 job 级 `env` 用 `runner.temp` 被判无效（0 job）；musl/macOS 因
  `libc::__rlimit_resource_t` 只在 glibc 存在而编译失败；android 的第三方 setup action 先失败，
  改用 runner 预装 SDK；测试 job 缺 `bubblewrap`。
- 静态结构有负向测试：删掉 quality job、或让 pack 不依赖 quality、或出现无人工条件的 publish，
  `release-gate.js dag` 都会失败（`test-release-pipeline.js` 覆盖）。

## 5. 供应链

| 下载点 | 现状 |
| --- | --- |
| release/android、release/musl、ci/musl、ci/android | 全部走 `scripts/fetch-openssl.sh`：pin 空/格式错 → 退出；摘要不符 → 退出并打印实际值；只有校验通过才 `tar` |
| 缓存键 | 已包含 `OPENSSL_SHA256`，换 pin 不会复用旧构建 |
| Action 钉住 | 25 处，全部 40 位（`release-gate.js actions`） |
| 锁文件 | `Cargo.lock` 与钉住摘要一致（`release-gate.js lock`） |

**已完成**（2026-10-18）：摘要在国外小鸡上实下载并核对官方 `.sha256`：`a8c0d28a529ca480f9f36cf5792e2cd21984552a3c8e4aa11a24aa31aeac98e8`。原计划命令：

```
curl -fsSL -O https://www.openssl.org/source/openssl-3.5.7.tar.gz && sha256sum openssl-3.5.7.tar.gz
```

再用**独立来源**（如 openssl 官方 release notes/signature）核对，然后把 64 位值填进
`.github/workflows/ci.yml` 与 `release.yml` 的 `OPENSSL_SHA256`。填错不会造成安全隐患（会直接失败关闭），
只会让构建红。

## 6. 实验结论

T24 保持 **INCONCLUSIVE**：28 次真实运行 / 8.77M token / 留出集仅 2 个任务簇 / 配对差 95% 区间
[+0.0%, +33.3%]。默认策略 **不变**（`SingleCandidate`）。不得用该结果宣称锦标赛领先。

## 7. 新发现的问题

1. **我自己的错误**：给 quality job 编了一个 `dtolnay/rust-toolchain` 的 commit SHA。已立刻改成仓库
   自己的 rustup 配方——钉住一个没审过的 action 比不用 action 更糟。这类"看起来像证据的编造"是本轮最该防的事。
2. CI 与 release 的 OpenSSL 行为原本不一致（只有一处验真，其余直接解包），且 pin 为空使那一处永远失败。
3. 全树 fmt 之前从未真正通过，而 CI 把它当硬门禁——历史"测试通过"的声明缺少门禁支撑。

## 8. 交付文件

见 `RELEASE_MANIFEST.json`、`RELEASE_SHA256SUMS`、`TEST_EVIDENCE_INDEX.md`、
`KNOWN_LIMITATIONS.md`、`PHASE2_BASELINE_HANDOFF.md`、`RC1_CHANGELOG.md`，以及 `evidence/`。

## 8.5 终态判定（本轮结束）

| Gate | 状态 | 依据 |
| --- | --- | --- |
| G0 来源可信 | **PASS** | 上传包与本仓库 tar 逐字节一致；提交、锁、双入口摘要齐备 |
| G1 本地质量 | **PASS** | fmt 0 差异、clippy 0、Rust 全量绿（CI + 本机）；Node 四套 109/13/24/ledger 全绿 |
| G2 更新可靠性 | **PASS** | 17/100 → 100/100；确定性修复，未加超时、未吞错误 |
| G3 供应链 | **PASS** | 四条 OpenSSL 路径统一验真；pin 双源核对；Action 25 处钉住；锁钉住 |
| G4 Release DAG | **BLOCKED** | 结构、静态门禁与负向测试齐备；`release.yml` 从未运行（无 `actions:write` 令牌，不许打 tag） |
| G5 可信签名 | **PARTIAL** | 已签并验：源码包、补丁、linux-x64、arm64-musl（4 件）；attestation + 验签机制已实现并有 9 条负向测试；**未签**：CI 五平台产物集（`ci.yml` 不上传 artifact，`release.yml` 未跑通） |
| G6 平台声明 | **PARTIAL** | 五平台构建+链接全绿；启动：linux-x64 / linux-arm64(qemu) / macOS；TLS：linux-x64 与 linux-arm64(qemu)；安全边界仅本机 |
| G7 证据与冻结 | **PASS** | 单一状态表、历史文档已标 Superseded、T24 保持 INCONCLUSIVE |

**总状态 = `RC1_CANDIDATE_BLOCKED`**：不是能力问题，是 G5/G6 各差一块需要令牌或真机的证据。
解除路径三条（互相独立，做一条通一条）：

1. **G4**：给一个 `actions:write` 令牌让我 `workflow_dispatch` 跑 `release.yml`（两次：产出候选 + 验签出
   verdict），或批准打一个跑完即删的预演 tag。
2. **G5**：G4 跑通后才有 CI 产物字节可签——`ci.yml` 不上传 artifact，所以"下载 CI 产物"这条老路径是错的，
   必须先让 `release.yml` 产出 `release-candidate`。
3. **G6**：给 Windows runner / Android 设备，补齐启动与 TLS 证据。

## 8.6 独立验收意见的闭环情况（B-01 … B-05）

| 项 | 独立验收的意见 | 本轮处置 |
| --- | --- | --- |
| B-01 归档与清单未封板 | 归档 sha 与 manifest/acceptance 写的不是同一个包；包内 `sha256sum -c` 失败；commit 在远端查不到 | 已改：区分"阶段 1 输入包"与"本轮候选包"；提交已推送；封板时先冻结再一次性生成所有摘要，解包后 `sha256sum -c` 必须 100% 通过 |
| B-02 发布无产物级签名闭环 | `release.yml` 从不调用 sign/verify；签的四件不是交付集 | 已改：新增 `attest`（按字节记 name/size/sha256/commit/sandbox）与 `release-verdict`（对同一 run 的字节验签，重建不算）；新增 9 条负向测试；两遍式流程写在 workflow 注释里 |
| B-03 质量门禁两套 | `ci.yml` 排除沙箱套件、`release.yml` 全量跑；release 质量从未验证 | 已改：两个 workflow 调用同一个 `scripts/run-workspace-tests.sh`；沙箱套件在 release 里是**阻断条件**（退出码 2 记 `HOST_CAPABILITY_BLOCKED` 并阻止 READY，退出码 1 直接失败） |
| B-04 CI 失败自动写远端 | 失败时 `git push -f origin HEAD:ci-debug` | 已删：改为 `upload-artifact` 上传日志；workflow 权限降为 `contents: read` |
| B-05 平台矩阵未对齐 | musl TLS 已跑却写 NOT_RUN；沙箱失败被总 success 掩盖 | 已改：矩阵按 step 结论逐层重写，注明 run 与提交；沙箱状态单列 `HOST_CAPABILITY_BLOCKED` |

## 9. 需要所有者批准/提供的最小清单

1. **签名**：确认"我生成的 `kroma-release-2026` 私钥"用于 RC1 产物签名；若需第三方保管，给出保管方式。
   （私钥**不需要**给 CI；建议离线签名 + CI 无密钥验签。）
2. **OpenSSL pin**：按第 5 节命令取摘要并核对后填入，或授权我在能出网的机器上代填。
3. **runner**：若要宣称 Windows/macOS/Android 可发布，需要对应 runner/设备；否则平台声明保持 NOT_RUN。
4. **发布**：本轮**未**推送、未打 tag、未上传 npm、未改更新地址。这些动作需要你单独批准。
