# SR-002 — Flagship model catalog update

## Scope

Self-review of README.md, docs/model-catalog.md,
src/model_catalog.rs, src/model_catalog/records.rs, src/config.rs,
src/app.rs, src/context.rs and tests/model_catalog_e2e.rs.
Existing untracked deep-review probes and local PowerShell utilities are not
part of this change. No CI gates or dependencies changed.

The independent reviewer attempt ended with HTTP 524 and produced no verdict;
this report does not count that attempt as a passed review.

## Findings

| ID | Severity | Location | Finding |
| --- | --- | --- | --- |
| SR-002-1 | medium | src/context.rs, Compression::summary_source | The first implementation applied independent input limits only to normal coding input. Checkpoint selection still checked only input + output against the shared window and could exceed a smaller independent input ceiling. |

## Dispositions

- SR-002-1: **fixed**. ContextConfig retains the independent ceiling even when
  output reservation already makes available coding input smaller. Checkpoint
  source selection checks that ceiling as well as the shared window, shortening
  only at safe conversation boundaries. The regression uses a 32K shared
  window and a 1,800-token input ceiling: the uncapped source fits the window,
  while the capped source must shorten and stay below the separate limit.

## Review notes

- Correctness: no provider defaults are promoted to documented hard maxima;
  uncertain/contradictory limits remain None. Profiles with absent tiers no
  longer suppress catalog fallback; explicit profile tiers retain precedence.
- Security: official hosts remain enumerated HTTPS/default-port matches.
  Proxy opt-in, exact case-sensitive IDs and unknown-model fallback survive.
- Architecture: the data table is separate from resolution logic; input-only
  limits remain separate from the shared window and output reservation.
- Performance: no new network requests in the completion path, dependencies,
  tasks or unbounded resources. Catalog lookup is a scan of 30 static records.
- Verification: behavior tests cover host spoofing, namespaced IDs, tier
  propagation, independent input budgets, checkpoint limits, explicit override
  precedence and unknown limits. Full test/lint/format gates are run by the
  parent session; public paid-provider inference is not claimed.

## Verdict

No remaining blocking code findings in the reviewed scope. Catalog membership
is a metadata statement, not a guarantee of account access or every vendor's
protocol extension; the snapshot documents those qualifications.

## Gate results

- `cargo test --locked --offline`: exit 0; library 563 passed / 4 ignored,
  all integration suites passed, including all 4 catalog end-to-end tests.
- `cargo clippy --locked --offline --all-targets --all-features -- -D warnings`:
  exit 0.
- `cargo fmt --all -- --check` and `git diff --check`: exit 0.
- `cargo check --locked --offline --manifest-path desktop/src-tauri/Cargo.toml --all-targets`:
  exit 0 (before the final static-record additions; no desktop API changed).

## Expanded scope after the completeness feedback

The second pass expands the same review to 110 exact API IDs / 18 providers,
including still-served legacy, coding/vision models and documented snapshots.
Reviewed files additionally include reviews/catalog-expansion/*.json (research
and curated audit evidence), plus the expanded records/docs and catalog tests.
CI, dependencies, unsafe code and completion protocols are unchanged.

| ID | Severity | Location | Finding |
| --- | --- | --- | --- |
| SR-002-2 | medium | src/model_catalog/records.rs, Gemini4 Argon | The earlier record advertised minimal although the current official thinking table lists only low/medium/high. Tier selection would offer an unsupported setting. |
| SR-002-3 | medium | reviews/catalog-expansion/domestic-missing.json, candidate records | Some research candidates inferred binary counts from shorthand alone, and an older indexed ERNIE5.0 table disagreed with the direct current official directory. Accepting the candidates verbatim would overstate budgets or current ID coverage. |

## Dispositions (expanded pass)

- SR-002-2: **fixed**. The compiled record and per-model regression now use
  low/medium/high. No family-wide none/off propagation to OpenAI Astra/Sol.
- SR-002-3: **fixed** before integration. The parent fetched per-model OpenAI
  and Claude specs, the direct current Baidu directory and Tencent overview;
  replaced tentative binary shorthand counts with documented conservative
  decimal bounds, withheld the stale ERNIE5.0 IDs, and selected stricter Qwen
  thinking-mode input limits. curated.json is the reviewed record set; the
  original research JSON files remain provenance, not runtime inputs.

### Five-axis follow-up

- Correctness: explicit aliases share only source-confirmed snapshot limits;
  retired IDs remain excluded. Unknown Hy4 caps remain None. Lifecycle and
  existing-account-only restrictions are documented rather than disguised as
  general access. New offline runtime tests passed for domestic/legacy aliases.
- Readability/architecture: sorted, typed static records remain separate from
  logic; the larger file is data, not additional resolver branches or a macro
  DSL. Source links accompany each documented ID. Raw research is not compiled.
- Security: six additional exact public hosts, HTTPS/443 unchanged. Lookalike,
  custom-proxy and nondefault-port tests apply to every new host. This does not
  implement vendor authentication (MiMo api-key remains an explicit caveat).
- Performance: a bounded scan of 110 static records, no added network requests,
  dependencies or runtime tasks. No changes to Kroma's coding output limit.
- Verification: the first new E2E test compile found an incorrect constructor
  signature and a call to a private helper; changed it to the existing public
  Config::from_profile and app::model_from_profile APIs. Focused 12 catalog
  unit tests and all 5 catalog E2E tests then passed.

## Test-environment finding and final gates (110-ID snapshot)

The final scope also includes tests/cli_steering_input.rs. A normal-HOME full
run passed all 565 library tests but then failed in CLI steering: reading the
child's stderr as UTF-8 failed. A fresh diagnostic reproduction captured its
raw bytes and session WAL. Personal Blender/Unity MCP servers were starting,
adding 57 tools; their stderr included non-UTF-8 output and their schemas pushed
the 32K fixture over budget before any model request. This was not a catalog
resolution failure. The CLI fixture now writes an empty, per-test MCP config
and explicitly selects it, without weakening its steering assertions.

The next run passed that fixture but exposed the same ambient-MCP assumption
in the MCP integration suite (58 vs 1, 59 vs 2, and 57 vs 0 tool counts).
Rather than remove assertions or disable project MCP checks, validation uses
a fresh empty HOME, preserving the actual CARGO_HOME/RUSTUP_HOME. This excludes
personal MCP configuration while leaving all project MCP/trust tests enabled.
The MCP suite then passed all 5 tests; the complete suite subsequently passed.
The user's personal configuration was not modified.

Final results:

- `cargo test --locked --offline` with isolated HOME: exit **0**; library
  **565 passed / 4 ignored**, main **1 passed**, every integration suite passed
  (catalog **5**, MCP **5**, plugins **51**); pre-existing timing lane ignored.
- Complete test log: `%TEMP%/kroma-catalog-110-isolated-tests.log`.
- `cargo clippy --locked --offline --all-targets --all-features -- -D warnings`:
  exit **0**, including the corrected CLI fixture.
- `cargo fmt --all -- --check` and `git diff --check`: exit **0**.
- `cargo check --locked --offline --manifest-path desktop/src-tauri/Cargo.toml --all-targets`:
  exit **0**, after the 110-record production expansion.
- Documentation consistency: **110 unique sourced IDs** match curated.json;
  compiled-record tests independently check uniqueness and **18 providers**.

## Final verdict

No remaining blocking code findings in this metadata expansion. This is not
proof of paid API inference, account eligibility, or every vendor's wire/auth
extensions. The documentation explicitly retains the remaining vendor/spec
coverage gaps instead of calling the snapshot a complete global directory.
