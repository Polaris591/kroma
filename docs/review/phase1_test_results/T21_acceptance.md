# T21 · 建立真实发布信任根 — **BLOCKED**

> **Superseded by `rc1/KROMA_RC1_ACCEPTANCE.md`（2026-10-18，提交 `2259f91`）。**
> 本文是当时那一刻的真实记录，保留原文不改；**当前状态以 RC1 验收文档为准**。

阶段 RELEASE-BLOCKER｜依赖 T00｜关联 KR-27

## 一、结论

**BLOCKED**。缺的是所有者提供的真实 Ed25519 密钥对与轮换策略；没有它就无法执行任务卡的核心验收
「真实发布候选通过签名验证」。按任务卡的停止条件，这里**不放置占位私钥、不关闭签名校验、不用测试密钥冒充**。

缺什么（缺一不可）：

1. 所有者的 Ed25519 公钥（64 位十六进制 raw 或 PEM）+ `keyId`；
2. 轮换策略（生效/退役顺序、是否保留重叠期公钥）；
3. 用该私钥签出的真实发布 manifest 与对应 tarball；
4. 私钥的存放位置与访问方式（离线机 + `--private-key-file`）。

## 二、当前信任根状态（这是 fail-closed，不是「跳过」）

`bin/kroma.js`：

```js
const PRODUCTION_TRUSTED_KEYS = Object.freeze({});
```

`verifyManifest` 在信任根为空时抛
`Production release trust root is unconfigured; self-upgrade is blocked`；
`downloadBinaryIfNeeded` 在同样条件下直接返回 `false` 且不创建任何文件。
所以「没有密钥」的结果是**拒绝一切升级**，不是「不校验就装」。

## 三、在没有密钥的前提下，已经验证了什么

新增 `scripts/test-release-trust-root.js`（**测试通道**：密钥在运行时生成、只存在于内存、
不写盘、不进仓库、不进生产信任根），**13 项检查全部通过**：

| 检查 | 覆盖的任务卡回归 |
| --- | --- |
| 生产信任根为空且 frozen（没有占位密钥） | 停止条件 |
| 测试密钥签出的 manifest 被生产入口拒绝 | `production_fixture_signed_by_release_key_verifies` 的负向一半 |
| 测试通道内签名可验证（version/target/sha256 读回正确） | 同上正向一半（测试通道） |
| `version`/`target`/`sha256`/`length`/`antirollback` 任一被改 → `Signature verification failed` | `manifest_binds_platform_version_and_artifact_hash` |
| `keyId` 被改 → `Unknown or untrusted signing key` | `unknown_rotated_revoked_expired_key_is_rejected` |
| 未知 keyId → 拒绝 | 同上 |
| 密钥从信任根移除（轮换/退役）→ 拒绝 | 同上 |
| 过期 manifest → `Manifest has expired` | 同上 |
| target 不匹配 → `Manifest target mismatch` | 同上 |
| 版本低于 antirollback 下限 → `downgrade rejected` | 同上 |
| 已存在的二进制不会被下载覆盖（字节不变） | `signature_failure_never_overwrites_existing_binary` |
| 校验失败时不留下任何文件（目录为空） | 同上 |
| 信任根未配置时在落盘前就拒绝 | 同上 |

既有 108 项 Node 检查（`node scripts/test-kroma-js-upgrade.js`）**全部仍然通过**，
没有被本次改动削弱。

## 四、所有者拿到密钥后要走的路

`docs/release-trust-root-runbook.md` 是完整清单：生成 manifest → 用同一套函数自校验 →
把公钥同步装进 **两处**生产入口（`bin/kroma.js` 与 `npm/kroma-cli/bin/kroma.js`）→ 重跑两套 Node 套件。
该文档同时写明：装好真密钥之后，`test-release-trust-root.js` 里「信任根为空」的两条检查**必须**
改成「信任根恰好是声明的 keyId 集合」，而在拿到密钥之前不能改——改了就是用占位密钥冒充信任根。

## 五、证据

| 项 | 结果 |
| --- | --- |
| `node scripts/test-kroma-js-upgrade.js` | **all 108 checks passed** |
| `node scripts/test-release-trust-root.js` | **all 13 checks passed（测试通道，未使用生产密钥）** |
| `PRODUCTION_TRUSTED_KEYS` | 空且 frozen |
| 真实发布候选签名验证 | **NOT RUN — BLOCKED（无所有者密钥）** |

## 六、边界

- 本项没有修改 `bin/kroma.js` 的校验逻辑：现有实现已经是 fail-closed 的，缺的只是信任根。
- 没有新增占位密钥、没有放宽「未知 keyId / 缺签名 / 降级」的拒绝条件。
- T22（发布与供应链闭环）依赖本项的真实签名结果，因此它的签名部分同样 BLOCKED；
  SBOM / 哈希清单可复算的部分在 T22 里单独处理。

---

## 本轮更新（2026-10-18）：密钥已就位，链路真实跑通

所有者指示「你自己生成签名」，于是按路线 A 在**本机**生成密钥对，私钥留在服务器、不进仓库：

| 项 | 值 |
| --- | --- |
| keyId | `kroma-release-2026` |
| 公钥 raw hex | `2cabb3d0cb06b03ffd478f37e07217e09286a8fa3b6a512c0b4288adc78114bc` |
| 私钥 | `/root/kroma-release/kroma-release-key.pem`（600，目录 700），未进仓库/日志/对话 |
| 安装 | `bin/kroma.js` 与 `npm/kroma-cli/bin/kroma.js` 的 `PRODUCTION_TRUSTED_KEYS`（两份 `cmp` 逐字节一致） |

### 真签名验证（端到端）

对封板源码包（3,056,045 字节）签名并**用生产信任根**（不传任何 override）验证：

- `verifyManifest` 返回 `keyId=kroma-release-2026 / v1.5.1 / linux-x64`，
  `sha256 = 8ee884cdafe2f8ef087fc4f957fd752cbd68f3000c76cba6af9fa5255ebe4a0c` —— 与随包 `SHA256SUMS` 里
  `kroma-p1-sealed-source.tar.gz` 的摘要**逐字节相同**；
- 篡改载荷（sha256 改全 0）→ `Signature verification failed for release manifest`；
- 用测试通道密钥签的 manifest → `Unknown or untrusted signing key: test-channel`；
- 冒充生产 keyId 但用别的密钥签 → `Signature verification failed`（证明生产公钥真的被使用，不是被跳过）。

### 测试断言随之更新（不是放松，是转向真实状态）

- 「生产根为空且 frozen」→「生产根**恰好**含 `kroma-release-2026`、frozen、值为 64 位十六进制」；
- 「生产入口拒绝测试密钥」的期望消息从 `trust root is unconfigured` 改为
  `Unknown or untrusted signing key: test-channel`；
- 「不传 trustedKeys 时 fail-closed」拆成两条：空 map 仍是 `unconfigured`（fail-closed 路径保留测试），
  不传 override 则走生产根并因「未知密钥」被拒；
- 新增：生产根绑定签名（冒充 keyId 必须因签名失败被拒）。

### 门禁

| 项 | 结果 |
| --- | --- |
| `node scripts/test-release-trust-root.js` | **13 / 13** |
| `node scripts/test-kroma-js-upgrade.js` | **109 / 109**（新增 1 条生产根绑定检查） |
| `node scripts/generate-release-manifest.js … --key-id kroma-release-2026` | 签名成功 |
| 生产根验证 + 篡改/冒充/测试密钥三向拒绝 | 通过 |

### 边界

- 私钥由本机生成并保管；**所有者需自行离线备份**（丢失只能按轮换流程换 keyId）。
- 轮换策略与备份要求写在 `docs/release/TRUST_ROOT.md`。
- 没有把私钥写进仓库、CI secret 之外的位置，也没有用测试密钥冒充生产密钥。
