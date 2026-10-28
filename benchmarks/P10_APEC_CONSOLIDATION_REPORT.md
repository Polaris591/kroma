# P10: APEC incremental-summary consolidation cost probe

Date: 2026-10-27. Production APEC strategy **unchanged**.

## Question

P7 had 34 successful background summary commits, requiring 107 model
requests. Replay of its frozen message histories accounted for 68 initial
chunk summaries, 33 consolidations inferred from runtime logs/algorithm,
6 truncation retries and no timeout retries. Can a consolidation request be
eliminated without increasing downstream prompt costs or losing facts?

## Probe

Used one frozen P7 Rust CLI history segment with exactly two APEC source
chunks under the production 86,732-byte limit. The configured existing
model produced the two initial summaries **once**. These exact summaries
were either retained together, or sent through one additional
`ModelClient::condense` request. Compared generated memory sizes, provider
usage and six simple requirement anchors.

| Metric | No consolidation | With consolidation |
|---|---:|---:|
| Initial summaries | 2 | 2 |
| Additional model requests | 0 | 1 |
| Summary memory bytes | 3,810 | 2,535 |
| Additional provider input tokens | 0 | 1,013 |
| Additional provider output tokens | 0 | 841 |
| Additional cached input tokens | 0 | 0 |
| Requirement anchors present | 6/6 | 6/6 |

Consolidation saved 1,275 UTF-8 memory bytes for later context views,
but incurred an extra request and reported 1,854 input+output tokens
for the immediate consolidation. Future token savings depend on model
tokenization, cache hit behavior and how often this summary is reused.

### Decision

**No change to the production consolidation threshold.** This one-case
measurement is not sufficient to claim the extra request is redundant or
that skipping it lowers full-session cost. Keyword anchors cannot establish
complete memory accuracy. To justify changing the rule, compare across
several languages, histories and downstream model-request counts, rather
than introducing adaptive policies prematurely.

### Reproduce

`KROMA_P10_APEC_LIVE=1 cargo test --locked --offline p10_apec_consolidation_probe -- --ignored --nocapture`

Raw isolated probe results: `/root/kroma-p9-apec-ab/p10_consolidation.json`.
No API key is copied to reports; this test is ignored by default.
