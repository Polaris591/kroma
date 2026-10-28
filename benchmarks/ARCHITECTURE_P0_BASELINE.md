# Architecture Foundation P0 baseline

Starting Git commit: `6f360c2`. Single Rust Cargo package, 11 source
modules. This is a **behavior-preserving architecture refactor**, not a
new compression strategy or plugin runtime.

## P0 frozen behavior

- `python3 benchmarks/architecture_contract.py` validates **the complete**
  JSON request bodies for both turns of a deterministic offline task:
  prompt, tool definitions, thinking parameter, tool-call replay, output
  and stable system-prompt projection. Random temporary workdir paths
  are normalized. The fixture is recorded once and changes must be
  explicitly reviewed, never silently regenerated.
- Existing independent regressions: `mock_profile_smoke.py`,
  `p9_batch1_regression.py`, `p9_batch1_legacy_regression.py`,
  `p9_batch2_regression.py`, `p10_config_regression.py`.
- Rust tests include session recovery, WAL write failure, cancelled
  commands, DeepSeek SSE, TRC planning and APEC snapshot consistency.
- Keep foreground model request and tool-call order unchanged. Preserve
  raw transcript / v3 checkpoint + WAL on disk with no automatic
  re-execution of interrupted commands.

## Target P1–P4 invariants

- Single Cargo package; no plugin loading, hot-reload, remote sandbox,
  approval layer, new model calls or altered TRC/APEC thresholds.
- Model and tool implementations can be swapped in internal tests
  without modifying the Agent loop.
- Context policy can be swapped without changing the Agent loop or
  sacrificing durable state. Legacy v3 session files remain readable.
- Prompt composition and runtime events remain lightweight; no
  model-visible prompt change for the built-in runtime.
- Run all deterministic regressions at each milestone and commit
  each independently.
