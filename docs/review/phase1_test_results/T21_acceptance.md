# T21 · 建立真实发布信任根 — **BLOCKED**

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
