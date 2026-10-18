# Kroma RC1 已知限制（当前状态，非历史文档）

本文件描述**当前这个交付包**的限制。每一条都对应一个可核对的证据或一条明确的解除路径；没有证据的
项目不写"已通过"。

## 1. 发布流水线尚未在 GitHub 上跑通

- `release.yml`（含 quality → 五平台构建 → pack → attest → release-verdict）**从未成功运行过**。
  它只由 `v*` tag 或 `workflow_dispatch` 触发，而本轮执行环境没有具备 `actions:write` 权限的令牌，
  无法触发；也不允许打 tag。
- 因此"Release DAG 真实跑通"这一条**没有证据**，状态是 `BLOCKED(需要令牌)`。
- 解除路径（任选一条）：
  1. 提供一个 `actions:write` + `contents:read` 的细粒度令牌，我用 `workflow_dispatch` 跑两次
     （pass 1 产出候选 + attestation，我离线签名，pass 2 验签出 verdict）；
  2. 所有者本人批准打一个**不发布的**预演 tag（例如 `v1.5.0-rc1-verify`），跑完即删。
- CI 的 `ci.yml` 是绿的（run `39103615371`，提交 `9c6bec59`），但那是 CI，**不是** Release。

## 2. 五平台只有"构建 + 链接"全覆盖

- 编译 / 链接：五个平台全部有真实 CI 证据。
- 启动：linux-x64、linux-arm64（qemu）、macOS 有；**Windows、Android 没有**。
- TLS 运行：linux-x64、linux-arm64（qemu）有；**macOS、Windows、Android 没有**。
- 安全边界：只有本机 linux-x64 有；CI 上是 `HOST_CAPABILITY_BLOCKED`（托管 runner 不给用户命名空间）。
- 详见 `CI_AND_PLATFORM_MATRIX.md`，每层单独判定。

## 3. 沙箱隔离在托管 runner 上跑不了

`bwrap` 在 GitHub 托管 runner 上装得上但创建不了命名空间（Ubuntu 24.04 的 AppArmor 限制 + 容器
seccomp）。处理方式是区分"没能力"与"失败"：本机 `scripts/run-sandbox-tests.sh` → `EXIT=0`（8 passed）；
CI 上记为 `HOST_CAPABILITY_BLOCKED` 并阻止 `RELEASE_READY`。**没有任何一处把这个状态写成 PASS。**

## 4. 签名覆盖的是本地构建的四件产物，不是 CI 五平台产物集

- 已签并验：`kroma-linux-x64`、`kroma-linux-arm64-musl`、`kroma-rc1-source.tar.gz`、`kroma_RC1.patch`。
- 未签：CI 各平台产出的二进制。原因是 `ci.yml` 不上传 artifact（`release.yml` 上传但从未跑通），
  没有可下载的字节。
- `release.yml` 现在已具备"attest 实际字节 → 离线签名 → 用同一 run 的字节验签"的机制与 9 条负向测试
  （改动字节 / 多出文件 / 缺失文件 / 错提交 / 沙箱未通过 全部拒绝），但该机制在 GitHub 上尚未实跑。

## 5. 私有密钥的离线备份仍待所有者完成

`/root/kroma-release/kroma-release-key.pem`（600，目录 700）只存在于这台服务器。丢失后只能换 `keyId`，
已发出的签名全部作废。见 `docs/release/TRUST_ROOT.md`。

## 6. T24 的结论是 Inconclusive，不是"锦标赛更好"

留出集 5/6 vs 6/6，配对差 +16.7%，置信区间下界为 0。默认策略仍是 `SingleCandidate`。任何把这个
结果读成"多候选更优"的说法都是错的。

## 7. 已知的不稳定测试

`docs/review/phase1_test_results/regressions.md` 记录了四条负载敏感用例。本轮本机全量跑过一次
`EXIT=0`（74 个测试二进制全绿）；也出现过一次 `mcp::call_lifetime_tests::subsequent_call_fails_closed_when_poisoned`
在后台编译并发时失败、单独跑 3 次全过的情况。它是既有 flake，不是本轮引入的回归。

## 8. 历史文档保持原文

`T21_acceptance.md`、`T22_acceptance.md`、`T30_sealing.md` 里的旧结论保留原样，只在顶部加了
`Superseded by` 指向本轮的单一状态表。不要把它们的旧结论当成本轮结论。
