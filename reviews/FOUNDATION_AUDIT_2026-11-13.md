# Kroma 基础链路深度审查：KISS / Unix / YAGNI

日期：2026-11-13。基线：HEAD `d6aa128` 加本轮核读时的未提交工作树，包含上一轮缓存修复；不是已安装二进制的验收。

## 结论

**主干结构是合理的，但基础功能尚不能称为打磨到极致。主要短板不是缺少功能，而是跨模块的状态身份、文件所有权和请求投影没有完全闭环。**

本轮确认的高优先级路径包括：外部上下文被静默重置；清空会话后模型装饰器仍可能复用上一代的参考缓存；分支没有继承新增工具归档的所有权；prune 把全工作区元数据当作独立会话删除且不检查活动会话锁；部分 Anthropic 自动续写内容在后续协议重放中缺失。

这些都是可以从具体代码链路推导的条件性缺陷。**本轮没有运行测试、基准、真实模型或真实会话操作，也没有复现实际用户数据损坏、服务商缓存清零或账单变化。** 以下明确区分静态确认、条件风险和设计取舍，不把源码推导写成线上复现。

本轮只新增此审查报告，不修改生产代码、现有配置、会话、旧报告或安装文件；未开启新子代理。

## 范围和方法

- 核读 `docs/architecture.md`、`docs/lifecycle.md`，对照实际运行路径，而不是以文档替代实现。
- 沿 Agent → SessionStore/WAL → ContextPolicy → ModelBackend/协议适配器，以及 ToolRegistry → MCP/插件 → 输出归档追踪数据。
- 检查 `/clear`、会话打开、fork、rewind、垃圾箱恢复、prune、取消、输出截断与续写这些边界。
- 对照上一轮修复记录，重新核实“已修复”覆盖的契约；旧测试失败仍作为未解决线索，而非因为旧就忽略。
- 不覆盖所有 Desktop 展示、第三方依赖或安全沙箱；文件检查点仅核读其独立 Git、恢复范围和锁边界，不声称完整验证所有平台行为。

## 一、已有基础中应保留的部分

1. **单一前台循环。** CLI/TUI/Desktop 共用 Agent，宿主不另做工具执行和会话恢复。
2. **WAL 先于成功通知。** 用户、模型和各工具结果分别落盘；`ToolFinished` 等发生在对应 append 后：`src/agent.rs:2423–2465`。
3. **持久化失败后停止继续写。** `SessionStore::append` 在不确定写入后 poison，后续 `ensure_writable` 拒绝继续，避免在坏尾部继续操作：`src/session.rs:400`、`:517–590`。
4. **恢复不自动重跑外部操作。** 未完成工具被标为结果未知，而不是自动重试：`src/session.rs:724` 起、`src/agent.rs:1297` 起。
5. **快照替换、generation、校验和及会话锁。** SessionStore 已有 fsync、原子替换、旧代 WAL 隔离和进程生命周期锁。新功能应复用这些规则。
6. **工具后处理失败不伪装执行失败。** 失败的 after hook 回滚其补丁并产生 warning：`src/tools.rs:410–424`。成功 hook 仍可按公开契约修改结果，这不是沙箱。
7. **MCP/Skill 按需加载。** 不默认注入所有远程工具定义和技能正文。
8. **原始会话不被压缩覆盖。** 内置摘要替换的是模型投影，保存历史仍存在；近期原文保留和工具配对保护值得保留。
9. **持久项目记忆不是纯内存。** 示例项目记忆插件使用 SQLite WAL、`synchronous=FULL` 和写事务：`examples/plugins/project-memory/src/store.rs:127–153`。不能把宿主参考快照丢失等同于数据库记录被删除。
10. **用量未知不算零。** 缓存读取/写入是输入组成部分，reasoning 是输出子集，不重复相加。

没有证据支持为此引入新 Agent 框架、事件总线、通用策略平台或新的记忆数据库。

## 二、高优先级发现

### B01 · P1：缺失外部上下文策略时，加载路径静默改用初始状态

**位置：** `src/agent.rs:427–435`、`:1250–1258`；对照 `src/context.rs:284–299`、`src/agent.rs:206–222`。

**静态确认：** 启动和 `open_session` 都有同样的分支：只要当前工厂的 initial 不是 External，而加载到的 state 是 External，就把加载状态替换为 `context_factory.initial()`。随后 restore 接收到的已不是原来的外部状态，绕过了 BuiltinContextFactory 对缺失外部策略的报错。

影响不只是一次缓存失效：后续正常 append/保存会把默认策略状态继续写入历史，原外部策略的派生记忆不再沿用。原始 messages 尚在，但不能据此说策略记忆没有丢失。

代码没有区分“用户明确关闭并重置该策略”和“插件缺失、配置变化、错误组合导致没有加载对应工厂”。已有 `recover_builtin_context` 正是为显式恢复提供的入口，因此这条无条件替换与显式恢复契约冲突。此前记录的 `architecture_backends::custom_context_state_survives_wal_and_process_restart` 失败也与这条路径吻合；本轮没有重跑它。

**最小方向：** 正常加载保留未知状态并明确报错；仅当用户显式选择重置/恢复时，复用现有 reset/recovery 事务。不要删除原状态后再宣称“会话可恢复”。

### B02 · P1：`/clear` 没有使模型侧缓存切换到新的会话代

**位置：** `src/agent.rs:493–519`、`:521–526`；`src/session.rs:389`、`:593–620`；`src/plugins/model.rs:1069–1087`、`:1205–1215`、`:1558–1589`。

**静态确认：**

- SessionStore.reset 会生成新 generation。
- Agent.reset 清理历史和内置压缩状态，但没有通知模型装饰器清空其 recall cache / ReferenceTable。
- request_packs 的 key 只有 `(archive, index:digest)`，没有 generation。
- ReferenceTable 的匹配是消息位置、用户序号、内容 digest；同文同位置可以重新命中。
- set_session_context 即使被调用，也只在 archive 路径不同才清空 ReferenceTable，无法识别同路径的新一代历史。

**可达条件：** 原会话第一个任务是 `继续`，已获得参考 R；`/clear` 后再次发送同文任务，archive/index/digest 可以全部相同，旧 pack/参考快照会被复用。即使两次之间数据源已经改变，也不能据该 key 判断这是原任务。

这不是“为了保护缓存保留旧历史”，而是**不同历史代缺少身份隔离**。同路径 rewind 后重做相同措辞也需要同一类语义检查。

**最小方向：** 复用已有 SessionStore generation 作为宿主状态身份的一部分，明确 clear/rewind 对临时投影缓存的失效语义。仅重读插件或只调用同路径 set_session_context 不足以解决。

### B03 · P1：新大工具输出归档未接入 fork/rewind 文件复制，清理源会话会破坏分支证据

**位置：** `src/agent.rs:1004–1025`、`:1124–1153`；`src/shell.rs:122–225`；`src/tool_archive.rs:87–107`、`:243–246`；`src/sessions_prune.rs:29–38`、`:76–104`。

**静态确认：** fork 和 rewind 的历史备份统一调用 `shell::clone_session_captures`。该函数已有 Shell `stdout_file`/`stderr_file`、MCP `image_reference` 的复制/硬链接与路径重映射，但不认识新预览中的 `archive_file`。

因此 fork 后仍会保存源会话 A 的 `A.tool-output-…txt` 路径，而不是分支 B 自有的引用。prune 按文件名把该归档归到 A，A 过期后删除；B 即使仍活跃、保留完整会话，也无法恢复该输出的原始字节。rewind 的“保留旧未来”备份同样受影响。

**最小方向：** 把新增归档接入现有文件复制/路径重映射机制，复用硬链接/复制逻辑；明确所有权，不能只补命名和删除规则。不需要新建资源管理数据库。

**相关问题：** `trash_session` 只移动 snapshot/WAL 与元数据到 `.deleted`（`src/sessions.rs:432–498`），没有移动这些归档。prune 可以把垃圾箱会话仍需的归档视为孤立源会话文件删除，之后恢复 transcript 不等于恢复完整证据。

### B04 · P1：prune 把工作区全局 sidecar 当成独立过期会话

**位置：** `src/sessions_prune.rs:24–38`、`:53–104`；`src/sessions.rs:71–88`、`:171`、`:248`、`:290`。

**静态确认：** pruner 对任意 `.json` 文件执行去后缀分组，没有核验它是会话 snapshot。SessionCatalog 把多个会话共享的 map 存在默认文件旁边，例如：

```text
abc.json                  → group abc
abc.last.json             → group abc
abc.names.json            → group abc.names
abc.models.json           → group abc.models
abc.usage.json            → group abc.usage
abc.capabilities.json     → group abc.capabilities
```

后四个 map 会按各自 mtime 独立过期。即使 `abc.json` 或分支会话刚更新，只要未重命名/改模型等，旧 map 仍可能被删除，丢失仍存活会话的名称、模型选择、统计或能力覆盖。能力覆盖丢失后回到默认能力，不一定符合用户此前选择。

**最小方向：** 区分真实会话主体、会话附属文件与共享 catalog；仅以已确认存在的会话及其所属文件清理。不能把所有 `.json` 一视同仁，也不应新建一套元数据存储系统。

### B05 · P1：prune 不遵守 SessionStore 的活动会话锁

**位置：** `src/sessions_prune.rs:42–108`，对照 `src/session.rs:332–380`、`src/sessions.rs:432–436`。

**静态确认：** prune 扫描 mtime 后直接 remove_file，没有获取/检查 Agent 使用的 `.json.lock`。与 trash_session 显式获取同一会话锁不同。

**条件风险：** 某进程保持一个已超过 cutoff 的会话打开，或在 prune 扫描后开始写入，另一个进程仍可删除该会话文件。Unix 下会话锁文件不保护被 unlink 的 snapshot/WAL；Windows 下锁住的也是 lock 文件，不是 snapshot/WAL 本身。下一次 append 可能留下缺失 snapshot 的 WAL，使正常重开被现有保护拒绝。

mtime 是保留策略，不是并发互斥。**最小方向：** 清理前复用同一会话锁，并在锁内重新检查年龄和文件集合；正在使用则跳过，不需新增进程注册表。

### B06 · P1（特定协议场景）：自动续写只合并展示文本，未同步原生重放内容

**位置：** `src/agent.rs:1992–2001`、`:2147–2153`；`src/provider/anthropic.rs:115–138`、`:652–659`、`:667–740`。

**静态确认的组合：**

1. 输出截断后的文本暂存到局部 `answer_prefix`。
2. 后一轮成功时，仅将 `answer_prefix + continued` 写回 `response.content`。
3. 若成功轮包含 thinking、原生搜索/引用或 pause，Anthropic 返回值会同时保存 `kroma_anthropic_content_blocks`，其中只有成功轮的原生 blocks。
4. 后续 Anthropic payload 遇到 native blocks 时优先重放这些 blocks，**不会再使用合并后的 `msg.content`**。

因而在上述条件下，UI/保存的 content 可以显示完整 A+B，下一次真实发送却只重放 B 的原生内容。普通无 native blocks 的 Anthropic 文本和 Chat 路径不能直接套用这个结论。

当前续写回归 `src/agent.rs:3028` 使用 Chat 模拟响应，只检查合并后的 content，未覆盖这一协议分叉。本轮未执行跨协议复现或真实请求。

**最小方向：** 为续写定义一个统一的“持久消息与协议重放”契约，不能仅拼展示字符串。不要为了补文本擅自修改 signed thinking；优先复用原始、完整回合的表示来保证事实和协议一致。

**另一个连续性风险：** 已展示的截断文本在续写全部结束前没有写入 WAL；若中间取消、进程退出，或下一轮变成非截断错误，局部 prefix 可能丢失。当前取消/非截断错误说明它不会保留部分流式内容，因此这是明确的产品行为，而不是“已持久保存的答案丢了”；但也不能把自动续写描述成中间阶段具备完整持久保证。

## 三、需要收敛的基础设计缺口

### B07 · P2：压缩触发、请求准入和最终注入仍不是同一投影

**位置：** `src/context.rs:1574–1620`；`src/agent.rs:1696–1749`；`src/plugins/model.rs:1365–1529`、`:1600–1610`。

`prepare_with_extra` 先对不含参考消息的视图计算触发水位；Agent 在 prepare 完成后才调用 admitted_projection 做准入；decorator 最后又执行召回/接纳/验证并决定是否降级。

服务端测量记录的前缀包含参考，但 prepare 的前缀不含参考，`prefix_hash_matches` 无法以同一视图匹配，常退回估算。**上一轮修复了 Agent 准入处的校准，不等于修好了压缩触发处的校准。**

结果可能是：有空间但估算偏大而提前摘要；或参考占用没有进入触发规划，直到发送前才撤回/降级。最新容量补丁避免了可选参考导致任务失败，但没有消除根因。原始消息不丢，模型当次可用参考仍可能减少；实际任务影响和费用未测量。

**KISS 方向：** 将当前已确定的投影事实用于触发、准入、发送及观察，而不是三个阶段各重建一次并增加退路。复用现有边界，避免再加一套“智能上下文规划器”。

### B08 · P2：ReferenceTable 的身份和生命周期未达到“稳定记忆连续性”

**位置：** `src/plugins/model.rs:669–680`、`:838–860`、`:1069–1074`、`:1558–1589`；`docs/project-memory.md:23–43`。

表结构注释把 boundary 称为 raw transcript index，生产却在 prepared/compacted view 上接纳并 retain_valid。检查点改变索引后，尚存任务的旧参考也会被丢掉；进程重开或重建装饰器时，整张表消失，只能重新召回当前任务。

数据库里的用户记忆可以仍然存在，但模型此前看到的参考快照不再可重放。**这既影响缓存，也可能影响继续任务时能看到哪些线索，不能一律写成“安全、只影响缓存”。**

不建议为了修它复制完整记忆数据库。先明确哪些资料属于：
- 用户明确持久化的项目规则；
- 已发送、需要会话连续重放的宿主参考；
- 可以随时重新计算的临时检索结果。

再复用已有会话身份、generation 和持久化边界。没有身份契约之前，不继续增加 stale/degraded/known 等布尔状态来补救各场景。

### B09 · P2：大结果投影发生得太早，改变了工具后处理与历史检索契约

**位置：** `src/mcp.rs:267`；`src/plugins.rs:2751–2770`、`:2905–2944`；`src/tools.rs:394–424`；`src/agent.rs:2423–2448`；`src/history_tools.rs:72–81`、`:115–173`。

MCP/plugin execute 内已经把完整结果换成 preview，after_tool_result 接收到的是预览；Agent 写入 WAL 的也是这个预览。完整字节仅在 artifact 中。

因此：
- 后处理 hook 若原来需要检查/脱敏/提取完整 JSON，现在可能看不到数据；原始 artifact 也已在 hook 前保存，不能把 after hook 当完整脱敏边界。
- history_search 只搜 WAL 中的 content/tool args，不跟随 archive_file；被预览省略的文本不再可被“完整历史搜索”命中。知道文件路径后仍可用 Shell 读取，不等于原搜索契约未变。
- 64 KiB/2000 行这个默认阈值改变任务的回读轮次，尚无当前 workload 的收益证据。

**Unix/KISS 方向：** 保持工具事实和模型展示的边界清楚；优先在统一结果边界处理稳定 artifact 引用，不让每个 adapter 各自改变 hook 输入。复用已有文件读取路径，不因此增加自动向量索引或语义去重系统。

### B10 · P2（掉电耐久性）：归档文件与指向它的 WAL 没有同等级提交顺序

**位置：** `src/tool_archive.rs:112–128`，对照 `src/session.rs:565–569`、`:696–705`。

归档只 write_all + File.flush，然后结果路径进入会 fsync 的 WAL。File.flush 不等于 sync_all，进程返回成功也不代表内容已耐久落盘。

**条件风险：** 系统崩溃/掉电后存在 WAL 保留引用而 artifact 原字节未完整落盘的窗口。普通进程退出并不自动证明数据会丢，本轮没有做故障注入。类似外部输出路径也应整体核对，不应只给新模块一个更强的宣传保证。

**最小方向：** 复用现有 private-file/持久文件写入约定，在提交唯一证据引用前保证需要的文件/目录耐久性；失败仍保留原结果，不重跑外部操作。无需引入对象存储或后台提交队列。

## 四、不是已确认缺陷，但应明确的取舍

1. **nested AGENTS 的提示变化。** `refresh_for_task` 每个新任务清空 nested_paths 并重建提示（`src/prompt.rs:60–66`、`:166–180`）。前一个任务发现过 nested 指令，下一个任务只说“继续”时，这段 system 贡献会先消失，再在路径重新发现时加入。属于条件性前缀变化，不是重读同文件就必定掉缓存。应兼顾作用域正确性，不应为了缓存把所有 nested 指令永久粘在 system 上。
2. **服务端缓存不受客户端完全控制。** TTL、路由、驱逐、模型变化仍可能导致 miss。目标是杜绝本地无理由改写，而不是承诺服务商 100% 命中；不定时付费预热来掩盖设计问题。
3. **32k 保留和 90% 触发不是自动成本最优。** 当前没有证据支持调整，保持默认比加经济预测器更符合 YAGNI。
4. **API 不应被称为纯内部。** `src/lib.rs` 公开导出 model 等模块，ModelBackend 和 Completion 都是公开类型。新增 required `Completion.provider_request_id` 会使外部 struct literal 失编译；不能因仓库内调用点全改完就声称保留了公共 Rust API。是否接受破坏性变更需要明确版本/兼容约定。
5. **模型测量身份范围。** `Compression::model_identity` 只有 protocol/base_url/model（`src/context.rs:707–713`）；Agent live reasoning 设置只转发到 backend（`src/agent.rs:616–618`）。请求设置变化是否影响输入统计/可重放性，仍应纳入同一个有效请求身份，而不是用更多散落缓存键补丁。此项未量化预算偏差，不按已复现超限报告。

## 五、按 KISS / Unix / YAGNI 的收敛建议

### 优先修基础契约，不先加新功能

1. **保存/恢复优先：** 关闭静默 external-state 替换；以已有 generation 隔离 clear/rewind 后的模型临时状态。
2. **文件所有权优先：** 新 artifact 接入现有 fork/rewind/trash/prune 生命周期；prune 区分 snapshot、全局 catalog 和附属文件，并遵守现有会话锁。
3. **一种消息事实：** 续写后的保存内容与各协议实际重放一致；UI 完整不代表协议重放完整。
4. **一种已确定投影：** 压缩触发、准入、发送、测量消费同一事实，不层层重建再互相兜底。
5. **明确原始结果/模型预览边界：** 工具完成、hook 后处理、归档、WAL 和模型展示按清楚的顺序组合。

### 暂不做

不重写 Agent；不增加框架、自动经济控制器、定时预热、语义答案缓存、每轮摘要或新的记忆数据库；没有具体需求和收益证据时，不继续扩展缓存配置矩阵。已存在的 opt-in 参数也不因“可选”就自动证明应当保留，需在收敛时核对现实需求与维护成本。

### 验收指标

优先顺序：状态/证据完整 → 中断与恢复正确 → 请求/记忆连续 → 任务完成质量 → 调用次数/延迟 → 累计输入输出及真实缓存费用。不能只以最后一条命中率或单次输入变短验收。

## 六、最小验证建议（本轮没有执行）

后续真要修复时，优先每个契约一个微型用例，不重跑全面矩阵：

- external state + builtin factory：拒绝静默替换，原文件不变。
- 同 archive `/clear` + 同文首任务：generation 改变，不沿用旧 pack/参考。
- 大 artifact → fork/rewind → 源会话 prune：分支原文仍能读。
- 旧 metadata map + 新 session：prune 保留仍需的共享选择与能力配置。
- 持有 session lock：prune 跳过，必要时仅此项用两个本地进程验证。
- Anthropic 截断 A + 带 native blocks 的成功 B：保存、展示、下一次 payload 都能表达应有的 A+B，同时保留协议约束。
- 参考占用推动过 soft：触发规划与真实请求使用同一水位，不只在尾部降级。
- hook 需要完整结果/历史查找原文：约定的边界和读取路径一致。

这些是建议的最小验收契约，不是本轮已经通过的测试。上一轮 809/22/47 等数字也不当作本轮状态的重新验收。

## 最终判断

Kroma 不是缺少“更聪明”的缓存或记忆策略，而是**生命周期边界还没有足够简单、统一、可靠**。新模块只在正常调用路径成立、没有同时进入 clear/fork/rewind/prune/reopen/协议重放的共同契约，是当前反复补丁的主要来源。

正确方向是减少重复事实与隐式状态，而不是继续扩大功能面。本报告不宣称已修复上述问题，也不宣称已证明实际省 token 或提高完成率。
