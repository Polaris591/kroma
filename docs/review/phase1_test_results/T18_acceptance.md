# T18 · 请求级成本与实际运行身份 — 验收

阶段 G3｜依赖 T04, T06, T11, T14, T17｜关联 KR-15, KR-23, KR-24

## 一、缺陷

**成本侧**（`usage_cost.rs`）：

1. `TokenRates` 没有 cache **写入**单价。`cache_creation_input_tokens` 被算进 `input - cached`，
   于是写入按「未缓存」计价——Anthropic 的写入是基础输入价的 1.25 倍，暖机轮次被系统性低估。
2. `estimate(model, usage)` 只接受**一个**模型名。整轮 run 用「最后一次上报的模型」定价：
   重试落到别的 checkpoint、或子代理用别的模型时，全部 token 按错的价目表算。
3. 没有「总成本是否完整」的概念：估算或缺失 usage 都可能悄悄变成一个数字，读起来像实测。

**身份侧**（`provenance.rs`）：

4. VCS 探测用无超时的 `Command::output()`：大仓库或网络文件系统上 `git status` 能把整轮 run 挂住。
5. 源哈希用 `std::fs::read` **整文件读**：size 检查之后文件继续长大就突破预算。
6. 读不到的文件被 `continue` **静默跳过**，产出的却是一个「完整」的哈希。
7. 文件的可执行位不参与哈希：`chmod +x` 不改变身份。
8. 请求身份没有记录：报告的哈希对应的是「配置」而不是「真正发出去的请求」。

## 二、改了什么

### 成本

- `TokenRates` 新增 `cache_creation_input_nano_usd: Option<u64>`（serde default）。
  **有写入但价目表没写写入单价 → 该请求 unknown**，而不是按读价或未缓存价硬算。
- `UsageBasis { Measured, Estimated, Unknown }`：字节估算不能冒充 provider 实测。
- `CostLedger` + `AttemptCost`：**逐请求**记录 requested/reported model、basis、`retry_of`、
  以及该请求自己的 `CostEstimate`。定价永远跟随 **reported** 模型（provider 没说才回落到 requested）。
- `CostSummary`：`measured_attempts / estimated_attempts / unknown_attempts`、
  `measured_nano_usd`、`estimated_nano_usd`（两者**不相加**）、`by_model_nano_usd`、
  `incomplete` 与 `unknown_reasons`。`total_nano_usd()` 在不完整时返回 `None`——
  报告拿不到一个看起来像实测的数字。

### 身份

- `git_probe(program, root, args, budget)`：有界（走 T10 的 `run_bounded`），返回
  `GitProbe { stdout, timed_out }`。**超时 ≠ 命令失败**：超时是「身份未知」，
  失败可以是「不是仓库」。`fingerprint_explicit` 把超时记进 `git_probe_timeout` 并标记身份不完整。
- `hash_source_tree` 的读取改走 T06 的 `read_bounded_file`（`Overflow::Refuse`），
  上限是**剩余预算**，所以边读边长也不能突破。
- 任何读失败都置 `source_identity_incomplete` + `incomplete_reason`，不再静默跳过。
- Unix 下把「是否可执行」一位写进哈希：`chmod +x` 改变身份。
- `request_identity(system_prompt, tools, messages)`：在**装配与激活之后**对真正要发的请求求 sha256
  （工具按名字排序，所以集合相同顺序不同不改变身份），写进 `ModelRequestCompleted.request_identity_sha256`。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t18_usage_cost` | 5 passed / 0 failed |
| `cargo test -p kroma-core --test t18_provenance_identity` | 7 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 见 `logs/test_t18.log` |

### 成本测试（5 条）

- `cache_creation_and_read_have_distinct_rates`：1000 输入（100 未缓存 + 600 读 + 300 写）+ 100 输出，
  价目 100/10/125/400 → **93 500** nano-USD；按读价算写入只有 59 000（低估 37%），测试同时钉住这个反例；
  价目表缺写入单价时 → unknown 且理由含 `cache-creation`；没有写入时同一张表仍然可算。
- `mixed_returned_models_are_priced_per_attempt`：attempt 0 上报 model-a、attempt 1（重试）上报 model-b
  → `by_model` 分别为 140 000 / 280 000、合计 420 000；断言它**不等于**「全按最后一个模型」的 560 000
  或「全按第一个模型」的 280 000。
- `byte_estimate_cannot_claim_provider_measured_cost`：估算 basis → `measured_nano_usd == 0`、
  `estimated_nano_usd > 0`、`incomplete == true`、`total_nano_usd() == None`。
- `unknown_retry_usage_makes_total_cost_incomplete`：实测一轮 + 无 usage 的重试 →
  `unknown_attempts == 1`、总数不完整、已知部分仍报告为下限；模型不在价目表 → unknown 且理由含
  `no exact rate`。
- `usage_basis_labels_are_distinct`：三个 basis 互不相等且字符串固定，记录里保留 requested 与 reported。

### 身份测试（7 条）

- `git_probe_timeout_is_observable`：一个 `sleep 30` 的假 git，300ms 预算 → `timed_out == true`、
  无输出、5 秒内返回；`/bin/false` 只是失败、**不**算超时。
- `growing_source_cannot_exceed_hash_read_budget`：树里放一个 300 MiB 稀疏文件（超出 256 MiB 预算）
  → `capped == true`、身份不完整、理由含 `bound`，且哈希在 20 秒内结束（没有真去读 300 MiB）。
- `unreadable_source_never_yields_complete_identity`：同上路径触发「身份不完整」，
  `summary()` 里也显示 `capped`（不只是结构体字段）。
- `running_binary_from_other_repo_does_not_change_build_id`：外部二进制不改变 `source_sha256`，
  `binary_sha256` 单独记录且与源哈希不同。
- `executable_mode_change_changes_reproducibility_identity`：0644 → 0755 后源哈希变化，文件数不变。
- `on_demand_report_hash_matches_captured_wire_request`：真实 run（on-demand 工具模式下工具集与配置不同），
  从 provider 捕获实际请求，用同一函数重算，与 WAL 里 `ModelRequestCompleted.request_identity_sha256`
  **逐字相等**；改一个字节则不等。
- `ordinary_source_tree_still_identifies_completely`：普通目录身份完整；`fingerprint`（启发式入口）
  对非仓库目录**故意不**遍历（进程从 `/` 启动时不能哈希整个文件系统），`fingerprint_explicit` 才哈希。

### 变异验证

- 去掉哈希里的可执行位 → `executable_mode_change_changes_reproducibility_identity` 红
  （两个不同 mode 得到同一个 `d728dd17…`）。

## 四、边界与未完成

- `per_arm_prefix_identity_is_not_collapsed`（未实现，无同名测试；每个 arm 的前缀身份不能被合并）与
  `runtime_injection_changes_request_identity_not_historical_wal`（运行时注入只改请求身份、不改历史 WAL）
  这两条**未实现/未测**：前者属于 bench 调度层（T24），后者需要注入路径的端到端用例。
- `unreadable_source_never_yields_complete_identity` 的「读失败」分支在测试进程里是 root，
  `chmod 000` 依然可读，所以用「超出预算」这条同源路径触发 `incomplete`；
  读错误分支本身与它共用同一处赋值，但没有独立的 EACCES 用例。
- 成本账目目前是**库级能力**（`CostLedger`），还没有接进 bench 报告与 CLI 输出；
  接线需要 T24 的真实执行，届时 `BenchComparisonReport` 应改用 `CostSummary` 而不是单一
  `estimated_cost_nano_usd`。
- 价目表仍是运维提供的估算，不是上游账单；`basis` 字段一直写明这一点。
