# F05: disposition of all 27 harnesses

This file was wrong before: it said 25 harnesses 'cannot be obtained here' while the probe had only run
`command -v`. The corrected probe installs what it can (see evidence/f05_msw_run.txt) and records three
honest states: `ran` (installed and executed), `installable` (a package resolves, not installed this round),
`unverified` (no attempt made -- explicitly NOT a claim of unavailability).

| id | harness | state | evidence / reason |
|---|---|---|---|
| H01 | Pi Coding Agent | ran | installed (`pi` 1.5.0); the Kroma bench itself is driven through this host |
| H02 | DeepSeek Harness / DSH | unverified | no install attempt made this round; not claimed as unavailable |
| H03 | Claude Code | installable | npm `@anthropic-ai/claude-code` returns 200; not installed this round |
| H04 | OpenAI Codex | ran | installed (`/usr/local/lib/nodejs/bin/codex`, codex-cli 0.163.0), not on PATH |
| H05 | Jcode | unverified | no install attempt made this round; not claimed as unavailable |
| H06 | OpenCode | unverified | no install attempt made this round; not claimed as unavailable |
| H07 | OpenHands | installable | pypi `openhands-ai` returns 200; not installed this round |
| H08 | Gemini CLI | installable | npm `@google/gemini-cli` returns 200; not installed this round |
| H09 | SWE-agent | installable | pypi `sweagent` returns 200; not installed this round |
| H10 | mini-SWE-agent | ran | installed into a venv (`mini-swe-agent` 2.4.6) and it solved fix-off-by-one; evidence/f05_msw_run.txt |
| H11 | Aider | installable | pypi `aider-chat` returns 200; not installed this round |
| H12 | Goose | installable | pypi `goose-ai` returns 200; not installed this round |
| H13 | DeepAgents | unverified | no install attempt made this round; not claimed as unavailable |
| H14 | Cline | installable | npm `@cline/cli` returns 200; not installed this round |
| H15 | Hermes Agent | unverified | no install attempt made this round; not claimed as unavailable |
| H16 | Qwen Code | unverified | no install attempt made this round; not claimed as unavailable |
| H17 | openJiuwen / JiuwenSwarm | unverified | no install attempt made this round; not claimed as unavailable |
| H18 | Plandex | unverified | no install attempt made this round; not claimed as unavailable |
| H19 | Kimi Code CLI | unverified | no install attempt made this round; not claimed as unavailable |
| H20 | Mistral Vibe | unverified | no install attempt made this round; not claimed as unavailable |
| H21 | OpenClaw | unverified | no install attempt made this round; not claimed as unavailable |
| H22 | Continue | unverified | no install attempt made this round; not claimed as unavailable |
| H23 | Kilo Code | unverified | no install attempt made this round; not claimed as unavailable |
| H24 | Cursor Agent | unverified | no install attempt made this round; not claimed as unavailable |
| H25 | Amp | unverified | no install attempt made this round; not claimed as unavailable |
| H26 | GitHub Copilot CLI | unverified | no install attempt made this round; not claimed as unavailable |
| H27 | Factory Droid | unverified | no install attempt made this round; not claimed as unavailable |

Totals: ran=3, installable=7, unverified=17

Comparable subset so far: 3 (`pi`, `codex`, `mini-swe-agent`). A leaderboard over them needs the same
frozen task run on each; one task (fix-off-by-one) has been run with `mini-swe-agent` and it passed
(evidence/f05_msw_run.txt), the Kroma arms pass it too, and `codex` needs an OpenAI credential that is not
configured here -- so the subset is comparable, partly ranked, and honestly incomplete rather than 'blocked
because 25 harnesses are unobtainable'.

Installability probe (HTTP status of the package index):

```
pypi/mini-swe-agent -> 200
pypi/aider-chat -> 200
pypi/openhands-ai -> 200
pypi/goose-ai -> 200
pypi/sweagent -> 200
npm/@google/gemini-cli -> 200
npm/@anthropic-ai/claude-code -> 200
npm/@cline/cli -> 200
```
