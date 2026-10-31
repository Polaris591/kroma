# 附加任务 A：Windows 上的 `nul` 文件（2026-11-01）

## 1. 复现（先验证假设）

环境：`MINGW64_NT-10.0-26200`，Git Bash（`/usr/bin/bash`），工作目录为新建临时目录。

```
$ cd <tmp> && /usr/bin/bash -c 'echo hi 2>nul'
hi
$ find . -maxdepth 1 -type f -printf '%f %s bytes\n'
nul 0 bytes
```

**复现成功**：Git Bash 不把 `nul` 当 Windows 设备名，而是创建同名普通文件。stdout 仍打印 `hi`（`echo` 写的是 stdout），stderr 重定向落到 `nul` 文件，所以这次是 0 字节。

仓库旁的真实文件也印证同一机制：

```
$ od -c D:/Kroma/nul
/usr/bin/bash: line 1: cd: too many arguments\n   （46 字节）
```

即模型曾执行带 `2>nul` 的命令，而 stderr 恰好是 `cd` 的错误文本（`cd` 参数含空格且未加引号，被拆成多个参数），错误被写进了名为 `nul` 的文件。

## 2. 改动

1. **工具描述（`src/shell.rs::tool_description`，仅 Bash 方言）**：明确 `Discard output with /dev/null, never nul.`。PowerShell / cmd / POSIX sh 的描述不变。
2. **命令改写（`src/shell.rs` + 新增 `src/shell_redirect.rs`）**：仅 `#[cfg(windows)]` 且 `Dialect::Bash` 时，在 `ShellChoice::command_text` 入口改写命令；其它方言/平台完全不变。
   - 匹配：`>nul`、`2>nul`、`>>nul`、`&>nul`、`> nul`（重定向前允许空格），大小写不敏感，**整词**（`nul.txt`、`nuldir` 不动）。
   - 不改：单/双引号或反引号内（`echo "2>nul"`、`echo '2>nul'`、`` echo `echo 2>nul` ``、`echo >'nul'`）、转义字符、`#` 注释之后、heredoc 正文（`<<'END'`、`<<-NEXT`，含 Tab 缩进的结束符）。
   - 输出：`/dev/null`，其余字节与顺序原样保留。
3. **测试**：
   - `shell::redirect::tests::rewrites_only_whole_unquoted_redirect_targets`：`2>NUL`、`>>NuL`、`&>nul`、`> nul` 被改写；`echo "2>nul"`、`echo '2>nul'`、`cat nul.txt`、`>nul.txt`、`>'nul'`、注释、反引号内不变。
   - `heredoc_bodies_and_quoted_delimiters_are_untouched`：两个 heredoc + 后续行；带引号 heredoc 里的 `2>nul` 保持原样。
   - `shell::tests::windows_bash_nul_redirect_in_spaced_workdir_does_not_create_a_file`（Windows、Bash 才运行）：**工作目录含空格**，执行 `echo hi 2>NUL` 后目录里不出现 `nul`，stdout 仍为 `hi`；`echo "2>nul"` 的引号内容保持原样。
   - 既有 `shell::tests::*`（工作目录、退出码、取消、PowerShell 中文/退出码）全部通过。

## 3. 工作目录 `cd` 前缀检查

`shell.rs` 启动命令用 `Command::current_dir(workdir)`，**没有**给命令加 `cd <dir> &&` 前缀，因此 Kroma 自身不会产生 `cd: too many arguments`；该错误来自模型自己写的 `cd` 命令（路径含空格未加引号）。新增的含空格工作目录端到端测试通过，确认 Kroma 的工作目录处理在空格路径下正确。若要进一步降低模型此类错误，可在 Bash 工具描述里补一句"路径含空格时加引号"，但这属于提示词范围，本任务未改。

## 4. 清理命令

`nul` 是 Windows 保留设备名，资源管理器无法删除/重命名，需用扩展路径前缀：

```
del "\?\D:\Kroma\nul"
```

（cmd.exe；PowerShell 可用 `cmd /c del "\?\D:\Kroma\nul"`。）本报告只给出命令，未删除该文件；删除属于破坏性操作，建议确认无其他用途后执行。

## 5. 剩余风险与边界

- 改写器是**词法过滤，不是 shell 解析器**：能覆盖常见的重定向写法与引号/heredoc/注释，但 `{nul}`、`nul` 作为变量展开结果（如 `>${TARGET}`）等动态形式不会被改写；这类情况保持原行为（可能仍生成文件），不会误改其它命令。
- 只对 **Windows + Git Bash** 生效；cmd/PowerShell 的 `nul` 是真正的空设备，不改写；Unix 上 `nul` 本就是普通文件名，符合预期。
- 改写不改变命令语义（仅把重定向目标换成 `/dev/null`），也不影响退出码、pipefail、捕获文件与超时策略。
- 未在真实模型会话里重放"模型写 `2>nul`"的完整链路；已有单元测试 + Windows 端到端 shell 测试覆盖。
