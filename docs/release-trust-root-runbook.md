# 发布信任根：所有者密钥操作清单（T21）

本文件是给**项目所有者**的操作清单。签名私钥不在仓库里、不在本会话里、也不会由本审查生成。

## 1. 现在的状态

| 项目 | 状态 |
| --- | --- |
| Ed25519 签名与校验代码 | 已实现（`bin/kroma.js`：`buildCanonicalSigningPayload` / `signManifest` / `serializeManifest` / `verifyManifest`） |
| 发布 manifest 生成器 | 已实现（`scripts/generate-release-manifest.js`，私钥只从受保护文件路径读取，不走 argv/环境变量/日志） |
| 生产信任根 `PRODUCTION_TRUSTED_KEYS` | **空且 frozen**——`verifyManifest` 在空信任根下抛 `Production release trust root is unconfigured; self-upgrade is blocked` |
| 真实发布候选签名验证 | **BLOCKED**：没有所有者提供的密钥对 |
| 测试通道校验行为 | 13 项检查通过（`scripts/test-release-trust-root.js`，用运行时生成的一次性密钥，明确不进入生产） |

生产入口是 fail-closed 的：空信任根不是「跳过校验」，而是**拒绝一切升级**。

## 2. 所有者需要提供什么（缺一不可）

1. **一个 Ed25519 公钥**，以 64 位十六进制字符串（raw 32 字节）或 PEM 形式给出，
   连同它的 `keyId`（`^[A-Za-z0-9_-]{1,64}$`）。
2. **轮换策略**：`keyId` 的生效与退役顺序；是否保留「上一把公钥」以支持重叠期。
3. **一个用该私钥签出的真实发布 manifest**（针对 `cli` 目标与至少一个平台目标），
   以及对应的 tarball，用于「真实发布候选通过签名验证」这条验收。
4. **私钥的存放位置与访问方式**（例如离线机器 + `--private-key-file` 路径）。
   私钥**不得**出现在：仓库、CI 日志、会话记录、环境变量、命令行参数。

## 3. 收到密钥后要执行的步骤

```bash
# 1) 生成 manifest（私钥只在本地受保护路径）
node scripts/generate-release-manifest.js \
  --private-key-file /secure/offline/kroma-release-1.key \
  --key-id kroma-release-1 \
  --version 1.5.0 \
  --target cli \
  --tarball ./dist/kroma-cli-1.5.0.tgz \
  --expires <unix-seconds> \
  --antirollback 1.5.0 \
  --out ./dist/kroma-manifest-cli.json

# 2) 用同一套函数校验（这一步必须成功，否则禁止上传）
node -e 'const k=require("./bin/kroma.js");const fs=require("fs");
const t=fs.readFileSync("./dist/kroma-manifest-cli.json","utf8");
console.log(k.verifyManifest(t,{trustedKeys:{kroma-release-1:process.env.PUBKEY},currentTarget:"cli"}).version);'

# 3) 把公钥装进生产入口
#    bin/kroma.js 的 PRODUCTION_TRUSTED_KEYS 与 npm/kroma-cli/bin/kroma.js 的副本必须同步
#    （两处都是生产入口；只改一处会导致 npm 安装的 CLI 与直接运行的 CLI 行为不一致）

# 4) 重跑两套 Node 套件
node scripts/test-kroma-js-upgrade.js      # 108 项旧检查，不得减少
node scripts/test-release-trust-root.js    # 13 项信任根检查
```

第 3 步之后，`scripts/test-release-trust-root.js` 里「生产信任根为空且 frozen」与
「测试密钥被生产入口拒绝」这两条检查会失败——这是**预期的**，届时需要把这两条改成
「生产信任根恰好包含所有者声明的 keyId 集合」，并把真实 manifest 的校验结果作为证据。
在拿到密钥之前不改这两条，因为改了就等于用一个占位密钥冒充信任根。

## 4. 必须同时满足的拒绝条件

无论信任根是否配置，以下都必须被拒绝（已由 `scripts/test-release-trust-root.js` 覆盖）：

- 未知 `keyId`；
- 已从信任根移除（退役/轮换掉）的 `keyId`；
- 签名被篡改，或 manifest 中 `version` / `target` / `sha256` / `length` / `antirollback`
  任一字段被改动；
- manifest 过期（`expires`）；
- target 不匹配；
- 版本低于 `antirollback` 下限；
- 签名校验失败时**不覆盖**已存在的二进制，也不留下任何文件。

## 5. 明确不做的事

- 不生成、不提交、不记录任何私钥；不使用占位私钥；不关闭签名校验。
- 不把测试通道的密钥写进 `PRODUCTION_TRUSTED_KEYS`。
- 不在本审查里替所有者选择许可证条款。
