# T15 · 分层证据存储与可用性合同 — 验收

阶段 G2｜依赖 T05, T06, T07｜关联 KR-11

## 一、缺陷

原始证据只有一层：`ContextProjector` 里一个 `HashMap<String, Arc<str>>`，超过 16 MiB 就按
「最老的先丢」淘汰，**丢掉的字节没有任何落点**。`recall_evidence` 于是只能说一句
「indexed but its raw bytes are no longer retained」——分不清是「从没写过」「被用户删了」
「被 GC 收了」还是「文件被篡改了」。

（`--resume` 能从 WAL 重建，那是另一条路径，不是证据存储的可用性合同：同一进程内淘汰之后，
在 resume 之前，字节就是没了。）

## 二、改了什么

### 1. `evidence_store::EvidenceStore`（新增模块）

**key 四段**：`session_id / branch_id / event_id / sha256`。

- `branch_id` 是 run 开始时所在的 leaf，**整个 run 固定**。所以 fork 出来的另一个 run 有不同分支段，
  读不到兄弟分支的证据。
- `sha256` 是内容摘要：**同名文件不等于同一份证据**，这是任务卡点名的边界条件。

**状态四态**（`ArtifactStatus`），没有可以乐观设置的 bool：

| 状态 | 含义 |
| --- | --- |
| `Available { text, bytes }` | 读到了，且内容哈希与 key 一致 |
| `Gone` | 没写过 / 用户删除 / 被 GC 收走 |
| `Corrupt { reason }` | 文件在，但哈希不匹配或不是合法 UTF-8 |
| `Forbidden { reason }` | 文件在，但目录不是当前用户私有、或不是常规文件 |

**写入**：先校验 key 的摘要与字节一致（不一致直接拒绝写），目录 `0700`（`ensure_private_dir`），
文件 `0600` 且不跟随 symlink（`create_new_no_symlink`），临时文件 + `rename` 发布，
所以崩溃不会留下半份 artifact 被后来读成 `Corrupt`。单文件上限 `MAX_ARTIFACT_BYTES = 32 MiB`。

**读取**：走 T06 的 `read_bounded_file`（`O_NOFOLLOW` + `O_NONBLOCK` + 句柄 `fstat` + 有界读），
然后**校验哈希**才返回。**绝不回落到工作区当前文件**——被编辑过的文件不能冒充旧证据。

**GC**：`gc(&live_keys)` 按 manifest 引用删除，不按时间删（「旧」不等于「没人引用」）；
删不掉的路径记进 `GcReport.failed` 并带原因，不静默跳过。

### 2. 接进投影器与 `recall_evidence`

- `ContextProjector::with_evidence_store(store, session_id, branch_id)`；
- `evict_evidence_over_cap` 在丢内存之前**先把字节交给持久层**；
- `evidence_lookup` 内存 miss → 持久层（哈希校验后返回）；
- 新增 `evidence_status(id) -> Option<ArtifactStatus>`，`recall_evidence` 用它给出**具体原因**：
  `gone` / `corrupt (…)` / `forbidden (…)`，而不是笼统一句「不再保留」。
- `agent_loop` 在构造投影器时接上 `state_dir/evidence`，session 与起始 leaf 作为 key 的两段。

### 3. P1 保持简单

只有「附加了 store」时才有持久层；没附加时行为与之前完全一致（`evidence_status` 返回 `None`，
`recall_evidence` 保留旧文案）。任务卡要求 P1 保持简单基线，所以没有强制所有模式都带持久层。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t15_evidence_store` | 5 passed / 0 failed |
| `cargo test -p kroma-core --test gate2_evidence_projection` | 18 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 921 passed / 1 failed（`logs/test_t15.log`） |

那 1 条是台账 §2.2 的既有 flake `b08c_git_metadata`（需 `--test-threads=1`），串行复跑 5/5 绿。

### 测试清单（5 条）

- `evicted_text_can_be_loaded_by_trusted_artifact_ref`：两份各 9 MiB 的证据（合计超过 16 MiB 内存上限）
  → 较老的一份被淘汰 → `evidence_status` 为 `Available`、`evidence_lookup` 仍能取回完整字节；
  较新的一份仍在内存里（`evidence_status` 为 `None`）。
- `expired_or_corrupt_artifact_returns_explicit_status`：未写过 → `Gone`；写入后 → `Available`；
  原地篡改 → `Corrupt`（报「hashes to …」）；目录改成 0777 → `Forbidden`（报「not private」）；
  用户删除 → `Gone`；超过 32 MiB 的写入被拒且之后仍是 `Gone`。
- `fork_does_not_cross_read_sibling_evidence`：同 session、同 event、同内容，仅分支段不同 → `Gone`；
  换 session 同样 `Gone`；原分支仍 `Available`。
- `historical_hash_differs_from_current_file`：捕获后文件被改写 → 读回的仍是**旧字节**；
  用当前内容的哈希构造的 key → `Gone`（证明不会回落到工作区文件）。
- `gc_keeps_referenced_artifacts_and_reports_failures`：只保留 manifest 引用的 2 个，删掉另外 2 个，
  `bytes_freed > 0`、`failed` 为空；第二次 GC 幂等（removed=0、kept=2）。

### 变异验证

把淘汰时「交给持久层」的那一步换成空操作，`evicted_text_can_be_loaded_by_trusted_artifact_ref`
立刻红：`expected the evicted artifact to be Available, got Some(Gone)`。

## 四、边界（数据保留说明）

- **不声称无限、无损、永久免费**：单 artifact 32 MiB 上限、内存 16 MiB 上限、GC 按 manifest 删除。
  超出上限的写入是拒绝，不是截断。
- 持久层只在附加了 store 时生效（`agent_loop` 已接）。未接的调用方（部分库测试）行为不变。
- `branch_id` 取 run 起始 leaf，**同一 run 内固定**：这保证写读一致；代价是同 session 的两个
  不同 run（含 `--resume`）会落在不同分支域，旧 artifact 不会被新 run 命中——宁可重写一份，
  也不跨 run 读到别人的字节。
- 图像证据仍以文本形式（base64 已在 T05 从 stub 里清掉）走同一套 artifact，没有单独的对象存储；
  任务卡要求「大图像不作为无界内联文本」，这一点由 32 MiB 上限与 `Truncate` 不适用（`Refuse`）保证。
- 没有做「按年龄/总量自动 GC 的调度」：GC 是显式调用（`gc(&live)`），调度留给运维，
  避免自动删除活跃会话还在引用的对象。
