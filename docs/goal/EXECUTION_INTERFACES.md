# 执行接口实录（M0-02）

本文只记录**实际跑过并核对过的接口事实**：入口、调用字段、身份与授权边界、待验证项。
它不是调度平台设计，不新建 goal CLI，也不替代蓝图第 13 节。凡未实测的项目一律标注为待验证，
本文不构成 M0-02 通过结论；通过与否由 Controller 独立验收。

对应任务：`contracts/GOAL_BACKLOG.json` 中 `M0-02`「验证真实 goal/Worker/CI 接口与授权」，
`source_scope` 为 `docs/goal/`、`scripts/`；本次只落 `docs/goal/` 一个新增文件，未触碰 `scripts/`。

## 1. 基线与身份

| 项 | 值 | 依据 |
| --- | --- | --- |
| 仓库 | `/opt/kroma-v4/source` | 现场 |
| 分支 | `integration/v4` | `git branch --show-current` |
| base commit | `0933a4a9a6ca94776d0122e581b46bd156762667` | `git rev-parse HEAD` |
| base tree | `7567f5c0cddd9e2860ce476d6fda4a21f8deab5c` | `git rev-parse HEAD^{tree}` |
| 归档 sha256 | `16e0b736…f9cbba`，406 文件，mismatches 为空 | `/opt/kroma-v4/evidence/baseline-identity.json` |
| 工作树 | 落本文件前干净（`git status --porcelain` 0 行） | 现场 |
| 远端 | 该 worktree **未配置任何 remote** | `git remote -v` 空 |
| worktree 数 | 1（无并行 worktree） | `git worktree list` |

「未配置 remote」与证据文件里 `known_remote=git@github.com:Polaris591/kroma.git`、
`new_source_remote_configured=false` 一致：远端是已知身份，不是本 worktree 的推送通道。
因此本任务任何结论都不能用「已推送」来支撑。

## 2. 已核实的 Worker 执行接口

### 2.1 入口

真实入口是 Pi 宿主里的 `subagent` 工具（已安装 `pi-subagents@0.76.1`），**不是**仓库内脚本：

- 单步：`subagent({ agent, task, model, output, … })`
- 工作流：`subagent({ workflow: "<绝对路径>.js", cwd, async: true, context: "fresh", missionId })`，
  脚本内部用 `await runs.run(key, {…})`（并行用 `runs.all`）。
- 读取真实帮助：`subagent({ action: "guide", topic: "tool-reference" })`、
  `subagent({ action: "list" / "models" })`；宿主文档在
  `/root/.pi/agent/npm/node_modules/pi-subagents/docs/{tool-reference,workflows,agents}.md`。
  本次字段全部来自这些实测输出，不靠猜。

仓库内没有 `runs.run` / `subagent` 调用脚本——`grep` 命中的 `subagents.rs`、
`bench-foundation.js` 里的 `subagent` 是 Kroma 产品自己的子代理特性，与本接口无关。
**仓内无调用脚本 ≠ 工具不存在**：接口在宿主运行时，证据是下面真实发起的 workflow 运行（M0-01 已完成并回收）。

### 2.2 实际调用字段（真实运行的参数）

顶层 `subagent` 请求（`/opt/kroma-v4/m0-intake-workflow.js` 与 `m0-interface-workflow.js` 的启动方式，
M0-01 运行 `5f699dcd-…` 已 `complete`；M0-02 为本次运行）：

| 字段 | 实测值 | 说明 |
| --- | --- | --- |
| `workflow` | `/opt/kroma-v4/m0-*.js` | 工作流脚本路径；脚本只做编排，不落仓库 |
| `cwd` | `/opt/kroma-v4/source` | 子代理工作目录 |
| `async` | `true` | 后台运行，靠原生通知回收，不做 sleep/轮询 |
| `context` | `"fresh"` | 不继承父会话上下文 |
| `missionId` | `89b9b6d5-7e3b-46f8-b520-e78d13edcd1f` | 重试沿用同一 mission |

`runs.run` 步骤字段：

| 字段 | 实测值 |
| --- | --- |
| `agent` | `scout`（M0-01）、`worker`（M0-02） |
| `label` | `Map BF14B1 baseline to v4 contracts` 等 |
| `model` | `workbuddy/deepseek:high` |
| `context` | `"fresh"` |
| `acceptance` | `{ level: "attested" }` |
| `task` | 完整工作包文本（范围、基线、禁止项、交付格式） |
| `output` | `m0/baseline-map.md`、`m0/interfaces-handoff.md` |
| `outputMode` | `"file-only"` |

返回值（回执可用字段）：`runId`、`output`、`outputReference`、`outputPathMapping`、`artifactPaths`。
M0-01 实际回执：`outputReference = /opt/pi-qq/sessions/subagent-artifacts/outputs/5f699dcd-dee4-4df8-a3e7-b1c8de2c1ca1/m0/baseline-map.md`，
见 `/tmp/pi-subagents-uid-0/async-subagent-runs/5f699dcd-dee4-4df8-a3e7-b1c8de2c1ca1/workflow-receipt.json`。

### 2.3 output 绑定语义（容易被误读的一点）

`output` 是**运行时路由字段**；任务文本里写的文件名只是指令，不覆盖路由。
实际落盘根是 `…/subagent-artifacts/outputs/<workflowRunId>/<output>`，
相对路径相对该根解析，**不是**相对仓库。`outputMode: "file-only"` 让报告只进文件，
避免正文重复进 stdout。依据：`docs/tool-reference.md` 第 155 行附近 + 上述回执。

### 2.4 完成回收：原生通知，不是轮询

M0-01 完成时宿主写入两条 `custom_message`，随后注入一条 user 消息「Subagent updates above.」：

- `subagent-incremental-child-notify`：「Workflow child completed: m0-01-map / Child run fae28b7d… / Output …」
- `subagent-notify`：「Background task completed: workflow」+ workflow receipt 路径 + 返回体

即：`async: true` 的运行**不需要** `bg_wait`/sleep 轮询，完成或需要关注时由宿主唤醒父会话。
可用 `subagent({ action: "status", id })` 做一次性查看，但不作为等待循环。
（同会话中曾出现 goal 自动续跑反复唤醒、空转数轮，这是调度侧问题，不是通知机制问题。）

### 2.5 运行记录（可作为身份证据的实体）

| 运行 | 值 |
| --- | --- |
| M0-01 workflow | `5f699dcd-dee4-4df8-a3e7-b1c8de2c1ca1`，state `complete` |
| M0-01 child | `fae28b7d-62e1-4dc0-a41e-0e1443ea1089`，agent `scout`，exitCode 0 |
| child 记录模型 | `workbuddy/deepseek:high`（requested 与 resolved 相同），usage 有 cost 0、58 turns |
| 状态/回执目录 | `/tmp/pi-subagents-uid-0/async-subagent-runs/<workflowRunId>/{status.json,workflow-receipt.json,workflow-children.jsonl,events.jsonl}` |
| 子代理 meta | `/opt/pi-qq/sessions/subagent-artifacts/fae28b7d-…_scout_meta.json` |

## 3. 身份与授权边界

### 3.1 Worker 模型身份

- 注册别名：`workbuddy/deepseek`（`/root/.pi/agent/models.json`，`thinkingLevelMap.high = "high"`，
  `supportsReasoningEffort: true`，contextWindow 1,000,000，maxTokens 128,000）。
- 记录调用名：`workbuddy/deepseek:high`；M0-01 子代理 meta 的 `model` 与 `requestedModel` 一致。
- **别名不证明后端 revision**：证据文件已写 `backend_revision: UNKNOWN`。不能据此宣称
  「已核实 DeepSeek v4.1 Flash 后端/权重」。Worker 维持该档位，本任务未切换模型。

### 3.2 付费实验授权（已批准但未启动）

- `cobblemon/gemini-3.8-flash-high`（`thinkingLevelMap` 仅 `high` 有效，contextWindow 1,048,576）。
- Owner 授权：**不限现金总额**；但每次实验仍需有限资源额度与完整账目（token/费用/耗时）。
- 状态：`started: false`——本次没有任何 Gemini 调用，不产生费用记录。

### 3.3 明确未授权（INV-20：自治不扩权）

其他付费模型、生产私钥/凭据、正式发布与签名、受保护 `main` 的写入或合并，均未授权。
本次也没有任何推送、合并、发布、密钥读取或跨模型调用。

### 3.4 goal 入口现状（按用户最新决定）

- goal 扩展存在（`settings.json` 的 `packages` 含 `npm:pi-goal-x`，版本 0.32.3，提供 `update_goal` 工具）；
  会话中确实调用过 `update_goal({status:"paused"})`。
- 用户决定：**停用 goal 自动续跑**，改由普通对话中的 Controller 收到子代理完成通知后继续；
  不恢复 goal、不造 goal CLI。本文件不记录任何 goal 调度实现。

### 3.5 CI / 工件读取能力

| 探测 | 结果 |
| --- | --- |
| `GET /repos/Polaris591/kroma` | HTTP 200，`private=false`，`default_branch=main`，`pushed_at=2026-10-18T07:03:29Z` |
| `GET …/actions/runs?per_page=1` | HTTP 200，`total_count=36`；最新 run `39106253179`，`head_sha=ce50fb5…`，`completed/success` |
| `GET …/runs/39106253179/artifacts` | HTTP 200，`total_count=0`，`artifacts=[]` |
| `gh` CLI | 不存在（`gh --version` 退出码 127） |

结论边界：公共 API 可读仓库与 CI 列表；**最新 run 无 artifact，因此下载权限未验证**
（`artifact_download_authorization: UNVERIFIED`）。最新 run 的 `head_sha` 也不是本候选树
（`0933a4a`/`7567f5c`），故 `candidate_tree_ci_evidence: NONE`，不得当作候选树 CI 证据。

## 4. 一次基础设施修复与同协议重试

**失败运行** `8723cdb7-f306-4148-9dc7-e55399e4a2a8`（同一 workflow 脚本、同字段、`async: true`）：
状态 `failed`，步骤 `m0-01-map` 无子运行 id，`resumability: not-resumable`（`child produced no run id`）。
原始错误：

```
Background children require the host npm package (@earendil-works/pi-coding-agent) with its dependencies;
/usr/local/lib/nodejs/lib/node_modules/@earendil-works/pi-coding-agent does not provide
@earendil-works/chord, @earendil-works/chord/context.
```

**修复**：在 `/opt/pi-subagent-runtime` 安装 `@earendil-works/chord@1.3.0`，并把该目录软链进宿主包
`/usr/local/lib/nodejs/lib/node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/chord`。
核对：`ls -la` 显示符号链接指向 `/opt/pi-subagent-runtime/node_modules/@earendil-works/chord`；
ESM `import('@earendil-works/chord/context')` 成功（导出 `createContextKey, withContextValue, …`）。
注意：`require.resolve('@earendil-works/chord/context')` 会以
`ERR_PACKAGE_PATH_NOT_EXPORTED` 失败，因为该包 `exports` 只有 `import` 条件——这是正常的
CJS/ESM 差异，**不是**修复不完整的证据；宿主按 ESM 加载。

**同协议重试**：同一脚本、同一字段、同一 `missionId`，仅换新运行
`5f699dcd-…`，结果 `complete`，child `fae28b7d-…` exitCode 0。修复尝试次数 1，
任务上限 `max_worker_repair_attempts: 2`。未使用任何降级执行模式。

## 5. 精确待验证项

1. `workbuddy/deepseek` 的**后端模型 revision/权重**：仅别名与 thinking 档位已验证，后端未验证。
2. CI **artifact 下载**权限与内容：最新 run 无 artifact，没有可下载对象，权限未证实。
3. 候选树（`7567f5c`）自身的 CI 结果：远端最新 run 指向 `ce50fb5`，与本树无关。
4. `scripts/` 侧是否需要一个可复现的调用封装：本任务未写（`source_scope` 允许，但用户限定单一文档缝隙）。
5. 前台（`async: false`）路径与 `chatProgress: live-card` 未实测；本次只用后台模式。
6. Gemini 实验的账单口径：授权与模型条目已核对，但未运行，费用表为空。
7. goal 入口在「停用自动续跑」后的长期形态：由 Controller 以普通对话推进，尚无独立验证。

## 6. 本次执行的命令与退出码

| 命令 | 退出码 | 用途 |
| --- | --- | --- |
| `git log --oneline -3; git rev-parse HEAD; git rev-parse HEAD^{tree}; git status --porcelain \| wc -l` | 0 | 基线与干净度（0 行变更） |
| `git branch --show-current; git remote -v; git worktree list` | 0 | 分支/远端/worktree（remote 为空） |
| `pi --version` | 0 | `1.3.0` |
| `node --version` / `git --version` / `cargo --version` | 0 | `v24.21.0` / `2.47.3` / `1.85.1`（仅记录，未编译） |
| `gh --version` | 127 | 未安装 |
| `curl -sS https://api.github.com/repos/Polaris591/kroma` | 0（HTTP 200） | 仓库可见性 |
| `curl -sS "…/actions/runs?per_page=1"` | 0（HTTP 200） | 最新 run |
| `curl -sS "…/runs/39106253179/artifacts"` | 0（HTTP 200，`total_count=0`） | 工件探测 |
| `python3 -c` 读 `GOAL_BACKLOG.json` / `models.json` / 证据 JSON | 0 | M0-02 合同、模型条目、证据交叉核对 |
| `ls -la …/node_modules/@earendil-works/` | 0 | chord 软链存在 |
| `node --input-type=module -e "import('@earendil-works/chord/context')…"` | 0 | 修复有效性（ESM 解析成功） |
| `node -e "require.resolve('@earendil-works/chord/context')"` | 1 | 预期失败（仅 `import` 条件），已解释 |
| `grep -rn "runs\.run\|subagent" --include=… .` | 0 | 仓内无本接口调用脚本，命中均为产品自有子代理代码 |

未执行：任何编译、测试、`cargo build/test/fmt/clippy`、其他模型调用、子代理委派、git 提交、推送。
因此本文件不声称任何构建/测试通过。

## 7. 变更与回滚

- 变更：仅新增本文件 `docs/goal/EXECUTION_INTERFACES.md`（新目录 `docs/goal/`）。
- 未修改产品代码、测试、CI、门禁；未 `git add`/`commit`，工作树除本文件外仍为基线状态。
- 回滚：删除该文件即可回到 base tree `7567f5c0…`；无数据迁移、无副作用。

## 8. 与 M0-02 验收规格的对照（待 Controller 独立验收）

| 验收规格 | 当前状态 |
| --- | --- |
| 读取真实工具版本/help 而非猜测参数 | 已做：`pi --version`、`subagent action:guide/list/models`、宿主 `docs/tool-reference.md`，字段与两次真实运行回执对齐 |
| 无调用接口时产生精确 BLOCKED | 已做：chord 缺失那次给出精确错误与不可恢复原因（`child produced no run id`），未把「仓内无脚本」误判为「工具不存在」 |
| 未授权费用/密钥操作拒绝 | 已做：未调用其他付费模型、未读密钥、未推送/发布；Gemini 授权仅记录未启动 |

以上为文档事实核对结果，**不构成 M0-02 全部 PASS**；独立验收、候选树门禁与证据归档另行进行。
