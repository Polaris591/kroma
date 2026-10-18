# CI 与跨平台矩阵（五层证据）

规则：**交叉编译不能冒充原生运行**。每一层只有两种合法取值：`PASS`（有真实证据 + 链接）或
`NOT_RUN`（写明缺什么、怎么解除）。`BLOCKED` 只用于缺外部设施。

## 1. CI 运行记录（真实 run）

| run | 结论 | 说明 |
| --- | --- | --- |
| [39090210718](https://github.com/Polaris591/kroma/actions/runs/39090210718) | failure（0 job） | workflow 被 GitHub 判无效：job 级 `env` 用了 `runner.temp` |
| [39091293298](https://github.com/Polaris591/kroma/actions/runs/39091293298) | failure | 首次真正跑起来：linux/windows 绿，musl/android/macOS 红 |
| [39092543677](https://github.com/Polaris591/kroma/actions/runs/39092543677) | failure | **五个平台构建全绿**；`cargo test` 红（缺 bwrap） |
| [39092921914](https://github.com/Polaris591/kroma/actions/runs/39092921914) | failure | 装了 bwrap 后 **五个平台仍全绿**，`cargo test` 仍红 → 另有原因，待定位 |

## 2. 逐平台五层

| 平台 | 编译 | 链接 | 启动 | TLS | 安全边界 |
| --- | --- | --- | --- | --- | --- |
| linux-x86_64 (gnu) | **PASS** 本机原生 `cargo build --release`；CI `linux-gnu` 绿 | **PASS** 动态链接 glibc/OpenSSL 系统库 | **PASS** `kroma --version` → `kroma 1.5.0` | **NOT_RUN** 本机无出网；CI 只构建不联外网 | **PASS** bwrap 真隔离 + docker 容器测试在本机全过 |
| linux-arm64 (musl) | **PASS** 本机 + CI 均绿（修掉 `__rlimit_resource_t` 后） | **PASS** 纯静态，含真 OpenSSL（`strings` 命中 `OpenSSL 3`） | **PASS** `qemu-aarch64 kroma --version` → `kroma 1.5.0` | **NOT_RUN** 无出网；历史上曾用 qemu + 真 HTTPS 验过，本轮未复跑 | **NOT_RUN** 未在 arm64 上跑沙箱测试 |
| windows-x86_64 (gnu) | **PASS** mingw 交叉构建 + CI `windows-gnu` 绿 | **PASS** PE32+，导入表无 libssl（用 Windows 原生加密） | **NOT_RUN** 无 Windows runner/机器 | **NOT_RUN** 同上 | **NOT_RUN** 同上 |
| macOS arm64 / x64 | **PASS** CI `macos-latest` 原生构建两个 target | **PASS** 链接 Security.framework/CoreFoundation（真 SDK） | **PASS** CI 里执行 `--version` 两个产物 | **NOT_RUN** CI 未联外网 | **NOT_RUN** 未在 macOS 上跑沙箱测试 |
| android-arm64 | **PASS** CI 用 runner 预装 SDK + NDK 构建 | **PASS** 静态链接真 OpenSSL（CI 用 llvm-readelf 断言无 libssl NEEDED） | **NOT_RUN** 无设备/模拟器 | **NOT_RUN** 无设备 | **NOT_RUN** 无设备 |

## 3. 每个 NOT_RUN 的解除条件

| 项 | 缺什么 | 解除方式 |
| --- | --- | --- |
| linux-x64 TLS | 出网 | 在能出网的机器上跑 `KROMA_*` 指向真实 provider，执行一次真实请求 + 一次自签证书拒绝 |
| musl TLS | 出网 + qemu | `qemu-aarch64` 跑二进制，对真实 https 端点发请求；对自签证书断言拒绝 |
| musl 安全边界 | arm64 机器或 qemu+bwrap | qemu 下跑 t09 子集（bwrap 在 qemu 用户态受限，需真机更稳） |
| windows 启动/TLS/安全 | Windows runner | 加 `windows-latest` job，跑 `cargo test -p kroma-core --lib session_ownership --target x86_64-pc-windows-msvc` 与一次 HTTPS |
| macOS TLS/安全 | macOS runner + 出网 | 在 macos-latest 上加一步真实 HTTPS 与自签拒绝 |
| android 运行 | 模拟器/真机 | 用 `reactivecircus/android-emulator-runner` 或真机跑 `kroma --version` 与 HTTPS |

## 4. 结论

- **构建层面五个平台全部真实通过**（不是 check，是 `cargo build` + 链接 + 可执行断言）。
- **运行层面只有 linux-x64 与 macOS 有启动证据**；TLS 与安全边界在 CI 里都未跑，标 `NOT_RUN`。
- 因此平台声明只能是「linux-x64 可运行；其余平台构建可用、运行未验证」，
  不允许写成「五平台发布就绪」。
