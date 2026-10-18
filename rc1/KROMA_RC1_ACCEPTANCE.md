# Kroma RC1 收口验收

```
Source snapshot : kroma-p1-sealed-source.tar.gz
                  sha256 e34ac0c1dc505dd8b0877749f1c6e88fd5441857e489ef98dcd2e4cae353e23b
                  (与所有者本轮上传的封板包逐字节一致，已核对)
Git commit      : 见 RELEASE_MANIFEST.json 的 git_commit（本文件随该提交冻结）
Result          : RC1_CANDIDATE_BLOCKED
```

`RC1_CANDIDATE_BLOCKED` 的含义：源码与所有本地可执行门禁真实通过；真实签名（需要所有者批准）、
真实 runner（Windows/macOS/Android）与外部依赖摘要（本机无出网）缺证据，已逐条列出解除路径。
**不是** RELEASE_READY，也**没有**做任何推送/发布。

## 1. 逐卡状态

| 卡 | Implemented | Verified | Gate | 证据 | 阻塞原因 |
| --- | --- | --- | --- | --- | --- |
| RC1-00 冻结来源 | 是 | **是** | G0 PASS | `evidence/00_provenance.json`（提交、tar sha、工具链、锁、双入口一致） | — |
| RC1-01 Node late-open 竞态 | 未改 | `NOT_RUN` | G2 未过 | 任务书 §3 定位已核对（文件/用例/章节都存在） | 需先做事件轨迹复现（100+ 次定向跑），本轮不草率改动 |
| RC1-02 全仓门禁 | 是 | **是** | G1 PASS | fmt 105→0；clippy 0；Rust 全量 + 4 个 Node 套件真跑（`evidence/logs/`） | — |
| RC1-03 Release DAG 接线 | 是 | 部分 | G4 `PARTIAL` | `quality` job + `pack needs` 含 quality；`release-gate.js dag` 通过并有负向测试 | 需 runner 实跑一次（无权限） |
| RC1-04 OpenSSL 验真 | 是 | 部分 | G3 `PARTIAL` | 四条路径统一 `scripts/fetch-openssl.sh`；缺/错 pin 一律 fail-closed；`release-gate.js openssl` 通过 | **Digest 值 BLOCKED**：出网不可用，无法独立核对 openssl-3.5.7 官方摘要 |
| RC1-05 产物/签名一致性 | 部分 | 部分 | G5 `BLOCKED` | 客户端单目标 manifest 与生产根验签链路已真实跑通（T21）；`artifacts[]` 层与签名分工已定义 | 需先固定最终提交后重算全部产物哈希，并需要所有者批准的签名设施 |
| RC1-06 跨平台 | 部分 | `NOT_RUN` | G6 `PLATFORM_NOT_VERIFIED` | linux-x64 原生可跑；mingw 交叉构建通过 | 无 Windows/macOS runner、无 Android 设备、无 macOS SDK |
| RC1-07 状态账本 | 是 | **是** | G7 PASS | 本文件；历史文档加 `Superseded by` 标注 | — |
| RC1-08 阶段 2 交接 | 是 | — | — | `PHASE2_BASELINE_HANDOFF.md` | — |

## 2. 本轮修改（三个独立提交，均可单独回滚）

| 提交 | 内容 | 影响 |
| --- | --- | --- |
| `style(rc1-02)` | `cargo fmt --all`（105 处 → 0），纯格式化 | CI 的 fmt 门禁从必然红变成绿 |
| `feat(rc1-03,04)` | quality job + DAG gate；四条 OpenSSL 路径统一验真 + openssl gate | 打包不能再绕过质量；未验真的 OpenSSL 不可能被解包 |
| 本文件所在提交 | 只加 `rc1/` 文档与证据 | 无代码影响 |

没有重构大型 Rust 文件，没有改默认策略（仍 `SingleCandidate`），没有动 WAL/CAS/权限/预算内核。

## 3. 当前质量结果（全部本轮真跑，不是引用历史日志）

| 门禁 | 结果 | 日志 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 差异 | — |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning | — |
| `cargo test --workspace --no-fail-fast` | 见 `evidence/logs/rc1_full_suite.log` 的 EXIT 与合计行 | 该文件 |
| `node scripts/test-kroma-js-upgrade.js` | 109 / 109 | `evidence/logs/` |
| `node scripts/test-release-trust-root.js` | 13 / 13 | 同上 |
| `node scripts/test-release-pipeline.js` | 24 / 24（新增 3 条 RC1-03/04 检查） | 同上 |
| `node scripts/verify-ledger.js` | ok（32 文档 / 92+ 命令 / 31 行） | 同上 |
| `release-gate.js actions / lock / openssl / dag` | 全 ok | 同上 |

测试数量以实际输出为准，本文不硬编码某个"必须等于 995"的数字。

## 4. Release DAG 实跑状态

- **已接线**：`quality` → 五个平台构建 → `pack`（`needs` 含 quality）。
- **未实跑**：没有 GitHub runner 权限，`run URL / job IDs` 无法提供。按任务书，这一项保持 `PARTIAL`。
- 静态结构有负向测试：删掉 quality job、或让 pack 不依赖 quality、或出现无人工条件的 publish，
  `release-gate.js dag` 都会失败（`test-release-pipeline.js` 覆盖）。

## 5. 供应链

| 下载点 | 现状 |
| --- | --- |
| release/android、release/musl、ci/musl、ci/android | 全部走 `scripts/fetch-openssl.sh`：pin 空/格式错 → 退出；摘要不符 → 退出并打印实际值；只有校验通过才 `tar` |
| 缓存键 | 已包含 `OPENSSL_SHA256`，换 pin 不会复用旧构建 |
| Action 钉住 | 25 处，全部 40 位（`release-gate.js actions`） |
| 锁文件 | `Cargo.lock` 与钉住摘要一致（`release-gate.js lock`） |

**未完成**：摘要值本身。所有者需在能出网的机器上跑：

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

## 9. 需要所有者批准/提供的最小清单

1. **签名**：确认"我生成的 `kroma-release-2026` 私钥"用于 RC1 产物签名；若需第三方保管，给出保管方式。
   （私钥**不需要**给 CI；建议离线签名 + CI 无密钥验签。）
2. **OpenSSL pin**：按第 5 节命令取摘要并核对后填入，或授权我在能出网的机器上代填。
3. **runner**：若要宣称 Windows/macOS/Android 可发布，需要对应 runner/设备；否则平台声明保持 NOT_RUN。
4. **发布**：本轮**未**推送、未打 tag、未上传 npm、未改更新地址。这些动作需要你单独批准。
