# Harness v1.0 实施 Gate 与评测计划

## Gate 0 — v0.8.1 安全封板

### 必须交付
- 每项已知漏洞：复现用例、修复提交、回归测试、残余风险说明。
- 干净环境构建日志，`Cargo.toml`、CLI `--version`、Git tag 与 Release Notes 一致。
- 安全测试覆盖：WAL resume、符号链接/路径竞态、凭据文件、终端转义、URL/header、Shell 超时/进程清理、ToolCall 协议、会话文件损坏。
- 不写“零漏洞”；定义支持 OS 与威胁模型。

### Gate 退出条件
所有已知阻断问题关闭，测试可重复运行；任何未覆盖风险明确登记。

---

## Gate 1 — Workspace Revision / Undo

### 设计选项评估
A. Harness shadow Git repository / Git Worktree。  
B. Manifest + content-addressed object store。  
C. Git 作为冷存储、manifest/dedup 为热路径。

先实现 A 作为 baseline，再基于文件数、工作区字节和变更比例测量。只有 A 不满足延迟/存储目标再尝试 B/C。

### 最小 API
```text
snapshot(workspace, policy) -> WorkspaceRevision
apply_mutation(parent_revision, expected_hashes, mutation) -> MutationResult
restore(target_revision, conflict_policy) -> RestoreResult
fork(target_revision) -> WorkspaceHandle
compare(revision_a, revision_b) -> Diff
```

`restore` 需要返回 Applied / Conflict / RecoveryRequired，而不是 bool。

### 必测故障注入
- Intent 写入后崩溃；
- 文件 mutation 中途崩溃；
- after snapshot 已写但 commit event 未写；
- 磁盘空间不足；
- 工作区外部修改文件；
- 新建目标竞态被其他进程抢先创建；
- symlink/特殊文件/大文件/ignored 文件策略；
- undo 再 undo、fork 后修改与恢复。

**不变量：** 恢复不得静默覆盖未纳入目标 Revision 的外部改动；日志不删除历史事件；未知外部副作用不声称可回滚。

---

## Gate 2 — Context Projection A/B

### Profile
- Baseline: 当前上下文算法。
- P1: 确定性预算 + 保留当前任务/活动文件/最近验证 + 旧输出占位符。
- P2: P1 + 可恢复的证据引用图。
- P3: P2 + 可选 ACE 风格 playbook 更新（实验功能，默认关闭）。

### 必需数据
- 本轮和累计 input/output tokens；
- system prefix、工具 schema、history、tool output 的字节/估算 token 分项；
- Provider 实际 usage/cached tokens（可得时）；
- 任务通过率、失败分类、重新取回证据成功率、延迟和成本。

### 任务集
至少含：长文件导航、跨文件 bug fix、重复错误恢复、上下文超预算、跨多轮执行任务。任务集需留出私有题，不与调参样本重叠。

### Gate 退出条件
上下文硬约束和工具消息配对测试为 100% 通过；能力和资源指标达到预先约定的阈值。未证明收益的 Context Graph 排序只保留为可选策略。

---

## Gate 3 — Plugins / Hooks / Skills

### API 设计要求
- `Plugin::mount(ctx) -> PluginHandle`；Handle 记录服务、工具、事件订阅、Hook、子任务、cleanup。
- mount 失败逆序 cleanup。
- unmount 停止新调用，取消/等待 in-flight 操作，执行 cleanup，报告不能撤销的外部副作用。
- `PolicyHook` 与 `ObserverHook` 分离，前者 fail-closed，后者可 fail-open；对每一类定义 timeout。
- Plugin manifest 带 API version、capabilities、trust level、source、checksum/signature（按分发模型）。
- 项目 Hook/Skills 不能由于加载就获得更多 OS 权限。

### Gate 测试
- 注册/卸载 1000 次不累积监听器/句柄；
- 插件 init 在任一阶段报错均恢复到未挂载状态；
- 在途工具执行时卸载；
- 恶意插件尝试覆盖 Deny、改预算、伪造结果；
- 项目中的新 Hook source 发生变化时重新要求信任。

---

## Gate 4 — Predictive Validation

### 从低风险开始
- 文件修改后记录 diff/hash。
- 只在确定语言/项目工具存在时跑轻量 parser/lint。
- baseline tests 与 after tests 必须使用相同命令、版本和环境。
- 失败归因先标记 candidate，不直接判定 root cause。
- 需要时用 revert/reapply 或缩小 patch 进行验证。

### 实验指标
- 是否在首次修复前抓住新增语法错误；
- Attribution Precision / Recall（对有已知注入点的合成任务）；
- 误报造成的错误回滚率；
- 额外执行时间和成本。

自动回滚必须能关闭，并且永远不能绕过用户对高风险副作用的审批。

---

## Gate 5 — Subagents / Worktree

### 角色
- Scout: 只读文件工具、只读 shell 或无 shell；报告带 artifact/source IDs。
- Worker: 独立 workspace/worktree、独立 Run/预算/日志。
- Speculative: 可选，最多并发数默认 2，须显式成本预算。

### 并发一致性
- Edit 请求带 expected file revision/hash。
- 文件变更通知只作为提示；工具写入必须校验 revision，冲突就拒绝并返回 STALE_FILE。
- 子任务收尾必须释放进程、句柄、tmp、worktree 和事件订阅。
- 禁止 speculative 分支对生产网络/数据库/部署执行不可逆动作。

### 选择候选
同一基线、相同测试和资源约束：通过任务 Gate → diff 检查 → 测试/安全检查 → 合并后再测试；不是“先过即赢”。

### Gate 评测
- 1/2/4 并发的成功率、墙钟时间、token、RSS/PSS；
- conflict rate；
- worker crash/timeout cleanup；
- 合并后测试成功率；
- 与单 Agent baseline 的成功率/成本对比。

---

## Gate 6 — Multi-surface Runtime

1. Headless CLI + versioned JSONL event stream。
2. ACP v1 adapter，明确声明支持的 session/cancel/update/permission 等 capability。
3. TUI 只作为 Runtime API 消费者，不能拥有私有 Session State。
4. Local daemon/SDK：默认 Unix socket，socket owner/mode、peer auth 或 token、session authorization、resource limits；HTTP 需额外 auth/CORS/CSRF 与 origin 防护。

### 兼容性
对 ACP、MCP、各模型 tool schema 与外部 Hook，使用规范化内部模型 + 明确 adapter；创建协议 compatibility tests，不用“兼容所有产品”作为未验证承诺。

---

## Benchmark protocol（必须版本化）

### 环境冻结
- Git commit、Rust toolchain、锁文件、OS/kernel、CPU/RAM、依赖缓存；
- provider/model version、采样参数、system prompt hash、tool schema hash；
- workspace image/repo commit、网络条件、预算、缓存状态。

### 比较规则
- 同一批任务做配对比较；随机化先后顺序；多次重跑；公开置信区间与 discordant tasks。
- 对小效应不要用几十题做强结论；最新 2026 Harness 比较预印本报告了在其试验中小样本难以辨别几个百分点差异，这提示测量要有功效分析。
- 公开 benchmark（SWE-bench Pro 等）和内部保留任务同时使用。公共 benchmark 不能成为唯一优化目标。

### 指标分组
1. Correctness: resolve rate/pass@1、回归失败率、长任务完成率。
2. Resource: $/successful task、input/output/cached tokens、calls、latency、peak RSS/PSS。
3. Reliability: resume/undo consistency、crash recovery、tool ID pairing、process cleanup、conflict detection。
4. Maintainability: multi-round regression、duplicate code、complexity concentration、change locality。
5. Security: denied operation bypass count、sensitive-file exposure、workspace escape、untrusted hook execution。

每次新功能提交都要附一份机器可读 evaluation record：配置、版本、原始结果、统计方法与结论。
