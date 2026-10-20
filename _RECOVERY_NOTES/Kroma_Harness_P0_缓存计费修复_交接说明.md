# Kroma e95845b / P0-01 缓存写入费用修复交接说明

> **性质：** 可应用的源码补丁 + 静态校验；**未完成 Rust 编译、测试、真实 API 验证，不能合入生产**。研究报告为 `Kroma_Harness_2026-10-20_前沿架构审计与升级蓝图.md`。

## 症状与准确行为

`crates/kroma-core/src/provider/openai_responses.rs` 的 `parse_responses_usage()` 把 `cache_creation_input_tokens` 永久置为 `None`，不符合新 Responses API 返回 `usage.input_tokens_details.cache_write_tokens` 的语义。`context_economy.rs` 的 `RunEconomy` 未保存写入 Token，导致其成本比较不能体现新模型 1.25x 等写入费率。

**本补丁只修复成本账目，不包含 GPT-5.6+/GPT-6 Prompt Cache 显式断点、tool search、MCP 2026、新 compaction。** 它们需按主研究报告另立开发单，并作 provider-capability 分支。

## 变更范围

| 文件 | 修改 | 兼容约束 |
|---|---|---|
| `crates/kroma-core/src/provider/openai_responses.rs` | 提取 `input_tokens_details.cache_write_tokens`；添加写入/零/缺失/完整响应 3 个测试 | 老模型缺失字段仍 `None`，明确的 0 仍 `Some(0)`；SSE 和非流复用同一函数 |
| `crates/kroma-core/src/context_economy.rs` | `RunEconomy` 增加可选写入 Token，纳入输入汇总和实际费用；`EconomyComparison` 增写入 delta；加 2 个测试 | serde 兼容旧报告，旧的未知写入状态不能冒充价格已知 |

## 已执行检查（可复现）

- `git apply --check` 对 e95845b 基线成功。
- `git diff --check` 无空格/冲突格式问题。
- 两个文件共 129 行新增、7 行删除。
- **没有 Rust toolchain**：`cargo`, `rustc` 在本轮容器 PATH 中均不存在，**不得宣称 5 个新测试已经通过**。

## 在开发环境的执行顺序

```bash
# 在 e95845b 完整源码根目录执行；先确认当前工作树没有待保存的修改

git apply --check Kroma_e95845b_P0_缓存写入计费修复_未编译.patch
git apply Kroma_e95845b_P0_缓存写入计费修复_未编译.patch

cargo fmt --all -- --check
cargo test -p kroma-core --lib cache_write_accounting_tests -- --nocapture
cargo test -p kroma-core --lib cache_writes_cannot_be_mispriced_as_free_or_uncached
cargo test -p kroma-core --lib cache_write_unknown_is_not_a_measured_zero_on_writing_models
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings
```

**必须追加的实网验收**：对同一个支持 `cache_write_tokens` 的 GPT-5.6+/GPT-6 模型，抓取带脱敏的使用量，验证解析值与实际 provider usage 一致；用生效的版本化价格表计算输入费用，确保输入 Token 被恰好分到 `uncached + cached + cache_write` 三段，并且 missing rate / missing field 导致 `cost unknown` 而不是“节省”。同时测试旧模型没有字段的 fallback。若真实 usage 模式与假设不符，停止推广并调整解析/报价规则。

## 重要额外审计事项

1. `docs/CAPABILITIES.md` 仍标缓存 canary `NOT_RUN`，需真实 API 后才能变更。
2. `context_economy.rs` 目前是库级比较器，不代表每一个 CLI/RPC 真实请求已接通这条比较路径；应同时审核 `usage_cost::CostLedger` 与 benchmark 的结果接线。
3. `README.md` 与 `DIRECT_REPAIR_VERIFICATION_2026-10-19.md` 对 sandbox 覆盖范围有新旧口径冲突，另立专项核验，不应被 P0 账目修复掩盖。
4. 只有完成 Rust 全量门禁和真实 provider canary 后，才能把该修复称为“验收通过”。

## 回退

```bash
# 只有已应用此 patch 且无后续同文件更改的工作树才可直接反向应用
git apply -R Kroma_e95845b_P0_缓存写入计费修复_未编译.patch
```
