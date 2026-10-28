# P12: Architecture Foundation review repairs

Source: P11 `0756d4f`. No change to the built-in TRC/APEC chunking, thresholds,
summary content, retry policy, WAL on-disk format, or default model request
payload. No dynamic plugin loader or approval/sandbox system added.

## Verified fixes

1. **Raw history is authoritative.** ContextCommit now takes only
   PersistedContext. The Agent binds the current immutable history to a
   private SessionStore committer. An extension cannot replace messages when
   committing its own compression state. The forged-projection regression
   verifies raw history survives while model-visible projection can change.
2. **Final request capacity.** Prompt contributions and tool schemas enter
   context planning as additional estimated tokens. After the complete
   request projection is assembled, a conservative no-network check rejects
   an over-budget payload. Regression covers giant prompt and giant tool
   schema, both with zero provider calls. The built-in path retains the
   previous planning thresholds and behavior on the golden task.
3. **Tokio Send.** Model and policy futures, streamed callbacks and Agent's
   task future are Send. A multi-thread Tokio test spawns an Agent task.
4. **Prompt switch/resume.** Explicit base-system replacement applies only to
   the outbound request projection, including existing sessions; the durable
   system entry remains unchanged. Default built-in prompt stays byte-identical.
5. **Tool schema guidance.** When custom tools are registered, the default
   system prompt no longer claims the only tool is Shell. Existing Shell-only
   prompts stay unchanged.
6. **Durable event semantics.** ModelFinished and ToolFinished are emitted
   after their WAL append succeeds. ToolFinished carries the exact result,
   which can reference a complete capture file for large Shell output. The
   regression checks both events against the WAL at notification time.
7. **Context budget interface.** ContextConfig has read-only accessors and an
   externally callable constructor; simple ContextPolicy lifecycle hooks now
   provide default implementations.
8. **Model separation.** ModelBackend exposes stream completion and generic,
   tool-free `generate_text`. APEC-specific system prompts and history JSON
   live in context.rs; their contents and serialized request layout are
   unchanged. The provider does not append any factual-memory instructions
   to unrelated callers of generic text generation. Dedicated offline tests
   check the exact built-in memory prompts and tool-free generic payload.
   Shared result types no longer live in the HTTP implementation.
9. **Unavailable strategy recovery.** `kroma recover-context [session-id]`
   explicitly resets derived context state to the built-in strategy, retaining
   all original conversation and tool messages. This is intentionally not an
   implicit fallback or file rollback. Existing session locks still apply.
10. **P0 artifacts.** Track the frozen P0 golden fixture, comparison script and
    baseline report so the wire contract can be checked on another checkout.

## Verification

```sh
cargo fmt --check
cargo test --locked --offline -q
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --release --locked --offline -q
python3 benchmarks/architecture_contract.py
python3 benchmarks/p11_architecture_baseline.py
python3 benchmarks/p9_batch1_regression.py
python3 benchmarks/p9_batch1_legacy_regression.py
python3 benchmarks/p9_batch2_regression.py
python3 benchmarks/p10_config_regression.py
python3 benchmarks/mock_profile_smoke.py
```

The test suite also includes direct extension-policy, prompt over-budget,
durable event, multi-thread spawn and custom context recovery regressions.

## Explicit limitations

- Capacity checks are conservative byte-based estimates, not a provider
  tokenizer; new schemas can trigger an early refusal rather than
  automatically compressing tool definitions.
- EventSink callbacks are synchronous and must be lightweight.
- Recovery clears derived summaries; they are not migrated across unrelated
  compression algorithms. Recover-context does not undo workspace file edits.
- These remain internal Rust interfaces; there is no stable plugin ABI yet.
