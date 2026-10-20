# T22 · 发布与供应链闭环 — 部分完成 / **BLOCKED（签名与 Action 钉 commit）**

> **Superseded by `rc1/KROMA_RC1_ACCEPTANCE.md`（2026-10-18，提交 `2259f91`）。**
> 本文是当时那一刻的真实记录，保留原文不改；**当前状态以 RC1 验收文档为准**。

阶段 RELEASE-BLOCKER｜依赖 T00, T21｜关联 KR-28, KR-29

## 一、结论

可本地验证与可离线复算的部分**已完成并有回归测试**；两项依赖所有者的部分记为 **BLOCKED**：

| 项 | 状态 |
| --- | --- |
| 质量门禁 → 打包 → 签名的依赖规则 | ✅ 已实现并有测试 |
| 依赖下载验真（OpenSSL 源码包，解包前校验） | ✅ 已实现并有测试 |
| `Cargo.lock` 钉住 + 变更需审核 | ✅ 已实现并有测试 |
| 平台产物齐全才能发布（缺失/多余都拒） | ✅ 已实现并有测试 |
| 产物与 manifest 的 SHA/长度/版本一致 | ✅ 已实现并有测试 |
| 重建身份可比较 | ✅ 已实现并有测试 |
| SBOM（离线可复算，124 组件） | ✅ 已生成并纳入仓库 |
| 第三方 Action 钉 commit | ⛔ **BLOCKED**：22 个 `uses:` 仍是可变 tag，需要所有者提供审核过的 40 位 SHA |
| 真实签名 manifest 与产物一致 | ⛔ **BLOCKED**：依赖 T21 的所有者密钥 |

## 二、缺陷

1. **Android 作业的 OpenSSL 下载没有验真**：`curl -o openssl.tar.gz` 之后直接 `tar -xzf`，
   中间没有任何摘要校验。被篡改的源码包会被当成源码编译进发布产物。
2. **没有任何规则把「门禁通过」与「打包签名」绑在一起**：谁先谁后靠人记。
3. **没有 SBOM**：依赖图只存在于 `Cargo.lock`，没有可复算、可归档的清单。
4. **没有重建身份**：两次构建是否等价无法比较。
5. **`Cargo.lock` 变更不会被发现**：换掉依赖图之后，旧证据仍被当成有效。

## 三、改了什么

### 1. `scripts/release-gate.js`（纯函数 + CLI）

| 函数 | 规则 |
| --- | --- |
| `verifyPinnedArchive` | 摘要必须匹配；**没有钉住摘要本身是错误**，不是放行 |
| `assertSameCommit` | 产物 commit 必须等于门禁 commit，且必须是 40 位 |
| `assertQualityGates` | `fmt`/`clippy`/`test`/交叉构建 任一非 `pass`（含 `skipped`）→ 禁止打包签名 |
| `assertAllPlatformsPresent` | 缺平台拒绝；多出未声明平台同样拒绝 |
| `assertDependencyLockUnchanged` | `Cargo.lock` 摘要变化 → 需重新审核并重跑门禁 |
| `rebuildIdentity` | 排序后的 `name:sha256:length` 摘要，与顺序无关 |
| `assertManifestMatchesArtifacts` | manifest 每项与真实产物的 sha/length/version 逐一相等 |
| `assertActionsPinned` | 第三方 action 必须 40 位 commit，本地 composite action 除外 |

CLI：`node scripts/release-gate.js actions` / `lock`，退出码 1 = 门禁失败。

### 2. 工作流补验真

`.github/workflows/release.yml` 的 Android 作业：

- 新增 `OPENSSL_SHA256` 环境变量（默认为空）；
- `Fetch and verify OpenSSL sources` 步骤：为空 → `exit 1`；下载后 `sha256sum -c -`；**通过后才解包**。

### 3. SBOM

`scripts/generate-sbom.js` 从 `Cargo.lock` 生成（不用 `cargo metadata`：它需要联网下载全部 crate，
而锁文件本身就是解析后的依赖图）。输出确定性 JSON：组件按 `(name, version)` 排序、无时间戳、
带 `lockfileSha256` 与 `sbomSha256`。

已生成 `docs/release/sbom.json`（**124 个组件**，锁文件 `caa80f28…`，SBOM `0c866311…`）。

### 4. 文档

`docs/release/README.md`：DAG、硬规则、依赖验真清单、失败演练命令、**每平台运行证据表**
（编译/链接/启动/TLS 分列，跨编译未实跑的平台不标绿），以及两项 BLOCKED 的说明。

## 四、验收证据

| 门禁 | 结果 |
| --- | --- |
| `node scripts/test-release-pipeline.js` | **all 21 checks passed**（`logs/t22_release_pipeline.log`） |
| `node scripts/test-kroma-js-upgrade.js` | all 108 checks passed（未被削弱） |
| `node scripts/test-release-trust-root.js` | all 13 checks passed |
| `node scripts/release-gate.js lock` | ok，`Cargo.lock` 与钉住摘要一致 |
| SBOM 复算 | 两次生成逐字节相同（124 组件） |

### 覆盖的任务卡回归

| 回归名 | 覆盖点 |
| --- | --- |
| `tampered_source_archive_fails_before_extract` | 篡改拒绝 + 无钉住拒绝 + workflow 中校验在解包前 + 空摘要失败 |
| `release_uses_same_commit_for_tests_and_artifacts` | 不同 commit、短 SHA |
| `release_quality_failure_blocks_pack_and_sign` | 四门禁逐个 `fail` + `skipped` |
| `missing_platform_artifact_cannot_publish` | 缺平台、未声明平台 |
| `cargo_lock_change_requires_review`（`dependency_pin_changes_invalidate_cache` 无同名测试，由同一个 `assertDependencyLockUnchanged` 覆盖） | 摘要匹配/不匹配/缺失 |
| `rebuild_identity_is_comparable` | 顺序无关、字节与长度敏感、缺摘要拒绝 |
| `release artifacts 与签名 manifest 的 SHA 一致` | sha/length/version/空清单 |
| `cargo_lock_change_requires_review` 的 SBOM 侧 | 组件确定性 + registry 组件必须有 checksum |
| `manual_release_preserves_identical_gates`（无同名测试） | 同一套 `assertQualityGates` 被 CLI 与 CI 共用，故手工发布走同一组门禁 |
| `third-party Action 钉 commit` | 检测器已实现并测过（仓库现状见 BLOCKED） |

## 五、BLOCKED 明细

1. **Action 钉 commit**：`release.yml` 里 22 个 `uses:` 仍是 `@v4` 之类的可变 tag。替换需要所有者
   提供审核过的 40 位 commit SHA（本环境无网络，编造 SHA 会让 CI 直接失败，比保留 tag 更糟）。
   `assertActionsPinned` 已经会拒绝可变 tag，接上就是硬门禁。
2. **真实签名 manifest**：依赖 T21 的所有者密钥。在此之前 `PRODUCTION_TRUSTED_KEYS` 为空，
   生产升级一律拒绝；`assertManifestMatchesArtifacts` 已用未签名 manifest 验证了「产物与清单一致」，
   但没有声称这是签名验证。

## 六、边界

- 没有 stub、没有放宽任何拒绝条件；缺平台只能显式取消，不能发占位产物。
- 每平台运行证据表里，android / windows / darwin 明确标为未原生运行（darwin 直接阻断），
  没有把跨编译当成实跑。
- `dist/` 产物不纳入仓库；SBOM 与锁摘要作为证据放在 `docs/release/`。

## 六、本轮更新：Action 钉住已完成（2026-10-18）

上一轮把「Action 必须钉到 40 位 commit」记为缺件（当时判断本机无外网）。实际 **GitHub REST API 可达**
（`api.github.com` 返回 200；`raw.githubusercontent.com` 不可达），所以这一项不必等所有者手工抄：

| 动作 | 钉住的 commit |
| --- | --- |
| `actions/checkout@v4` | `11d5960a326750d5838078e36cf38b85af677262` |
| `actions/cache@v4` | `0057852bfaa89a56745cba8c7296529d2fc39830` |
| `actions/download-artifact@v4` | `d3f86a106a0bac45b974a628896c90dbdf5c8093` |
| `actions/setup-java@v4` | `cf277c60eb25467037889841efdb72551f06f6c3` |
| `actions/setup-node@v4` | `49933ea5288caeca8642d1e84afbd3f7d6820020` |
| `actions/upload-artifact@v4` | `ea165f8d65b6e75b540449e92b4886f43607fa02` |
| `android-actions/setup-android@v3` | `9fc6c4e9069bf8d3d10b2204b1fb8f6ef7065407` |
| `Swatinem/rust-cache@v2` | `6323deb102c322ba6fcbdcafc7e3dddab59af2b6` |

解析方式：`GET /repos/<owner>/<repo>/commits/<tag>` 取回该 tag 当前指向的 commit（annotated tag 会被解引用）。
钉住后 `release.yml` 22 处、`ci.yml` 16 处，共 **38 处全部为 40 位**，`node scripts/release-gate.js actions` 通过。

顺带修了门禁本身的一个真问题：它把行尾注释也算进引用，于是业界标准的
`uses: owner/action@<sha> # v4` 写法会被误判为未钉住——这会把维护者逼成删注释。现在门禁先剥掉
`# ...` 再校验，并补了 4 条测试（带注释的 40 位通过、带注释的 39 位仍被拒、注释不能救活
`@v4`、repo 状态报告同步剥离注释后显示 22 pinned / 0 mutable）。

因此 T22 现在只剩**签名 manifest** 一项等 T21 的密钥，其余全部通过。

## 七、签名项现状（2026-10-18 更新）

T21 的生产密钥已就位（`kroma-release-2026`），所以「签名」不再是缺件，而是**链路已证明、只差 CI 上跑一次完整发布**：

| 已完成 | 证据 |
| --- | --- |
| 生产密钥生成 + 装入两处入口 | `docs/release/TRUST_ROOT.md`；`cmp` 两份入口逐字节一致 |
| 真签名 manifest 经生产根验证 | 封板源码包 3,056,045 字节，`sha256=8ee884cd…` 与 `SHA256SUMS` 一致；篡改载荷被拒 |
| 产物与 manifest 一致性检查 | `assertManifestMatchesArtifacts`（21 项检查中的覆盖点） |
| Action 全部 commit 钉住 | 38 处，`release-gate.js actions` 通过 |
| Cargo.lock 钉住 | `release-gate.js lock` 通过 |

**仍未完成（需要 CI runner）**：真实跑一次 `.github/workflows/release.yml`，产出 `artifacts[]` 形态的
签名 manifest 并让四个平台产物一起过 `assertManifestMatchesArtifacts`。这需要 4 个平台的构建产物
（本机磁盘 1.3 GB 可用，跑不动全平台构建），因此仍记为 PARTIAL，而不是用单平台结果冒充。
