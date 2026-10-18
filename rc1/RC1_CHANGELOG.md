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
