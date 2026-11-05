# linux.do 发帖草稿（DRAFT）

> **这是草稿，尚未发布。** 发布前必须由仓库所有者确认包名、发布状态和下列数字/
> 说法，并先完成 [releasing.md](releasing.md) 中的门槛。不要使用本文件中的任何
> 内容作为“已被采用”“性能提升”或“已发布 npm 包”的证据。

## 标题（备选）

1. Kroma：一个可观察、可回滚的 Rust 终端 AI 编码助手（公测）
2. 分享一个自用的 Rust AI agent harness：CLI/TUI、MCP、文件快照

## 正文草稿

最近把自己在用的一个终端 AI 编码助手 Kroma 整理到了可以公测的程度，来听听反馈。
它是 Rust 写的，MIT 许可，目前是 **CLI 与 TUI 的公测阶段（0.4.0）**，npm 包还没有
发布，暂时只能从源码构建。

项目地址：<https://github.com/Polaris591/kroma>

### 它是什么

- 终端 TUI + 行式 CLI，同一套运行时；另有可选的 Desktop 预览（不在 npm 安装器中，
  也不是使用前提）。
- `--print` / `--json` 适合脚本：默认新会话，可并行；需要续聊时显式 `--continue`
  或 `--session`。
- 文件快照：在 Git 工作树内，任务开始前把文件快照到 `~/.kroma/checkpoints/`，
  使用独立的私有 GIT_DIR 和 index，不碰你仓库的 `.git`、HEAD、index 或 hooks。
  `/checkpoints` 查看，`/restore-files <n>` 显式恢复（包括删除快照后新建的文件）。
- MCP 懒加载：启动时不拉起服务器，模型需要时才通过 `mcp_search` / `mcp_call`
  发现和调用，工具 schema 不提前占用上下文。
- 上下文压缩、prompt caching、provider 上报用量；Ctrl+C 会停止模型请求或 shell
  进程树，但不会假装已发生的副作用被撤销。

### 安全边界（先说清楚）

- **没有逐条命令审批**，命令以你的用户权限执行；**它不是沙箱**。
- 快照只覆盖工作树内、未被 `.gitignore` 忽略的文件；**不能撤销** `git push`、
  数据库写入、网络请求、已经发送或打印的密钥，以及工作树之外的改动。
- 任务描述里的“不要修改文件”不是权限限制。
- 建议在一次性工作树、容器或虚拟机里跑不信任的任务。细节见
  <https://github.com/Polaris591/kroma/blob/main/docs/security.md>。
- 只在手动执行 `kroma update` 时才访问更新服务，不做启动检查、不做后台轮询；源码
  构建不会被自我覆盖。

### 安装与更新（当前）

npm 包尚未发布，公测阶段请从源码构建（需要 Rust 1.85+）：

```bash
git clone https://github.com/Polaris591/kroma
cd kroma
cargo build --release --locked
./target/release/kroma
```

首次启动会进入引导式模型设置（需要可访问的模型端点；本机 Ollama 也可以）。文件
快照需要 Git。更新完全手动：先用 `kroma update --check` 查看渠道版本，再自行重新
构建（`cargo build --release --locked` 或 `cargo install`）；源码安装下 `kroma
update` 只打印手动升级命令，不会自我覆盖。

npm 预编译包只规划了五个基线：Windows x64（MSVC）、glibc Linux x64（Ubuntu 22.04 /
glibc 2.35 基线）、glibc Linux arm64（Ubuntu 24.04 / glibc 2.39 基线）、macOS Intel
与 Apple Silicon。Alpine/musl、Windows ARM64、32 位系统不支持。

### 已知限制（诚实版）

- 公测，CLI/TUI 优先；Desktop 只是可选预览。
- 五个平台基线是 CI 构建/安装目标，不代表所有更老的系统或 glibc 版本都验证过。
- 我的本机验证在 Windows 上完成；Linux/macOS 的 CI 结果尚未在此环境确认。Windows
  GNU 下 Desktop 库测试需要已知的资源 manifest 变通方法，标准测试命令并非无条件
  通过。
- 没有自动更新检查；npm 包名仍是暂定，发布流程尚未执行，因此没有真实的 registry
  安装结果。
- 我没有任何采用数据、用户反馈或端到端性能基准，所以不宣称速度提升。仓库里的
  planner 微基准只是一个受限压缩场景的本地测量，**不是**任务整体提速。

### 适合谁 / 不适合谁

适合：想要可见、可回滚、可脚本化的终端 agent，且愿意在容器/一次性工作树里跑任务的
人。不适合：需要权限审批、强沙箱隔离，或必须使用 Alpine/musl、Windows ARM64 的
环境。

欢迎 issue 反馈，尤其是安装问题、TUI 交互和 MCP/插件边界。不要在 issue 或截图里粘贴
API Key；安全问题请按 <https://github.com/Polaris591/kroma/blob/main/SECURITY.md>
私下报告。

## 发布前检查（维护者）

- [ ] 所有者已确认包名与仓库归属，且本文件中的“未发布/暂定”表述与事实一致。
- [ ] 若 npm 已发布，改写“安装与更新”一节并附上真实的 clean install / update
      验证记录；不要沿用草稿里的占位说法。
- [ ] 重跑真实 CI（五平台）并在发帖前查看结果；不要声称未观察到的通过。
- [ ] 保持安全边界段落完整，不要为了观感删减。
- [ ] 删除或改写任何无法复现的数字；不得把本地微基准写成任务收益。
- [ ] 不涉及 Desktop 发布承诺，不承诺响应时间。
