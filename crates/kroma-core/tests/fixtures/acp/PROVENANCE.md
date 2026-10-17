# ACP v1 schema fixture (provenance)

`schema.v1.json` is an unmodified copy of the official Agent Client Protocol v1 stable JSON
Schema. It is vendored so the ACP contract tests validate the adapter's wire output against the
external specification instead of against hand-written expectations.

- Source: <https://github.com/agentclientprotocol/agent-client-protocol>
- Raw file: <https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/schema/v1/schema.json>
- Commit: `f54fce8eaecf81dd135ef70d983a453e9b90e942` (2026-10-14)
- Schema version: `1.25.0` (see `schema/v1/CHANGELOG.md`)
- Negotiated ACP `protocolVersion`: `1` (the schema version only bumps for breaking changes)
- SHA-256: `4bcd1d2051ff7ed379bd9b234fb9b09c5ab9245478698e9fab805bd9b52a33a6`

To refresh:

```
curl -sSL -o crates/kroma-core/tests/fixtures/acp/schema.v1.json \
  https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/schema/v1/schema.json
sha256sum crates/kroma-core/tests/fixtures/acp/schema.v1.json
```

The tests import the fixture with `include_str!` and build a per-root validator by injecting
`"$ref": "#/$defs/<Root>"`, so each response/notification is checked against the exact official
definition (`InitializeResponse`, `NewSessionResponse`, `PromptResponse`, `SessionNotification`,
`RequestPermissionRequest`, ...).
