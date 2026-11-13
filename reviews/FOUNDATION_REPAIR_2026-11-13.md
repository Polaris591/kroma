# Kroma 基础链路修复记录

对应审查：`reviews/FOUNDATION_AUDIT_2026-11-13.md`。基线为本轮开始时的脏工作树，完整副本保存在仓库外：
`C:/Users/24524/AppData/Local/Temp/kroma-foundation-baseline-1794503597704`。

没有 reset、stash、提交、安装、修改真实会话/配置或调用付费模型。没有启动新子代理。保留原有用户改动。

## 核心选择：复用现有事实，不增加另一套恢复系统

1. 已发送的宿主参考现在是独立、带类型标记的 WAL 输入消息。Agent 收到模型结果后，先提交该请求新加入的参考，再提交回复/错误边界；不改写用户原文。
2. 重开、fork、rewind、压缩直接使用已有消息。临时接纳表只保留当前任务的待发送项，不再承担历史重放；实际已保存参考不会因为临时表重建或索引改变消失。
3. 工具 adapter 返回完整事实，after hooks 处理完整结果。Agent 随后归档并保存完整 post-hook content，预览放在 metadata。模型请求和展示取预览，history_search/history_read 仍能访问完整原文。
4. 续写不再把展示用的拼接文本写回最终协议响应。截断片段和宿主续写请求逐条保存，最终响应及其签名块保持原样；仅 API 返回值/界面按需合并展示。

## 逐项处置

| 审查项 | 修复 |
| --- | --- |
| B01 外部上下文静默重置 | 删除启动和 open_session 的隐式 External→initial 替换。缺失策略由对应工厂明确拒绝；已有显式 recover 入口保留。此前的恢复失败用例现在通过。用户主动切换运行时策略仍按显式能力变更流程处理。 |
| B02 clear 后跨代缓存 | clear/rewind 在持久状态切换成功后调用已有 invalidate_ephemeral_context，清理召回缓存和待发送参考。切换 archive 也清理 recall cache。已保存参考归属 WAL，不靠同位置同文的临时 cache 重建。 |
| B03 分支/回退/垃圾箱证据 | clone_session_captures 接入新归档的独立硬链接/复制与 archive_file 重映射，覆盖 metadata 预览和旧 preview-only 消息。prune 仅删除存在已确认 snapshot 的文件组，不再把垃圾箱依赖的无 live snapshot 归档当孤立会话删除。 |
| B04 prune 误删共享 map | 以已确认 version/workdir/messages 的 snapshot 为主体；只清理其 snapshot、WAL、所属输出。names/models/usage/capabilities/last 等共享 sidecar 及锁文件不独立按年龄删除。 |
| B05 prune 忽视锁 | 使用 SessionStore 同一 OS 锁，并在锁内重新读取文件集合、核验 snapshot 和年龄。活动或不可加锁的会话跳过，锁文件永不 unlink。CLI 天数乘法增加溢出检查。 |
| B06 续写丢失协议内容 | 截断片段与续写控制消息先持久保存；不再改写最终 response.content。Anthropic 原生块继续逐字重放自己的响应，先前文本通过独立片段保留。控制消息不计作真实用户输入，UI 不生成假用户轮次。 |
| B07 多份预算投影 | 普通历史现在已经包含已发送参考，prepare/准入/观察天然计入同一记录。prepare 也消费已有 admitted_projection；不再为已保存参考减掉 transcript message_count。HTTP、外部模型与自定义 context 通过统一的 tool projection helper 获取模型内容。 |
| B08 进程内快照丢失 | 已发送参考进入既有 WAL，不增加单独持久化格式或记忆数据库。保留任务的参考随原始消息保留；覆盖历史的参考随合法检查点一起摘要，完整原文仍在历史中。 |
| B09 预览破坏 hook/search 契约 | 移除 MCP/plugin execute 内的文本归档投影，改在 after hooks 后处理。WAL content 保留完整事实，model preview 是显式 metadata；实时通知/TUI/Desktop 用预览，不把大内容全部塞进展示。 |
| B10 归档耐久性 | archive file sync_all，Unix 同步目录，然后才允许 WAL 引用它。失败保留完整结果、记录 warning、不重跑外部操作。 |

## 其他审查观察

- `Completion.provider_request_id` 的必填 Rust 字段已撤回，恢复此前 Completion struct 的字段集合；request id 改由可选 WireObservation 提供。未再把公开库接口称作“纯内部”。
- 模型测量身份加入 reasoning/deepseek/native-search 配置，并在每轮规划前刷新，配置改变不能继续套用旧输入测量。
- nested AGENTS 的任务作用域不改成永久粘附。作用域变化可导致 system 前缀变化，但这是需要诚实记录的正确性取舍，不通过长期保留无关指令来换命中率。
- 90%/32k 默认策略未修改，没有增加预热、自动经济决策或新缓存参数。

## 定向验证

遵守少跑测试的要求：没有执行全量单元测试、全量集成测试或 benchmark。

| 命令/范围 | 结果 |
| --- | --- |
| `cargo test --offline --lib foundation_` | 5 通过：参考 WAL 生命周期、完整工具证据及分支/prune、共享 map/锁/垃圾箱保留、活动/近期会话跳过、Anthropic 原生内容重放。最终 Rust 结构调整后再次通过。 |
| `cargo test --offline --lib plugins::model::tests::` | 22 通过。旧 fixture 改为模拟 Agent 提交实际发送的参考，继续验证完整前缀、校准、检查点与数据变更；不是仅比较用户原文。 |
| 两个具名截断/重复续写测试 | 各 1 通过，确认片段持久保存、重复内容不会再次写成答案。 |
| `cargo test --offline --test architecture_backends custom_context_state_survives` | 1 通过；原先记录的外部上下文恢复失败已解决。 |
| `cargo test --offline --test plugins_e2e v2_request_context_and_task_summary_are_budgeted_and_fail_open` | 1 通过，覆盖真实本地插件工作进程、缓存失效、参考更新及恢复后的历史。 |
| `cargo test --offline --test plugins_e2e memory_recall_survives_a_host_control_reference_in_the_projection` | 1 通过，宿主控制内容不成为用户引用或召回意图。 |
| `cargo check --offline --tests` | 通过，仅编译检查，没有执行全部测试。 |
| Desktop `vitest run src/lib/transcript.test.ts -t foundation` | 2 通过，8 个无关用例跳过：宿主输入不产生假轮次、展示组合片段且原生内容不改、完整工具内容与预览分离。 |

开发过程中发现旧 fixture 尚未提交参考输入而失败，已修正 fixture 以符合新的持久消息契约后重跑对应小组；没有把失败隐去或记为成功。旧 800 余项结果未被当成本轮全面验收。

## 边界与未执行项目

- 首次请求尚未收到响应时，新增参考还没写入 WAL；该不确定请求和任何流式数据一样，不宣称有完整的中断持久保证。已提交的输出片段不因后续中断消失。
- 老版本在修复前已经丢掉的参考无法凭空恢复；旧 preview-only 消息仍可读取/分支保存其文件，但原文件若已被删除无法还原。
- 完整 post-hook 工具结果保留在 WAL，磁盘占用可能增加；这换取可搜索的原始事实，不承诺磁盘最优。模型请求仍使用预览，不等于重复发送完整结果。
- 不给无 live snapshot 的归档做激进孤儿清理，以免破坏垃圾箱恢复；未来确有需求再提供明确的清理入口。
- hook 不是沙箱；遗忘项目规则不自动抹掉已经保存的会话历史，旧参考有历史身份，不应当作新权限。
- 未运行完整 Desktop/Tauri 构建、全量平台测试、掉电故障注入或真实服务商验收。没有声称服务端 100% 命中、实际费用下降或任务完成率的量化提升。

## 交付

原始审查及上一轮报告保留为历史记录，本记录描述当前变化。变更未提交、未安装、未重启用户程序。
