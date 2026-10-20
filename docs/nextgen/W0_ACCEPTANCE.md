# W0 验收报告：固定基线与任务账本

日期：2026-10-21
覆盖任务：KR-A01、KR-A02、KR-A05
基线提交：`61ffcc30c5783e443275abf0399468aa27d542c8`（560 文件，未改动任何源码）
W0 提交：`6c583d9`（只新增 docs/nextgen/ 与 4 个脚本）

## 一、这一轮实际做了什么

1. 把交付源码就地纳入 git，基线提交锁在 61ffcc3，工作区干净。
2. 产出三份机器可读账本：baseline.json、host_capabilities.json、coverage_ledger.json。
3. 初始化 120 项任务账本 progress.json，3 项 DONE、117 项 TODO。
4. 4 个校验脚本都带负向用例，且都实测跑过。

## 二、实测证据（本机重跑，不是引用包内日志）

| 项目 | 结果 | 日志 |
|---|---|---|
| cargo test --workspace --locked --no-fail-fast | 108 harness，1281 通过 / 0 失败 / 0 ignored | evidence/w0-workspace-test.log |
| scripts/run-sandbox-tests.sh | 7 个 target，56 通过 / 0 失败（真 bwrap 隔离） | evidence/w0-sandbox.log |
| scripts/nextgen_rust_gate.sh | 完整跑通，exit 0，末行 RUST OFFLINE GATES PASSED | evidence/w0-nextgen-gate.log |
| 门禁 [6] 全量回归 | 1281 通过 / 0 失败 / 0 ignored | 同上 |

工具链 Rust/Cargo 1.85.1，与 rust-toolchain.toml 一致。

### 与交付包记录的差异

交付包记录为 1231 通过 / 49 失败，并把 49 项归因为宿主能力不足。本轮在具备
bwrap 0.12.0、user namespace、AF_UNIX 和一致 /proc 的宿主上重跑，49 项全部通过，
未删除测试、未添加 ignore、未放宽隔离。

因此包内 KR-A01 的 current_state「全 Rust 非全绿」已过时，本轮以实测证据替换。

## 三、宿主能力（KR-A02）

- bwrap 0.12.0，`--unshare-all` 命名空间探测成功
- `unshare -Urn` 成功
- AF_UNIX bind 成功
- /proc/self/stat 的 PID 与 getpid() 一致

两个 tier 明确记为 BLOCKED，不是 PASS：

- cross_platform：单台 Linux 永远不能证明 Windows/macOS 行为
- paid_network：本轮未授权任何真实 provider 花费

负向用例（都实测过）：

- 把 bwrap 从 PATH 移除 → run-sandbox-tests.sh exit 2，输出「bwrap is not installed」
- bwrap 存在但命名空间被拒 → exit 2，并原样输出 bwrap 的 stderr
- 结论：能力缺失不会被折算成 PASS

## 四、覆盖账本（KR-A05）

565 个文件全部登记 sha256 与模块归属；107 个生产 Rust 模块映射到 epic，
未映射数为 0。64 个测试 target 全部在源码中真实存在。

四态严格区分：implemented / wired / executed / measured。wired 一律为 null，
只有验证过调用路径才会置真；measured 全为 false。

O（插件生命周期）和 Q（提示注入）当前没有任何独立生产模块，epic 覆盖计数为 0。
这不是漏映射，而是真实缺口：这两个 epic 需要从零建立契约与实现。

负向用例：临时加一个无映射模块 zz_unmapped_probe.rs，账本 exit 1、gate_passed=false；
删除后回到 exit 0。

## 五、回滚

W0 全部是新增文件，不改任何交付源码。回滚方式：`git reset --hard 61ffcc3`，
或只删 docs/nextgen/ 与 4 个 nextgen_*.py 脚本。基线本身不受影响。

## 六、本轮未做（不属于 W0）

- A03/A04/A06 及 B 组之后的全部任务：117 项仍为 TODO
- 真实 provider canary、外部 MCP 互操作、付费 A/B：记为 BLOCKED，留待 W3
- 跨平台构建：本机无法证明
