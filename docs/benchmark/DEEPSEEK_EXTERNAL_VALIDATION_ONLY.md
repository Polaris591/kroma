# DeepSeek 最小外部验收任务：Benchmark Foundation v1

## 原则

本任务只有当前 ChatGPT 容器无法执行的 **Rust 原生编译与真实二进制 Smoke**。其余 Node 开发、测试、发布代码审查已由 ChatGPT 实际完成，禁止重复开发或擅自改架构。

- **严禁**调用付费模型 / 使用 KROMA_API_KEY 发起调用 / 运行 `--executor provider`；
- **严禁**直接推送、创建 Release、使用生产签名密钥；
- **严禁**根据运行输出删除失败测试、修改冻结样本让它变绿，或把合成成绩解释为模型真实能力；
- 若发现确凿代码缺陷，可出最小补丁和复现日志，但应先报告原因及影响。

## 1. 环境

在联网、有 Rust pinned toolchain 的 Linux 工作机：

```bash
rustup show active-toolchain
rustc --version
cargo --version
node --version
```

记录 OS、Rust、Node、sandbox 环境。源码使用 Kroma Benchmark Foundation v1 完整源码包，不能把它混用成旧 RC1 发布候选签名包。

## 2. Rust 静态与编译

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p kroma-core --test bf01_foundation_v1_fingerprint -- --nocapture
cargo test --workspace
```

如操作系统不具备某类真实沙箱功能，只能明确记为 `ENV_BLOCKED`，不得静默跳过后声称全量通过。可以按照原项目规定分开执行普通 Rust 测试和沙箱依赖测试，并报告两类状态。

## 3. 本地无 API 的实际 Rust CLI 烟雾测试

```bash
cargo build --release -p kroma-cli
npm run test:bench

out="$(mktemp -d)"
node scripts/bench-foundation.js freeze \
  --manifest experiments/foundation-v1/manifest.json \
  --suite experiments/foundation-v1/suite.json \
  --prices experiments/foundation-v1/prices.json \
  --executor synthetic \
  --budget-tokens 10000 \
  --seed 12345 \
  --out "$out/frozen"
node scripts/bench-foundation.js smoke \
  --lock "$out/frozen/lock.json" \
  --kroma ./target/release/kroma \
  --out "$out/analysis"
cat "$out/analysis/report.md"
```

预期 `smoke` 标记 `not_applicable_synthetic`。**必须**记录 Rust `suite_fingerprint == 8b51d4aae03f6c7c`。若失败，回传实际序列化差异，不要改成绕过 Fingerprint 校验。

## 4. 最小回传文件

`ENVIRONMENT.md`（环境摘要）、`CARGO_FMT.log`、`CARGO_CLIPPY.log`、`RUST_FINGERPRINT_TEST.log`、`RUST_WORKSPACE_TEST.log`、`SYNTHETIC_SMOKE.log`、`SYNTHETIC_METRICS.json`，以及每项 exit code。不要附原始含密钥环境变量或完整模型轨迹。

完成以上事项后停止。`BF-10/11/12`（真实模型 Manifest 接线、子模型成本、真实隔离）是下一代码里程碑，**不是**本次外部验收任务。
