# T30 · 发布候选封板与可追溯交付

> **Superseded by `rc1/KROMA_RC1_ACCEPTANCE.md`（2026-10-18，提交 `2259f91`）。**
> 本文是当时那一刻的真实记录，保留原文不改；**当前状态以 RC1 验收文档为准**。

封板对象：`/opt/kroma-p1`（2026-10-17 上传的 Kroma v1.5.0 源码 + 第一批补丁，叠加 T00–T30 的实现）。
封板提交：`c85db07`（`style(t30)` 之后无未提交改动）。

## 一、交付物与摘要

| 文件 | 大小 | SHA-256 |
| --- | --- | --- |
| `kroma_T00_T30.patch` | ~4 MB | 见随包 `SHA256SUMS`（产物在封板提交之后生成，摘要以该文件为准） |
| `kroma-p1-sealed-source.tar.gz` | 3.0 MB（不含 `target/`、`.git/`、`dist-t30/`） | 见随包 `SHA256SUMS` |
| `commits.txt` | 33 个提交的完整列表（基线 `19723b6` → `c85db07`） | — |
| `sbom.json`（仓库内 `docs/release/`） | 124 个组件 | `39b661593775bd84…` |

补丁基线：`git diff 19723b6..c85db07`，即「交付源码 + 第一批补丁」之上叠加的全部改动。

## 二、门禁结果（封板树，最终状态）

| 门禁 | 结果 |
| --- | --- |
| `cargo test --workspace --no-fail-fast` | **995 passed / 0 failed**（`logs/test_t30_final.log`，exit 0） |
| `cargo clippy --workspace --all-targets -- -D warnings` | **0 warning** |
| rustfmt（本次改动的 95 个文件） | **0 diffs**（`rustfmt --check` 逐文件通过） |
| rustfmt（全树 `cargo fmt --all -- --check`） | 105 处差异，**全部位于本次未触碰的文件**；交付基线为 **507** 处，故本次只减少不增加 |
| `cargo build --target x86_64-pc-windows-gnu` | 链接成功（T08 证据，`logs/t08_windows_build.log`） |
| `node scripts/test-kroma-js-upgrade.js` | 108 / 108 |
| `node scripts/test-release-trust-root.js` | 13 / 13（测试通道） |
| `node scripts/test-release-pipeline.js` | 21 / 21 |
| SBOM 复算 | 两次生成逐字节相同（124 组件） |
| `kroma --experiment experiments/exp-00/manifest.json` | exit 0，48 jobs，digest `05757bf3fe69c9f9` |

每项任务的定向测试与变异验证见 `T30_LEDGER.md` 与各 `T??_acceptance.md`；
命令与退出码汇总在 `commands.json`，原始日志在 `logs/`。

## 三、台账结论

- **25 项 PASS**（T00–T20 除 T21/T22/T24 外全部、以及 T23、T25–T30）。
- **T21 BLOCKED**：缺所有者 Ed25519 密钥对与轮换策略；校验路径已用测试通道 13 项固定，
  生产信任根保持为空且 frozen（拒绝一切升级，而非跳过校验）。
- **T22 PARTIAL**：供应链检查 21 项通过（OpenSSL 包解包前验真、同 commit、锁钉住、平台齐全、
  重建身份、SBOM）；缺 Action 的 40 位 commit 钉住（需所有者提供审核过的 SHA）与真实签名 manifest。
- **T24 BLOCKED**：无费用上限授权，未调用任何付费服务；已交付冻结的 EXP-00 可执行配置。
- T25–T29 的实验部分（EXP-01…06）随 T24 一并 BLOCKED；机制与**离线**指标全部就绪并通过测试，
  默认开关保持关闭（未证明即不启用）。

## 四、复现命令

```bash
cd /opt/kroma-p1
cargo test --workspace --no-fail-fast          # 995 passed / 0 failed
cargo clippy --workspace --all-targets -- -D warnings
rustfmt --check --edition 2021 $(git diff --name-only 19723b6..HEAD | grep '\.rs$')
node scripts/test-kroma-js-upgrade.js          # 108 checks
node scripts/test-release-trust-root.js        # 13 checks (test channel)
node scripts/test-release-pipeline.js          # 21 checks
node scripts/release-gate.js lock              # Cargo.lock 与钉住摘要一致
node scripts/generate-sbom.js --out /tmp/sbom.json
kroma --experiment experiments/exp-00/manifest.json
```

应用补丁到交付基线：

```bash
cd <交付源码目录> && git apply --check /path/kroma_T00_T30.patch && git apply /path/kroma_T00_T30.patch
```

## 五、已知限制与既有 flake

- 既有 flake（与本次改动无关，复跑均绿）：`b05_execution_context::run_deadline_clamps_mcp_timeout`、
  `b08c_git_metadata`（需 `--test-threads=1`）、
  `mcp::call_lifetime_tests::subsequent_call_fails_closed_when_poisoned`、
  `session_ownership::tests::different_workspaces_are_independent_and_same_workspace_is_exclusive`、
  `b05 agent_runner_real_toolchain_clamps_run_deadline`（负载敏感）。详见 `regressions.md`。
- Windows 三个跨进程用例**已编译未执行**（本机无 Windows runner）。
- 实网缓存 canary、真实模型基线、五项模型侧实验：**NOT_RUN**（需预算授权）。
- 全树 rustfmt 仍有 105 处**交付时就存在**的差异；本次改动的文件全部干净，且总数从 507 降到 105。
- 没有推送远端、没有发布任何包：本包是**发布候选**，签名与上传需所有者按
  `docs/release-trust-root-runbook.md` 执行。

## 六、文档索引

| 想找什么 | 去哪 |
| --- | --- |
| 逐任务状态与证据 | `T30_LEDGER.md` |
| 单项验收（做法、变异验证、边界） | `T00_baseline.md`、`T01…T29_acceptance.md` |
| 命令与退出码 | `commands.json` |
| 原始日志 | `logs/` |
| 能力清单（机器可校验） | `docs/capabilities.json` + `docs/CAPABILITIES.md` |
| 设计决策 | `docs/adr/0001-snapshot-identity.md`、`docs/adr/0002-task-state-and-semantic-compression.md` |
| 发布流程与阻塞项 | `docs/release/README.md`、`docs/release-trust-root-runbook.md` |
| 语言能力矩阵 | `docs/release/language-matrix.md` |
| 实验配置 | `experiments/exp-00/` |
