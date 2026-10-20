# T06 · 统一安全打开与流式有界读取 — 验收

阶段 G1｜依赖 T00｜关联 KR-06, KR-32

## 一、缺陷

「打开安全」和「读取有界」是两件被分别实现、分别遗漏的事：

- 有些调用点做了 `symlink_metadata` 前置检查，然后用 `fs::File::open` 真读——检查与打开之间
  路径可以被换掉，前置 `stat` 不是边界。
- 有些调用点用 `take(limit)` 读了，但没验文件类型：`O_NOFOLLOW` 不阻止打开 FIFO，
  而 `open(O_RDONLY)` 一个没有写端的 FIFO 会**永久阻塞**。安全打开 ≠ 非阻塞。
- `async_tasks` 的注册表用 `.take(MAX_TASKS)` 静默截断，重复 id 直接覆盖前一条——合法任务被丢掉且无人知晓。
- `task_host::read_report` 先 `fs::read` 整份再比长度：分配已经发生了。

## 二、改了什么

### 1. 共享有界读取器（`secure_fs.rs`）

三道边界，各管一件事，互不替代：

| 边界 | 做法 |
| --- | --- |
| 打开 | Unix：`O_NOFOLLOW \| O_NONBLOCK`（NOFOLLOW 拒绝 symlink 叶子，NONBLOCK 让 FIFO 打开不阻塞）；Windows：`FILE_FLAG_OPEN_REPARSE_POINT` + 前后两次 reparse 检查 |
| 类型 | 对**已打开的句柄** `fstat`，必须是常规文件；FIFO / 目录 / socket / 设备在读第一个字节前就被拒 |
| 读取 | 从同一句柄最多流式读 `limit + 1` 字节，所以「打开后继续增长的文件」也止于 limit |

新增 API：
- `read_bounded_file(path, limit, Overflow)` / `read_bounded_handle(&mut File, display, limit, Overflow)`
  / `read_bounded_utf8(path, limit, Overflow)`
- `Overflow::{Refuse, Truncate}`：结构化输入（配置、注册表、CAS、报告）一律 `Refuse`；
  只有合同允许截断的（ignore 文件）用 `Truncate`，且 `truncated` 显式返回，不静默。
- `HARD_MAX_READ_BYTES = 64 MiB`：调用方传再大的 limit 也会被夹到硬上限。
- 错误类型新增 `NotRegularFile` / `TooLarge` / `NotUtf8`（UTF-8 判定发生在大小检查**之后**，
  所以 4 GiB 二进制按「太大」拒绝，而不是先分配再报编码错）。

### 2. 逐调用点迁移

| 调用点 | 之前 | 现在 |
| --- | --- | --- |
| `.kroma.toml`（workspace） | `symlink_metadata` 预检 + `File::open` + `take` | `read_bounded_utf8(..., Refuse)`；预检只保留为友好报错 |
| 用户全局配置 | `fs::read_to_string` | 同上 |
| `.gitignore` / `.ignore` | `read_to_string`（无上限） | `read_bounded_handle(..., MAX_IGNORE_FILE_BYTES=1 MiB, Truncate)` |
| edit 输入 | `read_to_end`（长度来自前置 `stat`） | `read_bounded_handle(..., max_edit_file_bytes, Refuse)`，超限报「读到一半变大」 |
| CAS blob | `read_to_end` | `read_bounded_handle(..., MAX_SNAPSHOT_FILE_BYTES, Refuse)` |
| 快照捕获 | `read_to_end` | 按该文件已记录的 `size` 有界读；超限视作「读中变化」，跳过而不记半份快照 |
| merge/fork/restore intent | `read_to_end` | `MAX_INTENT_BYTES = 64 KiB`，`Refuse` |
| revision 记录 | `read_to_end` | `MAX_REVISION_RECORD_BYTES = 1 MiB`，`Refuse` |
| 任务报告 | `fs::read` 后比长度 | `read_bounded_file(..., 记录的字节数, Refuse)`：超限或 symlink 在分配前失败 |
| async 注册表 | `fs::read_to_string` + `.take(MAX_TASKS)` | 有界读 + **重复 id 报错** + **超记录数报错**（不再静默裁掉） |

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo test -p kroma-core --lib secure_fs` | 21 passed（原 14 + 新 7） |
| `cargo test -p kroma-core --test t06_bounded_io` | 7 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 见 `logs/test_t06.log` |

### 单元测试（`secure_fs`，7 条新增）

- `bounded_read_truncates_at_the_limit_and_reports_it`：超限截断并 `truncated=true`；
  恰好等于 limit 时不标记截断。
- `bounded_read_refuses_oversized_content_before_decoding`：超限 → `TooLarge{limit}`；
  同一份二进制在 limit 内 → `NotUtf8`（证明顺序是先大小后解码）。
- `bounded_read_refuses_a_symlinked_leaf`：`ELOOP`，没有读到目标内容。
- `bounded_read_refuses_a_fifo_without_blocking`：无写端的 FIFO，2 秒内返回 `NotRegularFile`。
- `bounded_read_refuses_a_directory`：目录 → `NotRegularFile`。
- `bounded_read_stops_a_growing_file_at_the_limit`：4 KiB 文件按 64 字节读，只拿到 64。
- `bounded_handle_read_uses_the_handle_type_not_the_path`：句柄版走同一套类型检查。

### 调用点测试（`t06_bounded_io`，7 条）

`workspace_config_fifo_never_blocks`、`workspace_config_swap_to_symlink_is_refused`、
`workspace_config_growth_stops_at_limit`、`failed_config_read_does_not_apply_partial_values`、
`huge_ignore_file_cannot_allocate_unbounded`、`growing_edit_reads_stop_at_limit`、
`registry_overflow_or_duplicate_id_is_not_silently_truncated`。

### 变异验证

- 去掉 `O_NOFOLLOW`：`bounded_read_refuses_a_symlinked_leaf` 立刻红，报错里直接带出读到的
  `bytes: [115, 101, 99, 114, 101, 116]`（"secret"）——测试不是空转。
- 去掉 `O_NONBLOCK`：`bounded_read_refuses_a_fifo_without_blocking` **挂死**，被 25 秒 `timeout` 杀掉，
  没有 `test result` 行。这正好证明 NOFOLLOW 不等于非阻塞，两者缺一不可。

## 四、边界

- Windows 侧用的是 `open_existing_no_symlink` 的 reparse-point 双检路径（打开前后各一次），
  没有 `O_NONBLOCK` 的对应物；Windows 的 FIFO 等价物（命名管道）本就不通过文件系统路径打开，
  所以「非阻塞」这一条在 Windows 上由类型检查承担。
- 没有改 `find`/`edit` 的**写出**路径，只改读取。
- 注册表「超记录数」现在报错而不是截断：这是行为变化，旧 WAL 若真有 65 条会被拒绝加载并
  明确报错，比静默丢掉第 65 条好。已记入 T07（WAL 写入/恢复限制）的输入。
- `oversized_or_symlink_report_is_refused_before_read`（无同名测试）没有做 host 级测试（`DurableTaskHost::new`
  需要 provider/snapshot store 等一整套夹具），当前由 `secure_fs` 的 symlink/超限单测 +
  `read_report` 的调用点代码共同覆盖；host 级用例留到 T17/T18 建好夹具后补。
