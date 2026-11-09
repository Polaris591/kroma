# Project Memory

Local project memory for Kroma: keep durable user preferences and decisions,
recall useful records, and let the user inspect, edit, unpin, forget or erase them.
The current behavior is specified in [the project memory guide](../../../docs/project-memory.md).

## Behavior

- **Original user wording.** Preference/decision writes require a nonempty
  contiguous quote from the actual current user message, supplied separately by
  the host. The quote itself is saved, not the model's paraphrase. A host control
  note or a model-written child task is not a human source. There is no deferred
  task-finished approval path.
- **No storage quotas or automatic eviction.** No five-per-turn or four-pinned
  rule limit, no 200-active/10,000-lifetime cap, and no automatic rule expiry.
- **Cross-task preferences.** Unscoped preferences default to pinned. Other
  cross-task rules can use `core=true`; `core=false` or `/memory unpin ID` makes
  them intent-scoped without deleting their content.
- **Complete records.** Recall selects whole records, never cuts off a rule's
  exceptions. The host owns the request budget; omitted content is reported and
  remains readable. The default `context_tokens=2048` is adjustable through the
  existing plugin settings, separately from durable storage.
- **Optional review notes.** Path and evidence references are required for model
  writes. They are unverified historical navigation, never proof of current
  correctness. File changes are marked for rechecking, not silently hidden.
- **No second task history.** The plugin does not keep a latest-user-request
  prefix or approve unverified proposals when a task finishes.
- **Scope.** Git common-directory identity shares memory across worktrees in
  the same clone; separate repositories are isolated. Non-Git projects use a
  canonical-workdir registry. Read-only Git metadata uses a registry keyed by the common directory, retaining an existing repository identity. Moving a non-Git project does not automatically
  merge private records from a different directory.

Memory is reference data, not instructions or permission. Current user requests,
project instructions and real source evidence take precedence.

## Install

Requires Kroma plugin API 2 with current-user tool context and `provide_context`.
The manifest launches `kroma-memory` from `PATH`; installing the manifest alone
is not enough.

For a prebuilt release, put the matching executable on `PATH`, then:

```console
kroma plugins install /path/to/project-memory/plugin
kroma plugins enable project-memory
```

From source:

```console
cargo install --path examples/plugins/project-memory
kroma plugins install examples/plugins/project-memory/plugin
kroma plugins enable project-memory
```

Restart Kroma after enabling, or reload idle plugins where supported. Prefer a
user-level install for private storage shared across worktrees. Project-level
private plugin data must stay ignored by Git; existing tracked data needs explicit
cleanup rather than a claim that adding an ignore rule makes it private.

## Tools and commands

| Tool | Purpose |
| --- | --- |
| `memory_remember(kind,text,quote?,core?,paths?,evidence?,supersedes?)` | Verified user rules or optional review notes. User-rule `text` is a compatibility/paraphrase field; the verified quote is authoritative. |
| `memory_search(query,paths?,limit?)` | Keywords or a natural sentence, up to 512 characters. The total result count honors `limit`. |
| `memory_read(ids)` | Up to eight IDs; available records plus missing IDs. Oversized batches return whole records and deferred IDs for the next read. |
| `memory_forget(id)` | Stop future use when the user asks; reversible with restore. |

`core` is optional: omission preserves an existing setting; an unscoped new
preference defaults to pinned. `supersedes` names the particular old record being
replaced, not a guessed topic. Empty optional placeholders from older gateways
are tolerated but never identify a replacement.

```text
/memory                         # status
/memory list [state] [offset]    # inspect stored records
/memory show ID
/memory search TEXT
/memory pin ID | unpin ID        # change recall priority, not content
/memory remember [kind] TEXT
/memory correct ID TEXT
/memory forget ID|TEXT
/memory restore ID
/memory purge ID --confirm
/memory off | on
/memory doctor
```

Old candidate acceptance and JSON-string-array import remain migration tools,
not a required daily workflow. Export is paged Markdown for inspection, not a
round-trippable database backup format.

## Storage, privacy and recovery

Storage is private local SQLite under `<host data_dir>/projects/<memory-id>/`.
Transactions commit before a successful save reply. No network, telemetry,
embedding service or background model calls. OS-user permissions protect local
storage; they do not sandbox it from that same user. Deliberate directory links may relocate private storage; database/sidecar file links are rejected, and real targets still need private permissions.

`off` stops automatic capture and recall; manual inspection and recovery remain
available. Forgetting retains a recoverable record. Purge clears this plugin's
live database content, original quote, historical bodies and evidence paths;
fingerprint tombstones prevent automatic replay, not a new explicit user save.
It is not forensic disk erasure and cannot rewrite chats, exports, backups or
provider logs. Secret detection is best-effort, not permission to store credentials.

Upgrade preserves existing data. Old archived records become available again;
unverified old proposals remain candidates rather than being auto-approved.
Legacy expiry and latest-task metadata are retired. Historical wording hashes
replace unbounded full-body snapshots.

Requests and replies are bounded to 128 KiB. Individual text/quotes are bounded
to 4000 characters; path/evidence batches and result pages remain bounded for
communication size, not as lifetime storage quotas.

## Verification

```console
cargo test --locked --manifest-path examples/plugins/project-memory/Cargo.toml
cargo fmt --manifest-path examples/plugins/project-memory/Cargo.toml --check
cargo clippy --locked --manifest-path examples/plugins/project-memory/Cargo.toml --all-targets -- -D warnings
cargo build --locked --manifest-path examples/plugins/project-memory/Cargo.toml
```

Set `KROMA_MEMORY_BIN` to the freshly built executable, then run
`cargo test --locked --test project_memory_e2e`. Without a binary the host test
prints a skip message; CI sets the path so the real worker is exercised.
