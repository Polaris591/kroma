# Kroma post-success exploration: evidence-led review (2026-11-10)

## Verdict
Questionable for the objective of reliable low-cost completion, but no established generic Agent Loop bug. Do not change default behavior or force the model to stop on a green test.

## Evidence and method
- Inspected Kroma src/agent.rs run_inner and src/prompt.rs EXECUTION_GUIDANCE, plus installed Pi 1.5.0 pi-agent-core/dist/agent-loop.js.
- Compared all eight complex Rust recovery traces from round9 (four repetitions, original vs complete-function-read). Parsed persisted WAL assistant tool calls and actual Shell tool result exit/status, not model claims.
- Offline reproducible tool: benchmarks/round9_post_success_audit.py. A qualifying pass is a post-first-edit Shell result with exit_code 0 and an actual Cargo summary with at least one passing test. This is only a *proxy* for sufficient verification, not proof that every required check has been run. Classifications such as 'waste' require human inspection of the task instructions.
- All eight final source snapshots passed independent recovery acceptance. One original-guidance run hit the 165-second limit without a tool-free final answer.

## Detailed incident: round9 rust_restore_metadata, repeat 4, baseline

| Milestone | Observed |
|---|---|
| First edit | Tool #16 repaired restore_session missing model/usage metadata |
| First successful post-edit test | Tool #17, cargo test --locked --offline --lib sessions::tests::, exit_code=0, test result: ok. 10 passed, 0 failed |
| Model's explicit decision | Next assistant message said: 'The sessions suite passes. Let me confirm the fix is what actually changes the outcome by temporarily reverting, and also run adjacent regression suites.' |
| Unrequested negative-control detour | Tool #18 attempted Python-based revert and also ran Cargo. Python was not available; compound command still exited 0 because a later command completed. Model noticed the Python failure. |
| Reversion | Tool #19 performed targeted edit to undo the fix; #20 ran cargo test on the broken behavior, exit_code=101, confirming failure |
| Recovery | Tools #21 and #23 performed additional edits to restore the fix; tool #25 reran sessions::tests::, 10 passed |
| Expanding check scope | Tools #26–29 ran session, desktop_host, file_lock, catalog_file and usage tests, plus test listing |
| Timeout | Tool #30 did another source/diff check; model request #24 started but task hit 165-second deadline, no normal tool-free final answer |

After the first successful post-edit test, this run had 13 further tool calls (including 3 edits), compared with 5 further calls for its complete-function-read counterpart. Three cargo sessions::tests-related invocations at tools #17, #20 and #25 took ~21.9s, ~21.2s and ~21.4s respectively; the repeated source edits likely caused costly recompiles. These are measurements of this one trace, not estimated universal savings.

## Eight-trace offline summary

| Repeat | Baseline calls after first post-edit passing test | Complete-read calls after first passing test | Baseline normal finish | Complete-read normal finish |
|---:|---:|---:|---|---|
| 1 | 1 | 4 | yes | yes |
| 2 | 1 | 8 | yes | yes |
| 3 | 7 | 5 | yes | yes |
| 4 | 13 | 5 | no | yes |

Not all extra tests after the first green command are wasteful. The baseline often runs a wider lib suite once after the focused regression, an appropriate precaution. Round9 repeat 4 is distinct because the model explicitly reverts a passing fix without user request, triggering a failure/re-repair chain. This is not a tool-result evidence-loss incident: actual stdout shows 10 passed with exit_code 0, and the assistant explicitly acknowledges success.

## Mechanism and alternatives

1. **P1 · observed: unrequested regression cycling.** A voluntary revert after a valid fix created additional edits, recompiles and model turns; the user requested repair and relevant verification, not demonstration that reintroducing the bug breaks the tests.
2. **P2 · observed: scope expansion after success.** Test re-execution and unrelated suites add time, but do not classify broad tests as errors without checking whether an API or behavior change warrants them.
3. **P2 · inspected source: a normal finish is model-led.** Kroma's basic loop finishes on a response without tool calls (with steering/host exceptions). Pi likewise continues when the model requests tools. There is no evidence that Pi's usual loop has a generic green-test forced-stop mechanism. Current Kroma EXECUTION_GUIDANCE already instructs finishing after required passing checks.
4. **P2 · observed: compound-shell result semantics.** Tool #18 reported exit_code 0 despite intermediate missing Python; a subsequent command completed successfully. Shell semantics explain it. The model observed the error in stderr. Do not silently change Bash execution semantics to forbid independent command sequences.

## Contemporary research
- Microsoft AgentLens (May 2026): reports 10.7% 'Lucky Pass' trajectories in its passing-trajectory subset and stresses classifying process quality beyond binary correctness. https://www.microsoft.com/en-us/research/publication/agentlens-revealing-the-lucky-pass-problem-in-swe-agent-evaluation/
- Hu et al., Analyzing and Mitigating Cost-Inefficient Behaviors in Coding Agents (revised October 2026): identifies subsumed retrieval, repeated scripts and test re-execution in large trajectory sets; structure-aware retrieval can increase costs, so more tooling does not automatically help. https://arxiv.org/abs/2609.30725
- Weinberger & Hozez, Prompt-Induced Waste (August 2026): prompting for multiple approaches / certainty can increase cost without accuracy gain; compact task scope and explicit stopping criteria sometimes help, but impact is model- and harness-specific. https://arxiv.org/abs/2608.01347

## Next discriminating test, NOT executed in this read-only review
Compare current default against a narrowly scoped experimental *replacement* of its verification sentence: after repairing a real failing case and seeing requested tests pass, do not intentionally revert a passing fix just to prove the old failure unless the user explicitly asked for that negative control; still run any regression suites genuinely necessary for changed behavior. Keep the same model, endpoint, High, task instructions, tool schema, and output. Cross over on >=2 different complex Rust tasks plus a short Rust control; judge independent correctness and requested-test compliance first, then unintended reverts, success-after-first-pass turns, wall time and cache-adjusted tokens. Explicitly score a counterexample where the first green test misses a genuine regression, to reject a policy that stops too early. Existing prompt already tells the model to finish; if the replacement gives no out-of-sample benefit, drop it.

Do NOT implement hard turn-count stops, tool vetoes, a verifier agent, a mandatory planner, or a global 'one test only' rule. No product fix is justified yet.

## Implementation status
Offline audit script created, its self-test passed, and eight existing traces analyzed. No billable model trial in this review. The existing experimental research worktree has older uncommitted round7–round9 changes; main remains untouched.
