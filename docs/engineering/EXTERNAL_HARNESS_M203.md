# M2-03：外部 harness 对照

数据：`/opt/kroma-v4/evidence/m2-03/`（`codex-dev.json`、`run.log`）。harness：`scripts/run-external-agent.py`。

选型：OpenAI Codex CLI `0.163.0`，Apache-2.0，命令
`codex exec --skip-git-repo-check -s workspace-write -m gpt-6-luna <prompt>`，
端点 cobblemon（`wire_api = "responses"`）。aider-chat 在 Python 3.13 上装不上，已放弃。

同模型（gpt-6-luna）、同套件（`d102119d…`）、同工作区外验证器：

| harness | 通过 | token | 墙钟 |
|---|---|---|---|
| kroma single_agent | 6/6 | 93 752 | 约 320 s |
| codex-cli 0.163.0 | 6/6 | 26 671 | 155 s |

3.5 倍的成本差是整条 harness 的差（提示词、工具集、循环步数），不归因单一算法；
两者质量相同，说明该套件对这两个 harness 都没有区分力。

意义：机制层面（E02/E04/E07/M2-02）全部显示"更多机制只会更贵"，
而外部 harness 说明**固定开销**还有 3.5 倍空间——该优化的是基线，不是继续叠加机制。
