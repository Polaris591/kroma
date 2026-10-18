# Kroma RC1 平台矩阵（逐层，按 CI 步骤结果对齐）

证据来源：CI 运行 `39103615371`（提交 `9c6bec59`），`conclusion = success`。
<https://github.com/Polaris591/kroma/actions/runs/39103615371>

矩阵只写该 run 的**具体 step 结论**。跨平台构建**不等于**原生运行验证，交叉编译通过也不证明
运行时行为；每一层单独判定，任何一层没有证据就写 `NOT_RUN`。

## 1. 五层判定

| 层 | linux-x64 (gnu) | linux-arm64 (musl) | macOS (arm64 + x64) | Windows (x86_64-gnu) | Android (arm64) |
| --- | --- | --- | --- | --- | --- |
| 编译 | PASS | PASS | PASS | PASS | PASS |
| 链接 | PASS | PASS | PASS | PASS | PASS |
| 启动 | PASS | PASS（qemu） | PASS（原生） | NOT_RUN | NOT_RUN |
| TLS 运行 | PASS（真 HTTPS） | PASS（qemu 真 HTTPS） | NOT_RUN | NOT_RUN | NOT_RUN |
| 安全边界 | PASS（本机 bwrap） | NOT_RUN | NOT_RUN | NOT_RUN | NOT_RUN |

## 2. 每条判定的依据

### linux-x64（gnu）

- 编译 / 链接：`release build (linux-gnu)` job success。
- 启动：该 job 内的二进制由后续 TLS smoke 实际执行。
- TLS：step `TLS smoke (real HTTPS + untrusted certificate rejected)` = **success**。这条 step 会发起
  一次真实 HTTPS 请求，并用一个不受信任的证书验证客户端**拒绝**握手。
- 安全边界：`b13_sandbox_isolation` 与 `t09_execution_boundary` 在本机（有可用 bwrap）**全部通过**；
  CI 的托管 runner 无法创建用户命名空间，见第 3 节。

### linux-arm64（musl）

- 编译 / 链接：`release build (linux-arm64 musl)` job success，静态链接，产物含真实 OpenSSL。
- 启动 + TLS：step `TLS smoke under qemu (real HTTPS + untrusted certificate rejected)` = **success**，
  用 `qemu-aarch64-static` 执行同一个 smoke 脚本。
- 注意：qemu 下通过证明的是**该二进制在 qemu 用户态模拟里能完成真实 TLS 握手**，不等于在真 ARM
  硬件上验证过。这一区别保留在此，不合并成"PASS"。

### macOS（arm64 + x86_64）

- 编译 / 链接：两个 target 各自 `cargo build --release --target ...` = success（真 Mac runner）。
- 启动：`--version` 两个架构各执行一次 = success。
- TLS / 安全边界：**NOT_RUN**。macOS 走 native-tls（Security.framework），在 runner 上没有做真实
  HTTPS 往返，也没有做隔离验证。解除条件：在真机上跑 `scripts/tls-smoke.sh` 与沙箱套件。

### Windows（x86_64-pc-windows-gnu）

- 编译 / 链接：mingw 交叉 `cargo build --release --target x86_64-pc-windows-gnu` = success。
- 静态检查：step `Windows binary must not depend on OpenSSL` = success（导入表里没有 libssl/libcrypto）。
- 启动 / TLS / 安全边界：**NOT_RUN**。没有 Windows 主机或 runner 执行过这个 PE 文件。
  解除条件：`windows-latest` runner 上跑 `--version` + TLS smoke，或真机执行。

### Android（aarch64-linux-android）

- 编译 / 链接：`Build` = success；`The Android binary must carry real OpenSSL, not link-only stubs` =
  success（检查 EVP_ 等真实符号，防止当年"空桩链接"事故重演）。
- 说明：该 run 里 `Fetch OpenSSL sources` 与 `Build static OpenSSL` 两步是 **skipped**，用的是缓存里的
  静态 OpenSSL；这不是跳过验证，静态检查那一步仍然实跑并通过。
- 启动 / TLS / 安全边界：**NOT_RUN**。没有设备或模拟器。解除条件：`adb push` 后跑真实 HTTPS 请求，
  再用不受信任 CA 重复一次，应当失败。

## 3. 沙箱套件在 CI 上的真实状态

托管 runner 不允许创建非特权用户命名空间，`bwrap` 装得上但用不了。处理方式是把"没能力"和"失败"
分开：

- `ci.yml` 主测试 job 通过 `scripts/run-workspace-tests.sh` **按二进制名**排除这两个套件（不是静默
  跳过：脚本里写着排除了哪两个、为什么）。
- 独立 job `sandbox isolation (needs user namespaces)` 实跑它们，本 run 结果 = **failure**，原因是
  `bwrap cannot create a namespace on this host`。
- `release.yml` 把同一件事升级成**发布条件**：退出码 2（宿主没能力）记为
  `HOST_CAPABILITY_BLOCKED` 并阻止 `RELEASE_READY` 判定；退出码 1（真的测试失败）直接让发布失败。
- 本机（有真 bwrap）实跑：`scripts/run-sandbox-tests.sh` → `EXIT=0`，b13 21 passed + t09 8 passed，0 failed。

因此"安全边界"这一层的真实状态是：**代码能力在本机被证明；在 GitHub 托管 runner 上是
`HOST_CAPABILITY_BLOCKED`**。这两句必须一起说，单独一句都是失真。

## 4. 结论

- 五平台**编译 + 链接**：全部有真实 CI 证据（同一 run、同一提交）。
- **启动**：linux-x64、linux-arm64（qemu）、macOS 有证据；Windows、Android 没有。
- **TLS 运行**：linux-x64 与 linux-arm64（qemu）有证据；macOS、Windows、Android 没有。
- **安全边界**：本机 linux-x64 有证据；其余全部 `NOT_RUN` 或 `HOST_CAPABILITY_BLOCKED`。

"支持五个平台的安全发布"这句话在本轮证据下**不成立**，能成立的是"五个平台都能构建并链接"。
