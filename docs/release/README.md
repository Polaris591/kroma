# 发布流水线：DAG、依赖验真与失败演练（T22）

## 1. 目标与现状

规则：**只发布同一源码已经过门禁、依赖可追溯、产物可复算的版本。**

| 项 | 状态 |
| --- | --- |
| 质量门禁 → 打包 → 签名的依赖顺序 | 已实现（`release-gate.js` 的 `assertQualityGates` / `assertSameCommit`，由 CI 调用） |
| 依赖下载验真（OpenSSL 源码包） | 已实现：`release.yml` 在 **解包前** `sha256sum -c`，未设置 `OPENSSL_SHA256` 直接失败 |
| 依赖锁钉住（`Cargo.lock`） | 已实现：`docs/release/Cargo.lock.sha256`，`release-gate.js lock` 校验 |
| 平台产物齐全才能发布 | 已实现：`assertAllPlatformsPresent`（缺失与多余都拒绝） |
| 产物与 manifest 的 SHA 一致 | 已实现：`assertManifestMatchesArtifacts` |
| 重建身份可比较 | 已实现：`rebuildIdentity`（排序后的 `name:sha256:length` 摘要） |
| SBOM | 已实现：`scripts/generate-sbom.js` 从 `Cargo.lock` 生成，124 个组件，离线可复算 |
| 第三方 Action 钉 commit | **未完成（BLOCKED）**：22 个 `uses:` 仍是可变 tag，需要所有者提供审核过的 40 位 commit SHA |
| 真实签名 manifest | **未完成（BLOCKED）**：依赖 T21 的所有者密钥 |

## 2. DAG

```
fmt ──┐
clippy├─► quality ──► build(每平台) ──► pack ──► sign ──► publish
test ─┘                     │                        ▲
                            └── artifact hashes ─────┘
```

硬规则（由 `release-gate.js` 强制，全部有回归测试）：

1. `fmt` / `clippy` / `test` / 目标交叉构建**任一未 pass**（`fail` 或 `skipped`）→ 禁止打包与签名。
2. 产物必须来自**与门禁相同的 40 位 commit**；短 SHA 不算证据。
3. 声明的每个平台都必须有产物；缺失 → 阻止发布（只能显式取消该平台，不能发 stub）。
4. 多出来的、未声明的产物同样拒绝——它没有过门禁。
5. `Cargo.lock` 摘要变化 → 需要重新审核并重跑全部门禁。
6. 签名 manifest 里的每个 `(name, sha256, length, version)` 必须与真实产物逐一相等。

## 3. 依赖验真

- **OpenSSL 源码包**：`release.yml` 的 `Fetch and verify OpenSSL sources` 步骤先检查
  `OPENSSL_SHA256` 是否为空（空 → 直接 `exit 1`），下载后 `sha256sum -c`，通过后才 `tar -xzf`。
  校验值必须由所有者核对后写入，不能从镜像页复制。
- **`Cargo.lock`**：`docs/release/Cargo.lock.sha256` 记录当前摘要；`node scripts/release-gate.js lock`
  在 CI 里校验，不一致即失败并提示「需要审核 + 重跑门禁」。
- **构建工具版本**：`rust-toolchain.toml` 钉住 Rust 1.85.1；`scripts/build-musl-openssl.sh` 使用
  `ZIG` 指向的 zig（pip `ziglang` 包），脚本里对不完整安装有显式失败（`zig-probe.c` 编译探测），
  不会退化成「用宿主编译器凑过去」。
- **未做**：Action 的 commit 钉住（见第 1 节）。

## 4. 失败演练（可本地复跑）

```bash
node scripts/test-release-pipeline.js     # 21 项检查
node scripts/test-release-trust-root.js   # 13 项（T21 测试通道）
node scripts/test-kroma-js-upgrade.js     # 108 项（既有）
```

`test-release-pipeline.js` 逐条覆盖：

- `tampered_source_archive_fails_before_extract`：篡改的归档被拒；没有钉住摘要也是错误而不是放行；
  并检查 workflow 里校验发生在解包**之前**、未设置摘要会失败。
- `release_uses_same_commit_for_tests_and_artifacts`：不同 commit、短 SHA 都拒绝。
- `release_quality_failure_blocks_pack_and_sign`：四个门禁逐个置为 `fail`，以及 `skipped`。
- `missing_platform_artifact_cannot_publish`：缺失平台、未声明平台都拒绝。
- `cargo_lock_change_requires_review`：仓库内摘要匹配；改锁或缺摘要都拒绝。
- `rebuild_identity_is_comparable`：顺序无关、字节或长度变化即变化、缺摘要不能参与。
- manifest 与产物不一致（sha / length / version / 空清单）逐项拒绝。
- SBOM 可复算（两次生成逐字节相同）、每个 registry 组件都有 checksum、缺 checksum 被拒。

## 5. 每平台运行证据（编译 / 链接 / 启动 / TLS）

| 平台 | 编译 | 链接 | 启动 | 真实 TLS | 说明 |
| --- | --- | --- | --- | --- | --- |
| linux-x64 | ✓ | ✓ | ✓ | ✓ 原生 | 本机 `cargo test` 全量 + 二进制实跑 |
| linux-arm64 (musl) | ✓ | ✓ | ✓ qemu | ✓ qemu 内 HTTPS | 需 zig + 静态 OpenSSL；本机未构建 |
| android-arm64 | ✓ | ✓ | ✗ | ✗ | 无设备/模拟器：静态验证（真 OpenSSL、无运行时 libssl 依赖），**不标绿** |
| win32-x64 | ✓ | ✓ | ✗ | ✗ | 本机可交叉构建（`cargo build --target x86_64-pc-windows-gnu` 已通过），无 Windows runner 实跑 |
| darwin-x64 / arm64 | ✗ | ✗ | ✗ | ✗ | 需要 macOS SDK 与 runner，**显式阻断**，不用 stub 替代 |

原则：跨编译但未原生运行的平台**不会被自动标绿**；缺少 runner/SDK 时只能阻断该平台发布或显式取消，
不允许用 stub 替代真实 TLS（musl 那个空桩二进制的教训）。

## 6. 未完成项（需要所有者）

1. **Action 钉 commit**：22 个 `uses:` 目前是 `@v4` 之类的可变 tag。需要所有者提供审核过的
   40 位 commit SHA 后替换；`assertActionsPinned` 已经会拒绝可变 tag。
2. **真实签名 manifest**：依赖 T21 的所有者密钥（见 `docs/release-trust-root-runbook.md`）。
   在此之前 `PRODUCTION_TRUSTED_KEYS` 为空，生产升级一律拒绝。

这两项在 T22 验收文档里记为 BLOCKED，不用任何占位值填坑。
