# 项目记忆

## 产品原则

记住用户明确表达的长期项目偏好和决定，在相关任务中提供参考。
用户能查看、修改、取消常驻、遗忘、恢复或彻底清除；插件不替用户淘汰规则。
记忆不是权限，不覆盖当前用户要求、项目指令或真实代码。

## 当前行为

- 用户规则保存经过宿主核验的**当前真实用户原话**，而不是模型的摘要。
  引文可以是短句，最长 4000 字；没有共同关键词、肯定/否定句形式等语义猜测。
- 宿主生成的目标提示、工具输出和模型撰写的子任务不能充当用户原话。
- 没有每轮 5 条、常驻 4 条、活跃 200 条或累计一万条的存储配额。
  不自动归档、过期或丢弃用户规则。
- 无路径范围的长期偏好默认常驻；其他跨任务规则可设 `core=true`。
  `core=false` 或 `/memory unpin ID` 取消常驻但保留原文。
- 常驻表示优先进入每次任务的候选内容，不代表绕过模型本身的输入容量。
  单次上下文选择与持久保存分开：只传完整规则，放不下时提示遗漏，不切掉末尾条件。
  可通过插件设置 `context_tokens` 调整默认 2048 的内容预算；超大规则同样会提示未放入，而不是被截断。
- 审查笔记是可选的历史线索，需路径和证据引用，不代表已核实或仍然正确。
  不按日期自动消失；文件变化标记为需要复核，仍可搜索和阅读。
- 不保存最近任务正文，不收集整段对话，不在任务结束时自动批准旧提案。
- `off` 停止自动保存和召回；手动查看、编辑和恢复入口仍保留。

## 接口

四个模型工具：

| 工具 | 行为 |
| --- | --- |
| `memory_remember(kind,text,quote?,core?,paths?,evidence?,supersedes?)` | 偏好/决定要求宿主核验当前原话，并保存原话。审查笔记保存正文及未核实证据。显式替换可重试。 |
| `memory_search(query,paths?,limit?)` | 支持关键词或自然句；查询最多 512 字，结果总数遵守 limit。 |
| `memory_read(ids)` | 每次最多 8 个，返回有效记录及缺失 ID，不因一个 ID 无效而丢掉其他结果。 |
| `memory_forget(id)` | 用户要求时停止未来使用；保留可恢复的正文。 |

人工入口：

```text
/memory
/memory list [state] [offset]
/memory show ID
/memory search TEXT
/memory pin ID
/memory unpin ID
/memory remember [kind] TEXT
/memory correct ID TEXT
/memory forget ID|TEXT
/memory restore ID
/memory purge ID --confirm
/memory off | on
/memory doctor
```

分页、请求大小和单条正文边界用于限制单次通信，不限制长期记忆数量。
旧候选的查看/接受及旧版 JSON 数组导入仅用于迁移，不是正常使用的强制流程。
导出为分页 Markdown，供检查，不冒充可直接往返导入的完整数据库备份。

## 存储和删除

独立本地 Rust 插件与 SQLite。事务提交后才报告保存成功；无网络、遥测或后台模型。
Git 项目按 common directory 身份关联同一仓库的 worktree；不同克隆默认隔离。
私有数据库不应被提交到 Git，项目级数据目录须有默认忽略保护。

遗忘和彻底清除不同：遗忘可以恢复；清除会移除本插件数据库内的正文、原话、
历史正文和证据路径，保留防止自动重放的内容摘要。用户的新明确保存可以重新记住。
这不是磁盘取证级擦除，也不会删除已有聊天、导出、备份或服务商日志。
秘密检测只是尽力提醒，不是存储秘密的安全保证。

## 升级

不清空已有数据库。旧归档记录重新可用；未核实的旧待定/丢弃提案保留为候选，
不会因为一次任务完成而自动生效。移除旧自动过期行为、项目级最近任务副本和轮次计数。
历史正文快照收敛为当前快照和旧措辞摘要，不无限复制正文。

## 检查

```console
cargo test --locked --manifest-path examples/plugins/project-memory/Cargo.toml
cargo fmt --manifest-path examples/plugins/project-memory/Cargo.toml --check
cargo clippy --locked --manifest-path examples/plugins/project-memory/Cargo.toml --all-targets -- -D warnings
cargo build --locked --manifest-path examples/plugins/project-memory/Cargo.toml
# 为宿主集成设置 KROMA_MEMORY_BIN，确保实际运行而不是因缺少二进制跳过
cargo test --locked --test project_memory_e2e
```

安装和工具细节见 [插件 README](../examples/plugins/project-memory/README.md)。
旧研究与接口记录保留在 [历史设计](project-memory-history.md)，不是当前规范。
