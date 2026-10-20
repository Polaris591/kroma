# RC1 缺口评估（先证据，后修改）

基线：`/opt/kroma-p1`，Git HEAD `065dbdd978d3231816fcb2af60bbb7b4ffb06635`（2026-10-18T09:32:20+08:00），
rust 1.85.1（钉住）、node v24.21.0。用户上传的封板源码包 SHA-256 =
`e34ac0c1dc505dd8b0877749f1c6e88fd5441857e489ef98dcd2e4cae353e23b`，
与本仓库 `dist-t30/kroma-p1-sealed-source.tar.gz` **逐字节一致**（同一份 tar，不是两份东西）。
`Cargo.lock` = `caa80f28…13f8c`；`bin/kroma.js` 与 `npm/kroma-cli/bin/kroma.js` **逐字节相同**。

## G1 本地质量：全树 fmt 失败（已实测）

- 实测 `cargo fmt --all -- --check` → **105 处**差异（与任务书一致）。
- `.github/workflows/ci.yml` 把全树 fmt 作为硬门禁 ⇒ **当前 CI 的 fmt job 必然红**。
- 危害：CI 不可信，任何"测试通过"的声明都缺少门禁支撑。
- 修法：单独一个纯格式化提交（不改逻辑），随后 clippy + 全量测试 + `git diff --check`。

## G3 供应链：OpenSSL 下载验真只覆盖 1/4 路径（已实测）

| 文件:行 | 路径 | 现状 |
| --- | --- | --- |
| `release.yml:140` | android（release） | 有：空值拒绝 + `sha256sum -c` + 解包前校验 |
| `release.yml:217` | musl（release） | **无**：`curl` 后直接 `tar -xzf` |
| `ci.yml:155` | musl（CI） | **无** |
| `ci.yml:211` | android（CI） | **无** |

- 且 `release.yml:27` `OPENSSL_SHA256: ''` 为空 ⇒ android 那条现在**必然被自己的空值检查拦住**。
- 危害：源码包未经摘要校验就解包/编译，供应链可被中间人替换（且 release 与 CI 行为不一致）。
- 修法：四条路径统一"先校验再解包"，缺 pin/格式错/摘要不符一律失败。
- **Digest 值本身 BLOCKED**：本机 egress 到 openssl.org 与 github release 均不可用（实测无响应），
  无法独立获取并核对 `openssl-3.5.7.tar.gz` 的官方摘要。按任务书"无法确认就标 BLOCKED，绝不编造"
  处理：只落实现验真管线，摘要留空 → fail-closed，并给出所有者一条可执行命令。

## G4 Release DAG：质量门禁未接线（已实测）

- `release.yml:255` `pack:` 的 `needs: [linux, windows, macos, android, musl]` —— **没有 quality job**。
- 全仓 `release.yml` 中**没有**任何 `release-gate.js` 或 `generate-release-manifest.js` 调用。
- ⇒ 纯函数有测试 ≠ CI 已接线（任务书的判断成立）。
- 危害：打包成功即可产出候选产物，质量/签名/一致性没有强制点。
- 修法：新增 `quality` job（fmt/clippy/rust tests/node 套件/gate 脚本），`pack` 与 `sign` 必须
  `needs: quality`；再加 `verify` job 做 `artifacts[]` 与公钥验签。真实运行证据需 runner（BLOCKED）。

## G5 签名：两种 manifest Schema 不能用一套测试冒充（已确认）

- `scripts/generate-release-manifest.js` 产出**单目标升级 manifest**
  （`target/version/sha256/length/signature`，供 `verifyManifest` 客户端协议用）。
- `release-gate.js::assertManifestMatchesArtifacts` 期望**产物集合** `artifacts[]`。
- 二者不是同一 Schema。现有 `test-release-pipeline.js` 用前者之外的 fixture 测后者，
  不能证明"真实发布产物被真实签名保护"。
- 修法：明确 `client-upgrade-manifest` 与 `release-provenance(artifacts[])` 两层，
  后者引用前者的 keyId 与签名摘要；负向测试覆盖两层不一致。

## G2 更新可靠性：Node late-open 竞态（任务书 §3，未复现）

- 文件位置与任务书一致：`bin/kroma.js::inspectAndExtractTarStream`、`scripts/test-kroma-js-upgrade.js`
  的 `gated late open after cleanup …` 用例、`regressions.md §2.3`。
- 本轮**未**真跑复现（需要定向跑 100 次以上）。标记 `NOT_RUN`，下一步先做事件轨迹复现。
- 危害：升级走 `ENOENT` 而不是 deadline 错误，用户看到误导性失败，且可能留孤儿 `.tmp`。

## G6 平台声明

| 平台 | 现状 | 状态 |
| --- | --- | --- |
| linux x86_64 | 本机原生可跑 | 可验证 |
| windows x86_64 | mingw **交叉编译**通过；无 Windows runner | `WINDOWS_NATIVE_NOT_RUN` |
| macOS | 无 SDK、无 runner | `NOT_RUN` |
| linux arm64 musl | 曾 qemu 跑通 TLS（历史证据）；本轮未复跑 | `MUSL_RUNTIME_NOT_RUN` |
| android arm64 | 仅构建与链接桩；无设备/模拟器 | `ANDROID_DEVICE_NOT_RUN` |

## G7 状态账本

- `T30_LEDGER.md` 的 T21/T22/T24 行含历史 `BLOCKED` 语境，需在 RC1 总报告中建立**唯一最新状态**，
  旧文档加 `Superseded by KROMA_RC1_ACCEPTANCE.md` 标注（不删原文、不改旧日志）。
- T24 结论保持 `INCONCLUSIVE`（28 次真实运行 / 8.77M token / 留出 2 个任务簇 / CI 下界 0），
  默认 `SingleCandidate` 不变。T22 在 Action 钉住与签名链路已进一部分，但**仍缺 CI 实跑**。

## 优先级与本次可完成范围

| 卡 | 本次状态 | 说明 |
| --- | --- | --- |
| RC1-00 | 完成 | 来源、Git、工具链、锁、双入口一致性已记录（`rc1/evidence/`） |
| RC1-01 | `NOT_RUN` | 需先做事件轨迹复现，再改代码；不在本轮草率修改 |
| RC1-02 | 执行中 | fmt 全树修复（105→0）+ clippy + 全量 Rust/Node |
| RC1-03 | 部分 | 可落地 quality job 与 needs 串联；真实 run URL 需 runner（BLOCKED） |
| RC1-04 | 部分 | 四条下载路径统一验真（fail-closed）；摘要值 BLOCKED(egress) |
| RC1-05 | 待 RC1-03 | 需 commit 固定后重算全部哈希与签名 |
| RC1-06 | BLOCKED | 缺 Windows/macOS/Android runner 与设备 |
| RC1-07 | 本轮出总报告 | 以 `RC1_CANDIDATE_BLOCKED` 交付 |
| RC1-08 | 可写档案 | `PHASE2_BASELINE_HANDOFF.md` |
