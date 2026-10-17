# T20 · 更新能力文档与版本化验证索引 — 验收

阶段 G3｜依赖 T11, T17, T18, T19｜关联 KR-34

## 一、缺陷

文档与代码各说各话，而且没有任何机制能发现：

1. 没有能力清单：哪些能力「实现了」「接线了」「有契约测试」「真机跑过」「默认开启」全靠散文。
2. 措辞把不同的隔离混为一谈：CAS 快照 / Git worktree / 宿主 shell / 沙箱 被写成同一种「隔离」；
   合成 bench 与实网基准都叫「基准」；AHE 的「预测成立」读起来像「自动自愈」。
3. 历史 Gate 报告与当前证据混在一起，旧报告可以被当成新门禁的通过依据。
4. 版本号散落在 `Cargo.toml` / `package.json` / `npm/kroma-cli/package.json`，没有一致性检查。

## 二、改了什么

### 1. 机器可校验的能力清单

- `crates/kroma-core/src/capability.rs`：`CapabilityManifest` / `Capability` / `CapabilityEvidence`，
  五列分别是 `implemented` / `wired`（入口列表）/ `contract_tested` / `live_tested` / `default_on`。
  `validate()` 拒绝自相矛盾的声明：
  - 未实现却 `default_on`；
  - `default_on` 却没有任何入口；
  - `contract_tested` 却没有具名测试；
  - `live_tested` 却没有真机证据；
  - 未知入口名、重复 id、未知字段（`deny_unknown_fields`）。
  `validate_evidence(sources)` 进一步把**每个引用的测试名**对仓库源码求存在性。
- `docs/capabilities.json`：20 条能力，逐条标注任务号、入口、证据与**已知限制**。
- `docs/CAPABILITIES.md`：人读版本（表格 + 措辞纠正 + 证据导航 + 历史报告归档规则 + 版本一致性 + 许可证留白）。

### 2. 措辞纠正

- README 的 `bash` 段落改为「默认不是沙箱；`execution_sandbox` 是显式选择，且当前只覆盖 `bash`，
  后端不可用时拒绝执行而不是回退宿主」。
- README 的锦标赛段落补上「worktree 是 git 的文件隔离，不是 OS 安全边界」，
  并写明自修复只产出候选补丁与报告、提升恒为人工决定。
- `docs/CAPABILITIES.md` 里逐条对照 CAS / worktree / 宿主 shell / 沙箱、合成测试 / 实网基准、
  预测成立 / 自动自愈这三组最容易混淆的说法。

### 3. 历史 Gate 报告：归档为历史

`docs/GATE*_CI_LOG.txt`、`GATE2_EVAL_METRICS.json`、`P0_*`、`NEXT_GEN_BLUEPRINT_v1.0.md` 保留原样，
但在能力文档里明确标注为「v1.5.0 之前的评审产物，不能用于当前门禁」。
`CapabilityManifest::evidence_is_current(report_version, current_version)` 只在版本完全一致时为真。

### 4. 版本一致性

`package_version` 必须同时等于 `Cargo.toml`、`package.json`、`npm/kroma-cli/package.json` 的版本，
由 `package_versions_and_release_manifest_agree` 校验。

### 5. 许可证不由本审查决定

能力文档里写明：许可证文件与条款需要项目所有者确认后再补；清单里没有「已选许可证」这种条目。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t20_capability_manifest` | 7 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | **963 passed / 0 failed**（`logs/test_t20.log`） |

### 测试清单（7 条）

- `capability_claim_has_current_evidence_reference`：清单自洽 + **每个引用的测试名都在树里** +
  条目数 ≥ 18（防止文件悄悄缩水）+ 每条都有任务号与摘要 + 未接线的能力不得 `default_on`。
- `documentation_commands_match_cli_help`：文档里写的每个 `--flag` 都必须出现在
  `crates/kroma-cli/src/main.rs` 的参数解析里（`"--flag"` 字面量）。
- `old_report_cannot_satisfy_new_release_gate`：`1.4.0` 的报告不能当作 `1.5.0` 的证据；空串也不行。
- `package_versions_and_release_manifest_agree`：三处版本号必须与清单一致。
- `manifest_rejects_contradictory_claims`：六种自相矛盾声明逐一被拒，合法的一条通过。
- `missing_cited_test_is_reported`：引用一个树里不存在的测试名 → `MissingEvidence`（名字运行时生成，
  避免被自己的扫描命中）。
- `capability_doc_and_manifest_do_not_drift`：`CAPABILITIES.md` 的表格行数 == 清单条目数；
  清单里的每个任务号都出现在文档里；文档必须写明 NOT_RUN、必须说明 worktree 不是安全边界、
  必须把许可证留给所有者。

### 变异验证

清单里的测试引用是**真的被检查**的：把任意一条 `evidence.tests` 改成不存在的名字，
`capability_claim_has_current_evidence_reference` 会以 `MissingEvidence { id, test }` 失败
（`missing_cited_test_is_reported` 就是这条路径的最小复现）。

## 四、边界

- 能力清单只声明到 T19；T21–T30 完成后需要在封板时补条目（T30 台账里列出）。
- `live_tested` 只有两项为真（bwrap 命名空间、真实 docker 容器）；其余一律 `false` 或写明 NOT_RUN，
  没有把 fixture 测试冒充真机验证。
- README 的其它历史段落没有逐句重写：本次只改了两处与本次改动直接冲突的表述
  （`bash` 沙箱、worktree 边界/自愈），其余在能力文档里统一纠正。
- 性能数字（冷启动 3.2ms 等）保留原样，因为它们是**旧版本测量值**，本次没有重测；
  能力文档里没有把任何性能声明列为本阶段证据。
