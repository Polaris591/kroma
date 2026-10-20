# M1-02 Credential Broker

代码：`crates/kroma-core/src/credential_broker.rs`
（`BrokerGrant` / `BrokerRequest` / `Deny` / `BrokerState` / `BrokerGate` / `BrokeredProvider`）。

授权是**绑定**不是白名单：job、subject、model、endpoint（精确串，无通配）、epoch、token 与
金额上限全部逐项比对，任一项不符即拒。

## 判定半边（C00-07 之前已收口）

- **A1 绑定**：换 model / 换 endpoint / 换 subject / 旧 epoch / 超额 各自返回不同的 `Deny`。
- **A2 重定向与私网**：`follows_redirect_to` 出现即 `RedirectRefused`（不跟随）；
  `169.254.169.254`、`127.0.0.1`、`10.0.0.5`、`[::1]` 即使被写进授权清单也一律
  `PrivateAddress`——元数据服务不是"模型 API 的另一种地址"。
- **A3 重启**：预留项与 `unknown_liability_tokens` 一并序列化/恢复；未知结算**不退额度**，
  只看作负债；恢复后可用余额按 `limit - 预留 - 未知 - 已结算` 饱和计算（不 wrap 成新额度）。

修掉的两个真问题：`[::1]:443` 的方括号让主机解析失败（于是私网检查被跳过），
以及从 400 里减 300 再减 200 的测试期望本身会下溢——后者改了期望并注明了饱和语义。

## 接线半边（本轮）

`BrokeredProvider` 是唯一实现：它是 `ModelProvider` 的装饰器，进 `complete` 的第一件事就是
`BrokerGate::admit`。判定和账本因此是同一个对象的两面，不是两套并行记账：

1. **派发前**：`admit` 用默认的 `authorize` 比对 grant 与"本次要发的 endpoint/model"
   （`DispatchBinding`，由协议客户端自己从真正要写进请求行/请求体的字段报出）＋调用方
   `BrokerIdentity`（job/subject/epoch）。任一不符 → `Deny`，请求不出进程，
   `ProviderError::NotDispatched(BrokerDenied)` 返回给调用方，其自有账本据此退额度。
   拿不到 binding 的 provider 一律 `Deny::UnboundDispatch`（"未知"不等于"无限制"）。
2. **派发前预留**：通过后按本次最坏 case（保守输入估计 + 请求声明的输出上限）写
   `BrokerState::reservations`。每次派发一个唯一 key（`<job>#<seq>`），同一 job 的多次派发
   互不覆盖（子 agent 并发也成立）。
3. **派发后结算**：provider 报了 usage → `settle_with_usage`（释放预留 + 记 `settled_tokens`，
   之后计入额度）；报了 usage 但 `attempts_used > 1`、或根本没报 usage、或失败/超时
   → `settle_unknown`，预留转成 `unknown_liability_tokens`，**不退额度**；
   只有 `is_provably_zero_consumption()` 且 `WireAttempts == 0` 时才 `release_undispatched`
   真正释放。

`settled_tokens` 是本轮给既有 `BrokerState` 加的唯一字段（`#[serde(default)]`，旧账本可读），
`authorize` 的可用额度改为 `limit - 预留 - 未知负债 - 已结算`。

测试：`tests/m102_broker_wiring.rs` B1–B9（含 model / endpoint 各改一个字符即拒、deny 时
transport 调用次数为 0、wire attempt 为 0、结算/未知负债/不退款/额度耗尽），
单元 B10–B12（presented identity 逐项比对、同一 job 多次派发各自结算、重启后已结算额度不再发放）。

## 接线边界：已接 / 未接

**已接**：任何被 `BrokeredProvider` 包住的 provider。包装一次即覆盖使用同一
`Arc<dyn ModelProvider>` 的所有派发——父 runner 的主循环、子 agent（`ProviderSubagentBackend`
克隆同一个 Arc）、benchmark 各 arm（`ProviderBenchExecutor`），因为它们最终都落到
`ModelProvider::complete`。

**已接（M1-06）**：

- `kroma run`、`kroma bench`、`kroma serve` 的 `/model` 热切换三处 provider 构造都经过
  `credential_broker::enforce`，grant 的目的地取自 provider 自己声明的 `dispatch_binding`，
  门禁与线上目标同源；
- 三处都先过 `owner_envelope`：没有声明的费用 envelope 就拒绝启动（`Some(0)` 也算拒绝）；
- 派发预占 → 派发 → 结算落在同一个 `BrokerState`，`m106_pilot_boundary` 有测试断言。

**仍未接**（如实保留）：

- 生产路径还没有构造 gate 的地方：CLI / bench / surface 目前都直接 `build_provider`，
  没有操作员的 grant 来源（谁签 grant、job_id/epoch 从哪来，属于未批准的产品决定）。
  因此默认跑起来的进程**不经过** broker，M1-06 的 pilot 门禁仍然拒绝。
- 裸 provider（未包装）仍然直接派发：broker 是"包装后才成立的检查点"，不是进程级唯一出口。
  库级调用方（`examples/`、测试夹具）仍可直接构造带字面密钥的 provider config，
  所以"唯一出口"目前是**产品路径**成立，不是类型系统强制。
- 子 agent 复用父的 grant/identity，没有各自的 job/grant（配额隔离属于 M1-03/05）。

**已知缺口**（本轮明确没做）：

- `cost_limit_micros` 没有价格表，`Reservation::cost_micros` 恒为 0，金额上限只被记录不被执行。
- 重定向拒绝仍由协议客户端"不跟随 301/302/303/307/308"保证；`BrokerRequest::follows_redirect_to`
  在接线路径上恒为 `None`（那是给未来 proxy 形态客户端准备的字段）。
- 请求没有声明输出上限、且协议客户端也不套默认上限时（Chat Completions / Responses 的
  `max_output_tokens = None`），预留只覆盖输入侧；Anthropic 客户端会套自己的默认上限，同样
  未进入预留计算。
- 派发后的 usage 由 provider 自报，是"provider 回报"而不是"账单对账"。

## 收口

本轮（接线半边）定向 9 条 + 单元 3 条全绿，clippy `-D warnings` 0，fmt 干净；
原始日志见 `evidence/m1-02-wiring/`。判定半边的历史证据见 `evidence/m1-02/`。

M1-02 的门禁与账本部分已完成；M1-06 pilot 的三条前置已有测量（见
`evidence/m1-06/RESULT.md`），剩余缺口是"类型级唯一出口"这一条，上面已写明。
