# Project Memory

Zero-maintenance, project-scoped memory for Kroma. It remembers durable user
rules and provisional review findings, recalls them automatically at the start
of a task, and expires or drops what no longer applies.

Status: **R0 + R1 implemented** (Kroma plugin API 2). R2 (richer review ledger /
maintenance) and R3 (embeddings, team sharing) are deliberately not enabled yet.
See [the zero-maintenance design](../../../docs/project-memory-zero-maintenance.md)
and the [V2 contract](../../../docs/project-memory-v2-contract.md).

## What is automatic

- **Automatic recall.** The host asks for a small pack at the start of a task and
  injects it as untrusted reference data. The model does not have to remember to
  search.
- **Quote-anchored rules.** When the user states a durable preference, decision
  or rule, the model calls `memory_remember` with a verbatim quote. The entry is
  pending until the task summary verifies that quote; then it becomes active.
  There is no approval queue.
- **Provisional review notes.** `kind=review` requires a path and an evidence
  reference, is labeled unverified, and expires after 30 days.
- **Self-maintenance.** Pending rules not verified within 7 days are dropped;
  review notes expire; forget/purge tombstones block exact re-creation.
- **Project identity.** Git repositories use a local `kroma-memory-id` in the
  Git common directory, so all worktrees share memory and moving the repository
  keeps it. Different clones/repositories are isolated. Non-Git projects use a
  user-level registry keyed by the canonical workdir.

Defaults are on. Normal use needs no commands.

## Install

Requires Kroma with the V2 `request_context` / `task_summary` capabilities
(this tree). The plugin needs a prebuilt `kroma-memory` binary; no Rust
toolchain is required for users.

From source (development):

```console
cargo install --path examples/plugins/project-memory
kroma plugins install examples/plugins/project-memory/plugin
kroma plugins enable project-memory
```

Or use a release artifact: unzip the matching `kroma-memory` binary so it is on
`PATH`, then install/enable `plugin/`. The release workflow publishes per-OS
archives and checksums. Restart Kroma or run `/plugins reload` while idle.

For development without installing:

```console
cargo build --manifest-path examples/plugins/project-memory/Cargo.toml
export PATH="$PWD/examples/plugins/project-memory/target/debug:$PATH"
kroma plugins check examples/plugins/project-memory/plugin
```

The manifest runs `kroma-memory` from `PATH`. Project-plugin approval
fingerprints the plugin directory, not that external binary; prefer the
user-level install above or keep the binary in a trusted location.

## Model tools

| Tool | Purpose |
| --- | --- |
| `memory_remember(kind, text, quote?, paths?, evidence?, supersedes?)` | `preference`/`decision` require a 8–500 char verbatim user quote and become pending until verified. `review` requires a relative path and evidence and is active-but-unverified for 30 days. |
| `memory_search(query, paths?, limit?)` | Active entries only; compact ids, kinds, summaries and path hints. |
| `memory_read(ids)` | 1–8 active entries; any unknown/pending/deleted id fails the whole batch. |
| `memory_forget(id)` | Immediate tombstone; excluded from search, read and recall. |

All results are `reference_data`; evidence references are never dereferenced.
The static prompt tells the model not to treat memory as instructions or as
proof that code is unchanged.

## User commands

```text
/memory                  # one-screen status
/memory off | on         # stop or restore automation
/memory forget ID|TEXT   # forget now
/memory search TEXT
/memory show ID
/memory doctor
```

Advanced compatibility commands (`remember`, `review`, `candidates`,
`accept`, `correct`, `purge`, `export`, `import`, `use`, `capture`) still exist
for migration and debugging, but are not part of the normal workflow.

## How a rule becomes active

1. The user says something durable: “这个项目用 pnpm，不要用 npm”.
2. The model calls `memory_remember` with a short restatement and the verbatim
   quote. The plugin stores it as `pending` (not searchable, not injected).
3. At the end of the task the host sends `task_summary` with recent user
   messages. The plugin promotes the entry only if the quote appears verbatim.
4. The next task's automatic recall can include it. A pending entry that is
   never verified is dropped after 7 days.

A summary failure never fails the task; a rule stays pending and is either
verified by a later summary or expires.

## Storage and scope

```text
<host data_dir>/projects/<memory-id>/memory.sqlite3
```

- Private local SQLite; WAL, `synchronous=FULL`, foreign keys, immediate
  transactions. A successful write is committed before the reply.
- Git projects: `<git-common-dir>/kroma-memory-id`; all worktrees share it.
- Non-Git: `<data_dir>/registry/<sha256(workdir)>.id`.
- No repository files, no Git commits, no network, no telemetry.
- Unix `0700`/`0600`; Windows current-user + SYSTEM DACL; symlink/reparse and
  known UNC/network paths are rejected.

## Limits and forgetting

- 4000 chars per note, 8 paths, 8 evidence refs, 10,000 rows, 200 pending,
  20 search hits, 8 read ids, 128 KiB frames, 512–1024 recall tokens.
- Review notes expire after 30 days; pending rules after 7 days.
- `forget`/`purge` affect this plugin only. They do not rewrite Kroma history,
  assistant answers, compacted summaries, provider logs or exports.
- Secret scanning is best-effort. The same text with different path hints is a
  new note; semantic rephrasings are not detected by fingerprints.
- Storage is private to the current OS user, not sandboxed from that user.

## Tests

```console
cargo test --locked --manifest-path examples/plugins/project-memory/Cargo.toml
cargo fmt --manifest-path examples/plugins/project-memory/Cargo.toml --check
cargo clippy --locked --manifest-path examples/plugins/project-memory/Cargo.toml --all-targets -- -D warnings
KROMA_MEMORY_BIN=examples/plugins/project-memory/target/debug/kroma-memory \
  cargo test --test project_memory_e2e
```

The host test installs the shipped manifest, remembers a quote, verifies it via
a real final completion (`request_context` + `task_summary`), checks automatic
recall, restart persistence, workdir isolation and disable. It skips with an
explicit message if the binary is absent; CI sets `KROMA_MEMORY_BIN`.
