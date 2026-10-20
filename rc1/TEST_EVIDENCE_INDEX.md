# 测试证据索引

所有命令都在 `/opt/kroma-p1` 执行，提交见 `RELEASE_MANIFEST.json` 的 `git_commit`。

| 门禁 | 命令 | 平台 | 结果 |
| --- | --- | --- | --- |
| 全树格式 | `cargo fmt --all -- --check` | linux x64 | 0 差异 |
| 静态检查 | `cargo clippy --workspace --all-targets -- -D warnings` | linux x64 | 0 warning |
| Rust 全量 | `cargo test --workspace --no-fail-fast` | linux x64 | 见 `evidence/logs/rc1_full_suite.log` |
| Node 升级 | `node scripts/test-kroma-js-upgrade.js` | node v24.21.0 | 109 pass |
| 信任根 | `node scripts/test-release-trust-root.js` | 同上 | 13 pass（测试通道；生产根含 kroma-release-2026） |
| 发布流水线 | `node scripts/test-release-pipeline.js` | 同上 | 24 pass（含 RC1-03/04 新增 3 条） |
| 台账 | `node scripts/verify-ledger.js` | 同上 | ok |
| Action 钉住 | `node scripts/release-gate.js actions` | 同上 | 25 处全 40 位 |
| 锁文件 | `node scripts/release-gate.js lock` | 同上 | 与钉住摘要一致 |
| OpenSSL 验真 | `node scripts/release-gate.js openssl` | 同上 | 四条路径统一；pin 为空 → 警告 + 运行期 fail-closed |
| Release DAG | `node scripts/release-gate.js dag` | 同上 | pack 依赖 quality；无 publish job |
| 双入口一致 | `cmp bin/kroma.js npm/kroma-cli/bin/kroma.js` | 同上 | 逐字节相同 |
| 真实模型基线 | `kroma bench --executor provider`（T24） | 同上 | 28 run / 8.77M token / Inconclusive |

历史证据（阶段 1 的 T00–T30）仍在 `docs/review/phase1_test_results/`，本轮未改动，
其"历史日志"属性不代表本轮已复跑。
