# T10 · 修复探测与容器取消的生命周期 — 验收

阶段 G2｜依赖 T09｜关联 KR-16, KR-17

## 一、缺陷

三处，各自都能让「取消」变成一句空话：

1. **探测没有超时**。`probe()` 用 `Command::status()`——`docker --version` 或 `docker image inspect`
   在守护进程卡住时会**永久阻塞**，整轮 run 挂死在那里。
2. **容器没有身份**。容器路径是 `docker run --rm ...` 前台进程：没有名字、没有 cidfile、没有
   create/start/stop/remove 任何一步可观测。杀掉客户端（超时/取消走的是进程组 SIGTERM→SIGKILL）
   之后容器仍在守护进程里跑——被取消的是客户端，不是工作。
3. **清理不可证伪**。`--rm` 让「删没删掉」完全取决于客户端是否正常退出，没有任何地方能回答
   「容器还在吗」。

## 二、改了什么

### 1. 有界控制命令（`run_bounded`）

`run_bounded(program, args, budget)`：spawn → 轮询 `try_wait` → 超预算就 `kill` + `wait`，
返回 `Err("did not finish within Nms and was killed")`。**不用 `status()`**，因为那正是会永久阻塞的调用。

### 2. 可注入 transport 的探测（`ProbeTransport` / `probe_within`）

- `ProcessProbeTransport`：真实实现，走 `run_bounded`。
- `probe_within(transport, budget)` 分三步问三个不同的问题，全部受同一个预算约束：
  1. 二进制在不在、答不答 `--version`；
  2. 镜像/命名空间能不能用——**真的跑一次空操作**（bwrap 用 `--ro-bind / / --unshare-net /bin/true`，
     容器用 `run --rm --network none <image> /bin/true`），不是只看 `inspect` 成功；
  3. 会不会挂——挂住按「不可用」处理，绝不按「假设可用」处理。
- **成功才缓存**（按 backend+image 键），失败不缓存：一次守护进程抖动不能永久禁用整轮 run，
  但每次 spawn 也不能重新跑一次冒烟（否则每次 bash 多花 1 秒）。
- 新增 `SandboxError::NamespaceUnavailable`：「装了但隔离不了」和「没装」是两回事，
  运维动作不同，两者都是 fail-closed。

### 3. 容器有身份、生命周期可观测（`ContainerWorkload`）

- `name` = `kroma-<pid>-<12 位哈希>`，`cidfile` 写在**可信状态目录**（`state_dir`），
  绝不写进模型可写的工作区。
- 生命周期显式：`create` → `start` → `cancel`（stop → rm -f → **inspect 验证**）。
  `create_argv` 与 `wrap_container_named` 共用同一套隔离参数（`--init`、`--network none`、
  `--read-only`、`--tmpfs`、资源上限、唯一可写挂载、`ro_extra` 只读挂载），
  区别只在于「跑得起来」还是「能被寻址」。
- `cancel` 返回 `CleanupOutcome`：
  - `NothingToClean`（从未创建）
  - `Cleaned { container_id }`（inspect 已找不到）
  - `RecoveryRequired { container_id, reason, retry_command }`——**没有 `cleaned=true` 可以乐观设置**，
    失败时保留 id 并给出可执行的重试命令（`docker stop --time 1 <name> && docker rm -f <name>`）。
- `adopt()`：按名字接管上一个进程留下的容器（崩溃恢复 / 残留资源清理）。
- `bash` 在**所有**退出路径（正常、超时、取消、输出洪泛）之后调用 `cancel()`；
  清理失败时把 `RecoveryRequired` 的说明追加进工具输出，让模型和运维都看得见。

### 4. 只读挂载

`ro_extra` 在容器后端映射成 `--mount type=bind,source=...,target=...,readonly`，
所以评分用的 oracle 目录容器改不动。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t10_container_lifecycle` | **9 passed / 0 failed**（2.9 秒，含 3 条真实 docker 用例） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 见 `logs/test_t10.log` |

真实容器用例使用本机 Docker 26.1.5 + 本地已有镜像 `alpine:3.20`。

### 测试清单（9 条）

- `cancel_leaves_no_live_container_or_grandchild`（**真容器**）：容器内 `while true; do date >> heartbeat; sleep 0.2; done`，
  等到心跳文件出现 → `cancel` → 断言 `Cleaned` → 1.5 秒后心跳文件长度**不再增长** → `inspect` 找不到容器。
- `kill_cli_still_cleans_daemon_workload`（**真容器**）：起一个 `docker run ... sleep 300` 客户端，
  `SIGKILL` 客户端 → 断言**容器仍在**（这就是原来的缺陷）→ 按名字 `cancel` → 容器消失。
- `container_cannot_modify_oracle_mount`（**真容器**）：只读挂载的 oracle 目录写不进去，宿主文件字节不变。
- `cleanup_failure_retains_container_id`（注入 transport）：`rm` 失败 → `RecoveryRequired` 且保留 id、
  给出重试命令；`rm` 成功但 inspect 仍能找到 → 同样 `RecoveryRequired`；真正删掉时 → `Cleaned`
  （第三条证明前两条不是空转）。
- `sandbox_probe_hang_is_cancelled_within_budget`：探测在预算内失败，不阻塞。
- `run_bounded_kills_a_hung_child`：真实 `sleep 60` 在 300ms 预算下被杀，5 秒内返回。
- `version_success_but_namespace_failure_is_unavailable`：`--version` 与 `inspect` 都成功、
  冒烟运行失败 → `NamespaceUnavailable`。
- `probe_never_falls_back_to_host_execution`：探测失败 → 启动被拒，错误文案明确写着
  「refusing to run unsandboxed」，不会退化成宿主 argv。
- `workload_names_and_cidfiles_are_unique`：32 次创建名字与 cidfile 全不重复。

### 变异验证

把 `cancel()` 里的「rm 之后 inspect 验证」换成「rm 成功即 Cleaned」，
`cleanup_failure_retains_container_id` 立刻红：
`rm success without a confirmed removal is not cleaned: Cleaned { container_id: Some("abcdef123456") }`。
恢复后绿。

## 四、边界

- 容器路径用的是「命名 + cidfile 的 `docker run`」而不是「create/start/attach 再 `docker logs -f`」：
  后者要重做 bash 的流式输出与取消语义，收益是能分离「客户端存活」与「容器存活」——但用名字寻址
  已经解决了取消问题（`kill_cli_still_cleans_daemon_workload` 证明）。`create`/`start` 接口已实现并被
  `cancel_leaves_no_live_container_or_grandchild` 使用，`docker run` 只是同参数的 `run` 形态。
- 没有做 Podman 的能力声明（本机没有 podman）。`SandboxBackend::Container` 只按 runtime 名调用，
  Podman 兼容 docker CLI 的部分可用，但**未验证**，不声称支持。
- 容器不可用时的行为是「拒绝执行」，不是「回退宿主」；`t09` 的 `no_backend_means_refuse_required_execution`
  钉住了这一点。
- 真实 docker 用例在缺少 runtime/镜像时**打印 SKIP 原因并返回**，不会静默通过；
  但它们确实不构成「没有 runtime 时的生命周期证据」——CI 上必须带 docker 跑。
