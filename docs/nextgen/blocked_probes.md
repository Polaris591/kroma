# 外部阻塞项的独立探测证据

本文件记录无法在本机完成的验收项，以及证明它们确实无法完成的探测命令与输出。
原则：缺能力就标 BLOCKED 并附证据，不折算成 PASS，也不把"没做"混进"未验证"。

探测环境：Linux VM-0-2-debian 6.12.95+deb13-amd64 x86_64。

## KR-T02 所有目标平台的真实发行物构建与运行

**已真实完成的部分（构建 + 链接）：**

```
$ cargo build --workspace --locked --target x86_64-pc-windows-gnu
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 39.79s

$ file target/x86_64-pc-windows-gnu/debug/kroma.exe
target/x86_64-pc-windows-gnu/debug/kroma.exe: PE32+ executable for MS Windows 5.02 (console), x86-64, 22 sections
```

链接器：`/usr/bin/x86_64-w64-mingw32-gcc`。产物 223 MB（debug，未 strip）。

**做不到的部分（运行）：**

```
$ rustup target list --installed
aarch64-apple-darwin
aarch64-linux-android
aarch64-unknown-linux-gnu
aarch64-unknown-linux-musl
x86_64-apple-darwin
x86_64-pc-windows-gnu
x86_64-unknown-linux-gnu

$ command -v osxcross || command -v o64-clang || echo absent
absent

$ cargo build --workspace --locked --target aarch64-unknown-linux-gnu
collect2: error: ld returned 1 exit status
error: could not compile `kroma-cli` (bin "kroma") due to 1 previous error
```

结论：Windows 交叉构建与链接**有真实证据**；但没有 Windows/macOS 真机，macOS 也没有 SDK
与链接器，ARM Linux 缺交叉链接器。任务卡明确"交叉编译不能冒充设备运行"，所以
"运行"这一格无法在本机产生证据。

因此 KR-T02 保持 BLOCKED，blocker 写的是运行证据缺失，而不是构建缺失——构建这部分是真的做过了。

## KR-Q06 独立安全复核与发布前攻击回归

```
$ for c in semgrep cargo-audit cargo-deny; do command -v $c || echo "$c absent"; done
semgrep absent
cargo-audit absent
cargo-deny absent
```

结论：本机没有第三方安全复核工具链，也没有独立评审方。我自己跑自己的攻击回归不构成独立
复核。任务卡要求"无法独立执行时保持待验收，不自我认证生产安全"。

## KR-S06 灰度、默认晋升与自动回退

```
$ ls /etc/kroma /var/lib/kroma 2>/dev/null || echo "no deployment present"
no deployment present
```

结论：本机没有 Kroma 的部署实例（无 systemd 单元、无生产配置目录），不存在可做灰度的用户群。
上游证据链（KR-S03/S04、KR-R06、KR-Q06）也未完成，因此灰度与自动回退无法验证。

## KR-T04 独立最终验收

依赖 KR-A03 / KR-Q06 / KR-T02 / KR-T03 / KR-F06。Q06、T02 外部阻塞（T03 现已 VERIFIED，此处描述的是当时状态），
F06 的负实验登记已有真实条目（candidate-on-demand-tools 因 dev/holdout 方向相反不晋升）。任务卡要求"所有 mandatory 适用门禁 VERIFIED 才可称发布候选验收通过；
仍有 blocked 必须醒目标出"——当前仍有 blocked，故不能通过，只能如实标出。

## KR-R01 资源基线（已修正误判）

p0（`kroma --version`，release，三次重复）：wall 4.197ms 中位数（4.384/4.197/3.391），树 PSS 2.5–3.1MB。

p2/p3/p4 曾经在 60 秒超时，我一度记成「ACP 的 workspace.graph 挂住」。**那是误判。** 真实原因是
ACP 在没有 owner spend envelope 时 fail-closed 退出（KR-E04 引入的安全行为），而测量脚本是那之前
写的、没传 `--max-tokens`，于是子进程立刻死掉，探测端读成了「没有响应」。

修好脚本参数后，同一台机器上：

```
p2  graph 397.6ms 中位数 / init 6.7ms / 树 PSS 62–65MB   (本仓 1200 文件，release)
p3  graph 33.968ms 中位数 / 树 PSS 15.5MB            (生成的小树, release; debug 曾测得 245ms/25.4MB)
p4  4 个并发 acp 会话，树 PSS 228.9MB 中位数（228.0–234.6，release）
```

全部 `exit_zero=True, timed_out=False`。源码没有改动，改的是 `scripts/measure-local-baseline.py`。
所以这一项从「本机可复现的缺陷」变成了「可复现的基线」，不再需要单独调查。
