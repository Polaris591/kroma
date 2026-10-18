# 已知限制（RC1）

每条都写明"没做什么、为什么、怎么补"，不写成已解决。

## L1 平台：四类真实运行证据缺失

| 平台 | 现状 | 补法 |
| --- | --- | --- |
| Windows x64 | 仅 mingw 交叉编译通过；T08 的多进程锁测试**未在 Windows 原生跑过** | `windows-latest` 上跑 `cargo test -p kroma-core --lib session_ownership --target x86_64-pc-windows-msvc` |
| macOS arm64/x64 | 无 SDK，无法构建/链接/启动 | 需要 macOS runner |
| linux arm64 musl | 历史有 qemu+TLS 证据，本轮未复跑 | 本机可复跑：qemu-aarch64 + 真实 HTTPS 请求 |
| Android arm64 | 仅构建与链接桩；无设备/模拟器 | 需模拟器或真机做 HTTPS 与工作区约束验证 |

## L2 供应链：OpenSSL 摘要未钉住

四条下载路径已统一验真，但 `OPENSSL_SHA256` 为空 → 现在 CI/release 只要涉及 OpenSSL 就会**失败关闭**。
本机出网到 openssl.org 与 GitHub releases 均不可用，无法独立获取并核对摘要，按任务书不编造。

## L3 Release DAG 只有结构证据

`quality` job 与 `pack needs` 已接线并有负向测试，但**没有真实 Actions 运行记录**。
无 runner 权限 ⇒ 状态保持 `PARTIAL`。

## L4 签名层次

- 已有：客户端单目标升级 manifest 的真实签名与生产根验签（T21，13/13）。
- 缺：`artifacts[]` 层（产物集合）与最终发布产物的签名，需要先冻结最终提交再签。
- 私钥在 `/root/kroma-release/kroma-release-key.pem`（600）。**未**进仓库/日志/CI；离线备份由所有者负责。

## L5 Node late-open 竞态未复现

`regressions.md §2.3` 的 `ENOENT lstat(...tmp...)` 现象与测试用例位置已核对存在，但本轮**没有**做
100 次定向复现。修复前必须有确定性事件轨迹，不能用"加大超时"或"吞 ENOENT"糊过去。

## L6 历史文档

`T21/T22/T30` 的主文含当时的 `BLOCKED` 描述，属真实历史，**未改写**；最新状态只在
`KROMA_RC1_ACCEPTANCE.md` 维护。

## L7 已存在的 flake

`b05_execution_context::run_deadline_clamps_mcp_timeout`、
`agent_runner_real_toolchain_clamps_run_deadline`、`b08c_git_metadata`（需 `--test-threads=1`）、
`mcp::call_lifetime_tests::subsequent_call_fails_closed_when_poisoned`、
`session_ownership::tests::different_workspaces...`。
本轮未给 CI 加 `--test-threads=1` 充当根治；这些用例单独复跑均绿。

## L8 实验统计

T24 只有 2 个留出任务簇（12 次运行）。要判"锦标赛是否有用"需要更多任务簇，加重复次数无用。
