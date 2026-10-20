# M1-06：授权 pilot

三条规格逐条落地，证据 `/opt/kroma-v4/evidence/m1-06/`（`commands.md` 含命令与退出码，`raw/` 含原始日志）。

| 规格 | 落地 | 证据 |
|---|---|---|
| 无有效 owner 费用 envelope 不得启动 | `owner_envelope` 无默认值；run/bench/serve 三个入口都接 | 无 envelope → exit 2 |
| 真实任务链跑通并如实记账 | broker 预占→派发→结算同一 `BrokerState` | 成功链 exit 0，in=2412 out=27 |
| 门禁未过拒绝而非降级 | 私网端点与超 envelope 都在派发前拒绝 | 两条故障链 exit 1，in=0 out=0 |

设计要点：`enforce` 的 grant 目的地取自 `ModelProvider::dispatch_binding()`，门禁与线上目标同源；
不声明目的地的 provider 原样返回并由 `provider_declares_destination()` 让调用方先问清楚。
`owner_envelope(Some(0))` 也是拒绝——0 不是小额度，是一次请求都发不出去的 run。

定向测试 `m106_pilot_boundary` 5 条 + `m106_pilot_gate` 2 条。
