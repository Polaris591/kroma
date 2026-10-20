# Gate 7 (`v1.2.0`) Acceptance Report — Frontier Priority 1

Three frontier capabilities, each pinned by a test that would fail if the capability regressed.

## 1. Symbol dependency graph + Personalized PageRank navigation (`code_graph`)

`crates/harness-core/src/repo_graph.rs` builds a deterministic file ⇄ symbol graph by walking the
workspace through `WorkspaceCap` (so symlink, sensitive-path and ignore rules are identical to
`find`/`grep`). Follows RepoGraph (ICLR 2025) and LocAgent (ACL 2025).

- **Definitions.** Line-oriented extractor with per-language rules for Rust, Python, TS/JS, Go,
  C/C++ and Java, including `impl <Type>` targets (a type-name query needs them) and
  `func (r *T) Name(` receivers.
- **Edges.** Comment- and string-stripped identifier sets are matched against symbol definitions in
  *other* files; each edge is weighted by reference count and by symbol rarity
  (`specificity / defining_file_count`), capped so one ubiquitous symbol cannot dominate.
- **Ranking.** Personalized PageRank, 15 iterations, dangling mass redistributed over the
  personalization vector. Two damping factors: `0.85` for a cold start (uniform restart → degree
  centrality, i.e. "what is the core of this repo?") and `0.35` for a seeded query (localized walk).
  Seeds are the files matching the query terms / `focus_files`, **plus their one-hop neighbours**, so
  the ranking reads "the file you asked about, then its call chain" instead of "the file, then the
  repository's hubs".
- **Kernel tool.** `code_graph` (`EffectClass::ReadOnly`, 11th built-in tool) is available to the
  parent agent and to a read-only `Scout` subagent. `--policy read-only` and `--tool-filter` gate it
  exactly like every other tool. `focus_files` entries are validated through
  `WorkspaceCap::validate_relative_components`, so a path escape is rejected.
- **Other surfaces.** REPL `/graph [<query>]` and JSON-RPC `workspace.graph` (advertised in
  `initialize` under `capabilities.graph`), so the IDE/daemon surfaces get the same navigation.

Measured on this repository (`docs/GATE7_EVAL_METRICS.json`): 64 files, 1420 symbols, 6331 edges,
58 ms full build, 0.1 ms average query, **10/10 Top-1** on a 10-query symbol benchmark.

## 2. Native multimodal vision input in `read`

`execute_read` now sniffs magic bytes for PNG/JPEG/GIF/WebP and parses exact pixel dimensions in pure
Rust (PNG IHDR, JPEG SOF0..SOF15, GIF logical screen descriptor, WebP VP8X/VP8/VP8L). Up to 1 MiB of
image data is base64-encoded (hand-rolled RFC 4648, zero new dependencies) and attached as an
`ImageAttachment { mime_type, base64_data, width, height, byte_len }` while `content` keeps a
one-line summary. The sniffed MIME type must also agree with the file extension, and a file that
claims to be an image but exceeds the cap fails immediately with that reason instead of falling
through to the 8 MiB binary scanner.

Design note: the attachment list lives on `ToolResultMeta`, which is the metadata struct *inside*
`ToolResultMessage`. That keeps the field WAL-persisted, projected and byte-accounted with the
message it belongs to without forcing 29 existing `ToolResultMessage` literals to change.
`ToolResultMessage::image_attachments()` / `image_attachment_bytes()` expose it.

- **Binary rejection is preserved.** A file that merely *claims* to be an image (`.png` with
  `\x00\x01\x02` bytes) is still rejected as binary; the image branch only fires when the magic bytes
  agree. The sniff runs on a dedicated handle so a non-image file is not read from a stale offset.
- **Provider projection.** Chat Completions emits `image_url` content parts; Anthropic emits
  `{type:"image", source:{type:"base64"}}` inside `tool_result`; Responses emits a follow-up user turn
  with `input_image` parts (its `function_call_output.output` must stay a string).
- **Budget.** The base64 length is counted by `TranscriptItem::estimated_bytes`, by
  `ContextBudgetBreakdown.tool_output_bytes` and by the `max_total_tool_output_bytes` check, and
  Step-Down Compaction clears the attachments together with the raw text, so older turns keep only
  the summary. The cap (1 MiB raw → ≤1.4 MiB base64) sits below the default high watermark
  (1.5 MiB), and if an attachment still cannot fit — e.g. several images in the protected tail
  window — the projector evicts every image but the newest (then the newest too) instead of
  returning `ContextBudgetExceeded`, so a valid multimodal `read` can never abort a run.

## 3. Native Anthropic `/v1/messages` and OpenAI `/v1/responses`

`--protocol auto|chat-completions|anthropic-messages|openai-responses` (or `HARNESS_PROTOCOL`)
selects the wire format; `auto` detects Anthropic from the base URL or a `claude-` model prefix and
Responses from a `/responses` base URL, and otherwise keeps the existing Chat Completions default.

- **Anthropic.** `x-api-key` + `anthropic-version: 2023-06-01` + `anthropic-beta:
  prompt-caching-2024-07-31`, and **three** `cache_control: {"type":"ephemeral"}` breakpoints — the
  static system block, the last tool definition, and the projector's frozen-prefix boundary
  (`PinnedBaselineEnd`, falling back to `TailWindowStart`). Full `thinking`/`signature` and
  `tool_use`/`tool_result` round-tripping; SSE assembly maps `cache_read_input_tokens` and
  `cache_creation_input_tokens` into `TokenUsage` so the existing cache-hit metric keeps working.
- **OpenAI Responses.** `instructions` + typed `input` items (`message`, `reasoning`,
  `function_call`, `function_call_output`, `input_image`), `tools[]` in the flat Responses shape,
  `store: false`, and streaming assembly for `response.output_item.added` /
  `response.function_call_arguments.delta` / `response.output_text.delta` / `response.completed`,
  with `usage.input_tokens_details.cached_tokens`.
- **Shared transport.** The hand-written TLS + HTTP/1.1 machinery moved into
  `provider/transport.rs` (and URL validation into `provider/endpoint.rs`, re-exported from
  `chat_completions` so the existing public path is unchanged). All three protocols now share
  connection, header parsing, chunked/content-length framing, SSE event framing and the
  transient/fatal/mid-stream failure classification, instead of the transport being duplicated three
  times. `validate_base_url` security rules are byte-identical across protocols.

`ProviderRequest` gained `cache_breakpoints`, populated from `ProjectedContext.breakpoints` in the
agent loop, so the protocol client can pin the frozen prefix without knowing anything about the
projector.

## 5. Post-review hardening (final `v1.2.0`)

A 3-way independent review of the first `v1.2.0` build found that a multimodal image escaped
Step-Down Compaction, and that both new protocols accepted truncated SSE streams as complete turns.
Both classes are fixed and pinned by regression tests in
`crates/harness-core/tests/gate7_repograph_vision_protocols.rs`.

- **Images were un-compactable.** `run_step_down_compaction` tested `tr.content.len()` (a ~70-byte
  summary) against `MIN_COMPACTABLE_TOOL_BYTES`, so every image was skipped, the pinned baseline
  advanced past it, and the base64 payload was locked into the immutable prefix. The test now uses
  `content.len() + image_attachment_bytes()` and reclaims the full payload on both the projected item
  and the canonical transcript. `tool_output_bytes` / `raw_tool_bytes` count the payload too.
- **Images could abort a run.** A 4 MiB cap was 2.7x the 1.5 MiB high watermark, and the tail window
  protects the newest turns unconditionally, so reading one large screenshot could fail the
  projection with `ContextBudgetExceeded`. The cap is now 1 MiB and an emergency eviction pass drops
  the oldest image payloads (then the newest) before that error is ever returned.
- **Truncated streams were accepted.** OpenAI Responses promoted in-flight `function_call` items to
  `{}` calls on a stream that never sent `response.completed`; Anthropic ignored a missing
  `message_stop` and silently dropped unclosed `tool_use` blocks. Both now return
  `ProviderError::IncompleteStream`, so a half-streamed tool call is never executed.
- **Anthropic wire validity.** Wire messages now strictly alternate `user`/`assistant` (consecutive
  same-role turns are merged with `tool_result` blocks first), assistant turns lead with
  `thinking`/`redacted_thinking`, empty text blocks and an empty `system` field are omitted, and the
  `cache_control` prefix breakpoint is translated through the transcript→wire merge map instead of
  drifting forward onto the newest turn.
- **RepoGraph.** `const `/`type ` prefixes trim leading whitespace before `ident_at` and slicing, so
  `const  函数 = () => {}` no longer panics on a UTF-8 char boundary, and `called_by` / `calls_into`
  aggregate edge weights per neighbour file instead of listing the same path once per symbol.

## Verification

| Check | Result |
| --- | --- |
| `cargo test --workspace` | **165 passed, 0 failed, 0 warnings** |
| `cargo check --workspace --target x86_64-pc-windows-gnu --all-targets` | 0 errors, 0 warnings |
| `cargo build --workspace --release` | ok (`harness 1.2.0`) |
| `cargo build --workspace --target x86_64-pc-windows-gnu --release` | ok (`harness.exe`) |
| New tests (`tests/gate7_repograph_vision_protocols.rs`) | 12 passed |
| Live REPL `/graph <symbol>` on this repository | 10/10 Top-1 (62 files, 1436 symbols, 6234 edges) |
| Live REPL `/graph authenticate_user` on a scratch workspace | `src/auth.rs` ranked #1 with `db.rs` in `calls_into`, `router.rs` in `called_by` |
| Live ACP `workspace.graph` over stdio | `capabilities.graph = true`, ranked list returned |

`docs/GATE7_EVAL_METRICS.json` records the measured numbers (not estimates).
