# 测试证据索引

所有命令都在 `/opt/kroma-p1` 执行。封板提交号见交付目录的 `DELIVERY.json`；日志头部记录了它们实际是在哪个树上跑出来的。

| 门禁 | 命令 | 平台 | 结果 |
| --- | --- | --- | --- |
| 全树格式 | `cargo fmt --all -- --check` | linux x64 | 0 差异 |
| 静态检查 | `cargo clippy --workspace --all-targets -- -D warnings` | linux x64 | 0 warning |
| Rust 全量 | `cargo test --workspace --no-fail-fast` | linux x64 | 见 `evidence/logs/rc1_full_suite.log` |
| Node 升级 | `node scripts/test-kroma-js-upgrade.js` | node v24.21.0 | 109 pass |
| 信任根 | `node scripts/test-release-trust-root.js` | 同上 | 13 pass（测试通道；生产根含 kroma-release-2026） |
| 发布流水线 | `node scripts/test-release-pipeline.js` | 同上 | 24 pass（含 RC1-03/04 新增 3 条） |
| 台账 | `node scripts/verify-ledger.js` | 同上 | ok |
| Action 钉住 | `node scripts/release-gate.js actions` | 同上 | 49 处（ci 17 + release 32）全 40 位 |
| 锁文件 | `node scripts/release-gate.js lock` | 同上 | 与钉住摘要一致 |
| OpenSSL 验真 | `node scripts/release-gate.js openssl` | 同上 | 四条路径统一；pin 为空 → 警告 + 运行期 fail-closed |
| Release DAG | `node scripts/release-gate.js dag` | 同上 | pack 依赖 quality；无 publish job |
| 双入口一致 | `cmp bin/kroma.js npm/kroma-cli/bin/kroma.js` | 同上 | 逐字节相同 |
| 真实模型基线 | `kroma bench --executor provider`（T24） | 同上 | 28 run / 8.77M token / Inconclusive |
| 沙箱隔离 | `scripts/run-sandbox-tests.sh` | linux x64（真 bwrap） | b13 21 + t09 8 passed，`evidence/logs/rc1_sandbox.log` |
| 签名负向 | 真密钥签错提交 / 签后改字节 | linux x64 | 两次均被拒，`evidence/logs/rc1_negative_checks.log` |

## 日志的出处

`rc1/evidence/logs/` 下的日志头部都写着 `# tree <git tree>` 与运行时间。它们是在**封板提交的树**上跑出来的；
封板提交与那棵树只差这些日志文件本身（日志无法记录自己的哈希）。此前有一版日志来自 `675fd0f`，
与最终代码不符，已被本轮重跑覆盖。

历史证据（阶段 1 的 T00–T30）仍在 `docs/review/phase1_test_results/`，本轮未改动，
其"历史日志"属性不代表本轮已复跑。
