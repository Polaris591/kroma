# F05: disposition of all 27 harnesses

Generated from the taskbook's own `harnesses.json` (27 entries). Availability was probed on this host:
`pi` and `codex` are both installed (codex only reachable by full path, its npm bin dir is not on PATH); nothing else in the list is installed and none of the others can be obtained
here (no package, no network source that resolves to a runnable harness). So the comparable subset has two
members and no leaderboard can be produced over the rest -- which is exactly what the task asks to record.

| id | harness | availability on this host | why it is not comparable here |
|---|---|---|---|
| H01 | Pi Coding Agent | installed (`pi`), runnable | -- comparable: this is the only harness that could be run here |
| H02 | DeepSeek Harness / DSH | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H03 | Claude Code | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H04 | OpenAI Codex | installed (`/usr/local/lib/nodejs/bin/codex`, codex-cli 0.163.0), runnable | -- comparable: second member of the subset |
| H05 | Jcode | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H06 | OpenCode | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H07 | OpenHands | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H08 | Gemini CLI | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H09 | SWE-agent | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H10 | mini-SWE-agent | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H11 | Aider | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H12 | Goose | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H13 | DeepAgents | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H14 | Cline | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H15 | Hermes Agent | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H16 | Qwen Code | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H17 | openJiuwen / JiuwenSwarm | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H18 | Plandex | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H19 | Kimi Code CLI | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H20 | Mistral Vibe | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H21 | OpenClaw | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H22 | Continue | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H23 | Kilo Code | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H24 | Cursor Agent | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H25 | Amp | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H26 | GitHub Copilot CLI | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H27 | Factory Droid | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |

Comparable subset: 2 of 27 (pi, codex-cli 0.163.0). The other 25 have no run, therefore no score, therefore no place
in a leaderboard; the reason for each is the same and is stated per row above rather than summarised as a number.
Probe: `command -v pi codex`, `npm ls -g`, and `/usr/local/lib/nodejs/bin/codex --version` (prints codex-cli 0.163.0);
output in `evidence/f05_availability.txt`. A leaderboard over the subset needs both harnesses run on the same frozen
tasks; codex needs an OpenAI credential that is not configured here, so the subset is recorded as comparable but unranked.
