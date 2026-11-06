# Configuration reference

## Profile file

`~/.kroma/config.json`, or the path in `KROMA_CONFIG_FILE`. It contains named
profiles and one active profile:

```json
{
  "active": "default",
  "profiles": {
    "default": {
      "base_url": "https://api.deepseek.com/v1",
      "model": "deepseek-chat",
      "api_key_env": "DEEPSEEK_API_KEY",
      "api_protocol": "chat",
      "reasoning_effort": "auto"
    }
  }
}
```

Fields:

| Field | Meaning |
| --- | --- |
| `base_url` | HTTP(S) API root, no query or fragment |
| `model` | Exact model ID sent to the provider |
| `api_key` | Stored secret (optional) |
| `api_key_env` | Environment variable holding the key; used if no stored key |
| `api_protocol` | `chat`, `responses` or `anthropic` |
| `reasoning_effort` | `auto`, `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `deepseek_thinking` | `off`, `enabled`, `high`, `max` for compatible endpoints only |
| `context_window` | Model context capacity in tokens |
| `max_output_tokens` | Requested completion maximum |
| `thinking_levels` | Optional allowed reasoning levels |
| `catalog_model` | Explicit `provider/model` alias for a proxy or renamed model |
| `native_web_search` | `true` sends the endpoint-run Responses `web_search` tool; off unless set |

Key precedence: `KROMA_API_KEY` → stored `api_key` → `api_key_env` environment
variable. Never put a key in a shell command or conversation.

## Environment

| Variable | Meaning |
| --- | --- |
| `KROMA_MODEL` | Model ID |
| `KROMA_BASE_URL` | API root |
| `KROMA_API_KEY` | API key override |
| `KROMA_API_PROTOCOL` | `chat`, `responses`, `anthropic` |
| `KROMA_PROVIDER_PROFILE` | Select a saved profile |
| `KROMA_CONFIG_FILE` | Alternate profile JSON |
| `KROMA_WORKDIR` | Project directory |
| `KROMA_NO_CHECKPOINTS` | Disable automatic file snapshots |
| `KROMA_SESSION_FILE` | Override session JSON path (explicit session flags win) |
| `KROMA_TUI_SESSION` | Internal: named TUI session selected by `--session` |
| `KROMA_QUIET` | Suppress startup and plugin diagnostics |
| `KROMA_VERBOSE` | Print extra runtime config diagnostics |
| `KROMA_NO_COLOR` / `NO_COLOR` | Disable colour |
| `KROMA_LANG` | TUI interface language: `en` or `zh` |
| `KROMA_CONTEXT_WINDOW` | Context capacity override |
| `KROMA_MAX_OUTPUT_TOKENS` | Output maximum override |
| `KROMA_OUTPUT_RESERVE_TOKENS` | Tokens reserved for completion |
| `KROMA_DEEPSEEK_THINKING` | DeepSeek-compatible thinking mode |
| `KROMA_REASONING_EFFORT` | Reasoning level |
| `KROMA_NO_MCP` | Skip MCP discovery and calls |
| `KROMA_MCP_EAGER` | Register every MCP tool eagerly (old behavior) |
| `KROMA_MCP_SERVERS` | Comma-separated MCP server allowlist |
| `KROMA_MCP_CONFIG` | Alternate MCP JSON file (treated as user config) |
| `KROMA_MCP_STDERR` | Set to `inherit` to show server logs |
| `KROMA_NO_CHECKPOINTS` | Disable automatic file snapshots |
| `KROMA_PLUGIN_DIR` | User plugin directory |
| `KROMA_PLUGIN_TRY` | Load one plugin directory for this run |
| `KROMA_PROFILE` | Set to `1` for task profiling output |

## MCP configuration

User file: `~/.kroma/mcp.json`. Project file: `<project>/.kroma/mcp.json`, inert
until `kroma trust-project` approves its exact content. Example:

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
      "cwd": "."
    },
    "remote": {
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ${TOKEN}" }
    }
  }
}
```

`./` is resolved against the project root for trusted project entries. Server
processes start only when `mcp_search` or `mcp_call` needs them. A single `mcp_search`
has a 30-second total budget across servers; slow or unavailable servers are
reported in the result instead of blocking discovery indefinitely. Idle lazy
servers are closed after five minutes and reconnect on the next use.

## Sessions and checkpoints

Sessions live under `~/.kroma/sessions/`. `--print` and `--json` create a fresh one
unless `--continue` or `--session` is used. File snapshots live under
`~/.kroma/checkpoints/` and are separate from the project `.git`. Snapshot history is
bounded per session (older batches are pruned once it exceeds 200 commits).
Delete that private store to reclaim its space.
