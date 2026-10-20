# 可信本地门禁第一片（M0-03）

对应任务：`contracts/GOAL_BACKLOG.json` 中 `M0-03`「可信基线门禁与证据回收」，
`source_scope` 为 `scripts/`、`.github/workflows/`、`docs/goal/`。本片只落 `scripts/`
两个脚本与本文件，未触碰产品代码、原测试、CI 工作流或旧门禁脚本。

**本文件不构成 M0-03 通过结论。** M0-03 仍是 partial：本片只覆盖「本地命令门禁 + 证据回收」，
CI 侧可信门禁与 artifact 读取（M0-03-A3）未实现，见第 8 节。

## 1. 这一片解决什么

候选树可以改自己的测试脚本、改自己的门禁配置、改自己的验收标准。若门禁从候选树里读配置，
候选就能把 FAIL 换成 PASS。本片的唯一主张：

> 门禁的**配置**（contract）和**执行者**（runner）都不来自候选树，因此候选改自己仓库里的
> 同名文件不会改变门禁结果。

实现是一个标准库 Python runner（`scripts/run-trusted-gate.py`），由 Controller 从**仓外冻结副本**
启动，显式传入候选 repo 路径与仓外可信 contract JSON，产出证据文件与原始日志。

不解决、也不声称解决的问题见第 7 节（同 UID 恶意进程、签名、沙箱、CI、完整防篡改）。

## 2. 信任边界与启动方式

```text
审核过的 scripts/run-trusted-gate.py
        │  Controller 冻结复制（记录 sha256）
        ▼
仓外 /<trusted>/run-trusted-gate.py   ──启动──▶  候选 worktree（只作为被测对象）
        │                                          ▲
        └── 显式传入 /<trusted>/gate-contract.json ─┘  （contract 也在候选 repo 之外）
```

Controller 的实际步骤（本片已验证）：

1. 审核 `scripts/run-trusted-gate.py`（本文件所在提交即为审核对象）。
2. `cp scripts/run-trusted-gate.py <trusted>/run-trusted-gate.py`，`sha256sum` 记录摘要；
   runner 会把自身摘要写进证据的 `producer.runner_sha256`，两边可对账。
3. 在 `<trusted>/` 写/取已审 contract JSON，`sha256sum` 记录摘要。
4. `python3 <trusted>/run-trusted-gate.py --repo <candidate-worktree> --contract <trusted>/gate-contract.json --out <trusted>/evidence`
5. 读 `--out` 下的 `gate-evidence.json` 与 `logs/`，对 `contract.sha256`、`producer.runner_sha256`、
   `subject_tree` 与 Controller 自己记的值做比对；runner 只**记录**这些摘要，**不**替 Controller 判断信任。

runner 主动拒绝的三种「直接从候选树加载」情形（退出码 2，不写证据）：

| 情形 | 拒绝理由 |
| --- | --- |
| runner 文件本身位于候选 repo 内 | 候选树里的 runner 不受信，必须用仓外冻结副本 |
| `--contract` 解析后位于候选 repo 内 | 门禁配置必须来自候选之外的可信位置 |
| `--out` 位于候选 repo 内 | 证据不得写入被测树（否则会污染被测树） |

## 3. contract 格式

```json
{
  "gate_baseline": "kroma.gate.local.v1",
  "notes": "可选的人类说明",
  "commands": [
    {"name": "fmt", "argv": ["cargo", "fmt", "--check"], "timeout_seconds": 300},
    {"name": "tests", "argv": ["bash", "scripts/run-workspace-tests.sh"]}
  ]
}
```

约束（违反即拒绝，退出码 2）：

- 顶层只允许 `gate_baseline`、`commands`、`notes`；未知键一律拒绝，避免出现「被忽略的 shell 字段」。
- `gate_baseline` 必须是非空字符串，原样写入证据的 `gate_baseline`，作为门禁基线身份。
- `commands` 非空数组；每条只允许 `name`、`argv`、`timeout_seconds`；`argv` 必须是非空字符串数组。
- **没有 shell 路径**：runner 用 `subprocess.Popen(argv, ...)` 直接 exec，从不拼字符串、
  从不 `shell=True`。含 `shell`/`command` 等字段的 contract 直接拒绝。
- `timeout_seconds` 可选，必须是有限正数；缺省 `900`。**不存在无上限的运行**：contract 只能
  在有限值之间调整，不能要求无限等待。超时后 runner 杀掉子进程所在的整个 session/进程组
  （先 SIGTERM，2s 宽限后 SIGKILL），保留已写日志，回执记 `timed_out: true`、`exit_code: 124`。
- `gate_baseline` 与 contract 字节 sha256 一起进入证据，二者共同绑定「这次用的是哪套门禁」。

## 4. 候选树身份

- 运行前要求候选 worktree **干净**（`git status --porcelain=v1 -z --untracked-files=all
  --ignore-submodules=none` 为空）。脏树直接拒绝：脏树时 HEAD 不能代表被测树，
  拿 HEAD 当 candidate tree 就是伪造身份。
- 干净树记录实际值：`commit`（`rev-parse HEAD`）、`subject_tree`（`rev-parse HEAD^{tree}`）、
  `branch`、`repo` 真实路径。
- `--repo` 必须是该 worktree 根（`rev-parse --show-toplevel` 与 `--repo` 真实路径相等），
  避免把父仓库的树当成本次被测树。
- 命令跑完后**再查一次** `commit` / `HEAD^{tree}` / 工作树状态，与运行前记录的值逐项比对，
  写入 `post_run`。任一项不一致（改了 tracked 文件、新建 commit、新出现未忽略文件）→
  `overall.status = failed`、`overall.failure_reason = subject_mutated`、退出码 90，并在
  `post_run.reasons` 写明具体原因。即：**命令全退出 0 也不算 PASS**。
- 这只是运行前后两点比对，不是完整防篡改：运行窗口内改了又改回、或写进 git-ignored 路径，
  这里看不到，也不做此声称。

## 5. 证据字段

`--out/gate-evidence.json`，`format = kroma.trusted_gate.evidence/1`，字段对齐蓝图 16.5：

| 字段 | 含义 |
| --- | --- |
| `producer` | `name`/`version`/`runner_path`/`runner_sha256`/`user`；producer 信任须独立核对 |
| `subject`、`subject_tree`、`commit` | 被测树身份（tree 为实际值，不是 HEAD 文本） |
| `gate_baseline` | 来自 contract 的门禁基线身份 |
| `contract` | `path`、`sha256`（`sha256:` 前缀的字节摘要）、`notes` |
| `binary_digest` | **没有构建产物时为 `null`**，不编造；本片不构建、不接收二进制 |
| `environment` | `platform`、`machine`、`python_version`、`python_executable`、`git_version`、`PATH` |
| `started`/`ended` | 整次运行与每条命令各自的 UTC 起止时间 |
| `commands[]` | `index`、`name`、`argv`、`timeout_seconds`、`timed_out`、`exit_code`、`signal`、`duration_seconds`、`raw_log_ref`、`log_hash` |
| `not_run[]` | 因前一条失败而未执行的命令名（按 contract 顺序） |
| `overall` | `status`（passed/failed）、`exit_code`、`failure_reason`（`command_failed`/`command_timeout`/`subject_mutated`/null）、`failed_command` |
| `post_run` | 运行后的 `commit`、`subject_tree`、`worktree_clean`、`unchanged`、`reasons[]` |
| `redaction_status` | 本片为 `not_redacted`：日志未做脱敏，Controller 分享前自行处理 |
| `coverage_limits` | 本片明确不覆盖的范围，见第 7 节 |

原始日志写在 `--out/logs/<index>-<slug>.log`：**子进程的 stdout+stderr 直接重定向到该文件**，
不经过 runner 内存，所以输出再大也只占磁盘；子进程结束后 runner 流式读取该文件计算
`log_hash = sha256(文件字节)`，可离线复算。日志里不掺 runner 自己的输出，哈希只覆盖子进程输出。

## 6. 退出码

| 退出码 | 含义 |
| --- | --- |
| 0 | 全部命令退出 0，**且**候选树/HEAD 与运行前一致 |
| 124 | 某条命令超过其 `timeout_seconds`，被杀；日志与回执保留 |
| 90 | 全部命令退出 0，但候选树/HEAD 在运行中变了（`failure_reason=subject_mutated`） |
| N | 第一条失败命令的退出码（被信号杀死时为 128+N，同时 `signal` 字段记录信号号） |
| 2 | 拒绝：输入非法、候选树脏、contract/runner/out 位于候选树内。**不写证据文件** |
| 1 | 内部错误（IO 等） |

失败命令的退出码**原样**传播，不被压成 1。因为门禁命令自己也可能返回 2/90/124，退出码存在
歧义，判定时以「有没有 `gate-evidence.json`」和 `overall.status` + `overall.failure_reason` 为准。

命令按 contract 顺序执行，第一条失败即停止（`not_run` 记录未执行项），已执行命令的回执与日志全部保留。

## 7. 明确不声称（本片的覆盖限制）

- **不防同 UID 恶意进程**：门禁命令与 Controller 同 UID、同环境、同主机，没有沙箱、
  没有权限分离，无法阻止候选代码在运行时改文件、读环境或做别的动作。
- **不把候选代码变成授权主体**：被执行的候选代码始终不可信；本片只保证候选改不了门禁配置。
- **不是签名/证明**：`contract.sha256`、`log_hash`、`runner_sha256` 只证明字节相同，
  不证明是谁产出的、在什么环境产出的；producer 与执行环境必须由 Controller 独立核对。
- **不是 CI 门禁**：不读 CI、不下载 artifact，CI 侧可信门禁与证据回收（M0-03-A3）未实现。
- **不是完整测试防篡改**：没有隐藏评分器、没有测试与实现的作者分离；候选仍可改自己的产品测试
  （这是允许的），只是改不动本 contract。运行前后的 `commit`/`tree`/工作树比对也只是两点检查：
  窗口内改了又改回、或写进 git-ignored 路径，都看不出来。
- **超时只在 POSIX 上完整**：子进程以 `start_new_session=True` 启动，超时后按进程组发信号；
  没有 `os.killpg` 的平台上只能杀直接子进程，其孙进程可能存活。且被杀不等于撤销：
  超时命令已经产生的副作用不会被回滚。
- 无 `binary_digest`（无产物）、子进程 stdin 为 `/dev/null`（交互式门禁跑不了）、
  stdout/stderr 合并到同一日志文件（不保留流分离与时序）。

## 8. M0-03 状态：A1/A2 已闭环，A3 外部阻断

| 验收规格 | 状态 |
| --- | --- |
| M0-03-A1 候选修改测试脚本不能绕过基线 Gate | **PASS**。contract/runner 均在候选树外；独立变异实测：候选把测试改成 `exit 1`、仓内放一份全过的伪 contract、并把仓内 `run-trusted-gate.py` 换成 `exit 0`，三者同时存在时门禁仍以 exit 1 判失败（`evidence/m0-03/a1-forged-contract-gate.log`）。脏树会被直接拒绝（exit 2），不给“测的不是这棵树”留空子 |
| M0-03-A2 日志记录 candidate tree 而非仅 HEAD 文字 | **PASS**。证据记 `subject.subject_tree`（`HEAD^{tree}`）、`commit`、`branch`、`worktree_clean`，跑完再比对一次，变了就不算 PASS（exit 90） |
| M0-03-A3 Controller 能直接读取 CI artifact | **读取路径 PASS，下载 BLOCKED**。`scripts/ci-artifact-ingest.py` 把下载到的 zip/目录转成 (name,size,sha256) 清单并可复算验证；10 项定向检查覆盖改一字节即 MISMATCH、缺文件即报缺失。但本树**没有 remote**，因此不存在“本树的 CI 产物”；环境也没有 `GITHUB_TOKEN`，下载路径为 `BLOCKED_NO_TOKEN` |

A3 的阻断不是缺实现，而是缺外部条件：要么所有者给 token 且给一个可以推的仓库（当前
`push_protected` 在 envelope 里是 REFUSED），要么接受“本地证据 + 无可读 CI 产物”的现状。
在此之前 M0-03 不宣称整体通过，也不把 public 仓库 `Polaris591/kroma` 的历史 run 顶替本树证据。

同样不成立的说法：runner 解决全部安全问题；本地门禁等于 CI 门禁；有 evidence JSON 等于被签名。

## 9. 本次验证的命令与退出码

在 `/opt/kroma-v4/source`（分支 `integration/v4`，基线 `0933a4a9a6ca94776d0122e581b46bd156762667`）实跑：

| 命令 | 退出码 | 结果 |
| --- | --- | --- |
| `python3 scripts/test-trusted-gate.py` | 0 | 4 个测试全过，共 1.66s（临时 git fixture + 临时 contract，无 cargo、无网络） |
| `python3 -m py_compile scripts/run-trusted-gate.py scripts/test-trusted-gate.py` | 0 | 语法检查 |
| `python3 <trusted>/run-trusted-gate.py --repo <tmp candidate> --contract <trusted>/gate-contract.json --out <tmp>/evidence`（命令 2 返回 4） | 4 | 失败命令退出码原样传播，证据与日志已写 |
| 同上，但命令 `time.sleep(30)` 且 `timeout_seconds: 1` | 124 | `duration_seconds=1.002`，`timed_out=true`，日志保留（含超时前已写出的 `started`） |
| 同上，但超时命令还 spawn 了一个睡 60s 的孙进程 | 124 | `pgrep` 残留 0：整个进程组被杀 |
| 同上，但命令退出 0 却改了 tracked 文件 | 90 | `failure_reason=subject_mutated`，`post_run.reasons` 含 `worktree is dirty after the run: M tracked.txt` |
| 同上，但命令退出 0 却新建了 commit | 90 | `post_run.commit != commit`，`reasons` 含 `commit changed from ...` |
| 同上，子进程连续输出 100 MiB | 0 | 日志文件恰为 104857600 字节，`log_hash` 与 `sha256sum` 一致；runner 峰值 RSS ≈ 20 MiB（输出不过内存） |
| 同上，但候选树内有未跟踪文件 | 2 | `refused: candidate worktree is dirty ... ?? untracked.txt`，无证据 |
| 同上，但 `--contract` 指向候选 repo 内 | 2 | `refused: ... is inside the candidate repo ...`，无证据 |
| 同上，但 runner 从候选 repo 内启动 | 2 | `refused: runner ... lives inside the candidate repo ... cannot be trusted`，无证据 |

本次验证的 runner 字节摘要：`sha256:4f37f1efe59d8eb403b5d4fe2230a9863aafa68ad437335df8889da5e6496376`；
测试脚本：`sha256:70fc4afda80d944a127efa2ec32fc11e1532e1066644d43ca5ca496907baa75b`。
摘要随实现变化，使用前用 `sha256sum` 重算，不要沿用本文的旧值。

未执行：任何 `cargo` 编译/测试、网络访问、CI 触发、其他模型调用、子代理委派、git 提交或推送。
因此本文不声称任何产品构建或产品测试通过。

## 10. 变更与回滚

- 新增：`scripts/run-trusted-gate.py`、`scripts/test-trusted-gate.py`、`docs/goal/TRUSTED_GATE.md`。
- 未修改：产品代码、原测试、`.github/workflows/`、既有门禁脚本（`release-gate.js` 等）。
- 未 `git add`/`commit`，工作树除上述三个新文件外仍为基线状态。
- 回滚：删除这三个文件即回到 base tree `7567f5c0cddd9e2860ce476d6fda4a21f8deab5c`；
  无数据迁移、无配置变更、无副作用。
