# Kroma Benchmark Foundation — BF-10～BF-12 代码迭代

日期：2026-10-18。基于已修复 RC1 + Foundation v1 源码。**本文件是实际修改记录，不是设计提案。**

## 总体状态

| 项目 | 已实现部分 | 尚未完成 / 不得假设 |
|---|---|---|
| BF-10 实验计划 → 原生 Runner | `freeze → plan → run → evaluate`；读取三项冻结输入，实际通过 argv 调用现有 Rust `kroma bench` 的**合成执行器**；严格校验矩阵、预算、指纹，生成脱敏报告与执行回执 | Native Rust `--experiment` 本身仍是 dry-run。**真实 Provider 自动批跑仍封闭**：Runner 内的最终判题沙箱不等于所有模型工具都在 OS 沙箱执行，且未获得付费授权 |
| BF-11 请求级成本 | Rust `BenchArmResult.request_usage` 新增父 Agent 每次 `ModelRequestCompleted` 的无正文证据：模型身份、实际发送请求 SHA、输入/输出、缓存读写、缓存写入、独立估价。CLI 新增 `--bench-model` 和 `--price-table`，支持预先锁定模型/价目表。Node 按每次请求分别用冻结费率重算，并拒绝错误的混合模型总价 | 子 Agent / Tournament 的逐请求账目仍未统一汇集；缺少重试真实用量、缓存拆分或返回模型身份时，总费用必须 `null`；不是 Provider 原始账单 |
| BF-12 独立判题与验证 | 冻结独立 Ed25519 评分公钥指纹，验证外部签名绑定的源码、Lock、原始报告字节、完整任务规则、沙箱与评分者身份；攻击性测试覆盖伪造沙箱 PASS / 换密钥 / 报告篡改；签名满足后最多进入人工审查 | 没有部署或运行真实独立评分服务；签名证明“哪个评分方做了声明”，不能证明评分者说的每件事都是真的；更不能自动晋升策略 |

## 1. 离线执行：冻结 Manifest 实际驱动 Rust CLI

```bash
# 在有 Rust 可执行文件的环境运行，不需要 API Key
node --test scripts/test-bench-foundation.js scripts/test-bench-phase2.js

node scripts/bench-foundation.js freeze \
  --manifest experiments/foundation-v1/manifest.json \
  --suite experiments/foundation-v1/suite.json \
  --prices experiments/foundation-v1/prices.json \
  --executor synthetic --budget-tokens 10000 --seed 12345 \
  --out /tmp/kroma-bf10-frozen

node scripts/bench-foundation.js plan --lock /tmp/kroma-bf10-frozen/lock.json

# 真正执行 `kroma bench --suite ...`，不使用原生 `--experiment`（它目前只打印 dry-run）
node scripts/bench-foundation.js run \
  --lock /tmp/kroma-bf10-frozen/lock.json \
  --kroma ./target/release/kroma \
  --out /tmp/kroma-bf10-execution
```

运行结果包含 `metrics.json`、`report.md`、`results.redacted.json`、`execution-receipt.json`。最后一项绑定 Lock 和实际可执行文件 SHA-256、原始 Native 报告 SHA-256、实际任务数与工具退出状态。原始报告仅在私有临时目录保存并随运行清理。`run` 可另传 `--expect-lock-sha256`（外部冻结的摘要）与 `--timeout-ms`。

**当前不提供 Node 自动 Provider 执行入口**：`freeze --executor provider` 和 `plan` 可生成不可变配置与明确的待执行命令；`run` 遇到 Provider Lock 会 fail-closed，直到实际工具沙箱与付费停止门禁通过验收。不能为了赶进度把 `--allow-write` 或隔离型的评分器当作对所有 Bash/工具的系统级隔离。

## 2. 请求级成本：防止错误计价

Rust 文件：

- `crates/kroma-core/src/bench.rs`：`BenchRequestUsage` + `BenchArmResult.request_usage`, `request_attempts_used`, `request_usage_complete`；从原始 `ModelRequestCompleted` 事件取值，绝不复制响应正文和环境变量。
- `crates/kroma-cli/src/main.rs`：`--bench-model MODEL` 锁定实际请求模型配置（不是伪造服务方返回身份）；`--price-table FILE` 从最大 1 MiB、非符号链接文件读取 `PriceTable` 并传入 `ProviderBenchSettings`。
- `scripts/bench-foundation.js`：独立验证逐请求实际返回模型、输入总量、缓存读取、缓存写入、输出、单价与 Rust 小计，校验没有遗漏的尝试、没有重复请求 SHA。

“已知成本”需要同时满足：每次父请求都被事件记录、无丢失重试、确切模型身份、Provider 真实 Token 用量与完整缓存分账、冻结价格表精确覆盖模型、没有不能逐次拆开的子代理消耗。否则显示 `null`，不能把未知费用当成零。现在候选多代理策略原则上仍未知，属于 BF-11 的后续研发缺口。

运营者价目表不是 Provider 账单；要测“真正美元支出”，仍需账单 / API Usage 交叉核验。

## 3. 独立评分 Ed25519 协议（仅在真实可信评测环境使用）

### Freeze 时预注册**公钥**

```bash
node scripts/bench-foundation.js freeze \
  --manifest /trusted/manifest.json --suite /trusted/suite.json \
  --prices /trusted/prices.json --executor provider \
  --budget-tokens 10000 --seed 12345 \
  --scorer-key /trusted/scorer-public.pem \
  --out /immutable/frozen-provider
```

注意以上命令只是冻结配置，不调用 Provider。**私钥不进入 Kroma 进程、Agent 工作区、仓库、测试 Artifact 或聊天窗口。**

独立评分方必须在 Agent 不可访问的独立 OS 权限与沙箱环境中，完成全部任务的独立评分、冻结资源核对、Run/Source 身份核对，并将隔离日志写入不可被 Agent 修改的介质。实现上必须是**真实验证事实后**才能出具以下 `payload`：

```json
{
  "experiment_id": "EXPERIMENT-ID",
  "lock_sha256": "64-HEX",
  "report_sha256": "64-HEX",
  "suite_sha256": "64-HEX",
  "oracle": "BenchTask::evaluate:v1",
  "source_sha256": "64-HEX",
  "scorer_binary_sha256": "64-HEX",
  "isolation_proof_sha256": "64-HEX",
  "sandbox_passed": true,
  "all_jobs_isolated": true,
  "scorer_unwritable_by_agent": true
}
```

使用递归按 JSON 键排序、无空格的 `canonicalJson(payload)` UTF-8 字节签名（Node 同目录导出 `canonicalJson` 可作权威一致性检查）。外层文件：

```json
{
  "schema": "kroma.scorer-attestation/1",
  "payload": { "...": "上面的字段" },
  "signature_b64": "ED25519-SIGNATURE-BASE64"
}
```

然后由**与模型执行环境分离**的分析环境运行：

```bash
node scripts/bench-foundation.js evaluate \
  --lock /immutable/frozen-provider/lock.json \
  --report /scorer-evidence/rust-bench-report.json \
  --attestation /scorer-evidence/score-attestation.json \
  --scorer-key /trusted/scorer-public.pem \
  --expect-lock-sha256 YOUR_EXTERNALLY_PINNED_LOCK_SHA256 \
  --out /immutable/assessment
```

验证逻辑会检查：公钥指纹等于 Freeze 时预注册的指纹；签名与内容相符；原始报告和 Lock 在本次校验中使用**一次读取的字节**计算 SHA，避免易见的前后读取不一致；Oracle 与来源身份匹配；三个隔离断言全为真；否则立即拒绝。即使签名通过，也只输出 `eligible_for_manual_review`（仅当统计效果也过门槛），绝不会自动发布或替换默认策略。**可信签名并不代替可信评测器运行日志。**

## 4. 验收记录与禁止误述

本次迭代添加 `scripts/test-bench-phase2.js`：12 项离线合同测试；Foundation v1 旧 36 项回归全部通过（48/48）。RC1 原有 Node 发布测试、Provenance、签名信任根已复跑通过。Rust 改动尚无 Cargo 编译证据，此环境没有 Rust 工具链、不能解析 rust-lang 下载主机；严禁声称 cargo fmt / clippy / test 已通过。

没有进行任何真实模型 API 调用、真实子代理跨模型计费、真实沙箱隔离和正式 Benchmark 统计。测试里的签名均使用**临时测试私钥**，不能用于正式评分认证。

## 5. 下一批需要完成什么

1. 在真实 Rust 工具链环境完成 CLI、Core、全 Workspace 编译及原生测试，修正若有的 rustfmt / Clippy / 类型问题（可将日志反馈由我负责修复）。
2. 做真正的模型工具执行沙箱（不是仅把最后评分命令放进沙箱）；验证不可读模型凭证、不可改冻结评分器、工作区外写入禁止、进程取消与网络禁用，必要时在隔离容器内运行整个 Agent。
3. 将每一次子代理、Tournament、Provider 重试（尤其失败但已经发送的请求）在父 Session 中以可审计账目记录；如 Provider 根本不返回账目，应明确无法计价。
4. 再实现带真实授权的 Provider 执行与**运行中**美元/Token/时间停止；当前静态 token 矩阵上限并不能限制 Provider 最终账单。
5. 建设独立外部 Oracle Runner 与测试集治理、签名服务隔离；然后才开展 Kroma / Pi / DSH 同模型 Benchmark。

完整回滚：本迭代与 Foundation v1 的差异仅限脚本、Core Bench、CLI Bench 与其测试/文档；RC1 Release、权限核心、WAL、Sandbox 原实现没有修改。撤销本迭代提交即可恢复 BF v1。禁止把当前开发产物误用作已签名 RC1 正式 Release。
