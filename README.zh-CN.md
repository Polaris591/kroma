# Kroma 中文说明

Kroma 是一个用 Rust 编写的、可观察的 AI 编码助手：提供终端界面（TUI）、行式命令行
（line CLI）、JSONL 事件、MCP 工具、Agent Skills 与可恢复的文件快照。MIT 许可。

**状态：CLI 与 TUI 公测（Public Beta），版本 0.4.0。** npm 包尚未发布，包名仍为
暂定；目前请使用源码构建。可选的 Desktop 预览版不包含在 npm 安装器中，也不是使用
Kroma 的前提。

Kroma 有意保持自主：它不会逐条命令请求批准。它以可见性、自动快照和显式回滚来替代
权限审批。请在运行前阅读[安全模型](docs/security.md)。英文主文档见
[README.md](README.md)。

## 前置条件

- 源码构建需要 Rust 1.85+；使用预编译 npm 包时最终用户不需要 Rust。
- Node.js 22 或更新版本（仅 npm 安装方式；不要使用 `--omit=optional`）。
- 一个可访问的模型端点与 API Key；本机 Ollama 端点也可以。
- Git：文件快照只在 Git 工作树内生效；未安装 Git 则无法使用快照。

npm 分发仅覆盖五个基线：Windows x64（MSVC）、glibc Linux x64（Ubuntu 22.04 /
glibc 2.35 基线）、glibc Linux arm64（Ubuntu 24.04 / glibc 2.39 基线）、macOS Intel
与 Apple Silicon。Alpine/musl、Windows ARM64、32 位系统不在支持范围内，可改用源码
构建。

## 快速开始

源码构建：

```bash
cargo build --release --locked
./target/release/kroma
```

首次启动若未配置模型，会进入引导式设置：选择服务商、选择或输入模型 ID、隐藏输入
密钥、可选地测试连接，确认后才保存。之后可用 `kroma setup` 重新运行。TUI 支持中英文
界面：`kroma --language zh`（或设置 `KROMA_LANG=zh`）。

非交互方式：

```bash
export KROMA_MODEL=deepseek-chat
export KROMA_BASE_URL=https://api.deepseek.com/v1
export KROMA_API_KEY=...
kroma --print "总结这个仓库"
kroma --json "说明这个仓库"
```

常用命令：

```text
kroma                        交互式 TUI，新会话
kroma [TASK]                 行式 CLI 执行一个任务
kroma --print TASK           一次性对话（默认新会话）
kroma --json TASK            JSONL 事件，隐含 --print
kroma --continue             继续最近的会话
kroma --session NAME TASK    使用或创建命名会话
kroma setup                  引导式模型设置
kroma update [--check]       手动更新，或仅检查新版本
kroma help [COMMAND]         帮助
```

完整参数见[命令行参考](docs/reference/cli.md)，首次运行流程见
[docs/quickstart.md](docs/quickstart.md)（英文）。

## 更新

更新完全手动，Kroma 不会在启动时检查更新，也没有后台轮询或自动重启：

```text
kroma update --check                 只读检查，打印当前渠道可用版本
kroma update                         更新到 latest 渠道（默认）
kroma update --channel next          更新到 next 预发布渠道（存在时）
```

`--check` 只通过 HTTPS 读取官方 npm registry，不做任何修改。通过 npm 安装时，更新由
npm 启动器在原生可执行文件启动前完成，使用你的 npm 包管理器和官方 HTTPS registry；
全局 npm 安装仍更新到原全局位置；本地直接依赖更新其所属项目的 package.json/锁文件，
不会随意更新当前目录。链接安装、间接依赖或其他包管理器的安装需手动处理；它不扫描
PATH，也不删除其他安装。缺失的平台包可以通过 npm 修复，成功后检查主包及平台包版本。
源码/Cargo 安装只给手动指引，不会自我覆盖。配置、会话和快照不会被改写。
默认渠道为 latest；仅发布在 next 的 Beta 应显式使用 `--channel next`。

## 安全边界（重要）

- Kroma 以你的用户权限执行命令和写入文件，**不会逐条命令请求批准**。
- 它**不是沙箱**。需要隔离时请使用容器、虚拟机或一次性工作树。
- Git 工作树快照只能恢复工作树内、未被 `.gitignore` 忽略的文件。它无法撤销
  `git push`、数据库写入、网络请求、已发送或打印的密钥，以及工作树之外的改动。
- 任务描述里的“不要修改文件”之类文字不构成权限限制。

完整说明：[docs/security.md](docs/security.md)；漏洞报告：[SECURITY.md](SECURITY.md)。

## npm 分发与链接

npm 包（`kroma` 与五个 `kroma-<platform>`）目前均未发布，名称在所有者确认前为暂定。
维护者的手动发布流程尚未执行，因此没有可用的 `npm install kroma` 结果。打包、CI
基线与本地检查见[docs/npm-distribution.md](docs/npm-distribution.md)，发布清单见
[docs/releasing.md](docs/releasing.md)。可选 Desktop 预览不属于 npm 安装器。

- 英文说明：[README.md](README.md)
- 首次运行：[docs/quickstart.md](docs/quickstart.md)
- 架构与生命周期：[docs/architecture.md](docs/architecture.md)、[docs/lifecycle.md](docs/lifecycle.md)
- 许可：MIT，见 [LICENSE](LICENSE)

## 反馈

请通过 <https://github.com/Polaris591/kroma> 的 issue 反馈问题；不要在 issue、日志或
截图中粘贴 API Key。安全漏洞请按 [SECURITY.md](SECURITY.md) 私下报告。
