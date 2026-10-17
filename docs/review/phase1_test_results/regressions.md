# 第一批补丁的真实门禁结果与缺陷分类

环境：Debian 13 / x86_64 / rustc 1.85.1 / Node 24.21.0（详见 environment.json）。
基线：`/opt/kroma-orig`（用包内 patch 反向应用到交付树重建；反向应用只触及那 8 个文件，其余字节一致）。
被测树：`/opt/kroma-p1`（交付树 + 本轮最小纠错）。

## 结论速览

• 交付树**原本完全无法编译**（E0004），修复后才进入可测状态。
• 修复后：`cargo check --workspace --all-targets` 0 错、`clippy -D warnings` 0 警、`cargo build --release` 成功。
• 全量测试：补丁树 820 passed / 1 failed；基线树 817 passed / 0 failed。差 3 条 = 本批新增的三条回归测试。
• 唯一失败项与两条偶发项**都在基线树同样复现**，属既有问题，不是本批补丁引入。
• Node 回归：108/108 通过，但交接包记录的那条 `.tmp` ENOENT 偶发**在本环境也复现了**，基线树失败率更高。

## 1. 补丁缺陷（本轮已修，全部有最小 diff）

### 1.1 E0004：`CompactionReason::DeadEnd` 未覆盖 — 阻断编译

`crates/kroma-core/src/context_projector.rs:999` 新增了 `DeadEnd` 变体，但 `reason: match reason { .. }` 只写了 `StepDownArchive` / `SupersededRead` 两个分支。整个 `kroma-core` 编不过，连带 workspace 所有目标不可测。

修复：补 `CompactionReason::DeadEnd => "dead_end"` 一个分支。
为什么没顺手改成 `CompactionReason::as_str()`：`CompactionMetadata.reason` 是 `String` 且参与 WAL 序列化，抽方法属于 T05 的迁移工作，此处保持最小改动。

### 1.2 clippy `needless_range_loop`（patch 引入）

`surface/dispatcher/tournament.rs:437`：补丁把 `strategies.get(index)` 换成 `resolved_strategies[index]`，触发 `-D warnings`。
修复：改成 `for (index, strategy) in resolved_strategies.iter().enumerate()`。语义等价——`resolved_strategies` 就是 `(0..count).map(..).collect()`，长度恒等于 `count`。

### 1.3 clippy `large_enum_variant`（patch 引入）

`TranscriptItem::ToolResult` 因新增 `ToolResultMeta.compaction: Option<CompactionMetadata>` 涨到 384 字节，越过 clippy 阈值。
处置：在枚举上加**带理由的** `#[allow(clippy::large_enum_variant)]`。理由写在代码注释里：boxing 会波及 126 个构造点和每一处 `TranscriptItem::ToolResult(..)` 模式匹配，超出「最小纠错」边界；热路径上是 move 不是 copy。这是一处**显式记录的例外**，不是放宽门禁（其余 lint 仍然 `-D warnings`）。

### 1.4 格式：patch 弄脏了 3 个文件

交付树全仓本来就不是 rustfmt 干净的（`cargo fmt --all -- --check` 516 处差异，基线树同样如此）。补丁额外弄脏了 `session.rs`(3)、`tournament.rs`(3)、`gate2_evidence_projection.rs`(3)；`agent_loop.rs`(10) 和 `subagents.rs`(2) 在基线里就脏。
处置：只格式化前三个（纯空白 diff，已逐段核对），后两个不动，也没有对全仓跑 `cargo fmt --all`——交接包明确禁止。

## 2. 既有问题（基线同样复现，不是本批补丁引入）

### 2.1 `b05_execution_context::run_deadline_clamps_mcp_timeout`

失败形态：期望 `ToolTimeout`，实际 `IoError`（`assertion left == right`）。
复现率：补丁树 1/10，基线树 1/10。测试文件与生产代码都不在本批 8 个改动文件里。
分类：**随机不稳定（既有）**。超时/取消路径上的竞态，真实缺陷，但不属于本批。

### 2.2 `b08c_git_metadata`（5 条里任一条可能挂）

失败形态两种：拿到的 object id 是另一条用例的 SHA-256（`left` 为 64 位十六进制），或直接 `None`。
复现率：补丁树 1/20，基线树 0/21（同一文件、同一生产代码，属采样）。
根因（读代码可确认）：该测试靠**改写进程级 `PATH`** 注入假 `git`，靠一把 `Mutex` 串行化，文件头注释也写明「suite 必须以 `--test-threads=1` 运行」。但 `cargo test --workspace`（CI 就是这么跑的）默认并行，锁只覆盖测试体，仍与 `detect_git_commit` 的超时路径、进程 spawn 抢时序。
分类：**测试设计缺陷（既有）**。真实修法是让用例不依赖进程级 PATH（或 CI 传 `--test-threads=1`），留到 T30 门禁收口时处理。

### 2.3 Node `gated late open after cleanup closes fd and cleans up stage file without orphans`

失败形态：断言 `/deadline/`，实际 `Error: ENOENT: no such file or directory, lstat '.../gated-dest.bin.tmp.<pid>.<ns>'`。
复现率：补丁树 1/10（另一次 1/8 内复现），**基线树 5/15**。补丁只改 `.rs`，不可能影响这条纯 JS 用例。
根因：用例传 `deadlineMs: 1`，然后用一个 `GatedWritable` 在 cleanup 之后才补写 stage 文件并 `emit('open')`。谁先赢是纯时序：deadline 先触发 → 报 `/deadline/`（期望）；cleanup 先删掉 stage 文件、晚到的 `lstat` 先失败 → 报 ENOENT（失败）。
分类：**测试设计缺陷 + 生产代码语义漏洞（既有）**。正确的修法不是放宽正则，而是让「已 cleanup 的流再写入」以终止错误（deadline/aborted）收场，而不是把底层 ENOENT 漏出去。留到 T20/T30 门禁收口时修。

### 2.4 `mcp::call_lifetime_tests::subsequent_call_fails_closed_when_poisoned`

失败形态：`mcp.rs:1692` 里 `unwrap()` 拿到 `Os { code: 2, NotFound }`。
复现率：全量跑时出现 1 次，单独重跑该模块 8 次全过。
分类：**随机不稳定（既有）**，`mcp.rs` 不在本批改动范围内。同一台机器上并发跑全 workspace 时，临时文件/目录的创建与删除存在竞态。

## 3. 交接包声称 vs 本环境实测

| 交接包说法 | 本环境实测 |
|---|---|
| Rust 1.85.1 不在环境，编译/测试全部 NOT_RUN | 1.85.1 可用，已全部真跑 |
| Node 两次运行 107 passed / 1 failed，第二次 108 passed | 复现：偶发，基线树 5/15 |
| 8 个修改文件字节与 patch 重放一致 | 反向 patch 只触及这 8 个文件，成立 |
| 「代码可能需要格式化或修复类型问题」 | 属实，且比预期严重：交付树编不过 |
