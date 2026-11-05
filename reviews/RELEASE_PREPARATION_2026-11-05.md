# 首发收口：更新入口、npm/GitHub 发布准备

日期：2026-11-05。保留前四批和用户已有工作；未提交、打 tag、推送、切换仓库可见性、向 npm 发布或向 linux.do 发帖。版本仍为 0.4.0，首发版本/名称/账号归属等待维护者确认。

## 已完成

- `kroma update --check [--channel latest|next]`：仅显式命令访问官方 HTTPS npm registry；无启动检查、后台轮询或自动重启。10 秒请求上限、响应大小限制、重定向限制和版本/渠道检查；预发布不进入默认 latest。
- npm `kroma update`：启动器在原生程序启动前处理更新，通过 Node 执行 npm CLI，不使用 shell 拼接命令。固定目标版本和 registry，关闭 lifecycle scripts，保留 optional dependencies。全局更新仍在原 prefix，本地更新所属项目并可能改其 package.json/lockfile；拒绝链接、间接依赖和其他包管理器的安装。成功前核对主包、平台包版本。
- Cargo/源码版不会自我覆盖；`update` 打印原安装方式的手动指引，`--check` 才联网。不会扫描 PATH、删除其他安装、使用 sudo 或结束其他会话。Windows 其他 Kroma 进程仍需用户先关闭；npm 非原子更新失败时应检查输出并重装，不承诺自动回滚。
- `npm.yml` 保留只读构建测试，支持复用。新 `release.yml` 仅手动触发，默认 `publish=false`、`channel=next`；校验 tag、Cargo 版本和 workflow/source SHA，构建同一提交的五平台产物并运行核心检查。
- `npm/release.cjs` 验证六个包、版本、平台、文件白名单、主包一致性和可执行位，生成 manifest/SHA256SUMS。发布时再次校验批准的来源/渠道，平台包先于主包，失败立即停下，不盲目重试。
- 发布 job 使用 Node 24/npm 11 OIDC/provenance，要求显式归属确认、公开仓库、npm-release 环境 required reviewers、远端 tag 未移动；随后创建 GitHub release 并附原始 tarball/checksum。普通 push/tag 不会发布。
- 补充中文 README、SECURITY、首发清单和 linux.do 草稿，修正发布/更新状态及渠道说明。Beta 仅在 next 时使用 `kroma@next` 与 `--channel next`，不假定存在 latest。

## 验证证据

| 检查 | 结果 |
| --- | --- |
| `cargo test --locked --tests` | 749 passed，10 ignored，0 failed |
| 新增 release_workflow 测试，单独执行 | 1 passed；验证 YAML 结构、默认门禁和来源传递 |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过 |
| Core/Desktop fmt、git diff --check | 通过 |
| `npm --prefix npm test` | 24 passed，1 Windows 平台跳过；NUL 源码转义后另复跑 release 3 项通过 |
| Windows GNU release binary build | 通过 |
| npm stage 与真实隔离安装 smoke | 通过；本地/全局安装、shim、更新修复、卸载、缺失 optional 的错误提示 |
| Desktop `cargo check --locked` | 通过 |
| 文档本地目标与代码块 | 34 个本地目标通过；代码块闭合 |
| 启发式凭据扫描 | 936 个工作区文本文件、165,443 行 Git 历史 diff，常见模式无匹配；跳过 7 个二进制图标 |

原始日志在 [release-prep](release-prep/core-tests.txt)，另见 [npm tests](release-prep/npm-tests.txt)、[安装 smoke](release-prep/install-smoke.txt)、[凭据扫描](release-prep/credential-scan.json)。本机打包产物位于 `target/npm-release-prep/tarballs/`，只是未提交工作区的本机验证产物，不是已批准发布的五平台 release。

更新 smoke 使用本地 registry 和隔离 prefix/cache/config，通过实际 npm 安装修复模拟过期版本元数据。它验证安装器路径，不代表从真实历史发行版或公共 registry 的迁移已经验证。未把 npm 的 exit 0 单独视为更新成功。

## 真正对外发布前仍需完成

1. 人工审查当前工作区、review/probe 和历史；启发式扫描不能替代完整凭据/隐私/授权审计。审核后选择要提交的文件，提交代码，再创建不可移动的版本 tag。
2. 确认六个 npm 名称与所有者；必要的首次名称注册/bootstrap 由账号所有者完成。配置六个包的 trusted publisher：`release.yml`、`npm-release`，并允许直接 npm publish；新配置可能默认仅允许 stage。
3. 审核后公开 GitHub 仓库，启用私密漏洞反馈与 release 环境审批。先运行 `publish=false`；实际观测五平台 CI 和终端取消/交互结果。本机没有替 Linux/macOS 或 Windows MSVC 声称通过。
4. 人工批准发布后，从真实 npm registry 做干净安装、检查/升级和卸载；检查 provenance、dist-tags 及 GitHub 附件，再发布社区介绍。首版无历史版本时，不伪造历史迁移验证结果。

具体操作见 [发布清单](../docs/releasing.md)；[linux.do 草稿](../docs/promotion-linux-do.md) 尚未发出。npm OIDC 要求依据 [官方 trusted publishing 文档](https://docs.npmjs.com/trusted-publishers/)，并非仅凭配置即可认定已授权。

并行协作超时后由主会话接管：文档存在部分编辑，发布实现没有生成；最终代码和验证均由主会话补齐，没有把子任务超时当作成功。未新增框架；Rust 新增轻量 semver 依赖用于正确比较语义版本，保留 Agent Loop 和任务执行策略。
