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

依赖 KR-A03 / KR-Q06 / KR-T02 / KR-T03 / KR-F06。Q06、T02 外部阻塞，T03 未完成，
F06 的负实验登记为空。任务卡要求"所有 mandatory 适用门禁 VERIFIED 才可称发布候选验收通过；
仍有 blocked 必须醒目标出"——当前仍有 blocked，故不能通过，只能如实标出。

## KR-R01 资源基线的 ACP 分支（本机可复现，不属外部阻塞）

p0（`kroma --version`）三次重复可复现：wall 4.1 / 4.3 / 61.2 ms，树 PSS 2.5–7.0 MB。

p2/p3/p4（`kroma acp` + `workspace.graph`）在 560 文件仓库上 60 秒仍无响应：

```
$ python3 scripts/measure-local-baseline.py --kroma ./target/debug/kroma \
      --out /opt/pi-qq/workspace/r01b --repo . --repeats 2 --timeout 60 --only p2
p2 repeats=2 exit_zero=False timed_out=True {"tree_peak_pss_kb": {"max": 5548, ...}}
```

这是本机可复现的现象：要么 ACP 的 `workspace.graph` 在中等仓库上过慢，要么测量驱动的握手
方式不对。两者都需要继续定位，因此该项保持 IMPLEMENTED_UNVERIFIED 并附本探测——它不缺外部
条件，所以不标 BLOCKED。
