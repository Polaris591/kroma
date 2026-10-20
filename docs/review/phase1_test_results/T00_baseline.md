# T00 · 冻结源码与真实可执行基线

状态：**完成**（2026-10-17）

## 1. 源码身份

| 项 | 值 |
|---|---|
| 交付包 | `Kroma_第一批修改与DeepSeek验收包.zip`（QQ 传输，2.0 MB） |
| 交付的修改源码 | `modified_source/Kroma_修改源码_第一批.tar.gz`，sha256 `0bf8e517bcd4ae2f9369f84925332bcc0e3fcd0d2d2f6cd807efa9102a1fd86a` |
| 上游原始包（本机没有） | `Kroma-源码-20261017.tar.gz`，sha256 `73adf8a6d5afccde8d7cfbe5ff75b6afb597889f19685a180c937513537896ee` |
| 补丁 | `patch/Kroma_第一批修复.patch`，sha256 `9c1dd0a9ee6929b50dbd32a59553f4be0f79e899d6560a06751614d482ca3e09` |
| crate 版本 | 1.5.0 |
| Cargo.lock sha256 | `caa80f28083d1243d9c8e2d0e873404620cd9adf0a052159d286536ccf713f8c` |
| 规模 | 119,714 行 Rust，47 个集成测试文件 |

**原始基线是重建的，不是原件**：原始 tar 不在交付包里。做法是 `patch -p1 -R` 反向应用到交付树，反向应用只触及补丁里的那 8 个文件（`git diff --stat` 已核对），所以对这 8 个文件之外的一切，重建基线与原件逐字节相同；对这 8 个文件，重建基线与「patch 正向应用到原件」得到的结果一致（这正是补丁可重放性的定义）。这一点在 environment.json 里同样写明。

## 2. 两棵树

| 路径 | 内容 |
|---|---|
| `/opt/kroma-p1` | 交付树 + 本轮最小纠错，独立 git 仓库（与所有者的 GitHub 仓库无历史关系） |
| `/opt/kroma-orig` | 重建的原始基线 |

## 3. 真实可执行基线（同一组门禁，两棵树都跑）

| 门禁 | 交付树（修复前） | 交付树（修复后） | 原始基线 |
|---|---|---|---|
| `cargo check --workspace --all-targets` | **FAIL**（E0004，`context_projector.rs:999`） | PASS | PASS |
| `cargo clippy --workspace --all-targets -- -D warnings` | FAIL（2 条，均补丁引入） | PASS | PASS |
| `cargo test --workspace --no-fail-fast` | NOT_RUN（编不过） | 820 passed / 1 failed | 817 passed / 0 failed |
| `cargo build --workspace --release` | NOT_RUN | PASS（16,216,200 字节） | 未跑（无发布诉求，且磁盘紧张） |
| `node scripts/test-kroma-js-upgrade.js` | PASS 108/108 | PASS 108/108 | PASS 108/108 |
| `cargo fmt --all -- --check` | 516 处（既有） | 516 处（既有，只格式化补丁弄脏的 3 个文件） | 516 处（既有） |

补丁树比基线多 3 条测试，正是本批新增的回归测试（T01 矩阵、T05 伪造 marker、T07 WAL 容量）。

## 4. 失败项归属

唯一失败项 `b05_execution_context::run_deadline_clamps_mcp_timeout` 与两条偶发（`b08c_git_metadata`、Node `gated late open`）**在基线树同样复现**，详见 `regressions.md`。三条都不是本批补丁引入。

## 5. 本轮最小纠错提交

1. `fix(context_projector): map CompactionReason::DeadEnd in the stub metadata` —— 恢复可编译
2. `fix(lint): satisfy clippy on the patched tree (needless range loop, large enum variant)`

## 6. 对「31 项已完成」说法的判定

**不成立。** 交付包只声称 T01/T02/T05/T07 的局部修复，且原文自己写了「尚无 Rust 运行验证」。实测这些修复确实能编译、能过测试，但：
• T01 缺真实工具入口（`spawn_subagent`）的端到端证明、operator-only 审计、冻结 oracle/protected paths；
• T02 缺「审批期间工作区内容漂移」的独立监控，CLI/工具/RPC 三入口还没共用同一份 canonical intent；
• T05 缺旧 P1 WAL（无 EvidenceLink/Typed Metadata）的迁移测试；
• T07 没有分段 WAL/rotation，也没有增量序列化（大 JSON 仍在限额检查前生成）。
四项都只能记为 CODE_PATCHED，不能记为 PASS。
