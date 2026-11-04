# Built-in model catalog

Verified **2026-11-04**, catalog revision/schema **4**. This is an audited
snapshot, not a live model directory or a promise of access to every model.
Scope: current flagship general-purpose, coding and reasoning API families,
current fast siblings, still-served popular predecessors, and explicitly
documented aliases/pinned snapshots. Retired predecessors, dedicated
image/audio/video generation and embedding/reranking endpoints are excluded.

Aliases and snapshots count as separate API IDs, not separate foundation
models. Membership is not a recommendation or an account-access guarantee.

The compiled table is `src/model_catalog/records.rs`. Model IDs and aliases
are case-sensitive and exact. Existing six DeepSeek/Claude records remain
available; the snapshot now contains **110 exact API IDs across 18 providers**.

## Coverage and lifecycle

The expansion is not just a list of this month's newest model from each vendor.
It also covers GPT-5/5.1/5.3-Codex/4.1/4o/o3, Claude 4.5/4.6/4.7/4.8/5,
Gemini 2.5/3.1/3.5/3.6, Qwen Plus/Max/Coder/VL, GLM 4.5/4.6/4.7,
Kimi K2 coding, older Seed LTS and Mistral code/generalist models.
Each ID in the table links directly to its specification or lifecycle source.

- **Deprecated is not retired.** Still-callable deprecated IDs are retained for
  existing integrations; do not select them by default for new deployments.
  [OpenAI's lifecycle table](https://developers.openai.com/api/docs/deprecations)
  sets GPT-5/mini/nano/pro and o3 retirement to 2026-12-11, GPT-4.1/4o to
  2027-01-14, and GPT-5.1/5.3-Codex to 2027-04-01. Access may be existing-only.
- [Claude lifecycle](https://platform.claude.com/docs/en/about-claude/model-deprecations):
  legacy Active differs from Deprecated. A "not sooner than" date is a floor,
  not a shutdown announcement (notably Haiku 4.5's elapsed 2026-10-15 floor).
- [Google lifecycle](https://aistudio.google.com/docs/deprecations): Gemini 2.5
  Pro/Flash are existing-user-only and scheduled to auto-route to newer
  models on 2026-12-01; this snapshot must be revalidated before that date.
  Flash-Lite is not deprecated but also has existing-user access constraints.
- [Kimi K2 lifecycle](https://platform.kimi.ai/docs/models.md): the retained
  K2.6/K2.7 coding IDs are closed to new accounts from 2026-10-31, with complete
  retirement planned for the end of 2026. K3 entries do not inherit this status.
- [Mistral directory](https://docs.mistral.ai/models): Medium 3.5/Large 3 were
  deprecated in late October; retirement listed as 2027-04-29. Per-model and
  directory notices differ by one day, so no exact deprecation day is asserted.
- Tencent legacy services are existing-purchase-only and migrating to
  TokenHub. MiMo UltraSpeed has negotiated quotas; preview IDs such as Step 5
  and Hy4 Preview are not stability/access promises.

### Explicit omissions / follow-up gaps

- Confirmed retired DeepSeek chat/reasoner, older Claude/Gemini endpoints and
  MiMo 2.5 generation are not reintroduced just to increase the record count.
- Baidu's direct current directory (updated 2026-10-29) lists ERNIE 5.1 but
  no longer contains the older indexed ERNIE 5.0/Thinking rows. Those stale
  search-extract IDs were withheld, not asserted to have identical 5.1 specs.
- [Perplexity's migration notice](https://docs.perplexity.ai/docs/sonar/models)
  says Sonar Chat support ended 2026-09-27. Sync/stream calls are progressively
  reformulated as Agent API requests. Old Sonar context figures are not safe
  fixed caps for that new serving path, so old Sonar rows were not fabricated.
- Meta hosted Llama, AI21 Jamba, SenseNova and iFlytek Spark still lack a verified
  current official serving-host/spec pairing in this pass. They are gaps,
  not claims that those vendors have no models. Smaller public snapshots and
  cloud resellers are not exhaustively enumerated.

## Recorded limits

All numbers are tokens, not bytes or prices. Links identify the official
source; this is not a claim that every shorthand number is an exact integer. `?` means **unknown**, not false
or unlimited. `input cap` is separate from the shared context window. When a
vendor only specifies an input limit, using that value as Kroma's planning
window is conservative; we do not add its output allowance to invent a larger
shared window. Existing decimal interpretations of `1M`/`128K`/`384K` are
retained as conservative bounds, not claims about a vendor's binary units.

The output column is the documented cap for the supported request path, not
the default/recommended request size. These figures do **not** raise Kroma's
32,768 per-turn coding output cap or its default 4,096 planning allowance.

| Provider/model alias (official source) | Planning window | Input cap | Output cap | Images | Effort tiers |
| --- | ---: | ---: | ---: | :---: | --- |
| [`alibaba/qwen-max`](https://www.alibabacloud.com/help/en/model-studio/text-generation-model) | 128,000 | — | ? | ? | ? |
| [`alibaba/qwen-plus`](https://www.alibabacloud.com/help/en/model-studio/qwen-plus) | 1,000,000 | 995,904 | 32,768 | no | ? |
| [`alibaba/qwen-plus-2025-12-01`](https://www.alibabacloud.com/help/en/model-studio/qwen-plus) | 1,000,000 | 995,904 | 32,768 | no | ? |
| [`alibaba/qwen3-coder-flash`](https://www.alibabacloud.com/help/en/model-studio/text-generation-model) | 1,000,000 | — | ? | ? | ? |
| [`alibaba/qwen3-coder-plus`](https://www.alibabacloud.com/help/en/model-studio/qwen3-coder-plus) | 1,000,000 | 997,952 | 65,536 | no | ? |
| [`alibaba/qwen3-coder-plus-2025-09-23`](https://www.alibabacloud.com/help/en/model-studio/qwen3-coder-plus) | 1,000,000 | 997,952 | 65,536 | no | ? |
| [`alibaba/qwen3-vl-plus`](https://www.alibabacloud.com/help/en/model-studio/qwen3-vl-plus) | 262,144 | 258,048 | 32,768 | yes | ? |
| [`alibaba/qwen3.5-plus`](https://www.alibabacloud.com/help/en/model-studio/text-generation-model) | 1,000,000 | — | ? | ? | ? |
| [`alibaba/qwen3.6-plus`](https://www.alibabacloud.com/help/en/model-studio/text-generation-model) | 1,000,000 | — | ? | ? | ? |
| [`alibaba/qwen3.7-max`](https://www.alibabacloud.com/help/en/model-studio/qwen3-7-max) | 1,000,000 | 983,616 | 131,072 | no | ? |
| [`alibaba/qwen3.7-max-2026-05-20`](https://www.alibabacloud.com/help/en/model-studio/qwen3-7-max) | 1,000,000 | 983,616 | 131,072 | no | ? |
| [`alibaba/qwen3.7-plus`](https://www.alibabacloud.com/help/en/model-studio/qwen3-7-plus) | 1,000,000 | 983,616 | 131,072 | yes | ? |
| [`alibaba/qwen3.7-plus-2026-05-26`](https://www.alibabacloud.com/help/en/model-studio/qwen3-7-plus) | 1,000,000 | 983,616 | 131,072 | yes | ? |
| [`alibaba/qwen3.8-flash`](https://www.alibabacloud.com/help/en/model-studio/qwen3-8-flash) | 1,000,000 | 983,616 | 131,072 | yes | low, medium, xhigh |
| [`alibaba/qwen3.8-max`](https://www.alibabacloud.com/help/en/model-studio/qwen3-8-max) | 1,000,000 | 983,616 | 131,072 | yes | low, medium, xhigh |
| [`amazon/nova-2-lite-v1`](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-amazon-nova-2-lite.html) | 1,000,000 | — | ? | yes | low, medium, high |
| [`amazon/nova-2-pro-v1`](https://aws.amazon.com/about-aws/whats-new/2025/12/nova-2-foundation-models-amazon-bedrock/) | 1,000,000 | — | ? | yes | ? |
| [`anthropic/claude-fable-5-1`](https://platform.claude.com/docs/en/models/overview) | 1,000,000 | — | 128,000 | yes | low, medium, high, xhigh, max |
| [`anthropic/claude-haiku-4-5`](https://platform.claude.com/docs/en/models/haiku-4-5/overview) | 200,000 | — | 64,000 | yes | ? |
| [`anthropic/claude-haiku-4-5-20251001`](https://platform.claude.com/docs/en/models/haiku-4-5/overview) | 200,000 | — | 64,000 | yes | ? |
| [`anthropic/claude-haiku-5-5`](https://platform.claude.com/docs/en/models/overview) | 1,000,000 | — | 128,000 | yes | low, medium, high, xhigh, max |
| [`anthropic/claude-mythos-5-1`](https://platform.claude.com/docs/en/models/fable-5-1/whats-new-fable-5-1) | 1,000,000 | — | 128,000 | yes | low, medium, high, xhigh, max |
| [`anthropic/claude-opus-4-6`](https://platform.claude.com/docs/en/models/opus-4-6/overview) | 1,000,000 | — | 128,000 | yes | ? |
| [`anthropic/claude-opus-4-7`](https://platform.claude.com/docs/en/models/opus-4-7/overview) | 1,000,000 | — | 128,000 | yes | ? |
| [`anthropic/claude-opus-4-8`](https://platform.claude.com/docs/en/models/opus-4-8/overview) | 1,000,000 | — | 128,000 | yes | low, medium, high, xhigh, max |
| [`anthropic/claude-opus-5`](https://platform.claude.com/docs/en/models/opus-5/overview) | 1,000,000 | — | 128,000 | yes | low, medium, high, xhigh, max |
| [`anthropic/claude-opus-5-5`](https://platform.claude.com/docs/en/models/overview) | 1,000,000 | — | 128,000 | yes | low, medium, high, xhigh, max |
| [`anthropic/claude-sonnet-4-6`](https://platform.claude.com/docs/en/models/sonnet-4-6/overview) | 1,000,000 | — | 128,000 | yes | ? |
| [`anthropic/claude-sonnet-5`](https://platform.claude.com/docs/en/models/sonnet-5/overview) | 1,000,000 | — | 128,000 | yes | low, medium, high, xhigh, max |
| [`anthropic/claude-sonnet-5-5`](https://platform.claude.com/docs/en/models/overview) | 1,000,000 | — | 128,000 | yes | low, medium, high, xhigh, max |
| [`baidu/ernie-5.1`](https://cloud.baidu.com/doc/qianfan/s/rmh4stp0j) | 128,000 | 119,000 | 65,536 | ? | ? |
| [`bytedance/doubao-seed-2-0-lite-260428`](https://docs.volcengine.com/docs/ark/model-list?lang=zh) | 256,000 | 224,000 | 128,000 | yes | ? |
| [`bytedance/doubao-seed-2-0-mini-260428`](https://docs.volcengine.com/docs/ark/model-list?lang=zh) | 256,000 | 224,000 | 128,000 | yes | ? |
| [`bytedance/doubao-seed-2-1-lite-260915`](https://docs.volcengine.com/docs/ark/model-list?lang=zh) | 1,024,000 | 1,024,000 | 256,000 | yes | ? |
| [`bytedance/doubao-seed-2-1-pro-260628`](https://docs.volcengine.com/docs/ark/model-list?lang=zh) | 256,000 | 256,000 | 256,000 | yes | ? |
| [`bytedance/doubao-seed-2-1-pro-260915`](https://docs.volcengine.com/docs/ark/model-list?lang=zh) | 1,024,000 | 1,024,000 | 256,000 | yes | ? |
| [`bytedance/doubao-seed-2-1-turbo-260628`](https://docs.volcengine.com/docs/ark/model-list?lang=zh) | 256,000 | 256,000 | 256,000 | yes | ? |
| [`bytedance/doubao-seed-evolving`](https://docs.volcengine.com/docs/ark/model-list?lang=zh) | 1,024,000 | 1,024,000 | 256,000 | yes | ? |
| [`cohere/command-a-03-2025`](https://docs.cohere.com/docs/command-a) | 256,000 | — | 8,000 | yes | ? |
| [`cohere/command-a-plus-05-2026`](https://docs.cohere.com/docs/command-a-plus) | 128,000 | 128,000 | 64,000 | yes | off, high |
| [`deepseek/deepseek-flash`](https://api-docs.deepseek.com/quick_start/pricing/) | 1,000,000 | — | 384,000 | yes | low, high, max |
| [`deepseek/deepseek-v4-pro`](https://api-docs.deepseek.com/quick_start/pricing/) | 1,000,000 | — | 384,000 | no | low, high, max |
| [`google/gemini-2.5-flash`](https://aistudio.google.com/docs/models/gemini-2.5-flash) | 1,048,576 | 1,048,576 | 65,536 | yes | low, medium, high |
| [`google/gemini-2.5-flash-lite`](https://aistudio.google.com/docs/models/gemini-2.5-flash-lite) | 1,048,576 | 1,048,576 | 65,536 | yes | low, medium, high |
| [`google/gemini-2.5-pro`](https://aistudio.google.com/docs/models/gemini-2.5-pro) | 1,048,576 | 1,048,576 | 65,536 | yes | low, medium, high |
| [`google/gemini-3.1-pro-preview`](https://aistudio.google.com/docs/models/gemini-3.1-pro-preview) | 1,048,576 | 1,048,576 | 65,536 | yes | low, medium, high |
| [`google/gemini-3.5-flash-lite`](https://aistudio.google.com/docs/models/gemini-3.5-flash-lite) | 1,048,576 | 1,048,576 | 65,536 | yes | minimal, low, medium, high |
| [`google/gemini-3.6-flash`](https://aistudio.google.com/docs/models/gemini-3.6-flash) | 1,048,576 | 1,048,576 | 65,536 | yes | minimal, low, medium, high |
| [`google/gemini-3.8-flash`](https://aistudio.google.com/docs/models/gemini-3.8-flash) | 1,048,576 | 1,048,576 | 65,536 | yes | low, medium, high |
| [`google/gemini-4-argon`](https://aistudio.google.com/docs/models/gemini-4-argon) | 1,048,576 | — | 65,536 | yes | low, medium, high |
| [`minimax/MiniMax-M3`](https://platform.minimax.io/docs/api-reference/text-chat-anthropic.md) | 1,000,000 | — | 524,288 | yes | ? |
| [`minimax/MiniMax-M3.1-Flash`](https://platform.minimax.io/docs/api-reference/text-chat-anthropic.md) | 1,000,000 | — | 524,288 | yes | low, medium, high, xhigh, max |
| [`mistral/codestral-2508`](https://docs.mistral.ai/models/codestral-25-08) | 128,000 | — | ? | no | ? |
| [`mistral/mistral-large-2512`](https://docs.mistral.ai/models/mistral-large-3-25-12) | 256,000 | — | ? | yes | ? |
| [`mistral/mistral-large-4`](https://docs.mistral.ai/models/mistral-large-4-0) | 1,000,000 | — | ? | yes | low, medium, high |
| [`mistral/mistral-large-4-0`](https://docs.mistral.ai/models/mistral-large-4-0) | 1,000,000 | — | ? | yes | low, medium, high |
| [`mistral/mistral-medium-3-5`](https://docs.mistral.ai/models/mistral-medium-3-5-26-04) | 256,000 | — | ? | yes | ? |
| [`mistral/mistral-small-2603`](https://docs.mistral.ai/models/mistral-small-4-0-26-03) | 256,000 | — | ? | ? | ? |
| [`moonshot/kimi-k2.6`](https://platform.kimi.ai/docs/models.md) | 262,144 | — | ? | yes | ? |
| [`moonshot/kimi-k2.7-code`](https://platform.kimi.ai/docs/models.md) | 262,144 | — | ? | ? | ? |
| [`moonshot/kimi-k2.7-code-highspeed`](https://platform.kimi.ai/docs/models.md) | 262,144 | — | ? | ? | ? |
| [`moonshot/kimi-k3`](https://platform.kimi.ai/docs/api/chat.md) | 1,048,576 | — | 1,048,576 | yes | low, high, max |
| [`moonshot/kimi-k3.1-pro`](https://platform.kimi.ai/docs/api/chat.md) | 1,048,576 | — | 1,048,576 | yes | low, high, max |
| [`moonshot/kimi-k3.1-pro-highspeed`](https://platform.kimi.ai/docs/api/chat.md) | 1,048,576 | — | 1,048,576 | yes | low, high, max |
| [`nvidia/nvidia/nemotron-3.5-super-vl-120b-a12b`](https://build.nvidia.com/nvidia/nemotron-3.5-super-vl-120b-a12b/modelcard) | 1,000,000 | — | ? | yes | ? |
| [`openai/gpt-4.1`](https://developers.openai.com/api/docs/models/gpt-4.1) | 1,047,576 | — | 32,768 | yes | ? |
| [`openai/gpt-4.1-2025-04-14`](https://developers.openai.com/api/docs/models/gpt-4.1) | 1,047,576 | — | 32,768 | yes | ? |
| [`openai/gpt-4o`](https://developers.openai.com/api/docs/models/gpt-4o) | 128,000 | — | 16,384 | yes | ? |
| [`openai/gpt-4o-2024-08-06`](https://developers.openai.com/api/docs/models/gpt-4o) | 128,000 | — | 16,384 | yes | ? |
| [`openai/gpt-4o-2024-11-20`](https://developers.openai.com/api/docs/models/gpt-4o) | 128,000 | — | 16,384 | yes | ? |
| [`openai/gpt-5`](https://developers.openai.com/api/docs/models/gpt-5) | 400,000 | — | 128,000 | yes | minimal, low, medium, high |
| [`openai/gpt-5-2025-08-07`](https://developers.openai.com/api/docs/models/gpt-5) | 400,000 | — | 128,000 | yes | minimal, low, medium, high |
| [`openai/gpt-5-mini`](https://developers.openai.com/api/docs/models/gpt-5-mini) | 400,000 | — | 128,000 | yes | ? |
| [`openai/gpt-5-mini-2025-08-07`](https://developers.openai.com/api/docs/models/gpt-5-mini) | 400,000 | — | 128,000 | yes | ? |
| [`openai/gpt-5-nano`](https://developers.openai.com/api/docs/models/gpt-5-nano) | 400,000 | — | 128,000 | yes | ? |
| [`openai/gpt-5-nano-2025-08-07`](https://developers.openai.com/api/docs/models/gpt-5-nano) | 400,000 | — | 128,000 | yes | ? |
| [`openai/gpt-5-pro`](https://developers.openai.com/api/docs/models/gpt-5-pro) | 400,000 | — | 272,000 | yes | high |
| [`openai/gpt-5-pro-2025-10-06`](https://developers.openai.com/api/docs/models/gpt-5-pro) | 400,000 | — | 272,000 | yes | high |
| [`openai/gpt-5.1`](https://developers.openai.com/api/docs/models/gpt-5.1) | 400,000 | — | 128,000 | yes | off, low, medium, high |
| [`openai/gpt-5.1-2025-11-13`](https://developers.openai.com/api/docs/models/gpt-5.1) | 400,000 | — | 128,000 | yes | off, low, medium, high |
| [`openai/gpt-5.3-codex`](https://developers.openai.com/api/docs/models/gpt-5.3-codex) | 400,000 | 272,000 | 128,000 | yes | low, medium, high, xhigh |
| [`openai/gpt-5.6`](https://developers.openai.com/api/docs/models/gpt-5.6-sol) | 1,050,000 | 922,000 | 128,000 | yes | off, low, medium, high, xhigh, max |
| [`openai/gpt-5.6-sol`](https://developers.openai.com/api/docs/models/gpt-5.6-sol) | 1,050,000 | 922,000 | 128,000 | yes | off, low, medium, high, xhigh, max |
| [`openai/gpt-6-astra`](https://developers.openai.com/api/docs/models/gpt-6-astra) | 1,050,000 | 922,000 | 128,000 | yes | low, medium, high, xhigh, max |
| [`openai/gpt-6-luna`](https://developers.openai.com/api/docs/models/compare) | 1,050,000 | — | 128,000 | yes | off, low, medium, high, xhigh, max |
| [`openai/gpt-6.1-sol`](https://developers.openai.com/api/docs/models/gpt-6.1-sol) | 1,050,000 | 922,000 | 128,000 | yes | low, medium, high, xhigh, max |
| [`openai/gpt-6.1-terra`](https://developers.openai.com/api/docs/models) | 1,050,000 | — | 128,000 | yes | off, low, medium, high, xhigh, max |
| [`openai/o3`](https://developers.openai.com/api/docs/models/o3) | 200,000 | — | 100,000 | yes | ? |
| [`openai/o3-2025-04-16`](https://developers.openai.com/api/docs/models/o3) | 200,000 | — | 100,000 | yes | ? |
| [`stepfun/step-3.5-flash`](https://platform.stepfun.com/docs/zh/guides/models/list.md) | 256,000 | — | ? | no | ? |
| [`stepfun/step-3.7-flash`](https://platform.stepfun.com/docs/zh/guides/models/list.md) | 256,000 | — | ? | yes | low, medium, high |
| [`stepfun/step-5-preview`](https://platform.stepfun.com/docs/zh/guides/models/step-5-preview.md) | 1,000,000 | 1,000,000 | 64,000 | yes | low, medium, high |
| [`tencent/hunyuan-a13b`](https://cloud.tencent.com/document/product/1729/104753) | 224,000 | 224,000 | 32,000 | no | ? |
| [`tencent/hunyuan-t1-vision-20250916`](https://cloud.tencent.com/document/product/1729/104753) | 28,000 | 28,000 | 20,000 | yes | ? |
| [`tencent/hunyuan-vision-1.5-instruct`](https://cloud.tencent.com/document/product/1729/104753) | 24,000 | 24,000 | 16,000 | yes | ? |
| [`tencent/hy3`](https://cloud.tencent.com/document/product/1823/130078) | ? | — | ? | ? | ? |
| [`tencent/hy4`](https://cloud.tencent.com/document/product/1823/130079) | ? | — | ? | ? | ? |
| [`tencent/hy4-preview`](https://cloud.tencent.com/document/product/1823/130078) | ? | — | ? | ? | ? |
| [`xai/grok-4.7`](https://docs.x.ai/developers/models/grok-4.7) | 500,000 | — | ? | yes | low, medium, high, xhigh |
| [`xiaomi/mimo-v2.6-flash`](https://mimo.mi.com/static/docs/quick-start/summary/model.md) | 1,000,000 | — | 128,000 | yes | ? |
| [`xiaomi/mimo-v2.6-pro`](https://mimo.mi.com/static/docs/quick-start/summary/model.md) | 1,000,000 | — | 128,000 | yes | ? |
| [`xiaomi/mimo-v2.6-pro-ultraspeed`](https://mimo.mi.com/static/docs/quick-start/summary/model.md) | 1,000,000 | — | 128,000 | yes | ? |
| [`zhipu/glm-4.5`](https://docs.z.ai/guides/overview/pricing.md) | 128,000 | — | ? | no | ? |
| [`zhipu/glm-4.6`](https://docs.z.ai/guides/llm/glm-4.6) | 200,000 | — | 128,000 | no | ? |
| [`zhipu/glm-4.7`](https://docs.z.ai/guides/llm/glm-4.7) | 200,000 | — | 128,000 | no | ? |
| [`zhipu/glm-4.7-flash`](https://docs.z.ai/guides/llm/glm-4.7) | 200,000 | — | 128,000 | no | ? |
| [`zhipu/glm-4.7-flashx`](https://docs.z.ai/guides/llm/glm-4.7) | 200,000 | — | 128,000 | no | ? |
| [`zhipu/glm-5.3`](https://docs.z.ai/guides/llm/glm-5.3) | 1,000,000 | — | 128,000 | no | low, high, max |
| [`zhipu/glm-5.3-flash`](https://docs.z.ai/guides/vlm/glm-5.3-flash) | 1,000,000 | — | 128,000 | yes | low, high, max |
| [`zhipu/glm-5.3-flashx`](https://docs.z.ai/guides/vlm/glm-5.3-flash) | 1,000,000 | — | 128,000 | yes | low, high, max |

## Official model sources

- [`deepseek-flash`, `deepseek-v4-pro`](https://api-docs.deepseek.com/quick_start/pricing/)
- [`claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5`, `claude-fable-5-1`](https://platform.claude.com/docs/en/models/overview)
- [`claude-mythos-5-1`](https://platform.claude.com/docs/en/models/fable-5-1/whats-new-fable-5-1)
- [`gpt-6-astra`](https://developers.openai.com/api/docs/models/gpt-6-astra)
- [`gpt-6.1-sol`](https://developers.openai.com/api/docs/models/gpt-6.1-sol)
- [`gemini-4-argon`](https://aistudio.google.com/docs/models/gemini-4-argon)
- [`gemini-3.8-flash`](https://aistudio.google.com/docs/models/gemini-3.8-flash)
- [`grok-4.7`](https://docs.x.ai/developers/models/grok-4.7)
- [`qwen3.8-max`](https://www.alibabacloud.com/help/en/model-studio/qwen3-8-max)
- [`qwen3.8-flash`](https://www.alibabacloud.com/help/en/model-studio/qwen3-8-flash)
- [`kimi-k3.1-pro`, `kimi-k3.1-pro-highspeed`, `kimi-k3`](https://platform.kimi.ai/docs/api/chat.md)
- [`glm-5.3`](https://docs.z.ai/guides/llm/glm-5.3)
- [`MiniMax-M3.1-Flash`, `MiniMax-M3`](https://platform.minimax.io/docs/api-reference/text-chat-anthropic.md)
- [`mistral-large-4`, `mistral-large-4-0`](https://docs.mistral.ai/models/mistral-large-4-0)
- [`command-a-plus-05-2026`](https://docs.cohere.com/docs/command-a-plus)
- [`doubao-seed-2-1-pro-260915`, `doubao-seed-evolving`](https://docs.volcengine.com/docs/ark/model-list?lang=zh)
- [`nova-2-lite-v1`](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-amazon-nova-2-lite.html)
- [`nova-2-pro-v1`](https://aws.amazon.com/about-aws/whats-new/2025/12/nova-2-foundation-models-amazon-bedrock/)

- [GLM-5.3 Flash / FlashX](https://docs.z.ai/guides/vlm/glm-5.3-flash)
- [Nemotron 3.5 Super VL model card](https://build.nvidia.com/nvidia/nemotron-3.5-super-vl-120b-a12b/modelcard) and [hosted API example](https://build.nvidia.com/nvidia/nemotron-3.5-super-vl-120b-a12b)

## Qualifications that affect configuration

- **OpenAI GPT-6 Astra / GPT-6.1 Sol:** 1,050,000 is the total window, but
  input is independently limited to 922,000. Kroma enforces both constraints,
  including when an explicit output reserve is configured. Use
  `api_protocol = "responses"` for tool calls: the official
  [reasoning guide](https://developers.openai.com/api/docs/guides/reasoning)
  says Chat Completions does not support function calling for these models.
  `none` and `minimal` reasoning are not advertised as supported.
- **Claude:** the five newest recorded IDs accept low/medium/high/xhigh/max
  according to the [effort guide](https://platform.claude.com/docs/en/build-with-claude/effort).
  Mythos 5.1 has Fable 5.1's specs but requires a verified organization; a
  catalog match does not grant access. Use the Anthropic protocol and its
  official metadata for thinking-mode capabilities; effort tiers do not prove
  that thinking can be disabled or that any particular thinking mode exists.
- **DeepSeek:** low/high/max are the distinct documented effort levels.
  Other spellings map onto these levels; thinking enable/disable is a separate
  control. See [Thinking Mode](https://api-docs.deepseek.com/guides/thinking_mode/).
- **Gemini 4 Argon:** the 1,048,576 window includes input, thinking and output.
  65,536 is the ordinary foreground **streaming** cap. The advertised
  one-million-token long decode needs background processing or continuation,
  which Kroma does not implement. Non-streaming has a smaller per-hop cap;
  Kroma's summary attempts remain below it. See
  [thinking](https://aistudio.google.com/docs/thinking) and
  [long decode](https://ai.google.dev/gemini-api/docs/long-decode).
  The [OpenAI compatibility mapping](https://aistudio.google.com/docs/openai)
  maps `minimal` to `low`. Gemini 3.8 Flash explicitly rejects `minimal`;
  its separately published input/output limits are not added together.
- **Qwen:** the 983,616 input ceiling on the newest Max/Flash records is the stricter thinking-mode value;
  the non-thinking ceiling is 991,808. The conservative value is used because
  the catalog does not choose thinking mode. The catalog exposes the distinct
  low/medium/xhigh tiers for Qwen3.8, not redundant effort aliases. Older
  Qwen tiers remain unknown unless verified separately. Qwen Plus uses its
  stricter thinking-mode ceiling 995,904, and Qwen3-VL-Plus uses 258,048. See
  [OpenAI-compatible parameters](https://www.alibabacloud.com/help/en/model-studio/qwen-api-via-openai-chat-completions).
- **Kimi K3:** 131,072 is the default output allowance, **not** the maximum.
  The documented settable maximum is 1,048,576; the API reduces it to the
  remaining shared context. See [models](https://platform.kimi.ai/docs/models.md)
  and [pricing/context](https://platform.kimi.ai/docs/pricing/chat.md).
- **GLM-5.3:** the main model is text-only, while Flash / FlashX support images; thinking cannot be disabled. 1M/128K use conservative
  decimal bounds. Access can depend on the Coding Plan and protocol. See the
  model source above and [API introduction](https://docs.z.ai/api-reference/introduction).
- **MiniMax:** 524,288 is the maximum, while 131,072 is recommended.
  M3.1-Flash accepts five effort tiers and cannot disable thinking. M3 ignores
  effort, so its tier list remains unknown. See
  [model invocation and protocol fields](https://platform.minimax.io/docs/guides/text-generation).
- **Grok 4.7:** the [Chat API](https://docs.x.ai/developers/rest-api-reference/inference/chat-completions)
  defaults visible output to 128,000, but xAI does not state a separate hard
  text output maximum. We do not record that default as a hard cap. Responses
  is the [recommended protocol](https://docs.x.ai/developers/grok-4-7);
  Chat Completions is deprecated.
- **Mistral Large 4:** public preview; 1M is shared input/output context.
  No independent numeric output maximum is published, so it stays unknown.
  [Reasoning](https://docs.mistral.ai/studio/conversations/reasoning) supports
  low/medium/high, but reasoning content uses structured chunks rather than a
  plain OpenAI string. Catalog metadata is not proof that Kroma supports every
  vendor-specific streaming/replay extension; `auto` is the safest starting
  setting pending live adapter verification.
- **Cohere:** Command A+ has 128,000 input and 64,000 output; the planning
  window stays conservatively at 128,000. The
  [Compatibility API](https://docs.cohere.com/docs/compatibility-api) accepts
  `none`/`high` effort; Kroma represents `none` as `off`. Image support is a
  model-level fact, not a live validation of every compatibility content part.
- **Doubao:** the model list publishes `1024k` context/input and `256k` output,
  without exact integer units on that page. Numeric limits remain unknown
  rather than choosing between 1,024,000 and 1,048,576. The effort-support
  tables are not consistently pinned to both current IDs, so tiers remain
  unknown too. Set explicit profile limits to use a larger budget than the
  conservative unknown-model fallback. `doubao-seed-evolving` is a rolling
  alias; `doubao-seed-2-1-pro-260915` is pinned. See
  [thinking](https://docs.volcengine.com/docs/ark/deep-thinking).
- **Amazon Nova:** the **direct Nova API** IDs differ from Bedrock IDs. Pro is
  gated preview. Lite's official [model card](https://docs.aws.amazon.com/bedrock/latest/userguide/model-card-amazon-nova-2-lite.html)
  says 64K output, the [Nova 2 overview](https://docs.aws.amazon.com/nova/latest/nova2-userguide/what-is-nova-2.html)
  says 65,536, and [Converse](https://docs.aws.amazon.com/nova/latest/nova2-userguide/using-converse-api.html)
  caps at 65,000. Neither Lite's conflicting limit nor Pro's unpublished
  output cap is guessed. Direct API model IDs and compatibility/auth are
  documented at [Nova API](https://nova.amazon.com/dev/api) and
  [Nova documentation](https://nova.amazon.com/dev/documentation).
  Lite exposes low/medium/high effort; its separate disable spelling is not
  advertised as an OpenAI `none` effort. Bedrock SigV4/Converse support is not
  implemented by adding these catalog rows.

- **NVIDIA Nemotron 3.5 Super VL:** the official API ID includes the
  `nvidia/` namespace; the catalog alias therefore deliberately contains two
  `nvidia/` prefixes. 1M is a shared context bound. The example output request
  (16,384) is not a documented hard maximum; output stays unknown. Thinking
  uses `chat_template_kwargs.enable_thinking`, not standard effort tiers.
  The catalog does not add that vendor-specific control to Kroma.

## Additional endpoint and protocol qualifications

- **StepFun:** [quick start](https://platform.stepfun.com/docs/zh/quickstart/overview.md)
  uses https://api.stepfun.com/v1. Step 5 Preview and Step 3.7 Flash enumerate
  low/medium/high effort; Step 3.5 Flash is text-only and does not inherit them.
- **MiMo:** [first call](https://mimo.mi.com/docs/en-US/quick-start/first-api-call)
  and [live specs](https://mimo.mi.com/static/docs/quick-start/summary/model.md).
  The 1M/128K shorthand is stored conservatively as 1,000,000/128,000.
  Deep thinking is not an effort-tier list. The docs require api-key auth;
  the catalog does not add that header or promise a validated MiMo adapter.
- **Tencent:** [TokenHub protocols](https://cloud.tencent.com/document/product/1823/130079),
  [endpoint/auth configuration](https://cloud.tencent.com/document/product/1823/130078),
  [legacy compatibility](https://cloud.tencent.com/document/product/1729/111007).
  TokenHub hy3/hy4/hy4-preview currently have IDs verified but caps unknown.
  Legacy Hunyuan publishes separate input/output caps, not a combined context:
  its conservative planning window equals the input cap, not their sum.
- **Baidu:** [current list](https://cloud.baidu.com/doc/qianfan/s/rmh4stp0j)
  and [Chat API](https://cloud.baidu.com/doc/qianfan-api/s/3m7of64lb) identify
  https://qianfan.baidubce.com/v2. ERNIE 5.1 uses conservative 128k/119k bounds
  and an explicit maximum 65,536 output. No image/effort capabilities are
  inferred from older ERNIE or third-party models on the same service.
- **Legacy specialists:** GPT-5 Pro and GPT-5.3-Codex need Responses, not Chat
  Completions. Haiku 4.5 uses manual extended thinking, not effort tiers.
  Mistral model cards with hidden "+N" aliases do not justify guessed aliases
  or unstated output limits. Image capability is unknown when only an icon
  survived extraction, rather than being inferred from the model family.

## Matching and precedence

- Only explicitly enumerated official HTTPS hosts on port 443 are recognized.
  No suffix wildcard, workspace host, managed-cloud endpoint or lookalike host
  inherits a vendor identity. An arbitrary proxy still needs an explicit
  `catalog_model = "provider/model-id"`.
- Examples: OpenAI `https://api.openai.com/v1` (`responses`),
  Google `https://generativelanguage.googleapis.com/v1beta/openai/` (`chat`),
  Cohere `https://api.cohere.ai/compatibility/v1` (`chat`),
  MiniMax `https://api.minimax.io/anthropic` (`anthropic`),
  direct Nova `https://api.nova.amazon.com/v1` (`chat`). The catalog does not
  auto-select protocols or authentication.
- [Qwen regional URLs](https://www.alibabacloud.com/help/en/model-studio/base-url),
  [Kimi endpoints](https://platform.kimi.ai/docs/api/overview.md), and the GLM
  and MiniMax references above document the other enumerated official hosts.
- Endpoint limits still take precedence over catalog fallback fields. Explicit
  configuration is retained subject to known hard ceilings. An independent
  catalog input ceiling additionally bounds input planning without changing
  the total window shown in diagnostics or the output reserve.
- Explicit profile `thinking_levels` override the catalog. An **absent** list
  now correctly falls back to the catalog in both diagnostics and the runtime
  model's tier selector. `auto` remains available as provider default.
- This audit used public official documentation, not paid authenticated
  inference. Offline tests verify selection, limits and runtime propagation;
  they do not assert live availability or full protocol compatibility.
