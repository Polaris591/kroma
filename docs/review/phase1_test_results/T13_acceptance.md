# T13 · 完成 MCP 当前版本的握手与目录分页 — 验收

阶段 G2｜依赖 T06, T10, T12｜关联 KR-19

## 一、缺陷

三处，都指向同一个问题：**客户端对自己声称支持的版本，只实现了一半**。

1. **握手不校验版本**。`initialize` 把 `protocolVersion: "2024-11-05"` 发出去，然后**不管服务端回什么**
   都直接 `notifications/initialized`——服务端回 `2099-01-01` 也照单全收，之后每一帧都按错误的规范解释。
2. **不检查 capabilities**。服务端没声明 `tools` 能力，客户端照样发 `tools/list`；那是协议违规，
   服务端可以回任何东西。
3. **`tools/list` 只取一页**。规范里它用 `nextCursor` 分页，单页实现会把第二页之后的工具全部丢掉——
   而且丢得悄无声息，那些工具直接变成不可调用。

## 二、改了什么

### 1. 握手（`initialize`）

- `SUPPORTED_PROTOCOL_VERSIONS = ["2024-11-05"]`——**故意只有一项**。任务卡要求「正确实现当前声明的
  支持范围，而不是盲目追版本」，声称支持未测过的版本是另一个方向的撒谎。
- 先解析服务端结果，校验 `protocolVersion` 属于支持集合，**不通过就返回
  `UnsupportedProtocolVersion` 并且不发 `notifications/initialized`**——服务端永远不会被告知会话已建立。
- 通过后把 capabilities 记在客户端上（`server_capabilities`），后续调用据此判断。

### 2. 能力检查（`declares_capability` + `list_tools`）

- 没声明 `tools` → `McpError::CapabilityMissing { capability: "tools" }`，**请求根本不发**。
- 例外：客户端已被 poison（字节流状态未知）时，先报传输错误——此时任何「能力判断」都只是猜测。
- 主机层（`McpToolHost`）遇到这种服务端**跳过并记录**（`skipped_servers()`），而不是让整个 host 启动失败：
  只提供 resources 的 MCP 服务端是合法配置，但运维要能看到它为什么一个工具都没贡献。

### 3. 目录分页（`list_tools`）

有界翻页，四个预算同时生效：

| 预算 | 值 |
| --- | --- |
| 页数 | `MAX_TOOL_PAGES = 32` |
| 工具总数 | `MAX_CATALOG_TOOLS = 256` |
| schema 字节（序列化后累加） | `MAX_CATALOG_SCHEMA_BYTES = 256 KiB` |
| 游标 | 重复出现即 `CursorCycle` |

另外：同名工具 → `DuplicateToolName`（目录歧义会让「工具名 → 宿主工具」的映射不确定），
超预算 → `CatalogBudgetExceeded`。所有拒绝都是错误，不是静默截断。

### 4. annotations 不得覆盖宿主判定

`McpToolDescriptor` 不解析 `annotations`，`to_registered_tool(server, effect_class)` 的 effect class
**由宿主传入**（默认 `Mutating`，只有运维显式把服务端标为 read_only 才降级）。服务端的
`readOnlyHint: true` 不会改变任何东西。

### 5. 目录变化使过期激活失效

T12 的激活集在每次构造请求时按**活的 registry** 解析（`prune_to`），所以目录变了、扩展卸载了，
激活记录自动消失，不会留下指向不存在工具的 schema。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --test t13_mcp_handshake` | 6 passed / 0 failed |
| `cargo test -p kroma-core --test mcp_config` | 9 passed / 0 failed |
| `cargo test -p kroma-core --lib mcp::` | 20 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 911 passed / 2 failed（`logs/test_t13.log`） |

那 2 条是台账记过的既有 flake：`session_ownership::tests::different_workspaces_are_independent...`（§2.5）、
`b08c_git_metadata::carries_a_sha256_commit_through`（§2.2，需 `--test-threads=1`），复跑均全绿。

### 新增测试（6 条，全部用真实 Python stdio 服务端）

- `mcp_unknown_version_refuses_before_initialized`：服务端回 `2099-01-01` → `UnsupportedProtocolVersion`；
  服务端侧的 marker 文件**不存在**，证明 `notifications/initialized` 没发出去。
- `mcp_no_tools_capability_is_explicit`：`capabilities: {}` → `CapabilityMissing`，且服务端的
  `tools/list` marker 不存在；host 层启动成功、`skipped_servers()` 记录该服务端与原因。
- `mcp_tools_pagination_is_complete_and_bounded`：两页（`nextCursor: "page-2"`）→ 两个工具都发现，
  顺序与服务器一致。
- `mcp_cursor_cycle_is_detected`：永远返回同一个 `nextCursor` → `CursorCycle`，不会无限翻。
- `mcp_duplicate_names_and_oversized_schemas_are_refused`：同名工具 → `DuplicateToolName`；
  16 个各 40 KB schema → `CatalogBudgetExceeded`。
- `annotations_do_not_override_host_effect_class`：`readOnlyHint: true` 不影响宿主传入的 `Mutating`，
  注解也不进入模型看到的定义。

### 变异验证

把 `list_tools` 的游标分支改成「永远单页」，`mcp_tools_pagination_is_complete_and_bounded` 立刻红：
`left: ["first"]`。恢复后 6/6 绿。

### 既有 fixture 的必要修正

`capabilities: {}` 却提供 `tools/list` 的测试服务端是不合规的（我们自己的 fixture），已全部改为
`{"tools": {}}`：`mcp.rs` 内 2 处、`mcp_config.rs` 6 处、`b05_execution_context.rs`、`b06_long_lived_contract.rs`、
`kroma-cli` 的 headless 用例各 1 处。这不是「放宽断言」，而是让 fixture 符合它声称实现的规范。

## 四、边界

- **不宣称**支持 resources / prompts / sampling / 远程传输（SSE、HTTP）：本客户端只实现 stdio + tools，
  任务卡明确禁止声称未测能力。`McpToolDescriptor` 也不解析 `annotations` 之外的扩展字段。
- 服务端请求（server→client 的 `roots/list`、`sampling/createMessage`）目前**不回应**：请求 ID 不会被
  伪造成结果，服务端会等到自己的超时。这一条属于「不支持的通知/服务端请求显式处理」的未完成部分，
  已记入 T30 台账（当前实现是「不回应」而不是「回一个假结果」）。
- 版本集合只有一个值，是刻意的：新增版本必须先有 wire fixture 与 round-trip 测试。
