# Kroma 本体与插件设计审查（2026-11-11）

按既定审查顺序：**功能该不该存在 → 设计是否合理 → 是否符合直觉 → 实现 → 性能 → 维护**。
仓库状态：`820b8bb`（v0.4.0），工作树干净（仅有 `reviews/` 下的未跟踪文件）。

## 0. 本轮实际做了什么

已执行的机器验证（非推测）：

| 项目 | 命令 | 结果 |
| --- | --- | --- |
| 全量测试 | `cargo test --all-targets --no-fail-fast` | **971 passed / 0 failed**（30 个测试二进制；10 个 `#[ignore]` 全是本地基准） |
| 静态检查 | `cargo clippy --all-targets` | **0 警告**，全仓仅 13 处 `#[allow]`，均为 `too_many_arguments` / `large_enum_variant` |
| 依赖面 | `Cargo.toml` | 18 个直接依赖，无框架级引入 |
| 启动耗时 | `kroma --version`（release） | **0.39 s**；`kroma plugins list` 0.03 s |
| 每个示例插件 | `kroma plugins check examples/plugins/*` | 14 个通过，**2 个失败**（见 F1、F2） |
| 代码规模 | `wc -l` | 核心 Rust 79,552 行；示例插件约 13,000 行 |

未做：逐行阅读 8 万行核心。下文每条结论都标注了它是**跑出来的**还是**读出来的**，以及具体位置。

---

## 1. 第一层：该不该存在、设计值不值这个代价

### F3（成本，读+测）每次请求的固定开销被一个插件吃掉三分之一

11-09 审计在**同一提交**上用本地模拟端点捕获了真实首请求（`reviews/KROMA_AUDIT_CAPTURE_*.json`）：

| 配置 | 工具 schema | 系统提示词 | 首请求合计 |
| --- | ---: | ---: | ---: |
| 只开内置工具 | 924 | 704 | 1,944 |
| 开 goal / project-memory / subagents / web | 4,142 | 1,303 | 5,766 |

四个插件带来的工具开销里，**`goal_checkpoint` 一个工具就占 1,268 tokens**（占插件工具开销 31%）。
本轮实测其 schema 在源码里是 **4,314 个字符**（`examples/plugins/goal/plugin.toml:30` 起），
共 15 个字段：`criteria[].source_ref`、`checks[].rationale`、`evidence[].call_id`、`assessment`、
`kind`、`check_ids`……而同一文件的提示词写着“**不要求先填验收表或登记检查计划**”（`plugin.toml:16`）。

即：规则说“别填表”，schema 却把一张 1.2k tokens 的表挂在每个请求上。表单比规则贵 8 倍。

### F4（成本，测）刚装的 `ask` 插件每请求约 310 tokens

`examples/plugins/ask/plugin.toml` 的 `prompt` 是 909 个字符（常驻系统提示词），加工具描述约 1,060 字符 ≈ **310 tokens/请求**——
比整个内置 `shell` 工具（243）还贵。它的实际触发条件（“真有岔路时才问”）意味着绝大多数请求里这 310 tokens 是纯支出。

### F5（设计，读）提示词里躺着 7 套并行文案，默认同时拼两段

`src/prompt.rs:10-90` 定义了 8 个常量、7 段独立文案：`BASELINE` / `COMPLETE_FUNCTION` /
`FINISH_ON_EVIDENCE` / `LEAN` / `FOCUS` / `FOCUS_FINISH` / `V7`。其中 5 段只能靠
`KROMA_EXPERIMENT_PROMPT_ARM=baseline|complete|lean|focus|focus_finish` 打开（`prompt.rs:42-69`）。

- `PromptComposer::default()`（`prompt.rs:93`）把 `selected_execution_guidance()` **和** `V7_GUIDANCE` 两段同时放进默认请求。
- 两段合计 **2,359 字节**（本轮实测源码行），而审计记录的默认系统提示词总量是 2,463 字节 —— **默认系统提示词 93% 是这两段散文**（约 660 tokens）。
- 文案已经分叉：`BASELINE` 结尾是 “finish with a concise result rather than running unrelated doctests…”，
  `FINISH_ON_EVIDENCE` 结尾是 “finish concisely. Do not intentionally revert a passing fix…”。同一件事，两种说法。

这不符合“最小充分实现”：一个项目只需要一段默认文案，其余属于基准实验，应留在 `benchmarks/`，不该跟着用户发版。

### F6（设计，读）6 个实验开关挂在生产路径上，其中一个把 Cargo 知识焊进核心

`src/agent.rs:1906-1916`、`src/tools.rs:493`、`src/prompt.rs:55-63` 一共 6 个 `KROMA_EXPERIMENT_*` 开关：

| 开关 | 作用 | 位置 |
| --- | --- | --- |
| `PROMPT_ARM` | 切换 6 套执行指引 | `prompt.rs:55` |
| `FINISH_ON_EVIDENCE` / `COMPLETE_FUNCTION_READ` | 额外实验臂 | `prompt.rs:58,61` |
| `READABLE_SHELL_RESULTS` | 把 shell 的 JSON 结果改写成散文再给模型 | `agent.rs:1907` |
| `ADAPTIVE_FINISH` | 测试通过后再多调几次工具就插入“该收尾了”的提示 | `agent.rs:1916` |
| `SHELL_TOOL_NAME` | 运行时改内置工具名字 | `tools.rs:493` |

`is_passing_cargo_check`（`agent.rs:101-133`）凭命令串里的 `cargo test` 和 stdout 里的 `test result: ok.`
判定“已经通过”，只为让基准里的收尾提示触发。这是基准调优知识，不是通用 harness 能力：
Go/Node/Python 项目永远拿不到同样对待，而它每个请求都在。`READABLE_SHELL_RESULTS` 更麻烦——
它改的是**模型看到的结果格式**，与 WAL 里存的 JSON 不同，一旦开过，同一份历史在两种格式间切换。

### F7（边界，读+测）核心为一个可选插件背了 1,954 行领域模型

| 核心模块 | 行数 | 为谁而存在 |
| --- | ---: | --- |
| `src/task_control.rs` | 1,954 | goal：`CheckpointBasis`、`CheckSpec`、`SourceRef`、`criteria/claim/assessment` |
| `src/tools.rs` 中的 `AskTool` | 149（`tools.rs:612-760`） | ask：`host = "ask_user"` 必须由宿主实现 |
| `src/plugins/model.rs` + `context.rs` + `stream.rs` | 1,346 | model/context 类插件 |

`task_control.rs` 骨架是通用的（`TaskController::new(owner)`，owner 是字符串），但**嵌进去的领域模型完全是 goal 的**。
不用 goal 的用户照样编译、照样携带这 1,954 行。这就是你计划改造的“每加一个新插件都要改核心”，
现有两个实例（goal、ask）可作证据：一个是整块领域模型，一个是宿主侧工具实现。

### F8（一致性，读）模型能力有两个真相来源

`src/model_catalog.rs` 有 `CatalogRecord.thinking_levels` 等能力字段，是能力的事实来源；
但 `src/provider.rs:1513-1516, 1963-1967, 2353-2355` 仍用
`self.model.to_ascii_lowercase().contains("deepseek")` 决定是否重放 reasoning、是否发 `reasoning_effort`。
catalog 里没有任何“是否重放 reasoning”的字段，两套判断并行。换供应商或改型号名（`deepseek-v4` vs `deepseek-reasoner`）就会静默改变行为。

---

## 2. 第二层：实现与健壮性

### 做得好的（本轮验证过，不虚构）

- **971 个测试全绿**，含 23 个 task_control 端到端、插件热插拔、崩溃恢复、WAL、快照往返、Windows Bash/PowerShell 引号保真。
- **clippy 零警告**；依赖 18 个，没有 DI 框架、没有事件总线抽象。
- **上下文溢出有真实兜底**：`agent.rs:2168-2182` 认出上游 `context_length_exceeded` / `prompt is too long` 后压缩一次并重试一次，不会直接失败。
- **快照作用域有守卫**：`checkpoints.rs:1089` 拒绝恢复“不属于本对话”的快照，`:1174` 拒绝覆盖非空阻塞文件。
- **启动 0.39 s**，`plugins list` 0.03 s —— “更快”这条在这层站得住。

### F1（缺陷，跑出来的）官方示例 `model-stream` 校验失败，且错误信息误导

```
$ kroma plugins check examples/plugins/model-stream
Error: plugin response request_id mismatch      (exit 1)
```

原因：`kroma plugins check` 会发一个无害的 `ping`；`examples/plugins/model-stream/main.js:80`
的兜底分支发送 `{ request_id: null, event: "error" }`，宿主在 `src/plugins.rs:3807-3809` 严格比对 id 后报“request_id 不匹配”。
其他 13 个示例在同样场景下都回 `is_error=true content=Unknown …` 并以 exit 0 通过 —— 说明契约是“**回错可以，但必须带上你的 id**”。
用户看到的是“id 不匹配”，不会想到“这个插件没处理 ping”。两处都可改，各一两行。

### F2（缺陷，跑出来的）`project-memory` 的安装路径对普通用户不友好

```
$ kroma plugins check examples/plugins/project-memory
Error: reading \\?\D:\Kroma\kroma\examples\plugins\project-memory\plugin.toml:
       系统找不到指定的文件。 (os error 2)
```

它的清单在 `examples/plugins/project-memory/plugin/plugin.toml`（README 也确实要求装 `plugin/` 子目录）。
问题在于报错是底层 OS 错误，没有任何“清单应在 `plugin/` 子目录”的提示。对一个只做决策、不读代码的用户，这一步就是死路。

### F9（口径，读）58 个 `KROMA_*` 环境变量

`grep -rh "KROMA_[A-Z0-9_]*" src/` 去重后 58 个（含测试用）。其中相当一部分是正当配置
（`KROMA_MODEL`/`KROMA_BASE_URL`/`KROMA_SHELL`/`KROMA_NO_MCP`…），但也混着实验臂、测试钩子、
内部子进程契约（`KROMA_CHILD_DEPTH`、`KROMA_JOB_OWNER_HELPER`、`KROMA_CHECKPOINT_CHILD_HELPER`）。
没有单一文档能回答“这 58 个里哪些是给我用的”。这是“可观察”理念的反面：能力都在，但看不见边界。

---

## 3. 第三层：插件投入产出

| 插件 | 规模 | 每请求成本 | 判断 |
| --- | ---: | ---: | --- |
| `project-memory` | **7,830 行 Rust**（`store.rs` 2,207 / `protocol.rs` 2,520 / `store_read.rs` 1,437 / `scope.rs` 1,295） | 只注入命中的块，有 `max_bytes` 上限、超限列 `omitted` 不静默截断（`protocol.rs:818-880`） | 设计诚实（引用原文、可追责），但**要单独 `cargo install` 才有 `kroma-memory`**，且体量已是中型项目。占全部插件代码的 60% |
| `web` | 2,109 行 / 7 个搜索后端（Exa、Brave、Tavily、You.com、Parallel、SearXNG、Mwmbl） | 工具 593（`web_search` 201 + `web_fetch` 392） | 工程质量好：手动重定向、每跳校验、字节上限、超时不重放 POST（`http.js:52-115`）。7 个后端是“用户自主”的代价，不是缺陷，但值得问一句“留 3 个够不够” |
| `goal` | 1,347 行（`main.js` 598 + 测试 515） | 1,268 + prompt 147 字符 | 成本最高、收益最不确定（F3） |
| `subagents` | 1,569 行 | 工具 498 | 默认子任务 1800 s 上限、最多 8 并发；工具描述里**没写“这会成倍花钱”**，只在 README 里 |
| `image-gen` / `memory` / `status` / `transform` / `hello` / `model-*` | 合计约 1,000 行 | 0（未启用） | 都是示例体量，合理 |

一个观察：插件代码量 13,000 行里，60% 集中在**一个**插件（project-memory）。而“核心极简、复杂能力下沉插件”的代价是
**复杂度只是搬了家，没有消失** —— 它现在住在一个需要独立工具链（Rust + 自建二进制 + PATH）的地方。

---

## 4. 安全口径（读）

- `web_fetch` **不拦截内网地址**（`examples/plugins/web/*.js` 全仓无 `169.254`/`loopback`/私网判断）。
  这是**有意**的：`web.test.js:74` 明确测试“SearXNG 可以活在私有主机上”。
  代价是：网页内容里若藏了“去读 `http://127.0.0.1:xxxx`”的注入指令，harness 会照做。Kroma 明示自己不是沙箱，所以这是被接受的取舍，不是漏洞——但应在 `docs/security.md` 里写一句。
- 快照/回滚的作用域与阻塞检查是有守卫的（见上），文档承诺与代码一致。

---

## 5. 建议的修复顺序（按“收益 ÷ 改动量”）

| 优先级 | 事项 | 预计收益 | 改动量 |
| --- | --- | --- | --- |
| P0 | `plugins check/install` 的两个报错（F1、F2）：把“id 不匹配”改成“插件未响应 ping”，把“找不到 plugin.toml”改成“清单应在子目录：`…/plugin/plugin.toml`” | 普通用户不再卡死 | 各 ~5 行 |
| P0 | `ask` 插件提示词从 909 字符压到 ~200 字符 | 每请求省 ~230 tokens | 改一个字符串 |
| P1 | `goal_checkpoint` schema 瘦身：`evidence`/`check_ids`/`source_ref`/`kind` 收进一个自由文本或删掉，保留 `summary`/`claim`/可选 `criteria` | 每请求省 800–1,000 tokens | 改 manifest + 插件校验逻辑 |
| P1 | 实验臂出核心：`prompt.rs` 只留 1 段默认文案，其余移到 `benchmarks/`；`READABLE_SHELL_RESULTS`、`SHELL_TOOL_NAME`、`ADAPTIVE_FINISH` 同上 | 少 5 段并行文案、少 4 个隐性分叉 | 中等，需跑基准确认不退化 |
| P1 | `is_passing_cargo_check` 明确标为基准专用并移出生产路径 | 去掉“只对 Rust 友好”的不对称 | 小 |
| P2 | `task_control` 的领域模型外移到 goal 插件，核心只留“每会话一块插件状态 + 宿主核验” | 核心少 1,954 行，为新插件铺路 | 大（就是你说的插件化改造） |
| P2 | 模型能力统一走 catalog，删掉 `contains("deepseek")` 三处 | 单一真相来源 | 中 |
| P2 | 写一张“58 个环境变量”的分类表进文档 | 可观察性 | 小 |

---

## 6. 不确定与未覆盖

1. **token 数是估算，不是账单**：来自 `src/token_estimate.rs` 的字类估算，且 11-09 捕获是在**同一提交**上做的；真实上游 usage 未取。
2. **未逐行读**：`src/mcp.rs`（3,048）、`src/tui/*`（约 9,000）、`src/provider/*`、`desktop/`（108 文件）、`src/skills.rs`、`src/attachments.rs` 只做了结构性抽查，未逐函数审查。
3. **Windows-only 环境**：Unix 信号/进程组、macOS 行为未实测。
4. **插件运行时**：只跑了 `plugins check` 与现有测试套件，未做真实 provider 下的多轮插件压测；web 的外部后端未做实网基准。
5. **F5/F6 的性能影响未做随机化消融**：移除实验臂后行为是否退化，必须用 `benchmarks/` 的方法验证，不能凭“代码更少”下结论。

---

## 7. 修复复核（同日，分支 `cleanup/plugin-boundaries-20261111` / `4aabc90`）

复核方式：在该分支的独立工作区实跑 `cargo test --all-targets` 与 `cargo clippy`，并用新构建的二进制实跑 `plugins check`。

| 条目 | 结果 | 证据 |
| --- | --- | --- |
| F2 清单在子目录的报错 | **已修** | 新二进制输出 “plugin manifest is under …/plugin/plugin.toml. Use that directory for …”，并新增 `plugins::tests::nested_plugin_manifest_error_explains_the_correct_install_path` |
| F1 `model-stream` 示例自检 | **已修（示例侧）** | 分支内 `plugins check` 退出码 0、`smoke=response is_error=false content=pong`；主机的 “request_id mismatch” 文案未改 |
| F3 `goal_checkpoint` 表单成本 | **已修** | 工具段 5,266 → 2,277 字节（-57%）；删去 `revision`/`source_ref`/`check_ids`/`kind`/`notes`/`rationale` 与全部字段说明；按原换算比例约 1,268 → 约 550 tokens/请求 |
| F4 `ask` 提示词成本 | **已修** | 909 → 258 字符，约 310 → 约 130 tokens/请求；已装副本与分支一致（md5 `c1a22cc2…`） |
| F5 七套并行提示词 | **已修（可维护性）** | 合并为单一 `EXECUTION_GUIDANCE`；逐字节比对与 main 的两段拼接**完全相同**（2,209 字符），故行为与成本不变，仅消除 7 份副本 |
| F6 运行时实验开关 | **已修** | `agent.rs`/`tools.rs` 中 `KROMA_EXPERIMENT_*` 归零，`is_passing_cargo_check` 删除；源码仅剩一条“文本不得含实验开关”的断言 |
| F8 模型能力双真相 | **已修（收敛）** | 统一为 `ModelClient::uses_deepseek_tool_reasoning()`，目录优先、代理名回退并注释；配 `deepseek_capability_is_consistent_for_catalog_models_and_proxies` |
| F7 核心承载 goal 领域模型 | **有意不修** | `docs/plugin-boundaries.md` 说明与 WAL/快照格式耦合，需先做版本化数据适配层；理由成立 |
| F9 环境变量口径 | **部分** | 58 → 52；未做分类文档 |
| 回归 | **无** | 967 passed / 0 failed（原 971；删除的 12 个用例逐个对应被移除的实验功能，新增 8 个覆盖新行为）；clippy 0 告警 |

**结论**：修复质量合格，没有“删测试蒙过关”，也没有改坏存档格式。两项遗留为已知设计取舍。

**部署状态（复核当天）**：修复在分支上，未并入 `main`；用户 PATH 首位原为 `~/.kroma/versions/820b8bb/bin`（旧构建），已改为 `~/.kroma/versions/4aabc90/bin`，备份见 `~/.kroma/path-backup-20261111-013518.txt`。此后新开终端解析到新构建；`main` 分支仍是旧行为。
