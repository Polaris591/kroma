# RC1 收口变更（仅本次）

## 格式

- `cargo fmt --all`：11 个文件、105 处差异 → 0。**纯格式化**，无逻辑变化。
  原因：`ci.yml` 把全树 fmt 当硬门禁，而此前从未真正通过。

## 供应链（RC1-04）

- 新增 `scripts/fetch-openssl.sh`：pin 空/格式错 → 退出；摘要不符 → 退出并打印实际摘要；
  只有校验通过才解包。
- `ci.yml` 与 `release.yml` 的**四条** OpenSSL 下载路径全部改为调用该脚本
  （此前只有 release/android 一处验真，其余直接 `tar -xzf`）。
- `ci.yml` 增加 `OPENSSL_SHA256` 声明（空值 → fail-closed）；两处缓存键加入 pin。
- 新增 `release-gate.js openssl` + 2 条测试。
- **未**填入摘要值（出网不可用，见 KNOWN_LIMITATIONS L2）。

## 发布 DAG（RC1-03）

- `release.yml` 新增 `quality` job：fmt、clippy、全量 Rust 测试、4 个 Node 套件、3 个 gate 脚本、
  双入口逐字节比较。
- `pack` 的 `needs` 加入 `quality`。
- 新增 `release-gate.js dag`：解析 job 图，pack/sign/publish 未依赖 quality 即失败；
  出现无人工条件的 publish job 也失败。
- 工具链安装用仓库自己的 rustup 配方，不引入新的第三方 action。
- **未**实跑（无 runner），状态 `PARTIAL`。

## 默认策略

未变：`SingleCandidate`。T24 仍 `INCONCLUSIVE`，没有因为"看起来更好"改默认值。

## 第二轮（CI 真跑后暴露并修掉的五个问题）

| # | 问题 | 证据 | 修法 |
| --- | --- | --- | --- |
| 1 | 两个 workflow 被 GitHub 判无效（每次 run 0 job） | `actionlint` 指出 job 级 `env` 用了 `${{ runner.temp }}` | 改 `${{ github.workspace }}/.zig-cache` |
| 2 | musl 与 macOS 编译失败 | `cannot find type __rlimit_resource_t in crate libc`（本机用 CI 配方复现） | 按 `target_env` 选 rlimit 资源类型 |
| 3 | android job 在第三方 setup action 就失败 | 步骤结论显示失败点在该 action | 改用 runner 预装 SDK + 自带 sdkmanager 装 NDK |
| 4 | 测试 job 缺 `bubblewrap` | t09 断言真隔离，缺工具即失败（设计如此） | CI 装 bwrap，不削弱测试 |
| 5 | `CI=true` 泄漏进子进程，测试只在 CI 红 | 失败日志 `BASH_OUT\|…\|CI=true`（应为 `CI=unset`） | 把 `CI`/`CONTINUOUS_INTEGRATION`/`NO_COLOR`/`FORCE_COLOR` 加入清理黑名单 |

每条都有「改之前会红、改之后会绿」的对照：第 2 条本机复现，第 5 条用变异验证（删掉 `CI` 那行即回红），
第 1 条用 actionlint 前后对比。
