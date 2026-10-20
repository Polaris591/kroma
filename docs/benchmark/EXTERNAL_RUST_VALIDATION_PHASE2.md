# BF-10～BF-12：仅限外部 Rust 工具链验收

**前置限制：本任务不是开发任务，只包含当前环境无法执行的编译验证。不能进行付费模型调用，不得推送仓库或正式发布，不得索取/展示 API Key 或生产签名私钥。**

1. 在独立分支导入本次增量补丁。记录 `git diff --check`、Rust 1.85.1、cargo、node、系统与构建平台。
2. 依次执行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p kroma-core --test bench_contract -- --nocapture
cargo test -p kroma-core --test bf01_foundation_v1_fingerprint -- --nocapture
cargo test -p kroma-cli
cargo test --workspace
cargo build --release -p kroma-cli
node --test scripts/test-bench-foundation.js scripts/test-bench-phase2.js
```

3. 在临时目录**只运行合成 Bench**：

```bash
tmpdir="$(mktemp -d)"
node scripts/bench-foundation.js freeze \
  --manifest experiments/foundation-v1/manifest.json \
  --suite experiments/foundation-v1/suite.json \
  --prices experiments/foundation-v1/prices.json \
  --executor synthetic --budget-tokens 10000 --seed 12345 \
  --out "$tmpdir/frozen"
node scripts/bench-foundation.js plan --lock "$tmpdir/frozen/lock.json"
node scripts/bench-foundation.js run \
  --lock "$tmpdir/frozen/lock.json" \
  --kroma ./target/release/kroma --out "$tmpdir/analysis"
cat "$tmpdir/analysis/metrics.json"
```

4. 观察 `source_fingerprint`、Rust `suite_fingerprint` 与 Bench 24 个预期 Job 是否符合 Lock。合成报告的 `promotion` 必须是 `not_applicable_synthetic`，不得出现真实模型成功率声明。
5. 若 OS 无法支持沙箱测试，清晰标明 `ENV_BLOCKED`；严禁删测试、改断言或补造通过日志。
6. 交付日志：`CARGO_FMT.log`、`CARGO_CLIPPY.log`、`CARGO_BENCH_CONTRACT.log`、`CARGO_FINGERPRINT.log`、`CARGO_TEST_WORKSPACE.log`、`CARGO_BUILD.log`、`NODE_PHASE2_TESTS.log`、`SYNTHETIC_SMOKE.log`、`SYNTHETIC_METRICS.json`，带每条命令退出码。错误请原样回传，后续由我优先修源码。

**不在本轮范围内：**付费 Provider 测试、生产私钥、外部评分签名、多 Agent 实测、真实 Release。不能把这些工作冒充为已通过。
