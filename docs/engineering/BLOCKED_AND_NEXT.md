# 状态与未做项（2026-10-19 最终树）

32 个工作包全部有证据通过，E01–E08 八组实验全部先预注册后执行。
门禁在**最终树**上重跑：`evidence/gate-final.log`，1178 passed / 0 failed / 93 二进制，
`clippy --workspace --all-targets -- -D warnings` 0 warning，`cargo fmt --all -- --check` 干净。

这份文件只列**没做的事**，不重复已完成的清单。

## 一、实验结论层面

| 组 | 结论 | 未做 |
|---|---|---|
| E01 | 存在税 = 首 token 延迟（14 040 ms vs 3 568 ms），内存与 token 无差 | 未做"按能力裁剪的最小构建"全量对照 |
| E02 | 四臂全不优于基线（taskstate +121%、deferred +23%、combo 5/6） | — |
| E03 | legacy 3/4 vs taskstate 4/4，6/8 跑被 420 s 上限截断 | 未把上限提高到自然收敛 |
| E04 | dev −44% 被全新留出集 +19% 推翻 → 不复制不晋升 | — |
| E05 | 质量非劣、成本 inconclusive | 冷缓存格由 M2-04 用块内位置构造，E05 本身仍是两格 |
| E06 | 导航由任务形状决定（单文件 +38%、跨文件 −9%） | 第三臂「图+历史」不可表达（`code_graph` 无历史模式） |
| E07 | 锦标赛判负（+30.5%，留出集 +70.6%） | — |
| E08 | 路由已接线、73 次决策零切换、当前阈值下惰性 | 未做"降低阈值让路由真正升级"的对照 |
| M2-04 | 缓存×压缩 factorial inconclusive，不主张质量非劣 | 缓存多数没热起来（15/24 命中率 0），因子强度不足 |

## 二、深度审查（2026-10-19）里已修与未修

已修：
- **KRV4-01** 快照 manifest 写读上限不一致 → 紧凑 JSON + 写前用读者同一个上限校验，
  6000 文件 capture→load→parent 链回归、超限拒绝且不留残骸（`krv401_manifest_ceiling`）。
- **KRV4-02** `run_bounded` 硬超时不硬 → 并发有界 drain、deadline 覆盖 drain、进程组清理，
  孙进程持管道与双通道 2 MB flood 两条回归（`krv402_bounded_drain`）。
- **KRV4-05** 预注册自检失效 → 夹具 hermetic、数量动态、结构/环境两级、`E*` 通配漏检修正，
  并进 CI/release 两个 workflow。
- **KRV4-06** 含空格安装路径被误拒 → 仍未改（见下）。

未修（如实列出，不声称已解决）：
- **KRV4-03** 每轮 mutation 后全量重扫 + SHA256：只有定性结论，没有 `snapshot.*` 指标与增量实现。
- **KRV4-04** JS 下载器无背压、ENOSPC 未处理错误：`bin/kroma.js` 未改。
- **KRV4-06** `isSafeInstallTarget` 仍拒绝含空格的合法绝对路径。
- **KRV4-07** `ci.yml` 的 sandbox-isolation 仍 `continue-on-error`。
- **KRV4-08** Zig 未锁版、仓库根仍无 LICENSE 正文。
- **KRV4-09** `goal-interface.py` 在无 `rustc` 时抛 `FileNotFoundError` 而非结构化 BLOCKED。

## 三、能力与契约层面

- C00-12 的"每个性能敏感能力必须有晋升结论"这条**正向**强制未做（`promote` 不被
  `default_on` 调用）；已做的三条见 `PERF_GATE_C0012.md`。
- M1-06 的"broker 是唯一出口"在产品路径成立，但库级调用方仍可构造字面密钥（类型级未强制）。
- C00-05 的调度器（activation 回收计数）仍未接进 `RunPlan`；C00-05 的验收规格由进程树测量回答。

## 四、交付

交付集在 `/opt/pi-qq/outbox/kroma-v4/`：补丁、源码包、`DELIVERY.md`、`SHA256SUMS`、
`progress.json`。接收方模拟（补丁 `git write-tree` == 封板树、源码包 `diff -r` 零差异）在
`DELIVERY.md` 里给了命令。
