# Harness P1 实现报告（Session WAL + CompletionGate）

## 状态

本次基于上传的 `harness-v0.6.3-完整源码.zip` 修改，保留原 workspace / crate 布局，没有新增第三方依赖。补丁压缩包名为 `harness-v0.6.3-p1.zip`。

**验证状态：尚未完成 Rust 编译与测试。** 当前执行环境没有 `cargo`、`rustc` 或 `rustfmt`，因此这里不能诚实地报告 `cargo test` 通过。已核对 TOML 结构与关键修改位置，但静态核对不能替代编译和测试。

## 已实现

### 1. Append-only Session WAL 与分支

- 新增 `harness_core::session::SessionJournal`，以 JSONL 追加式 WAL 记录结构化 `TranscriptItem`、父节点 / entry ID、turn、活动叶子选择及 checkpoint。
- 每条记录带 `seq`、前一记录 SHA-256 和当前记录哈希；每条写入执行 `flush + sync_data`。损坏的完整行会拒绝恢复；只修复文件末尾未写完整的一行。
- WAL 文件权限设为 `0600`。它保存的是完整结构化历史，包括 `ReasoningBlock` 中现有的 `signature` 和 `opaque_payload` 字段，不主动解析或丢弃这些字段。
- 新增带 `projected_at_leaf_id` 的 `ProjectionUpdate` 追加记录，保存上下文压缩后的投影状态。旧 WAL 行不被改写；overlay 只作用于包含投影叶子的分支。分叉回到更早节点时，不会把后续分支压缩出来的 outline stub 错带回去。
- checkpoint 持久化 `pinned_baseline_end`、`last_touched_turn` 文件账本和 reasoning-block 摘要；恢复时恢复 pinned cursor 与热文件账本。
- WAL 持久化终态验收记录：通过会标记 `verified`，无可识别测试命令时会显式记录 `skipped` 与原因，不把跳过伪装成测试通过。恢复时，只把最近一次终态记录之后的文件修改视为待处理项；进程中断前尚未完成验收的改动仍会触发验收，而纯问答不会因已处理过的历史改动重复跑测试。
- 进程在工具批次中断后恢复时，为没有被确认完成的剩余工具调用追加 `NOT_EXECUTED_CANCELLED` 结果，不会猜测工具已经执行成功。
- CLI 增加 `--resume <WAL>` 和 `--fork <TURN>`；分叉仅追加活动叶子切换记录，不删除另一条历史分支。

### 2. Steer 通道

- 保留库层既有的 `steer_rx` 注入方式，并将排队中的用户插话在完整工具批次结束后写入 WAL，作为下一次模型请求的高优先级 steering 指令。
- 该补丁**没有**新建交互式 TUI 或后台键盘读取线程。当前 CLI 仍是单任务运行器；程序集成方可以通过 `with_steer_receiver(...)` 注入通道。想直接在终端随时敲字，需要另做 CLI 输入任务 / 生命周期与取消逻辑。

### 3. CompletionGate

- 新增 `WorkspaceCompletionGate`。存在 `Cargo.toml` 时默认执行 `cargo test --workspace --no-fail-fast`；另外识别 Go、pytest、npm 项目。可通过 `HARNESS_VERIFY_COMMAND` 提供 argv 风格的覆盖命令，超时通过 `HARNESS_VERIFY_TIMEOUT_SECS` 配置，默认 180 秒。
- 没有待处理的文件修改时短路，不启动测试子进程。未知项目类型返回 `Skipped`，而不是伪装成测试通过；该跳过结果会连同原因写入 WAL，非 quiet CLI 也会打印原因。需要强制验收时应设置 `HARNESS_VERIFY_COMMAND`。
- 子进程 stdout / stderr 通过现有 `StreamLineCollector` 与 `HeadTailBuffer` 流式读取，不一次性把任意大小的测试输出读入内存；反馈使用 16 KiB 头部 + 48 KiB 滚动尾部，并提取错误、失败、断言和 traceback 等诊断行。
- 修正了输出缓冲器在未配置 spill 文件时仍声称“完整日志已保存”的提示。
- 失败特征哈希优先选择诊断行，减少编译进度噪声造成的哈希变化。默认最多 3 次 CompletionGate 拒绝；连续相同指纹时注入“先 read 检查上下文、不要原样重复上一次 edit”的退火提示；达到限制后停机，最终错误包含指纹和受限长度的失败摘录。
- Gate 拒绝提示追加进同一 WAL，`--resume` 会恢复未解决会话的拒绝次数与最近失败指纹，进程重启不会自动清零熔断预算。达到限制后需从失败之前的分支 `--fork`，或新建会话，避免无意中重置熔断。

## 用法

首次运行会在 stderr 打印 WAL 路径：

```bash
harness run --workspace /path/to/repo --task "实现功能 X"
```

用新的任务指令在同一会话上继续：

```bash
harness run --workspace /path/to/repo --resume /path/to/session.wal.jsonl --task "继续上次工作，先检查剩余问题"
```

从第 10 轮附近分叉：

```bash
harness run --workspace /path/to/repo --resume /path/to/session.wal.jsonl --fork 10 --task "尝试另一种实现路径"
```

指定项目验收命令：

```bash
HARNESS_VERIFY_COMMAND="cargo test --workspace" HARNESS_VERIFY_TIMEOUT_SECS=180 HARNESS_GATE_MAX_RETRIES=3 \
  harness run --workspace /path/to/repo --task "修复问题"
```

`HARNESS_VERIFY_COMMAND` 是拆分成 argv 的命令，不会交给 shell 求值；不要把它当作支持 shell 管道、重定向或复杂引号的脚本入口。

## 已知边界与安全说明

1. **没有保证 99% 缓存命中率。** WAL 保存稳定的结构化会话与投影状态，不是保存并重放供应商 HTTP 请求的原始字节流。服务商是否命中缓存仍取决于请求序列化、缓存边界、模型 / API 实现和服务端策略，应根据 `cached_input_tokens` 实测。
2. **WAL 不是加密容器。** `ReasoningBlock` 的字段被序列化保存，文件权限是 `0600`，但没有进行静态加密；SHA-256 链用于发现意外损坏，不防止有权限的攻击者重写整个文件并重算哈希。请把 state directory 放在受保护或加密的磁盘上，不要把 WAL 上传到不可信位置。
3. **验收命令会执行项目代码。** Cargo build scripts、测试、npm scripts 等可在当前 OS 用户权限下运行；虽然 gate 复用了 Bash 的敏感环境变量过滤，并在超时后杀掉专属进程组，但它不是容器沙箱。只对可信仓库运行，需额外隔离时请在外层容器 / VM 中运行 Harness。
4. **单写者假设。** 同一个 WAL 不要被两个 Harness 进程同时 `--resume`；此版本未添加跨进程文件锁。
5. **项目自动识别有限。** 未命中 Cargo / Go / pytest / npm 规则时会跳过验收，必须自行配置 `HARNESS_VERIFY_COMMAND`；默认 Cargo 命令也可能不适合需要特殊服务或环境变量的项目。
6. **Steer 只有库层接收口。** 还没有终端实时输入 UI，插话在整批工具调用完成后送入下一轮，而非中断正在运行的工具。
7. **测试尚未运行。** 在具备 Rust 工具链的环境解压后执行：

   ```bash
   cargo fmt --all -- --check
   cargo test --workspace
   ```

   应特别验证 WAL 崩溃尾行恢复、同一 WAL 多分支往返、分支隔离的压缩投影、验收后脏文件基线重置、熔断次数跨 `--resume` 恢复、CompletionGate 的失败 / 超时 / dirty-file 短路，以及现有 `context_cache_stability` / `shell_supervisor` 集成测试。

## 论文引用校正

- [TokenPilot — Cache-Efficient Context Management for LLM Agents, arXiv:2606.17016](https://arxiv.org/abs/2606.17016)：讨论稳定 prompt prefix 与保守管理上下文片段；并不表示 WAL 本身可以保证某个固定缓存命中率。
- [SWE-Search — Enhancing Software Agents with Monte Carlo Tree Search and Iterative Refinement, arXiv:2410.20285](https://arxiv.org/abs/2410.20285)：是 MCTS 与迭代自我改进的软件 Agent 研究；本补丁采用的是会话树分叉机制，不等价于完整实现其搜索算法。
- [arXiv:2601.06007](https://arxiv.org/abs/2601.06007) 实际题为 *Don’t Break the Cache: An Evaluation of Prompt Caching for Long-Horizon Agentic Tasks*，研究多家供应商缓存策略，不是 *The Harness Effect*。
- [The Harness Effect, arXiv:2607.06906](https://arxiv.org/abs/2607.06906) 才是 *The Harness Effect: How Orchestration Design Sets the Token Economics of Enterprise Agentic AI*。
