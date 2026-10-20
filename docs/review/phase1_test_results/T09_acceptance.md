# T09 · 贯通工具执行环境与沙箱政策 — 验收

阶段 G2｜依赖 T01, T02, T03, T04, T06｜关联 KR-18, KR-33

## 一、缺陷

沙箱只接在评分路径上：`SandboxSpec` 的唯一使用点是 `bench.rs`（验证器/评分）。**agent 的 `bash`
工具从来没有经过沙箱**——`tools/bash.rs` 直接 `ProcessLaunch::new(shell_bin, shell_args)`。
于是「开了沙箱」只意味着「评分被隔离」，模型自己敲的命令仍在宿主上跑。任务卡里那条
`sandbox_required_covers_agent_bash_not_only_scorer` 说的就是这个。

## 二、改了什么

### 1. 显式的执行策略（`sandbox.rs`）

`ExecutionClass`（agent_bash / verifier / hook / mcp_server / probe）+ `ClassRule{sandboxed, network}`
+ `ExecutionMode{Host, Isolated(template)}` + `ExecutionPolicy`。

类表就是合同，没有 per-call 覆盖，所以模型参数无法把某一类挪进或挪出沙箱：

| 类 | isolated 模式 | 网络 |
| --- | --- | --- |
| agent_bash | 沙箱内 | 禁止 |
| verifier | 沙箱内 | 禁止 |
| hook | 沙箱内 | 禁止 |
| probe | 沙箱内 | 禁止 |
| mcp_server | 宿主 | 允许 |

`mcp_server` 明确不沙箱：它是运维安装的基础设施，通常要网络和自己的工具链，假装沙箱只会把它弄坏。
host 模式是**显式选择**，不是回退，而且照样把 backend 记成 `host`，让差异可见而不是被暗示。

### 2. fail-closed 的启动服务

`ExecutionPolicy::launch_argv(class, workspace, program, args) -> Result<PreparedLaunch, SandboxError>`：

- 该类要求沙箱时，先 `spec.probe()`（探测后端是否真能用），再 `wrap_argv` 翻译 argv。
  探测放在翻译之前，因为 `wrap_argv` 是纯函数——bwrap 没装它照样能拼出一份 argv，
  那正是必须堵死的静默降级路径。
- 探测失败 → 返回 `SandboxError`，调用方**不得**执行。代码里没有任何分支会为「要求隔离的类」
  产出宿主 argv。
- `PreparedLaunch.backend` 记录实际执行者（`host` / `bwrap` / `container:<runtime>`）。

### 3. 接进真实 spawn

- `execute_bash` / `execute_bash_with_deadline` 增加 `policy: &ExecutionPolicy`；spawn 前先过
  `launch_argv`，失败返回 `ErrorCode::SandboxUnavailable`（新增的错误码），
  `ToolResultMeta.execution_backend` 记 `none:refused`。
- 沙箱内不再重复套 `shell_invocation`（shell 调用已经在 bwrap argv 的尾部），
  宿主模式保持原样（Windows 需要 `cmd.exe /D /S /C "<raw>"`）。
- 进程的 cwd 仍是**宿主**工作区路径；沙箱内部的位置由 bwrap 自己的 `--chdir <guest>` 决定——
  这两者混用会让 spawn 直接 `ENOENT`（实现时踩到过，已修正）。
- `ToolResultMeta` 新增 `execution_backend: Option<String>`（serde default + skip，旧 WAL 兼容）。
- `ToolRuntime` 新增 `execution` 字段 + `with_execution_policy()`，`bash` 分支把它传下去。

### 4. 配置接线（不做就等于没做）

- 新配置键 `execution_sandbox`：`host` | `bwrap` | `container:<runtime>:<image>`，
  非法值**在配置加载期就失败**，不会悄悄落回宿主。
- 该键进 `SECURITY_SENSITIVE_KEYS`：工作区 `.kroma.toml` 写了也会被拒绝并记入
  `rejected_workspace_keys`——能关沙箱的仓库等于没有沙箱。
- 环境变量 `KROMA_EXECUTION_SANDBOX`。
- `kroma-cli` 在构造 runtime 处调用 `execution_policy_from_config()` 并
  `.with_execution_policy(...)`，这是**唯一**把字符串变成策略的地方，所以各入口不会各说各话。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t09_execution_boundary` | 8 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 见 `logs/test_t09.log` |

### 新增测试（8 条）

- `execution_class_matrix_is_explicit`：类表逐项断言（四类沙箱+禁网，MCP 宿主+联网；host 模式全宿主）。
- `sandbox_required_covers_agent_bash_not_only_scorer`：isolated 下 `bash` 写工作区内成功且
  backend=`bwrap`；`../` 逃逸与**绝对路径**写父目录都不会在宿主上留下文件。
- `isolated_bash_has_no_network`：`/dev/tcp` 连不上；同一命令在 host 策略下成功（对照组证明
  不是命令本身坏了，而是边界真的生效）。
- `no_backend_means_refuse_required_execution`：不存在的容器 runtime → `SandboxUnavailable`、
  backend=`none:refused`、**副作用文件不存在**（没在任何地方执行）。
- `host_policy_reports_the_host_backend`：宿主跑就是 `host`，不能冒充隔离。
- `worker_cannot_write_parent_via_absolute_path`：绝对路径写父工作区被拒，父文件字节不变。
- `workspace_cannot_declare_the_execution_boundary`：工作区设 `execution_sandbox = "host"`
  被拒并记录；运维层设 `bwrap` 生效。
- `invalid_execution_boundary_fails_at_start`：`docker` / `container:` / `container:docker` / 空串
  全部拒绝；三种合法值映射到正确的 `ExecutionMode`。

### 变异验证

把 `launch_argv` 里的 `if !self.requires_sandbox(class)` 改成 `if true`（即引入静默宿主回退），
4 条测试立刻红：`sandbox_required_covers_agent_bash_not_only_scorer`、
`no_backend_means_refuse_required_execution`、`isolated_bash_has_no_network`、
`worker_cannot_write_parent_via_absolute_path`。恢复后 8/8 绿。

## 四、边界与未完成

- **hook / verifier / probe / MCP 的 spawn 还没有逐个走 `launch_argv`**：策略里已给出规则，
  `bash` 已真正接线，`bench.rs` 原有的 sandbox 参数保持不变。剩下的接线属于 T11
  （Runner 装配与机制接线验证），本项不宣称它们已生效——任务卡明确禁止「未接线却宣称生效」。
- `feature_matrix_covers_cli_rpc_bench_worker_resume`、`same_run_plan_has_same_policy_across_all_surfaces`、
  `refactor_preserves_golden_protocol_traces` 这三条需要多入口装配，留给 T11。
- 容器后端在本机可用 `docker`，但本项只验证了「后端不可用时拒绝」这条路径；容器真实隔离
  的负面测试需要拉镜像，记在 T11/T24 的执行环境里。
- `execute_bash` 参数达到 8 个，加了 `#[allow(clippy::too_many_arguments)]` 并写明理由
  （安全决策不该塞进资源限制结构里）。
