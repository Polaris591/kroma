#!/usr/bin/env python3
"""A tiny standard MCP stdio server used by the frontier MCP contract test.

It speaks newline-delimited JSON-RPC 2.0 and implements just `initialize`, `tools/list` and
`tools/call`, which is enough to exercise the adapter's handshake, descriptor parsing and call
round-trip against a real child process.
"""
import json
import sys


def reply(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


TOOLS = [
    {
        "name": "echo",
        "description": "Echo back the provided text.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "boom",
        "description": "Always returns an MCP tool error.",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = msg.get("method")
        if "id" not in msg:  # notification; nothing to reply to
            continue
        rid = msg["id"]
        if method == "initialize":
            reply({
                "jsonrpc": "2.0",
                "id": rid,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "echo-server", "version": "0.1.0"},
                },
            })
        elif method == "tools/list":
            reply({"jsonrpc": "2.0", "id": rid, "result": {"tools": TOOLS}})
        elif method == "tools/call":
            params = msg.get("params", {})
            name = params.get("name")
            args = params.get("arguments", {})
            if name == "echo":
                reply({
                    "jsonrpc": "2.0",
                    "id": rid,
                    "result": {"content": [{"type": "text", "text": args.get("text", "")}]},
                })
            elif name == "boom":
                reply({
                    "jsonrpc": "2.0",
                    "id": rid,
                    "result": {"isError": True, "content": [{"type": "text", "text": "kaboom"}]},
                })
            else:
                reply({
                    "jsonrpc": "2.0",
                    "id": rid,
                    "error": {"code": -32602, "message": "unknown tool '%s'" % name},
                })
        else:
            reply({
                "jsonrpc": "2.0",
                "id": rid,
                "error": {"code": -32601, "message": "method not found"},
            })


if __name__ == "__main__":
    main()
