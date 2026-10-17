# T08 · 完成 Windows 跨进程工作区写者所有权 — 验收

阶段 G1｜依赖 T06, T07｜关联 KR-26

## 一、缺陷

`acquire_workspace_lock` 在非 Unix 上直接 `Ok(None)`：

```rust
#[cfg(not(unix))]
{
    let _ = (canonical_root, owner_id);
    Ok(None)          // <- 没有跨进程保护
}
```

也就是说 Windows 上两个 `kroma` 进程可以同时写同一个工作区，唯一的门是进程内 registry——
这正是任务卡禁止的「以进程内 Mutex 替代」。`workspace_lock_dir()` 还用了 `temp_dir()`，
在 Windows 上它读 TMP/TEMP，任务改环境变量就能拿到第二个锁域。

## 二、改了什么

1. **Windows 原生句柄锁**（不是降级、不是 Mutex）
   - 锁文件以 `share_mode(READ|WRITE|DELETE)` 打开（这样竞争者仍能读到持有者是谁），
     然后 `LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY, 0, 1, 0)` 抢第 0 字节。
     这是 `flock(LOCK_EX | LOCK_NB)` 的对应物：非阻塞、排他。
   - 释放依赖句柄关闭——**进程被杀时内核也会释放**，所以不存在需要清理的陈旧租约。
   - 冲突时 `ERROR_LOCK_VIOLATION` / `ERROR_SHARING_VIOLATION` → 读取锁文件里的 `owner\tpid`
     作为冲突信息，和 Unix 的报错形状一致。
   - 打开后对**句柄**做 reparse point / 常规文件检查，与 Unix 的 `O_NOFOLLOW` + `fstat` 对齐。

2. **稳定工作区标识**
   - `canonical_key` 在 Windows 上额外做两件事：去掉 `\\?\` verbatim 前缀、整体小写。
     于是 `C:\Work\Repo`、`c:\work\repo`、`\\?\C:\Work\Repo` 三种写法落到同一个锁文件。
   - 锁目录不再用 `temp_dir()`：优先 `C:\ProgramData\kroma-writer-locks`（机器级，不受
     TMP/TEMP 影响），只有镜像里没有 ProgramData 时才回退到 temp 并在代码里写明这是回退。
   - Windows 版 `ensure_lock_dir`：拒绝 reparse point、拒绝非目录。

3. **读写兼容矩阵**（写进文档并测试）

   | 持有者 \ 请求者 | writer | reader |
   | --- | --- | --- |
   | 无 | 允许 | 允许 |
   | writer | **拒绝** | 允许 |
   | reader | 允许 | 允许 |

   读者不取租约，所以既不阻塞别人也不被别人阻塞，读到的是最后一次已提交的 WAL 状态。
   这是设计选择，不是意外，因此用测试钉住。

4. **顺手修掉的两个 Windows 编译阻塞**（既有问题，非本次引入）
   - `provenance.rs::special_files_are_never_read_and_cannot_stall_the_walk` 用了 `libc::mkfifo` 却没有
     `#[cfg(unix)]` → Windows 测试目标编不过。
   - `bench_contract.rs::a_held_out_suite_symlinked_into_the_workspace_is_refused` 用了
     `std::os::unix::fs::symlink`，同样缺 cfg。
   两处都是 Unix 专有的测试手段，加门控后 Windows 测试代码可以整体编译。

## 三、验收证据

| 门禁 | 结果 |
| --- | --- |
| `cargo build -p kroma-cli --target x86_64-pc-windows-gnu` | **Finished**，产出 `kroma.exe`（199,409,491 字节）——是 build 不是 check，链接真的过了 |
| `cargo check -p kroma-core --tests --target x86_64-pc-windows-gnu` | 0 error（全部测试代码在 Windows 上类型检查通过，含新增的三个 windows 用例） |
| `cargo test -p kroma-core --lib session_ownership` | 8 passed（Linux） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warning |
| `cargo test --workspace --no-fail-fast` | 见 `logs/test_t08.log` |

日志：`logs/t08_windows_build.log`、`logs/t08_windows_tests_check.log`。

### 新增测试

- `lock_path_is_stable_across_equivalent_spellings`（Linux 可跑）：`ws` / `ws/.` / `ws/sub/..`
  三个写法落到同一个锁文件。
- `a_reader_is_never_blocked_by_a_writer_lease`（Linux 可跑）：矩阵里「writer 持有时读者仍可读、
  第二个 writer 被拒且报出持有者」。
- `windows_two_processes_same_workspace_have_one_writer`（**cfg(windows)，需真机执行**）：
  子进程持有租约并 ack → 父进程被拒且冲突信息点名 `windows-holder` → kill 子进程 →
  父进程立刻拿到租约（验证「被杀即释放」）。
- `windows_lock_holder_child`（cfg(windows)）：上条的持有者子进程，未设环境变量时是 no-op。
- `windows_path_aliases_share_lock_domain`（**cfg(windows)，需真机执行**）：
  大小写不同、带 `\\?\` 前缀、带尾分隔符三种写法必须同域。

## 四、BLOCKED：真机运行

任务卡要求「两个独立 Windows 子进程竞争、kill 后重获、同路径不同表示、读者/写者模式都在**真实
Windows runner** 测试」。本机是 Debian 13 x86_64：

- 没有 Windows 主机，也没有 wine（未安装；且 wine 只是 ABI 兼容层，不满足「真实 Windows runner」）。
- 因此三个 `windows_*` 用例**已实现并编译通过，但未执行**。

缺失依赖：一台 Windows runner（或 CI 里的 windows-latest）。执行方式：
`cargo test -p kroma-core --lib session_ownership --target x86_64-pc-windows-msvc`
（或 gnu 目标在 Windows 上直接跑）。这三条在台账里记为 **BLOCKED（runtime）**，
平台能力声明没有被删除，也没有用进程内 Mutex 掩盖。

## 五、边界

- 没有改 Unix 的锁实现（`flock` + `O_NOFOLLOW` + uid 检查原样保留）。
- Windows 侧没有做「不同用户隔离」：`C:\ProgramData` 是机器级，同一台机器的不同用户会共享锁域。
  这比「不同用户各写各的」更保守（宁可误拒也不误放），已在此注明。
- `UnlockFileEx` 没有显式调用：句柄关闭即释放，显式解锁只是多一处可失败路径。
