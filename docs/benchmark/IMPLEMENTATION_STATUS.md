# Benchmark Foundation v1 — 执行与验收台账

日期：2026-10-18。开发目标：第一批真实代码实现，不是空任务书。

## 已实施

| ID | 模块 | 状态 | 验证 |
|---|---|---|---|
| BF-01 | 版本化 Manifest v1、同模型/两臂、冻结输入及原始哈希 | IMPLEMENTED | Node 合同测试（36 项，全通过） |
| BF-02 | BenchTask 任务/Oracle/路径校验、任务重复 ID 与可满足性检查 | IMPLEMENTED | Node 负向测试 |
| BF-03 | Holdout split、预算矩阵上限和预注册决策门槛 | IMPLEMENTED | Node 合同测试 |
| BF-04 | Rust `BenchComparisonReport` 元数据与结果矩阵逐项验证 | IMPLEMENTED | Node 负向测试 |
| BF-05 | 结果脱敏、只写新目录、原始运行日志不进入交付报告 | IMPLEMENTED | Node 敏感字符串测试 |
| BF-06 | 成功率、每成功任务成本（严格允许未知）、任务簇配对 Bootstrap | IMPLEMENTED | Node 数值与负例测试 |
| BF-07 | 合成 Smoke 调用已有 Kroma CLI（无付费 API） | IMPLEMENTED; RUST BINARY NOT_RUN | Node 假可执行文件端到端测试 |
| BF-08 | Rust/Node Suite Fingerprint 桥接合同 | CODE WRITTEN; RUST NOT_RUN | Node/Python 独立散列交叉检查；Cargo 待验证 |

## 明确未完成 / 阻断

| ID | 工作 | 原因与下一步 |
|---|---|---|
| BF-09 | Native Rust 端到端运行、fmt/Clippy | 此环境无 Rust 工具链，网络 DNS 不可访问；外部 cargo 环境执行 |
| BF-10 | `--experiment` Manifest → 真正的可配置 Arm 执行 | RC1 只有 dry-run，当前实现拒绝任意 Arm 替换，下一代码里程碑 |
| BF-11 | 逐尝试缓存/计费与真实 Provider 成本单据 | Rust BenchArmResult 缺逐子模型身份；不能可信计算 Tournament 的统一费用 |
| BF-12 | 外部独立裁判、实时安全隔离、严格模型调用成本停止 | 必须加强 Rust 执行/验证边界，再做付费测试 |
| BF-13 | Pi/DSH/Codex 适配器和公平跨 Harness CI | 下一里程碑，先冻结可复现 Harness 对照协议 |
| BF-14 | 50–100 独立真实编码任务多次实测、独立复现 | 尚未运行；需要预算授权及真实可验证任务集 |

## 本地验收命令

```bash
node --check scripts/bench-foundation.js
node --check scripts/test-bench-foundation.js
node --test scripts/test-bench-foundation.js
node scripts/test-release-attestation.js
node scripts/test-release-pipeline.js
node scripts/test-provenance.js
node scripts/test-release-trust-root.js
node scripts/verify-ledger.js
```

安全要求：不把测试模拟输出当作 Provider 效果，不使用 Work 模式，不需要联网，不请求真实模型密钥。下一版本优先修 BF-10/11/12，在条件齐备前不提供“自助付费批跑”按钮或自动晋升按钮。


## 2026-10-18：BF-10～BF-12 增量里程碑（后续更新）

此前的 BF-10～BF-12 “未完成”描述保留作为 BF v1 历史记录，本段为新状态。

- **BF-10（部分实现）**：冻结 Manifest 可经 `plan/run` 指挥已有 Rust 合成 Bench，产出运行回执；Provider 自动运行仍因工具沙箱和授权缺口被拒。Rust 自身的 `--experiment` 仍只 dry-run。
- **BF-11（父请求记账代码已实现，Rust 待编译）**：新增请求级用量记录、模型锁定和价目表入口、Node 独立定价；子代理重试账目仍需新工程。
- **BF-12（离线证据合同已实现，独立评分服务未落地）**：支持冻结 Ed25519 评分公钥与精确字节绑定的外部签名，核验错误必须拒绝；绝不自动晋升。
- **本地验证**：Node Foundation 36 项 + Phase2 12 项均通过；原 RC1 发布测试全绿。Rust 编译、真实 Provider 与真实独立 Sandbox 未进行。详见 `BF10_BF12_IMPLEMENTATION.md` 和 `EXTERNAL_RUST_VALIDATION_PHASE2.md`。

## 2026-10-18 BF-13 additive milestone (not native-verified)

`BF13_CHILD_REQUEST_LEDGER.md` describes the new per-child/per-candidate request evidence: child diagnostic events → parent `ChildRequestUsageRecorded` → native Bench result → independent frozen-table repricing. All losing/failed Tournament candidates are included; missing usage, retries and cancellations remain unknown rather than free. This milestone does **not** enable real paid Provider auto-dispatch or independent scorer execution. See BF-13 document and its Rust-only follow-up gate.

## 2026-10-18 BF-14A / BF-14B incremental status (supersedes older planning rows)

- **BF-13**: accepted by external native Rust validation: fmt, `cargo check`, strict Clippy, all 1000 Rust tests, Release build, and 54 Node Bench tests passed on the fixed BF-13 tree. See the separate external BF-13 Rust gate evidence; the historical "Rust NOT_RUN" statements above apply to older source trees, not this baseline.
- **BF-14A**: Ed25519-signed, exhaustive isolation/scorer job matrix verification implemented and offline-tested. This does **not** demonstrate kernel-enforced isolation.
- **BF-14B/1 (this commit)**: operator-parent **offline** bwrap whole-run supervisor, fixed real namespace/denial/nested-subprocess probe and immutable parent-only receipt; synthetic CLI run can be executed under the whole-process boundary on a host with a real bwrap installation. Real Provider is still blocked. The signed scoring review now requires possession and exact digest checks of four raw artifacts for every signed job; without them no `eligible_for_manual_review`.
- **Local Node tests**: includes BF14B attack/rejection/host-control cases. Kernel preflight is **ENV_BLOCKED** when `/usr/bin/bwrap` is absent; it is never called PASS by an argv-only test. No Rust source changed by this BF-14B slice.
- **Open BF-14B gates**: no Provider credential/network broker; no Oracle-blind task runner; no per-job kernel execution/provenance; no live cgroup/resource budget; no independent scorer deployment; no real paid model trial. The next gate is a real OS probe on an authorized bwrap host, followed by architecture changes to split task execution from held-out Oracle data and credentials.

See `BF14B_KERNEL_SANDBOX_AND_RAW_EVIDENCE.md` and `BF14B_EXTERNAL_KERNEL_REVALIDATION_ONLY.md`.
