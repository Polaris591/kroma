# F05: disposition of all 27 harnesses

Generated from the taskbook's own `harnesses.json` (27 entries). Availability was probed on this host:
`pi` is installed (codex is NOT on this host); nothing else in the list is installed and none of the others can be obtained
here (no package, no network source that resolves to a runnable harness). So the comparable subset has two
members and no leaderboard can be produced over the rest -- which is exactly what the task asks to record.

| id | harness | availability on this host | why it is not comparable here |
|---|---|---|---|
| H01 | Pi Coding Agent | installed (`pi`), runnable | -- comparable: this is the only harness that could be run here |
| H02 | DeepSeek Harness / DSH | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H03 | Claude Code | not installed, not obtainable | not installed and no obtainable package/source on this host, so no run could be produced |
| H04 | OpenAI Codex | not installed (probe: `command -v codex` empty) | not obtainable here, so no run |
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

Comparable subset: 1 of 27 (pi). The other 26 have no run, therefore no score, therefore no place
in a leaderboard; the reason for each is the same and is stated per row above rather than summarised as a number.
Probe: `command -v pi codex` and a scan of installed packages; output in `evidence/f05_availability.txt`.
