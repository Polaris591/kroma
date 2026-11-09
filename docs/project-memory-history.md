# 项目记忆历史设计记录（非当前规范）

> 此文件保留旧研究、P0/V2 和零维护流程的原始记录。不得用作当前行为规范；当前产品与接口见 [project-memory.md](project-memory.md)。

# Kroma 项目记忆（Project Memory）

> 本文合并了原先三份文档（设计研究记录与 P0 工程依据、V2 实现契约、零维护产品流程与路线图），原文件已删除，正文内容逐字保留。
>
> 阅读入口：当前产品默认值见「三、零维护流程」，已实现的接口契约见「二、V2 契约」，「一、设计」保留为完整研究记录和 P0 工程依据。
> 各部分保留原文的章节编号，编号只在所属部分内有效。

## 一、设计：Kroma 项目级记忆插件设计（Project Memory）

> 产品默认值已被本文「三、零维护流程」取代。
> 本文保留为完整研究记录和 P0 工程依据；其中“人工审核优先”的交互不再是推荐方向。

状态：**P0 已实现，P1/P2 仍为设计提案**。可运行代码在
[`examples/plugins/project-memory`](../examples/plugins/project-memory/README.md)：
本地 SQLite、按 canonical workdir 隔离、手动保存、候选审核、纠正/遗忘/清除、
检索、导入导出与诊断。它使用现有插件 API 1，不改宿主核心、插件协议、会话格式或压缩策略。
自动召回（P1a/P1b）、可靠来源通路的采集（P2）和跨 worktree 共享**尚未实现**，
本文第 11、13 节的新宿主接口与第 17 节的收益目标仍是提案与待测项。

### 1. 核心决策

> 记住项目中无法便宜地重新发现的知识，在正确的任务、代码版本和权限范围内，
> 用最少的上下文呈现，并能回答“为什么记住、凭什么相信、什么时候失效”。

推荐形态：**本地优先的独立进程插件 + SQLite 事实库 + 可追溯的原子记忆 +
作用域优先的混合检索 + 宿主托管的有预算请求投影**。

- 项目规则、长期知识、会话任务状态分开，不能合并成不断增长的 `MEMORY.md`。
- 先把手动记忆、隔离、事务、删除、中文检索做正确，再增加自动学习和向量检索。
- 不替换 Kroma 的内置压缩策略，不修改原始对话 WAL，不改写工具执行结果。
- 记忆不是权限系统，不能授权命令、突破用户限制或覆盖当前权威项目规则。
- 默认没有外部 embedding、后台模型调用、HTTP 服务、遥测或跨项目共享。
- 启用插件与启用自动学习是两个独立动作；插件本身仍默认关闭。

“最棒”的判据不是存得最多，而是：**少犯重复错误、少查重复资料、不给错误记忆更高权威，
且收益超过检索、token、缓存和维护成本**。

#### 明确不做

第一版不做全仓库代码向量化、知识图谱数据库、多代理投票、云同步平台、
自动执行记忆中的脚本、自动修改 `AGENTS.md`、每次工具调用都让 LLM 总结。
代码检索仍用源码；原始历史仍用 `history_search` / `history_read`；重复工作流优先成为 skill。

### 2. 当前 Kroma 的事实与设计约束

| 已确认的现状 | 影响 | 源码证据（当前工作树） |
| --- | --- | --- |
| 记忆文件写到插件代码目录 `__dirname/memory.json` | 用户级安装会跨项目共用；卸载目录会带走便签；项目级安装写入便签还会改变审批指纹 | `examples/plugins/memory/memory.js:4`；`src/plugins.rs:1238–1274`；`src/plugins/approval.rs:12–29` |
| 仅有字符串数组，所有便签一起注入，按 4000 个 JS 字符截断 | 无相关性、来源、版本、冲突和失效管理，可能在条目中间截断 | `examples/plugins/memory/memory.js:19–50` |
| 原子 rename，但没有跨进程读改写事务 | 两个会话可能覆盖彼此的新记忆，原子替换不等于原子更新 | `examples/plugins/memory/memory.js:14–18,25–28` |
| hook 每次返回完整 `messages`，即使没有便签 | 深拷贝和 IPC 成本；宿主不能复用原投影的实测 token 校准 | `examples/plugins/memory/memory.js:44–49`；`src/plugins/model.rs:457–512` |
| hook 错误直接向上传播，响应帧上限 2 MiB | 坏 JSON、进程错误或过大的整段历史回传可能阻断模型请求 | `src/plugins/model.rs:475–484`；`src/plugins.rs:2937` |
| `[context]` 是上下文策略独占替换，而非追加贡献 | 不应该为了记忆接管压缩、恢复和全部上下文状态 | `src/plugins.rs:1686–1712` |
| 插件事件队列是有界尽力投递，慢订阅者可丢通知 | 不可把 `task_finished` / `tool_finished` 当可靠采集日志 | `src/plugins.rs:2370–2390`；`docs/architecture.md:16` |
| 请求已带 `config/data_dir/workdir`，部分请求有 `session_id/archive/call_id` | 存储应走 `data_dir`；作用域仍需显式设计，不能只按插件 ID | `src/plugins.rs:2739–2762`；`examples/plugins/README.md:292–362` |
| 会话默认按 workdir，checkpoint 按 Git 根，插件发现按 workdir | 这三者不是同一个“项目身份”，不能直接挪用 session 文件名 | `src/session.rs:279–311`；`src/checkpoints.rs:56–88`；`src/plugins.rs:770–825` |
| checkpoint 请求故意跳过 `before_model_request` | 任意 hook 注入不能自然保证摘要和正常请求拥有相同附加信息 | `src/plugins/model.rs:596–625` |
| 插件拥有用户的 OS 权限，审批不是沙箱 | 声明“禁止网络”本身不能限制恶意插件 | `docs/security.md:64–73`；`src/plugins/approval.rs:9–20` |

上述是源码行为分析，不等于已测量所有生产故障。缓存损耗的实际幅度、不同平台崩溃恢复表现仍需专项评测。
当前 after-tool hook 失败会保留已完成工具的真实结果；本设计不会把后处理失败变成重试副作用的理由。

### 3. 外部方案：借鉴机制，不照搬宣传指标

| 方案 | 一手资料中的机制 | Kroma 应借鉴 | 不应直接照搬 |
| --- | --- | --- | --- |
| Claude Code [S1] | 人写的项目指令与自动记忆分开；记忆索引在会话开始加载有限内容，细节分文件；文档说明 repository/worktree 范围 | 精简索引、渐进读取、把规则和学习分开、用户可检查 | 不把行数限制当 token 预算；文件可读不代表具备事务、分支有效性或可靠删除 |
| Codex [S2][S3] | `AGENTS.md` 承担必需指导；记忆是辅助召回；可分开控制使用与生成，后台生成考虑会话空闲和配额 | 使用/采集双开关、费用门控、保留证据、规则不依赖记忆 | 不直接复制其目录或假定 Kroma 已有相同后台模型服务 |
| Cursor Rules [S4] | 按路径、手动、相关性或始终应用；引用权威文件而非复制文件 | 模块作用域、路径匹配、可见的激活理由 | 不把历史上曾出现的 Memories 功能当作当前可依赖的接口 |
| claude-mem [S5][S6] | 工具观察、SQLite/FTS5、后台处理；search → timeline → 详情的渐进披露 | 短索引先行、按需证据、可检查的学习记录 | 不默认捕获所有原始输出，不引入其整套 Bun/HTTP/UI/向量服务栈；不采用未经 Kroma 复测的“10 倍节省”等数字 |
| LongMemEval [S9] | 将更新、时间推理、跨会话推理与拒答纳入评测 | 测“旧知识被纠正”和“不知道时不乱记”，不只测命中率 | 它是对话记忆基准，不能替代真实编码任务或证明 Kroma 的收益 |

外部产品文档会随版本变化；本文使用本次读取的一手页面，不推断其他版本的默认行为。

### 4. 记忆分层：只有两层属于长期事实库

| 层 | 内容 | 权威来源 / 保存位置 | 默认使用方式 |
| --- | --- | --- | --- |
| 项目规范 | 构建命令、强制约定、权限限制 | 现有 `AGENTS.md`、团队文档、宿主配置 | 由原有指令系统加载；记忆最多保存指针，不再复制一套规则 |
| 项目长期知识 | 决策的原因、已验证的坑、用户对本项目的明确纠正、外部资料入口 | 插件事实库；共享部分经审核导出文档 | 相关时召回；受代码、环境和来源有效性约束 |
| 个人项目偏好 | 仅当前用户在本项目的非强制偏好 | 插件私有事实库 | 当前明确要求优先；不自动推广为跨项目偏好 |
| 任务工作集 | 当前目标、未完成项、失败尝试、下一步 | 会话/任务范围的派生状态 | 恢复当前任务时用，不自动成为项目事实 |
| 历史证据 | 原始用户纠正、工具调用、测试输出 | Kroma 原始会话/WAL、受控文件引用 | 按引用读取，不整体复制进记忆库 |

长期类型只先支持：`decision`、`pitfall`、`preference`、`reference`。
`task_state` 和 `observation` 可以入库，但默认短期且不冒充长期事实。
“某测试在某提交和环境下通过”是 observation，不能概括为“项目测试总是通过”。

值得记：为什么某条看似可行的路线被放弃、用户反复纠正的项目专属误解、难复现环境坑的条件。
不值得记：从文件列表即可获取的目录结构、整段源码、普通工具输出、助手未验证的猜测、秘密值。

### 5. 项目身份与分支语义

#### 5.1 隔离键

逻辑作用域：`用户配置域 × workspace × module/path × applicability`。
`applicability` 可以进一步约束 worktree、提交/代码依赖、环境或会话 lineage。

- Git 项目：规范化的 Git common directory 识别同一克隆的 worktree 集合，
  在本地注册表映射为 opaque `workspace_id`；每个 worktree 另有 `worktree_id`。
- 不按 remote URL 自动合并：fork、客户仓库和不同克隆即使 remote 相同，也可能必须隔离。
- 两个独立 clone 默认不同 workspace；移动目录或希望合并时，用户显式执行 link/migrate。
- 非 Git 项目：显式选定 root，默认 canonical workdir；同名目录不合并，也不猜测父目录是项目。
- 子模块默认独立 workspace；monorepo 用相对路径作用域，不默认混入所有服务的知识。
- 路径规范化处理符号链接、Windows 盘符/UNC 和真实文件系统大小写规则；
  不把所有路径盲目小写。存储身份原文用于碰撞和错误绑定校验。
- Git 查询由宿主受控执行，只读、不通过 shell 拼接、不覆盖用户 Git 安全设置，不改 index/HEAD。

**基础版 P0**采用 canonical workdir 的保守命名空间；可自行解析 Git root 作为相关性元数据，
但不自动把不同 workdir 合并共享，也不宣称已统一 Kroma 的项目概念。
**建议宿主版本**提供只读 `ProjectIdentity`，各入口使用同一份结果，不改变现有 session 命名。

#### 5.2 Git 分支不是隔离的全部

以下为宿主身份能力接通后的目标语义；P0 不自动跨 workdir 共享。

- 分支名只是标签，不是稳定 ID；删除重建、rebase、cherry-pick 都会破坏简单“同名即有效”。
- 认可的项目偏好可跨 worktree；实现相关结论默认限制当前 worktree/依赖指纹，
  检查过后才能进入更广范围，不因“成功一次”自动发布为全项目事实。
- 依赖文件改变、dirty worktree 或环境不匹配时，标记待复核，而不是仅比较 HEAD。
- `git merge-base --is-ancestor` 只能辅助判断，不能证明结论仍然成立；浅克隆和缺失提交按未知处理。

#### 5.3 fork / rewind / 子代理

- 会话 fork 继承 checkpoint 之前的任务记忆快照，之后的任务笔记分别发展。
- rewind 之后不可看到来自被撤销未来的任务状态；未来证据仍可作为历史存在，但不能当当前事实。
- 已经由人认可的项目决策不随会话 rewind 静默删除；若失去唯一适用证据则提示复核。
- 子代理默认可读同作用域的已认可项目知识，只能写 candidate，不能直接推广到父任务/全项目。
- 明确区分 `Git worktree/branch` 和 `conversation lineage/epoch`，不能用一个 branch 字段混装。
- `/memory off` 或禁止采集的会话持久记录排除范围，未来后台重放也不得补采。

### 6. 数据模型：原子主张 + 版本 + 证据

每条记忆表达一项可纠正的主张，而不是一整份会话摘要。下例为**拟议数据对象**，不是现有协议：

```json
{
  "id": "mem_opaque_id",
  "version": 3,
  "workspace_id": "ws_opaque_id",
  "kind": "pitfall",
  "claim_key": "subsystem.condition.failure_mode",
  "summary": "一个带适用条件的、可独立核验的结论",
  "detail": "原因、替代方案和限制；不复制完整日志",
  "state": "accepted",
  "scope": {"paths": ["src/plugins/**"], "worktree_id": "wt_opaque_id"},
  "applicability": {"dependency_fingerprint": "digest", "environment": "env_id"},
  "provenance": {"origin": "user_confirmed", "taint": ["local_session"]},
  "evidence_refs": ["ev_opaque_id"],
  "supersedes": [],
  "recorded_at": "RFC3339 timestamp",
  "valid_from": "RFC3339 timestamp",
  "valid_until": null,
  "last_checked_at": "RFC3339 timestamp"
}
```

实现时做结构化列和约束，不是把所有字段塞进一个无索引 JSON。
关键补充字段：来源 session/epoch/message index、tool call ID、源码内容摘要/符号锚点、
更新者、生成模型及版本（若用过）、保密级别、撤销版本、存储 schema version。

生命周期：`candidate → accepted → stale / contested / superseded → tombstoned`。
`accepted` 只表示已认可，**不等于客观正确**；证据强度、来源权限、内容有效性是不同维度。
不要把 LLM 自报的 `confidence=0.93` 当可信度证明；未经校准不对用户显示伪精确概率。

建议最小表：

| 表/实体 | 用途 |
| --- | --- |
| `memories` + `memory_versions` | 当前状态、有限的不可变版本，带 workspace 和适用范围 |
| `evidence_refs` | 有界引用与经脱敏的短摘录，不持有任意可读路径能力 |
| `memory_edges` | 仅 `supports/contradicts/supersedes` 三种关系，不上图数据库 |
| `source_cursors` + `ingest_jobs` | 来源进度、幂等键、租约/重试；提交事实和推进游标同一事务 |
| `revocations` + `source_suppressions` | 撤销代数、禁止从指定来源重新生成，避免重放“复活” |
| FTS / 可选 embeddings | 可重建派生索引；embedding 必须带模型、维度和内容版本 |

任务投影 ledger 归宿主管，不归插件事实库：只记录 task 边界、记忆 ID/版本/hash 和渲染版本，
不创建第二份对话消息存储。手动原创便签是重要持久数据，不能宣称“数据库全部都可从 WAL 重建”。

### 7. 存储与运行形态

首选独立 Rust 二进制 `kroma-memory`，通过现有 JSONL 插件边界运行，SQLite 随二进制打包。
不把 SQLite 加入 Kroma 核心依赖；无 Node/Bun/Python 环境要求，无默认监听端口。
这属于落地技术建议，尚未验证具体 SQLite crate 版本、MSRV 或发行包尺寸。

推荐**用户级安装、项目级分区**：

```text
<req.data_dir>/
  registry.sqlite3
  workspaces/<workspace_id>/
    memory.sqlite3
    memory.sqlite3-wal
    memory.sqlite3-shm
```

- 每个存储实例内一 workspace 一库，减少跨项目误检索；记录独立 `store_id`。
  同一 workspace 在不同 data_dir 的库是不同副本，不承诺全局只有一库；每个接口仍校验作用域。
- 项目级安装也必须使用宿主给出的 `data_dir`，不能写代码目录；
  当前项目 data_dir 在仓库内，允许作为私有、被 Git 忽略的运行数据；提供忽略规则和明显提示。
  不擅自修改用户已有忽略配置，也不偷偷换到用户目录；若无法确保不被跟踪则提示并拒绝敏感写入。
- 项目级安装位于不同 workdir 的 data_dir 不天然共享；跨 worktree 共享优先采用用户级安装，
  或经用户确认迁移，不能仅生成同一个 workspace ID 就声称已共享数据。
  注册表发现同一 workspace 的已知多个 store 时显示副本信息；link/migrate 先展示版本冲突与合并计划，
  不能静默覆盖，也不为发现副本而扫描用户全部磁盘。
- 启用 SQLite WAL、外键和短事务；初版主库 `synchronous=FULL`，
  明确保证“保存成功”发生在事务提交之后，不仅仅进入进程内队列。
- SQLite 仍然只有一个 writer；短 busy timeout + 有界重试，不让检索等待分钟级锁。
  查询失败可跳过可选召回，显式保存失败必须报错，不能显示“已记住”。
- 多宿主/事件 worker 用独立连接，同库事务协调；后台工作使用租约，崩溃后可重取。
  不依赖不同 worker 的进程内缓存互相可见。
- WAL 模式不支持网络文件系统 [S7]。检测到不支持的共享路径时拒绝该存储配置或提示改本机路径，
  不把 SQLite 数据库纳入 Git 跟踪或网盘同步当协同方案；这不禁止仓库内被忽略的私有 data_dir。
- Unix 私有目录/文件按 0700/0600；Windows 检查/设置适当 ACL，不能用 chmod 假装等价。
- 有界迁移和备份；备份用 SQLite backup 或一致性快照，不在活跃写入时只复制 `.sqlite3`。
  索引损坏可重建，事实库损坏应隔离并提示恢复，不能直接清空后假装没数据。
- 一插件主进程即可，本地检索和后台任务分线程/连接；即使启用事件订阅产生另一个 worker，
  也不额外启动全局 daemon。空闲/取消/关机时允许丢弃可重建计算，不丢已确认保存。

### 8. 写入：默认可控，而不是自动污染

```text
可靠来源 / 用户明确记录
  → 排除与秘密检测
  → 有界证据归档引用
  → 规则提取或可选 LLM 提取
  → schema/作用域/证据校验
  → 去重与冲突检测
  → candidate
  → 人确认（或未来显式开启的严格自动认可策略）
  → 事务提交 accepted + 索引/任务进度
```

默认模式为 `review`：

1. `/memory remember ...` 表示用户显式保存，可直接 accepted，但不自动扩大范围。
2. 模型只能通过 `memory_propose` 写 candidate，不能传 `state=accepted`、伪造来源或删除别人的记忆。
3. 工具原始结果只证明“执行观察”；模型的“测试都过了”不是工具证据，
   interrupted/unknown 结果不能当 success，失败任务也不能提取为成功经验。
4. 先用便宜的确定性规则提议用户纠正和明确决策；复杂 LLM 提取默认关闭。
5. 开启 LLM 提取后，在任务完成且证据稳定时合并处理，默认每个符合条件的任务至多一次，
   有独立输入/输出 token、时间、日额度限制；超额跳过，而非阻塞下一轮主任务。
6. 队列、候选条数、摘录长度均有限额；来源排除与脱敏必须在索引/embedding/模型发送前完成。
7. 同一证据重复重放、从摘要再提取、助手引用旧记忆，都不构成新的独立支持证据。
8. 外部网页、MCP 输出和导入文件带 taint；默认不自动认可其指令性内容，
   用户确认保存也不会把外部数据提升成能覆盖系统约束的指令。

自动化可按类型逐步开放，例如“原始工具退出码 + 环境指纹”形成短期 observation，
但“为什么采用某架构”仍需明确决策来源。不要用“重复出现三次”代替来源校验。

### 9. 冲突、失效与真正的遗忘

#### 冲突

- 同一 claim key + 重叠作用域出现矛盾，建立冲突，不做全文 last-write-wins。
- 用户明确纠正可产生 superseding 版本，保留关系；机器推测不得覆盖人工记录。
- 对实现事实，当前源码/真实工具结果比旧描述更值得核验；对约定，按现有指令体系处理。
  不把一个简单全局优先级公式套到所有类型。
- 无法消歧时停止自动注入矛盾主张；显式搜索可以返回双方、来源和“需要确认”。
- 回答历史问题用当时有效版本，回答“现在怎么做”用当前适用版本；
  `recorded_at` 与 `valid_from/valid_until` 分开，晚记录不必然是晚发生。

#### 失效

- 保存依赖指纹和符号/内容锚点，行号只用于导航，不能作为稳定身份。
- 检索时做便宜的适用性检查；证据文件消失、相关内容变化、环境变化时降为 stale。
- 一般代码改变不等于结论必然为假；重新验证或向用户提示，而不是自动反转主张。
- 初始保留策略建议：candidate 14 天，已结束任务状态 7 天，observation 90 天；
  明确保存的偏好/决策不按时间静默删除，仅复核或被替代。数值是可调整默认值，不是已验证最优值。
- TTL 到期是检索资格变化，物理清理另行执行；频繁被召回不能自动延长真值有效期。

#### 遗忘

- 人工 forget 先提交 tombstone 和 revocation revision，再使查询缓存/投影包失效。
  删除确认之后开始组装的请求不得再注入该内容；已经发出的请求无法撤回。
- 普通 forget 阻止未来自动召回；purge 进一步移除详情、旧版本、摘录、FTS、embedding 和本地生成备份。
  列出用户导出文件、源会话和远端 provider 等不在自动擦除范围内的副本。
- 最小 tombstone 只留 ID/来源抑制信息，不留敏感正文；重放不得从被遗忘来源重新提取。
  不能声称正文 hash 能识别所有语义改写；“忘掉某话题”需要用户明确的持续排除范围。
- SQLite/WAL/SSD 的逻辑删除不保证物理取证不可恢复；提供受控 checkpoint/重建清理，
  对高敏感资料依赖加密磁盘/受管密钥，不承诺无法证明的彻底擦除。
- “遗忘记忆”不自动修改原始对话历史。工具输出、助手引用和压缩摘要都可能保留原内容或改写副本；
  已有任务中不能保证语义遗忘，更不能声称 purge 撤回 provider 已收到的数据。
  显示受影响的可追溯副本，建议开启不带旧历史的新会话；如提供历史投影脱敏，必须另行确认，
  且注明无法可靠识别所有语义改写，不自动改写权威 WAL。

### 10. 检索：作用域先行，相关性其次

```text
宿主绑定当前项目 / 会话 / 用户允许范围
  → 排除 tombstone、过期、未来 lineage、环境不适用、未经认可的自动候选
  → ID/路径/符号精确匹配 + FTS5 候选
  → 可选语义召回并同样限制 workspace
  → 按 ID 再次鉴权与有效性检查
  → RRF 等秩融合 + 去重/多样性 + token 成本选择
  → 小索引/短摘要，必要时按 ID 获取详情与证据
```

- 查询信号只用当前用户意图、当前路径、明确的任务模块、已知错误签名等小对象。
  不每轮把全部历史发给插件，也不默认用 LLM 改写查询。
- 路径提示是相关性线索，不是事实或授权；目前主要工具是 shell，
  从任意 shell 文本精确推断“读了哪些文件”不可靠，不能承诺基于猜测的完美自动附加。
- 初版候选上限建议 60，自动包最多 6 条；显式搜索默认 8 条，支持有界分页。
- 融合使用可解释 rank，不把 BM25、向量相似度与主观 confidence 原值直接相加。
- 回答不足可以返回空；不因“得返回 8 条”而塞入不相关信息。
- search/read/timeline 都必须绑定 workspace；知道别人的记忆 ID 不能绕过隔离。
  ANN 过滤和水合后的二次检查都要做，不能只在展示层过滤。
- 当前任务已包含相同主张时去重；依赖冲突的关联证据不能只取一条而隐藏限制条件。

#### 中文、英文与代码标识符

SQLite 默认 `unicode61` 不会自动完成中文分词；`trigram MATCH` 对不足三个 Unicode 字符的查询无匹配 [S8]。
因此不能简单宣称“加 FTS5 就支持中文”。

第一版：保留标识符原文 + camelCase/snake_case 拆词字段；英文用 unicode61；
中文长子串可用 trigram，常见短查询如“记忆”“分支”走**已限定 workspace、限制扫描量**的字面子串后备。
`LIKE` 必须绑定参数并正确转义 `%/_`，FTS 表达式由安全字面词构造，不能把用户输入直接当操作符。
短查询数据规模增长后，再用自定义 CJK bigram/分词索引替代线性后备，并通过中文评测决定。

向量不是 MVP 前置条件。只有同义表达/中英跨语言漏召回明显时，再加可禁用的本地多语言 embedding；
模型下载须同意，版本/维度改变应重建索引，缺失模型降级 FTS，不阻断记忆功能。

### 11. 上下文注入：由宿主控制预算和稳定性

#### 11.1 不把学习内容写进 system 权威层

静态插件 prompt 只说明“记忆是待核验参考、如何使用搜索工具”。
动态内容由 provider adapter 渲染为清楚标记的**非权威数据块**，不改写原始 system 消息、
不假造已经执行过的工具调用、不插进 assistant/tool 配对中间。
XML/JSON 边界、转义和“data not instructions”只能降低混淆，不能解决 prompt injection 本身。

每条自动呈现内容包含：`id/version`、一句主张、适用范围、证据指针和过期/复核标记。
非必要的完整详情通过显式 read 工具获取，不把所有证据都塞进常驻前缀。

#### 11.2 预算

建议初始自动预算上限：`min(1536 tokens, 当前模型有效输入容量的 1%)`，
包含索引和包装成本；极小窗口可以为 0。常驻使用说明/索引目标不超过 256 tokens。

宿主依据最终 provider 投影、工具 schema、图像和安全余量计算实际剩余额度，
而不是插件按“字符数 / 4”声称精确。处理顺序：

1. 内置策略准备会话投影，宿主同时追踪已存在的可选记忆贡献。
2. 优先回收可选贡献，再决定是否需要压缩原会话；不改变既有压缩阈值和重试政策。
3. 新任务的检索请求收到实际可用预算；没有空间就不自动注入。
4. 插件返回带边界的条目，宿主计量、整条裁剪、渲染，再做最终模型预算验证。
5. 若最终仍超限，移除可选记忆重试组装，不能因记忆迫使原本合法的主任务失败。
6. 宿主贡献在核心预算投影层组装，而不是走旧模型请求 hook；记录源历史水位、贡献 digest、
   provider 渲染版本和最终请求 fingerprint，将返回的 usage 绑定到实际发送的投影。
   冻结 pack 且映射仍有效时复用该投影实测水位；pack/渲染变化先用保守估算，
   到下一次可匹配实测再校准。不能仅因“这是宿主加的”就假定旧水位仍精确。
7. 任意旧 hook 或外部 backend 继续改写投影时，保留现有保守规则；无法确认 usage 对应最终投影，
   就不校准。基础版也不声称解决了旧 hook 的计量限制。

这里的容量/token 目标需要实现后校准；不保证所有 provider 都有可在本地精确运行的 tokenizer。

#### 11.3 缓存与任务快照

只承诺**不随意改写稳定前缀**，不承诺“动态 RAG 零缓存损耗”。

- 自动检索默认发生在新用户任务；同一任务内部的工具/模型循环冻结 pack，避免每轮内容漂移。
- 新 pack 只追加在当前新用户输入对应的投影尾部，处于后续 assistant/tool 配对之前，
  不向更早的稳定前缀插入；provider adapter 选合法的非权威消息表达，不伪造 tool 角色。
  后续请求固定该任务边界，先前 pack 在被压缩/淘汰前不移动、不做就地版本刷新。
- ledger 持久化于宿主拥有的版本化投影元数据，记录 archive/epoch、历史 generation、
  边界消息的 index + digest、context revision、记忆 ID/版本/hash、渲染版本和 render digest。
  resume 必须能按固定版本逐字节重现，锚点或依赖缺失就省略并警告，不猜位置、不悄悄放最新版本。
  此为新的核心状态扩展，旧会话无 ledger 即空贡献，不伪装成纯插件现有能力。
- 当前 checkpoint 跳过 hook 的行为不变；拟议新贡献在核心投影层，普通请求和 checkpoint
  共享已冻结的同一 pack，不在摘要时重新调用插件选择另一套内容。摘要指令显式保留其低信任属性，
  summary 元数据记录依赖的 memory ID/版本，不允许把摘要当独立证据再计票。
  checkpoint 提交后，与被替换前缀相交的 ledger 条目一并失效；仍在保留后缀的条目按锚点保留，
  不把已概括内容当新发现再自动注入。摘要可能转述记忆，撤销时按第 9 节的会话副本边界提示，
  不能保证仅删除 pack 就擦除摘要语义。
- fork 只复制 checkpoint 之前仍有效的锚点并重绑定新 archive/epoch；rewind 丢弃边界之后条目。
  context revision/generation 改变时重新校验，锚点失配即省略；版本或插件缺失不能阻断恢复。
- forget、失效淘汰和预算回收是前缀稳定性的明确例外，可能使其后缓存失效；
  新版本只能在新任务追加，撤销优先于缓存，不能用“冻结快照”保留已撤销的自动数据块。
- cache fingerprint/诊断必须包含贡献 ID/版本/digest，才能解释哪次记忆变化导致缓存前缀变化。

显式 `memory_read` 的结果依现有工具契约进入对话历史；这是主动请求的内容，
与可选自动投影不同。不要为“记忆零 WAL”破坏已存在的工具审计语义。

### 12. 插件用户接口（拟议）

保留三个短 schema，避免工具定义本身吃掉大量上下文。正式名字可加 `project_memory_` 前缀防冲突。

| 接口 | 作用与边界 |
| --- | --- |
| `memory_search(query, paths?, limit?)` | 返回 ID、短标题、适用范围、来源类型；workspace 由宿主绑定，不能任意传另一个项目 |
| `memory_read(ids, include_evidence?)` | 批量有界读取，重新校验授权、撤销和版本；可选相关时间线，不自动递归拉满全部历史 |
| `memory_propose(kind, text, evidence_refs?, scope_hints?)` | 创建候选；宿主/采集器验证 evidence，模型声明的引用不自动被认为真实 |

显式读写失败返回结构化错误并说明未完成；自动召回失败仅警告/指标后跳过。
默认不提供模型可直接调用的 purge、scope-link、accept 或规则编辑工具。

用户命令统一在 `/memory`：

```text
/memory                         # 状态、作用域、待审核、当前采集/使用模式
/memory remember <text>          # 明确保存；缺省为本用户当前项目
/memory search <query>
/memory show <id>
/memory accept <id>              # 检查内容和范围后认可候选
/memory correct <id> <text>      # 产生替代版本
/memory forget <id>              # 撤销未来召回
/memory purge <id>               # 显示擦除范围并确认
/memory why <id>                 # 来源、适用条件、本次被选中的理由
/memory doctor                  # 存储、过期、冲突、游标和索引健康
/memory use off                  # 不用旧记忆；不必同时停止新候选采集
/memory capture off              # 不贡献新记忆；可继续只读召回
/memory off                      # 两者关闭；禁止后台补采本范围
/memory export --shared          # 审核和脱敏后导出可审阅 Markdown
```

工具接口的权限限制不等于防御拥有同一 OS 用户权限的恶意代码；当前 Kroma 没有这样的沙箱保证。
命令的 human origin 应由宿主标记，不能信任模型传一个 `origin=user`。

TUI 只用现有动态面板呈现状态、当前项目、候选/过期计数和查询解释；
实时 token 使用状态栏需要新的宿主状态通路，现有静态 `ui.status` 不能冒充实时推送。
CLI 输出相同信息的文本/JSON，不单独实现另一套记忆逻辑。

### 13. 哪些现在能做，哪些必须扩展宿主

#### A. 无核心改动：可发布的安全基础版

使用现有 tools、commands、settings、data_dir、动态面板：

- 保守 workdir 命名空间、Git root 元数据解析、SQLite 事务、手动保存/审核/删除、
  作用域检索、中文后备、迁移、导出；跨 workdir/worktree 自动共享不属于 P0。
- prompt 只教模型何时搜索；详情通过工具按需返回，保留现有工具 WAL 语义。
- 不注册 `[context]`，不默认注册 `before_model_request`，不假装已经实现可靠自动召回。
- 可以把 event 当提速提示/诊断，但持久来源未接通前不承诺自动学习完整性。
- 自动召回只是模型按说明主动调用工具，模型可能不调用，这是基础版明确的能力边界。

可选实验性 hook 若实现：无变化回 `{}`；捕获插件内错误后回空 patch；
避免大历史整数组回传。但宿主层超时/崩溃仍会失败，且现有 hook 没有有预算的增量块接口，
所以它不是最终生产路径，也不承诺仅靠 try/catch 就能 fail-open。

#### B. 生产自动召回必要宿主扩展（职责最小，不是改动行数少）

**以下全部是新提案，不是现有 Kroma API。**优先做三个职责清楚的能力，而非通用 agent 框架：

1. **受控上下文贡献 `request_context`**：接收最小任务信号/预算，返回可独立丢弃的数据块；
   宿主负责超时、取消、预算、provider 格式、ledger、缓存诊断和低信任语义。
2. **只读、分页、可重放的会话来源通路**：宿主提供已提交消息和来源游标，复用 SessionStore 的
   snapshot/WAL 校验，而非让插件自行猜测活跃文件格式；事件只唤醒读取。
3. **统一身份快照 `ProjectIdentity + SessionLineage`**：root/workspace/worktree、HEAD/dirty 指纹、
   task/session/epoch 和允许的父 checkpoint 范围，不让插件把路径猜测当权威身份。
   `task_id/epoch` 是新元数据：宿主创建、版本化持久化并定义 fork/rewind 映射，
   旧会话恢复时初始化兼容状态，不假装现有事件里已经带这些稳定 ID。

协议建议升级为 **插件 API V2，宿主同时兼容 V1**，有显式能力协商；
不能把新 manifest 字段默默塞给严格 `api_version == 1` 的现有宿主。
这与已有 `[context] version = 2` 是两个不同版本号；后者并不提供本文的追加贡献能力。
实现要拆开当前全局常量的使用：宿主接受 manifest API 1/2，worker 保存自己的协商版本；
V1 仍收发 `protocol_version=1` 并走旧响应校验，V2 才进行能力握手和 V2 帧校验，
不能仅把 `PLUGIN_API_VERSION` 改成 2 就声称兼容。P0 发行 API 1 manifest；
需要新能力的包显式要求 V2 宿主，旧宿主应在启动插件前清楚拒绝，而非静默失去隔离/容错。

拟议上下文能力的数据对象（省略 V2 外层请求关联字段）：

```json
{
  "type": "request_context",
  "project": {"workspace_id": "ws_id", "worktree_id": "wt_id"},
  "session": {"id": "session_id", "epoch": "epoch_id", "task_id": "task_id"},
  "query": {"intent": "有界任务描述", "paths": ["src/plugins/**"]},
  "budget": {"max_tokens": 1024, "max_bytes": 16384, "deadline_ms": 100}
}
```

拟议响应数据：

```json
{
  "blocks": [{
    "id": "mem_id@3",
    "trust": "reference_data",
    "text": "有条件的短结论与证据指针",
    "evidence_refs": ["ev_id"]
  }],
  "revision": 42,
  "status": "ok"
}
```

`max_tokens` 最终由宿主执行，插件不能把字段写成 10 就让长文本逃过预算。
插件自行声明的 `trust` 只能更低，不能提升内容权限。超时、格式错、崩溃、超大小时，
宿主跳过这一个可选贡献并提示，不让整个模型请求失败；显式用户存储命令仍报告真实错误。
这些容错策略只适用于新贡献接口，不能偷偷改变现有 hook 的拒绝语义。

#### 来源通路的可靠性契约

- 游标不能只有消息序号：绑定 archive identity、generation、lineage/epoch 和消息位置/内容摘要。
  rewind、snapshot 重置和 fork 产生新的可识别边界，不能让旧未来记录误入当前任务。
- snapshot/WAL 是会话权威来源。插件在任务完成/恢复时通过宿主驱动的有界页读取，
  支持按已提交游标继续；如采用插件主动读取，则需明确实现 V2 双向 RPC，V1 目前没有它。
- 单页字节/条数限制；记录幂等 ID，不以活跃 `.wal` 的文件偏移作为稳定 cursor。
  当前 `read_archive_messages` 是内部完整读取函数，不是现成分页 API；需要在保留其校验语义下
  新增一致读边界与有界访问实现，评测大历史恢复成本，不能只改可见性就宣称完成。
- 插件事务提交 facts/jobs + cursor 后才确认；崩溃允许重投，通过唯一键去重。
  这叫 **at-least-once + 幂等提交**，不宣传跨宿主与 SQLite 的分布式 exactly-once。
- 来源被用户删除或明确禁止采集时不补采，并反映资料不完整；不偷偷扫全部历史目录。
- foreground 只提供边界/轻量提交，摘要/embedding 后台做；退出前不强制等待昂贵学习。

可选的后续能力是受宿主管理、可取消、独立计费的后台模型调用；
没有这一服务时默认不用模型，或由用户明确配置独立 provider 并在插件账本披露费用。
不能借主会话暗中运行无记账的额外 Kroma 子代理做记忆提取。

### 14. 安全、隐私与团队共享

- Local storage 不等于内容不出机：一旦召回进入模型请求，就发给当前 provider；首次启用明确说明。
- 不存 API key、令牌、私钥、cookie、完整 `.env`、凭据 URL；
  `.memoryignore` 与显式 capture 排除先于提取和向量化，默认不读取环境变量值。
- 脱敏器是防误存防线，不是数学保证；未知敏感信息依赖用户审核、数据最小化和组织策略。
- 记忆中的 shell 命令只是历史数据，绝不自动执行；路径引用不能跟随到未授权项目或任意系统文件。
- 文件/导入大小、数量、解析深度、查询长度、候选数和输出都有上限，防存储/上下文 DoS。
- 防提示注入同时依赖低信任呈现、来源 taint、候选审核、不允许模型直接认可/授权，
  以及现有宿主安全边界；不声称一个标签就能消灭模型级注入。
- 无默认网络、遥测、远端同步；依赖和发布产物可校验。
  capability 声明是最小权限设计的依据，但当前插件 OS 权限仍无沙箱强制。
- 团队共享通过审核后的 `docs/...` 或约定的 `.kroma/memory/shared/*.md` 导出和 Git review，
  后者只是拟议约定，不是当前宿主内建扫描目录。绝不同步 SQLite/WAL、私有偏好、原始会话或机器路径。
- 导入内容作为外部来源 candidate，不能自动成为规则；删除共享源会使导入版本待复核。
  若需要强制指导，用户另行把合适内容写入 `AGENTS.md`，插件不自动替用户改写。

### 15. 故障行为与可观测性

自动贡献遵循 **fail-open 主任务、fail-closed 越权读取**：

| 故障 | 行为 |
| --- | --- |
| 库被锁、索引不可用、查询超时 | 跳过可选召回，给出简短 warning；不拿上次别的项目结果兜底 |
| 身份不明确或授权不满足 | 不读不写该范围，提示选择 root；不能改成全局搜索 |
| 保存失败/磁盘满 | 明确“未保存”，不推进来源游标，不显示成功 |
| 插件进程崩溃 | 新贡献接口由宿主熔断/跳过；下次可重启，已提交数据不受影响 |
| 源数据缺失/未来被 rewind | 跳过不适用任务状态，标记证据不可核验 |
| embedding 下载或模型额度不足 | 退回本地字面检索/暂停学习；不影响已保存便签 |
| 用户取消 | 停止当前查询/后台任务；已提交事务保留，未提交事务回滚，不重放工具副作用 |

指标：检索 p50/p95、超时/跳过数、最终注入 tokens、IPC 字节、候选认可/拒绝率、
过期/冲突/撤销数、来源处理滞后、后台模型 calls/tokens/费用、记忆导致的投影 digest 变化。
日志默认不写正文、用户 prompt、密钥或原始工具输出。
UI 的“本轮使用 4 条、约 680 tokens”必须来自真实宿主计量；未知就显示未知，不编造节省比例。

### 16. 分阶段实施与迁移

#### P0：可信的手动项目记忆，纯插件（已实现）

- 独立 Rust crate + 插件清单：`examples/plugins/project-memory`；根工作区不依赖它。
- 已实现 SQLite（WAL/FULL/IMMEDIATE）、按 canonical workdir 隔离、search/read/propose、
  `/memory` 用户命令、候选审核、纠正/遗忘/清除、旧 `memory.json` 导入、分页导出、doctor。
- 只有人类能接受候选；模型提案永远是 candidate；遗忘/清除保留同一 kind/text/paths
  及历史版本指纹，阻止精确复活和导入重放。
- model search/read 只返回 accepted，遵守 workspace 的 `use` 模式；无网络、无 model hook、
  无 `[context]` 替换、无自动注入。
- 默认按 canonical workdir 隔离，不合并 Git worktree/clone；移动目录视为新 workspace。
- 验收：42 个 crate 单元测试通过（含跨连接并发写、纠正后遗忘的历史版本、FTS 生命周期），
  宿主集成测试通过真实 `PluginCatalog`/工具/命令/重启/隔离/禁用流程；
  Windows 私有 DACL、Unix 0700/0600、重解析点拒绝有测试。并发写由 SQLite 单写者事务串行化，
  但未做多机或网络文件系统验证。
- 已知限制：secret 检测是尽力而为；`forget/purge` 不改写历史、摘要或 provider 副本；
  语义改写的新文本不会被指纹拦截；同用户恶意进程不受沙箱保护；
  没有自动上下文预算（见 P1b），因此“省 token”取决于模型主动调用检索。

#### P1a：宿主身份、来源和兼容协议

- 做 API 1/2 分版本 worker、能力协商、统一身份、新 task/epoch 元数据及其持久化/分支规则。
- 实现只读有界来源通路，先验证重放、权限和游标，不急着启动自动学习。
- 验收：旧插件帧不变、旧会话可恢复、fork/rewind 来源范围正确、重投去重、跨 worktree 共享须明确授权。

#### P1b：受预算约束的自动召回

- 在 P1a 基础上实现低信任投影、预算/usage fingerprint、fail-open 和带锚点的可恢复 ledger。
- 不改内置压缩算法和原始 WAL 语义；新增投影状态需要单独迁移与恢复测试，不能当作几行 hook。
- 验收：空记忆无贡献；无大历史整段 IPC；预算、取消、compaction、resume/fork/rewind 一致。

#### P2：可靠来源与保守学习

- 接通可重放游标、后台队列、证据校验、冲突/失效管理；用户选择开启 LLM 提取。
- 验收：事件丢失可补、重投无重复、被禁止采集范围不会补采、未来状态不会污染回退会话。
- 不把“任务结束”当“所有结论正确”；需要用户认可或明确类型的强证据策略。

#### P3：有证据再优化

- 用真实漏召回样例决定是否添加本地多语言 embedding、重排或更细的模块激活。
- 团队导出审核、过期治理、批量压缩只在质量指标支持时加入。
- 只有实际编码任务的净收益成立才宣传效率；不以数据库行数、向量数量或 QA 命中率代替。

#### 现有 `memory.json` 迁移

1. 用户选择源文件并先备份/导出；迁移成功前不要卸载旧插件，其数据目前位于安装目录。
2. 旧数据没有可靠 project/source 元数据，必须选择目标 workspace；
   用户级旧文件不自动复制给所有项目，不能事后假造来源。
3. 记录 `origin=legacy_import`、导入 ID/hash 和原始序号，只导入一次；默认 candidate，支持用户批量审核。
4. 新插件启用后停用旧 memory，避免工具/命令冲突和双重注入；不要通过读取旧目录长期双写。
5. 如果提供旧 `/remember` 等别名，必须先检查冲突；旧数字删除命令不可直接映射到新列表中变化的行号，
   应提示使用稳定 ID，而不是冒险删除另一条。
6. 卸载新代码保留 data_dir；清除记忆必须是独立、明确确认的数据操作。

### 17. 评测与验收：先证正确，再谈“最棒”

#### 确定性回归矩阵

这是完整方案矩阵，不是 P0 全部已具备的能力。P0 验 workdir 隔离/手动读写/存储；
宿主身份、投影、分支、API V2 等行在 P1a/P1b 分别验收，后台学习在 P2 验收。

| 维度 | 至少覆盖 |
| --- | --- |
| 隔离 | 同名不同仓库、同 remote 不同 clone、monorepo、子目录、子模块、worktree、非 Git root、ID 越权读取 |
| 生命周期 | 新会话、退出恢复、进程被杀、fork/rewind、Git checkout/rebase、dirty 文件、缺失历史、插件被禁用 |
| 持久性 | 多进程并发写、读改写冲突、busy、断电/硬杀、磁盘满、损坏库、迁移中断、备份恢复 |
| 检索 | 中文 1/2/3 字查询、中英混写、camelCase、snake_case、符号/路径、引号与 MATCH 特殊词、空结果 |
| 信任 | 外部提示注入、伪造 user 来源、伪造测试成功、摘要重复计票、恶意路径、秘密值、超大候选 |
| 记忆质量 | 纠正旧值、时间版本、相互矛盾、依赖变化、证据消失、没有证据时拒绝结论 |
| 遗忘 | 所有版本/FTS/向量/缓存/ledger 撤销，重放不复活，工具历史副本边界提示 |
| 宿主 | 无插件零 IPC；空记忆零注入；V1 不回归；2 MiB 旧响应限制；工具 call ID 配对；hook/插件组合 |
| 上下文 | 最终预算、图像/tool schema、极小窗口、实际 token 校准、缓存诊断、compaction/checkpoint 一致性 |
| 前端 | CLI/TUI/Desktop 共用核心行为，动态面板失败不进入对话，不暗中增加模型调用 |

#### 真实任务实验

建议建立至少 200 个带时间顺序的项目任务样例，覆盖多个语言/项目，人工标注应该记住、
应该召回、必须拒绝及已过期的主张。未来任务/最终解法不得进入较早任务的索引。
按项目与时间拆分调参/验收集，避免相同证据泄漏。

同一模型/配置、同一代码快照、相同权限做配对比较：

- 无记忆；
- 当前便签示例；
- 新插件仅手动检索；
- 新插件自动召回；
- 需要时再比较加入自动学习/向量后的增量。

主要指标：真实任务成功率、用户重复纠正率、重复无效工具调用、错误/过期主张采用率、
端到端耗时、模型总输入和缓存命中用量、后台费用、人工审核负担。
检索的 precision/recall@k 只作为解释指标；采用记忆后答错比不召回更需要惩罚。
记录多次运行的离散度/置信区间，不凭一次生成宣称提升。

以下是**尚未测得的初始工程目标**，不是当前保证：

- 本机 SSD、单 workspace 一万条短记忆、无 embedding：热检索 p95 ≤ 50 ms；
  前台可选贡献预算 ≤ 100 ms，超时就跳过；冷启动另测，不混入热缓存数字。
- 自动包 ≤ 第 11 节上限，默认不额外调用模型；可选学习通常每合格任务 ≤ 1 次。
- 已标注应召回集合的自动呈现 precision 目标 ≥ 90%，宁可少给；
  跨项目泄漏、撤销后复活、伪造工具成功在确定性/对抗测试集中必须为 0。
- 没有足够证据证明端到端任务收益和成本优势，就不默认启用自动学习或向量组件。

#### 本次研究实际做了什么

- 静态阅读当前插件、模型请求、会话/WAL、审批、上下文与生命周期契约；没有改动实现。
- 委托审计运行现有 `node --test examples/plugins/memory/memory.test.js`，1/1 通过；
  它只覆盖简单便签生命周期，不覆盖本文生产设计。
- 委托审计用临时文件按 A 读 → B 写 → A 旧快照写的确定性交错，观察到 B 的便签丢失；
  普通并发压力未稳定触发不代表不存在读改写窗口。
- 本机 Node 22 的内存 SQLite 3.51.3 实验做 4 项断言：默认 unicode61 不命中中文子串；
  trigram 不命中两字 MATCH；四字 MATCH 命中；LIKE 后备命中两字。全部通过。
  这是索引选择实验，不证明最终发行二进制已支持这些能力；Node 的 SQLite 在该版本提示 experimental。
- P0 插件随后实现并验证：crate 内 42 个测试、`cargo fmt`、`cargo clippy -D warnings`、
  宿主 `tests/project_memory_e2e.rs`（安装真实清单、候选→接受、重启持久化、workspace 隔离、禁用）
  均通过；`kroma plugins check` 的 ping 冒烟通过。
- V2 协议、自动注入、延迟目标、缓存收益和真实编码收益均尚未实现/测量。

### 18. 建议的改动边界与源码导航

后续实现优先落插件，核心只增加前述通用边界，不让 `agent.rs` 承担记忆业务逻辑。

| 位置 | 建议责任 |
| --- | --- |
| 新插件源码与 tests | 存储、scope、检索、提取、失效、命令和迁移；不复制 Agent Loop |
| `src/plugins.rs` / 新增专门适配模块 | V1/V2 协商、worker 协议、可选贡献故障策略 |
| `src/context.rs`、`src/prompt.rs`、provider 投影边界 | 低信任块、预算、稳定插入、缓存计量；保留内置压缩 |
| `src/session.rs`、`src/context_state.rs`、`src/ports.rs` | 有界已提交来源读取、lineage 与投影元数据一致提交；会话 WAL 仍归宿主 |
| 现有身份解析/`app.rs` composition | 统一只读身份，不重新实现一套 checkpoint 或修改用户 Git 状态 |
| `tests/plugins_e2e.rs`、`tests/context_compaction_e2e.rs`、生命周期与恢复测试 | 新能力 + V1/WAL/预算/取消/恢复回归 |

不应为了记忆改动 Shell 输出保留方式、工具执行并发、现有恢复副作用语义或另建宿主事件总线。

相关本地资料：

- [插件协议与示例](../examples/plugins/README.md)
- [现有便签实现](../examples/plugins/memory/memory.js)
- [模型请求 hook](../src/plugins/model.rs)
- [插件装配与进程](../src/plugins.rs)
- [会话与 WAL](../src/session.rs)
- [运行时边界](architecture.md)
- [生命周期](lifecycle.md)
- [安全边界](security.md)
- [上下文与缓存设计记录](design-notes.md)
- [插件路线图](../PLUGIN_ROADMAP.md)

### 19. 外部一手参考

- **[S1]** Claude Code, How Claude remembers your project：<https://code.claude.com/docs/en/memory>
- **[S2]** OpenAI, Memories（本次请求由 developers.openai.com 重定向至此）：<https://learn.chatgpt.com/docs/customization/memories?surface=app>
- **[S3]** OpenAI, Custom instructions with AGENTS.md：<https://learn.chatgpt.com/docs/agent-configuration/agents-md>
- **[S4]** Cursor, Rules：<https://cursor.com/docs/rules>
- **[S5]** claude-mem, Architecture Overview（项目原始文档）：<https://raw.githubusercontent.com/thedotmack/claude-mem/main/docs/public/architecture/overview.mdx>
- **[S6]** claude-mem, Smart Memory Search：<https://raw.githubusercontent.com/thedotmack/claude-mem/main/docs/public/usage/search-tools.mdx>
- **[S7]** SQLite, Write-Ahead Logging：<https://sqlite.org/wal.html>
- **[S8]** SQLite, FTS5 / Tokenizers：<https://sqlite.org/fts5.html#tokenizers>
- **[S9]** LongMemEval，原作者仓库与评测说明：<https://github.com/xiaowu0162/LongMemEval>


## 二、V2 契约：Project Memory v2 contract（实现）

Status: implemented for host and plugin (2026-11-06). Additive; V1 plugins unchanged.

### Manifest

```toml
api_version = 2
capabilities = ["provide_context", "task_finished"]
```

- Preferred generic capabilities: `provide_context`, `task_finished`.
- Legacy V2 `request_context` and `task_summary` remain supported without a migration step.
- Both require `command`.
- A V2 plugin may also declare tools/commands; V2 only changes the worker protocol version
  and enables the two host calls.
- V1 manifests must not declare capabilities.

### Worker protocol

Every request envelope keeps `protocol_version`, `request_id`, `config`, `data_dir`,
`workdir`; for V2 `protocol_version` is `2`. Responses echo the same `protocol_version`
and `request_id`, with `content` and `is_error` as today.

### Host -> plugin: provide_context (legacy: request_context)

Sent at most once per user task, before the first model request of that task, only to
enabled plugins declaring the capability.

```json
{
  "type": "provide_context",
  "name": "provide_context",
  "arguments": {
    "intent": "latest user request, bounded",
    "paths": ["optional relative hints"],
    "budget": {"max_tokens": 512, "max_bytes": 8192},
    "project": {"workspace_id": "opaque", "git_root": null, "workdir": "..."},
    "session": {"id": "...", "archive": "..."}
  }
}
```

Response `content` is a JSON string:

```json
{"blocks": [{"id": "opaque", "kind": "preference", "text": "bounded text", "paths": []}]}
```

Host rules:

- max 6 blocks, max 1024 tokens, per-block text <= 1200 chars; whole entries only.
- empty blocks => no injection, byte-identical request.
- timeout/error/malformed/oversized => skip this plugin, never fail the model request.
- inject as a low-trust `<plugin-context>` block appended to the last user
  message (projection only; WAL untouched). Each block carries the originating
  plugin ID, and text is escaped, not interpreted as instructions or markup.
- `conflict` and `last_activity` ephemeral notices can omit a durable ID;
  normal rule/review entries must include one.
- stable pack cache keyed by archive + boundary user-message digest; re-apply identical packs on
  later tool rounds and later turns until compaction/rewind makes the boundary mismatch.
- validate the final request; if the new pack breaks the budget, remove it and retry once.
- do not set `usage_matches_projection=false` merely because a host pack was added; V1 hook
  rewrites keep the existing conservative behavior.

### Host -> plugin: task_finished (legacy: task_summary)

Sent after the Agent finishes a task and the final WAL boundary has been
committed or attempted, **not** after an intermediate model message without
tool calls. Calls are non-blocking, bounded, best effort; a slow or crashed
plugin does not delay the next user operation. The host captures the session
ID and archive path before dispatch so switching sessions cannot retarget the
notification. Model-only consumers should call the explicit lifecycle hook.

```json
{
  "type": "task_finished",
  "name": "task_finished",
  "arguments": {
    "status": "completed",
    "recent_users": ["last up to 8 user messages, each <= 2000 chars, total <= 8192 bytes"],
    "project": {"workspace_id": "opaque", "git_root": null, "workdir": "..."},
    "session": {"id": "...", "archive": "..."}
  }
}
```

Response content: `{"ok": true}` or any JSON; host ignores content. Duplicate summaries are
allowed; the plugin must be idempotent.

### Plugin write semantics

- `memory_remember(kind, text, quote?, core?, paths?, evidence?, supersedes?)`
  - `preference` / `decision`: require `quote` 8..500 chars. Host-verified
    current-user quotes become active immediately (up to five distinct rules
    per user turn) only when the restatement shares a distinctive term with the
    quote (one ASCII word of >= 4 chars or one two-character CJK run). A
    restatement without a shared term, and any entry from a legacy host without
    trusted turn context, is stored `pending` and is not searchable until later
    promotion.
  - `review`: require >=1 path and >=1 evidence reference. Store active with a 30-day expiry;
    always labeled unverified in output.
  - `core` (optional boolean, default `false`) pins the entry as one of at most
    four active core project rules. A non-boolean value, `kind = "review"`, or
    an entry that would only be stored `pending` is rejected with
    `invalid_request`, since core status requires an accepted
    `preference`/`decision`. A fifth core write fails with "Core project rules
    are full (4); forget an obsolete rule first". The pin is written in the same
    transaction as the memory, a `supersedes` replacement inherits the replaced
    rule's core status, and core rows are exempt from the active-memory archive
    limit. Only this compact pack bypasses task-keyword matching and is injected
    on every task.
  - Secret scan, size limits, path validation; no raw tool output.
- `task_finished`: for each pending entry, normalize whitespace/case and promote only if its
  quote is a contiguous substring of the current (latest) user message. Apply `supersedes` on
  promotion. Drop a pending entry when that message does not match its quote; the 7-day bound only
  applies to a database that never receives this call. Idempotent. The bounded user-request hint is
  kept only when this task wrote a memory through `memory_remember` or promoted one here; otherwise
  only the status is stored.
- `memory_search` / `memory_read`: active, unexpired entries only.
- `memory_forget(id)`: immediate tombstone; future search/read/request_context exclude it.
  `/memory restore ID` is the explicit undo for an accidental forget (purge is irreversible).
- `memory_remember` and `memory_forget` declare `invalidates_context=true` so
  a same-turn follow-up model request re-fetches context after a mutation.
- `/memory off` stops new writes and returns empty recall; `/memory on` restores.
- Existing advanced commands may remain but are not required for normal use.

### Plugin identity and storage

- Git: resolve the common dir (handle `.git` file/worktree `commondir`), read or create
  `kroma-memory-id` there. All worktrees share it; moving the repo keeps it; clones do not.
- Non-Git: fall back to a user-level registry keyed by canonical workdir.
- Database: `<host data_dir>/projects/<id>/memory.sqlite3`; private permissions; no repo writes.
- Defaults: automation enabled; no approval queue for the tool path.

### Failure policy

Fail-open for recall and summary. Explicit remember/forget may return a structured error.
No network. No raw tool output capture. No automatic execution of memory text.


## 三、零维护流程：零维护项目记忆（Project Memory v2 设计）

状态：**R0 + R1 已实现**（2026-11-06）。P0（`examples/plugins/project-memory`）是上一版
“人工审核优先”的实现，存储、隔离、事务和隐私底座可以复用，但默认交互要按本文重做。
本文取代本文「一、设计」中的产品默认值和 P1 路线。
宿主 V2 能力、插件自动召回、原文锚定、pending 验证、Git 项目身份和 7/30 天过期已实现；
R2（更完整的审查台账/维护）和 R3（embedding/团队）仍未启用。实现契约见
本文「二、V2 契约」。

### 1. 一句话

**默认自动记、自动用、自动过期；用户正常使用零命令，只在记错时说一句。**

好的项目记忆像靠谱的同事：平时不打扰，相关时提一句，错了改一次就记住。

### 2. 产品承诺

- 安装不超过一步，不需要 Rust、Node 或数据库运行时。
- 正常使用一周，用户主动操作不超过一次。
- 只记用户明确说过、或工具结果真正验证过的事；不记模型猜测。
- 相关时自动出现；不确定时不出现；没有结果就什么都不加。
- 记错一次即可纠正，之后不再犯。
- 本地优先，无网络、无遥测、无后台账号。

**非目标：**

- 不是聊天归档、知识库、团队 wiki 或个人第二大脑。
- 不追求“记住所有事”；宁可少记，也不制造维护负担。
- 不做审批队列、标签体系、手动整理、定期清理。
- 不自动执行记忆中的命令、建议或权限。

### 3. 旧设计为什么不合格

| 旧设计 | 用户实际感受 |
| --- | --- |
| 模型提案进入候选队列，等 `/memory accept` | 每次会话多出待办，几周后无人处理 |
| 模型必须主动调用搜索，否则记忆不存在 | 用户不知道它有没有用，也不敢依赖 |
| 15 个命令、5 种类型、use/capture 双开关 | 先读文档、再记命令，不是开箱即用 |
| `cargo install` 并配置 PATH | 非 Rust 用户直接放弃 |
| 完整版本、Git worktree 身份、V2 协议 | 工程复杂度高，但日常体验没变好 |
| 让用户判断每条记忆对不对 | 把系统该做的质量工作转嫁给了用户 |

根因：把“防止污染”实现成了“用户审批”。正确的做法是**提高自动准入的质量、默认过期、一步撤销**，
而不是要求用户维护一个队列。

### 4. “零维护”的量化定义

| 指标 | 目标 |
| --- | --- |
| 安装到首次可用 | ≤ 2 分钟，1 个命令，无工具链 |
| 正常使用主动操作 | ≤ 1 次/周；目标 0 |
| 错误记忆率 | ≤ 1 次/50 个任务，且一次纠正后不再出现 |
| 自动召回准确率 | 注入内容 ≥ 80% 与当前任务相关 |
| 上下文开销 | 默认 ≤ 512 tokens/任务，硬上限 1024 |
| 过期清理 | 100% 自动，无需用户操作 |
| 记忆数量 | 每项目活跃 ≤ 200 条，自动合并/归档 |

这些是验收目标，不是当前实测结果。

### 5. 核心机制：四道闸

自动记忆必须有闸门，否则“零维护”会变成“零信任”。

1. **原文锚定准入**
   模型想记一条用户规则，必须同时提交用户原话片段；插件校验片段确实出现在最近用户消息中。
   对不上就不自动保存。这一条挡住大多数模型臆想。

2. **默认过期**
   偏好/决定长期有效，但可被新说法替代；审查发现、任务观察默认 7–30 天过期；
   审查发现记录的文件变化后，只在该 worktree 内退出召回。用户永远不需要清理。

3. **显式替代，不做语义猜测**
   文本不同不等于冲突：同一主题下的两条规则可以同时有效。只有模型传入
   `supersedes` 指向 `memory_search` 返回的具体 id 时，旧规则才被撤销。
   判断语义属于模型，Harness 只记录模型给出的替换关系。

4. **一步撤销**
   用户说“忘掉刚才那条”或“不是 X，是 Y”，模型调用 forget/correct 立即生效；
   无需确认、无需进设置页。删除留下指纹，防止同样内容被重新学回来。

### 6. 写入：三种来源，一条主路径

#### A. 用户明确规则（主路径）

用户在对话中说出持久规则，例如“这个项目用 pnpm，不要 npm”“提交前先跑 clippy”。

- 模型调用 `memory_remember(kind, text, quote)`。
- 插件校验：
  - `quote` 是最新用户消息的连续子串（只归一化空白和大小写）；
  - 长度 8–500 字符；
  - `kind` 只能是 `preference` 或 `decision`；
  - 每条用户消息最多自动接受一条；
  - 通过秘密扫描、长度和路径校验。
- 通过后立即 active，**不排队、不要求确认**。
- 用户界面上最多出现一行“记住了：提交前跑 clippy（说‘忘掉’可撤销）”，不阻塞对话。

#### B. 用户纠正

用户说“不对，应该用 bun”。模型调用 `memory_remember(kind, text, quote, supersedes=id)`。

- 必须带原话锚定；
- 旧条目立即 superseded，只保留上一版用于 undo；
- 不弹确认框，不进入审核队列；
- 指纹记录旧内容，防止重犯。

#### C. 模型标记的审查发现

模型在审查任务中调用 `memory_remember(kind=review, text, paths, evidence)`。

- 必须带至少一个路径提示和一条证据引用（命令、文件、会话 id）；
- 状态为 `provisional`，不是“已确认事实”；
- 默认 30 天过期，或相关文件指纹变化后 stale；
- 自动召回时明确标注“未验证的历史审查记录”；
- 不把“未发现问题”记成“代码安全”。

#### D. 任务摘要（可选，低风险）

宿主在任务结束时发送有界的任务摘要（时间、用户请求、状态、提交号、变更文件）。
插件只保存 `last_activity`，默认 7 天，用于回答“上次做到哪”，不自动升级为规则。
只有本任务通过 `memory_remember` 写入、或在任务结束时提升了记忆，才保存有界的用户请求前缀；
否则只保存任务状态。

**一律不自动记：** 工具原始输出、网页/MCP 内容、模型猜测、助手总结、秘密值、
与代码一眼可见的结构信息。

### 7. 读取：任务开始时的一次注入

- 只在**新用户任务开始**时召回一次；同一任务的工具轮次不重复注入。
- 查询信号：当前用户请求 + 最近路径/符号 + 当前项目。
- 过滤：active、未过期、未争议、路径存在；provisional 可以注入但必须标注。
- 排序：精确路径/符号 > 全文检索 > 用户明确确认 > 新近程度。
- 预算：默认 512 tokens，硬上限 1024；最多 6 条；单条 ≤ 400 字符。
- 没有足够相关的结果就**什么都不注入**。
- 失败（超时、插件崩溃、格式错误）直接跳过，不影响主任务。
- 注入格式是低信任数据块，例如：

```text
<project-memory trust="unverified-reference" workspace="kroma">
- [preference] 提交前先跑 clippy（用户明确要求）
- [review, src/plugins.rs] 该模块的 hook 超时会让整个模型请求失败（2026-10-12，未验证）
</project-memory>
```

- 模型仍可调用 search/read 深挖，但这是可选能力，不是日常路径。
- 缓存：先接受“每次任务一次注入”的跨轮缓存损失；测量后如果损失大于收益，再做持久 pack ledger。

### 8. 项目身份：把 ID 放进 `.git`，用户不用配置

- Git 项目：在 Git common dir 写入 `kroma-memory-id`（worktree 的 `.git` 文件指向它；
  只写本地 Git 元数据，不进仓库、不被提交）。
  - 同一仓库的所有 worktree 共享 common dir，因此自动共享记忆。
  - 移动/重命名仓库后 ID 仍在，记忆不丢。
  - 重新 clone 得到新 ID；fork/副本不会自动继承原项目记忆。
  - 子模块有自己的 `.git`，默认独立。
- 非 Git 项目：在用户目录的 registry 里按 canonical root 记录一个 ID；移动目录视为新项目。
- 数据放在 `~/.kroma/memory/<id>/memory.sqlite3`，不写项目仓库，不产生 Git 变更，
  不触发插件审批指纹问题。
- 用户无需选择项目根、无需 link/migrate、无需理解 worktree。
- 子代理默认只读继承同一项目记忆，不自动写入，避免审查子任务污染主项目。

这比“按 canonical workdir 隔离”更符合开箱即用：从任意子目录或 worktree 打开，都是同一个项目。

### 9. 用户界面：正常路径零命令

**模型工具（4 个，短 schema）：**

| 工具 | 用途 |
| --- | --- |
| `memory_remember` | 记用户规则、审查发现；必须带原话或证据 |
| `memory_search` | 深挖历史，不用于日常自动召回 |
| `memory_read` | 按 id 读详情 |
| `memory_forget` | 用户说“忘掉”时撤销 |

**用户命令（只保留 3 个）：**

```text
/memory                  # 一屏状态：最近记忆、是否开启、是否有冲突
/memory forget <id|文本>  # 忘掉
/memory off|on           # 总开关
```

- 主要交互是自然语言；“记住这个”“忘掉刚才那条”由模型翻译成工具调用。
- 不暴露 kind、路径、版本、候选队列、冲突列表。
- 冲突只在相关话题再次出现时提示一次，例如：
  “关于包管理器有两条冲突记忆（pnpm / bun），已暂停自动使用。说‘用 bun’即可解决。”
- `/memory` 默认只显示 5 条最近记忆和一行健康状态，不要求整理。
- 旧的 accept/correct/candidates/export/doctor 不作为日常入口；最多放在 `/memory all` 的只读视图。

### 10. 数据模型：保留可纠正性，去掉档案学

```text
memories(
  id, kind, text, state, paths, evidence,
  fingerprint, supersedes, created_at, updated_at, last_used_at, expires_at
)
```

- `state`：`active` / `provisional` / `removed`（`stale` 是按 worktree 记录的证据结论，
  不再是全局记忆状态）。
- 不保存完整版本历史；只保留上一版用于 undo，purge 时一并删除。
- `evidence` 是有界引用和短摘录，不是任意可读路径能力。
- `fingerprint` 用于去重和防止已忘内容复活。
- 索引：FTS5 + 路径/符号精确匹配；第一版不做向量。
- 每项目活跃上限 200 条；超出时归档最久未用、非用户确认的条目。
- 合并：相同 kind、相同路径、文本高度相似时合并证据和时间戳。

### 11. 最小宿主改动：两个能力，不做 V2 全家桶

只需要两个窄接口，不引入完整协议框架：

1. **`provide_context`**（旧称 `request_context`）：任务开始时，宿主带预算和最小上下文调用插件，插件返回低信任块；
   宿主负责预算、格式、超时、失败跳过、缓存测量。
2. **`task_finished`**（旧称 `task_summary`）：WAL 任务结束后，宿主非阻塞发送有界摘要，插件幂等保存 `last_activity`
   和可选的审查运行记录；失败不影响任务。

实现方式：

- 在现有插件清单增加 `api_version = 2` 和 `capabilities = ["provide_context", "task_finished"]`（兼容旧名称）；
- 宿主只对声明了能力的 V2 插件调用；V1 插件完全不变；
- 不做握手、不做字段级协议框架、不做来源分页、不做持久 ledger；
- 两个调用都有硬超时，失败只记本地诊断，不向用户报错；
- 注入由宿主在最终投影层完成，不伪装成用户或工具消息。

这比旧 P1a/P1b 的“API V2 + 身份 + 来源 + ledger”小一个数量级，也足够支撑零维护体验。

### 12. 生命周期：自动维护，不建后台服务

在插件启动、任务结束、下一次召回前做廉价维护，毫秒级完成：

- 过期：`provisional` 7–30 天；用户规则不按时间删除，只被新说法替代。
- 失效：审查发现记录的文件变化或消失时，仅在该 worktree 退出召回；其他 worktree 保留自己的结论。
- 合并：重复内容合并，保留最早来源和最近使用时间。
- 限额：活跃 200 条、单条 4KB、单项目数据库有界；超出归档，不弹窗。
- 删除：forget 留指纹；purge 删除正文、证据和上一版，留最小 tombstone。
  误删可用 `/memory restore ID` 显式恢复当前措辞；purge 不可恢复。
- 没有后台 daemon、没有定时任务、没有用户清理入口。

### 13. 信任、隐私与安全

- 记忆是**数据**，不是指令、不是权限、不是已执行的事实。
- 记忆不能授权命令、不能突破权限、不能覆盖当前用户请求和仓库规则。
- 只存用户原话和工具证据；默认不存原始工具输出、网页正文、MCP 返回。
- 秘密扫描是准入的一部分，但只是防线，不是数学保证；高敏感内容仍靠本地存储和用户判断。
- 本地数据库 0700/0600；Windows 当前用户 DACL；不写仓库、不上传、不遥测。
- 召回内容会进入模型请求，因此会发给当前 provider；安装时说明一次，不每次打扰。
- 忘掉/清除不修改历史对话、摘要、provider 日志或已发出的请求；界面必须说明这一边界。
- 同用户恶意进程不受沙箱保护；插件不是安全边界。

### 14. 分发：开箱即用的另一半

功能默认再好，需要 `cargo install` 就不算开箱即用。

- 发布预编译二进制：Windows x64/arm64、macOS x64/arm64、Linux x64/arm64；
  校验和 + 签名；SQLite 静态编译，无运行时依赖。
- 安装入口做成一件事：`kroma plugins install project-memory` 自动下载匹配产物并启用；
  如果 Kroma 安全策略要求显式启用，这是唯一不可省的一步。开发者仍可从源码构建。
- 若 Kroma 插件管理暂时不支持下载，则随 Kroma 发行包附带二进制，避免用户装工具链。
- 版本不兼容时明确拒绝并保持主任务可用，不静默降级。
- 项目级安装不需要审批第三方代码；二进制来源和校验和应可检查。

### 15. 一个典型的一天

- 09:00 安装并启用，没有任何配置。
- 09:05 用户说“以后用 pnpm，不要 npm”。模型调用 remember，插件校验原话后保存；
  界面最多一行提示，用户不审批。
- 10:00 新会话让模型审查改动。宿主自动注入 3 条相关记忆，用户不需要提醒它去查。
- 11:00 用户发现一条记错：“不是 pnpm，是 bun”。模型调用 remember + supersedes；
  旧条目失效，之后不会再出现。
- 一周后：用户没有打开过 `/memory`，没有清理过任何东西；过期条目已自动归档。

### 16. 评测：先证明不用维护，再证明省 token

本地计数，不做遥测：

| 维度 | 指标 |
| --- | --- |
| 维护负担 | 用户命令/周、纠正次数/周、安装耗时 |
| 写入质量 | 自动接受准确率、误记率、冲突率、过期率 |
| 召回质量 | 注入任务占比、相关率、模型引用/采纳率、空注入率 |
| 成本 | 注入 tokens、检索延迟 p95、缓存前缀变化、额外模型调用 |
| 结果 | 重复纠正次数、任务成功率、审查准备时间 |

验收门槛建议：

- 两周 dogfood：用户主动操作 ≤ 2 次/周，错误记忆 ≤ 1 次/50 任务。
- 100+ 真实任务 A/B（有记忆/无记忆）：任务成功率不下降，重复纠正和准备时间有改善。
- 上下文开销 ≤ 1%；若缓存损失超过收益，再考虑持久 pack ledger。
- 自动学习/向量检索只有在漏召回样本足够多时才引入。

没有达到门槛，就不宣传“省 token”，也不默认开启更激进的自动写入。

### 17. 路线图与“不做什么”

**R0：把 P0 变成零维护（已实现）**

- 去掉审批队列：quote 锚定的自动接受；
- `/memory` 缩成状态 + forget + off；自然语言为主；
- 预编译二进制和一步安装；
- 保留现有 SQLite/隔离/事务/隐私底座；
- 不碰核心协议。

**R1：最小自动召回（已实现）**

- `provide_context` + `task_finished` 两个能力；
- 硬预算、失败跳过、低信任注入、任务级一次注入；
- 用真实使用测量相关率和 token 开销。

**R2：自动维护与审查台账**

- 过期/合并/限额自动执行；
- 审查运行记录：时间、提交、变更文件、未解决问题；
- 冲突隔离和一次性提示。

**R3：只有数据支持才做**

- 本地多语言 embedding / 重排；
- 跨机器/团队共享；
- 更激进的自动提取。

**明确不做：**

- 完整 API V2 框架、持久 projection ledger、source 分页；
- 审批队列、候选列表、标签体系；
- 自动 LLM 总结每个任务；
- 向量库作为默认依赖；
- 云端同步和遥测。

### 18. 主要风险与对策

| 风险 | 对策 |
| --- | --- |
| 模型不调用 remember，记不住 | 静态 prompt 明确“用户说持久规则时调用”；用真实使用测量漏记率，必要时在任务结束做一次轻量提醒 |
| 自动接受错误规则 | 原话锚定 + 只允许 preference/decision + 一条/轮 + 一步撤销 |
| 记忆污染上下文 | 硬预算、相关度门槛、空结果不注入、低信任标注 |
| 过期规则误导 | 文件/环境指纹、默认过期、stale 不注入 |
| 秘密进入记忆 | 准入扫描、只存用户原话、不存原始工具输出、本地权限 |
| 缓存损失抵消收益 | 任务级一次注入；先测量再决定是否做 ledger |
| 分发失败 | 预编译 + 校验和 + 明确错误；不从源码偷偷构建 |
| 跨项目泄漏 | `.git` 项目 ID + 每项目独立数据库；查询只绑定当前项目 |

### 19. 参考

- Claude Code, How Claude remembers your project：<https://code.claude.com/docs/en/memory>
- OpenAI, Memories：<https://learn.chatgpt.com/docs/customization/memories?surface=app>
- Cursor, Rules：<https://cursor.com/docs/rules>
- claude-mem, Smart Memory Search（渐进披露）：<https://raw.githubusercontent.com/thedotmack/claude-mem/main/docs/public/usage/search-tools.mdx>
- LongMemEval（更新、时间推理、拒答）：<https://github.com/xiaowu0162/LongMemEval>
- SQLite FTS5：<https://sqlite.org/fts5.html>
