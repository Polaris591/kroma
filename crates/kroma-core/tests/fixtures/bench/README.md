# Held-out bench smoke suite

`held_out_smoke.json` is a **small** held-out task set for `kroma bench`:

* 3 self-contained tasks, each declaring its initial files, its prompt, and an independent
  behavioural `verify_command`.
* **Behavioural checks only.** Every task is graded by running real Python and asserting observable
  behaviour (return values and the exception type). No check greps for a specific spelling, so an
  equivalent fix passes and a comment repeating the "expected" text does not.
  `crates/kroma-core/tests/bench_contract.rs` proves both directions.
* The grader runs **outside** the agent (in the arm's isolated workspace, as a separate process),
  never writes an expected answer, and the model cannot satisfy a task by editing the check.
* The tasks need only the file tools (`read`/`write`/`edit`) — none of them needs a shell. Under
  `--allow-write` (no `--allow-shell`) both arms therefore have the same capability envelope.
  A task that genuinely needs a shell (for example deleting a file, which the file-tool set cannot
  express) requires `--allow-shell`; `kroma bench` prints that reminder.
* Three tasks cannot establish that a mechanism improves real task resolution — they can surface a
  large regression, a scoring bug or a broken executor. Treat `pass_rate` differences as
  diagnostics, not evidence.

Requirements: `python3` on the machine that runs the bench.

Run it with:

```
kroma bench --suite crates/kroma-core/tests/fixtures/bench/held_out_smoke.json \
  --executor provider --allow-write --budget-tokens 60000 --repetitions 1 --json
```

The default `single_agent` arm hides `spawn_subagent` (`max_subagents=0`), so a "single agent"
number can never contain child runs; the adaptive arm keeps the configured quota. Both envelopes are
recorded on every result and printed in the report's `capabilities:` block.
