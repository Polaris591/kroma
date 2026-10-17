# 发布信任根（生产）

## 1. 现状

| 项 | 值 |
| --- | --- |
| keyId | `kroma-release-2026` |
| 算法 | Ed25519（PKCS#8 私钥 / raw 32 字节公钥） |
| 公钥（raw hex） | `2cabb3d0cb06b03ffd478f37e07217e09286a8fa3b6a512c0b4288adc78114bc` |
| 公钥（PEM） | `MCowBQYDK2VwAyEALKuz0MsGsD/9R4834HIX4JKGqPo7alEsC0KIrceBFLw=` |
| 安装位置 | `bin/kroma.js` 与 `npm/kroma-cli/bin/kroma.js` 的 `PRODUCTION_TRUSTED_KEYS`（两份必须逐字节一致） |
| 私钥位置 | `/root/kroma-release/kroma-release-key.pem`（权限 600，目录 700） |
| 生成时间 | 2026-10-18，由 PI智能助手在本机生成（所有者指示「你自己生成签名」） |

私钥**从未**进入仓库、日志或对话；仓库里只有公钥。

## 2. 校验行为

- 生产入口只接受 `keyId = kroma-release-2026` 且签名有效的 manifest；
- 其它 keyId（包括测试通道的 `test-channel`）一律 `Unknown or untrusted signing key`；
- 签名不符 → `Signature verification failed for release manifest`；
- 信任根为空时 → `Production release trust root is unconfigured; self-upgrade is blocked`（fail-closed 路径仍在，用空 map 显式测试）。

## 3. 签名与验证

```bash
# 签名（私钥只在本机，权限 600）
node scripts/generate-release-manifest.js \
  --target linux-x64 --version <版本> \
  --tarball <产物路径> --url <https 地址> \
  --key-id kroma-release-2026 \
  --private-key-file /root/kroma-release/kroma-release-key.pem \
  --out <manifest 路径>

# 验证（用生产信任根，不传任何 override）
node -e 'const k=require("./bin/kroma.js");const fs=require("node:fs");
         console.log(k.verifyManifest(fs.readFileSync(process.argv[1],"utf8")))' <manifest 路径>
```

## 4. 轮换策略

1. **新增**：在新版本里给 `PRODUCTION_TRUSTED_KEYS` 加第二个 keyId（同时保留旧的），发版；
   等客户端升级覆盖后，再用下一个版本删除旧 keyId。
2. **撤销**：从 map 里删除 keyId 即生效（已经发布的旧 manifest 会被拒绝）。
3. **持有者变更**：按 1 的方式过渡，不要在同一个版本里"删旧增新"——那会让仍持旧版本的客户端无法验证。
4. **过期**：manifest 自带 `expires`；建议签名时给 90 天有效期，过期后重新签，不延长旧 manifest。

## 5. 备份（所有者必做）

私钥丢失 ⇒ 无法再为已发布的 keyId 签名，只能按第 4 节轮换到新 keyId 并让客户端升级。
建议立刻做至少两份离线备份（例如加密压缩后放两台不同设备），并在备份标签上写明 keyId 与生成日期。
本项目不接受把私钥放进仓库、CI secret 之外的任何位置。

## 6. 谁验证过

`scripts/test-release-trust-root.js`（13 项）与 `scripts/test-kroma-js-upgrade.js`（109 项）在信任根配置后全部通过，
其中包括：生产根只含声明的 keyId、测试通道密钥被拒、冒充 keyId 的签名被拒、篡改载荷被拒。
