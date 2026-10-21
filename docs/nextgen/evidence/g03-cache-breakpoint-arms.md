# KR-G03 缓存断点三臂（OpenAI Responses）

机制早已实现（`OpenAiCacheMode::{Off, Implicit, Explicit}`，`openai_responses.rs:57`），但从未作为臂测过。
因为 mode 是 provider 级（`KROMA_OPENAI_CACHE_MODE`），一个 bench 进程只能用一个值，所以用三次运行对比
**同一个 baseline arm**。

## 结果（dev 6 任务，gpt-6.1-sol，openai-responses 协议）

| mode | tokens | 通过率 | 缓存命中率 |
|---|---|---|---|
| off | 86147 | 6/6 | 0.299 |
| implicit | 89162 | 5/6 | 0.665 |
| explicit | 87557 | 6/6 | 0.760 |

原始：`exp-g03-{off,implicit,explicit}.json`。

## 结论

1. **缓存确实生效**：命中率从 0.299（基线噪声）升到 0.665 / 0.76。
2. **但短任务上不省钱**：implicit +3.5%、explicit +1.6% token。缓存写入本身计费，而 dev 任务的历史太短，
   写一次用不上几次，写入成本盖过读取收益。
3. **explicit 优于 implicit**：命中率更高（0.76 vs 0.665）、总 token 更低。若将来要开缓存，应选 explicit。
4. 因此**默认保持 off 是对的**（`KROMA_OPENAI_CACHE_MODE` 默认 `off`），这一轮的数据支持这个默认值。

## 局限

三次运行不是同一进程内的配对（mode 是 provider 级），且 implicit 那次通过率是 5/6 而其余是 6/6 —— 落在
已知的单次重复噪声带内。要严格判定需要长历史任务 + 多次重复。
