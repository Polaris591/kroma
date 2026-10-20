# Threat model and deployment boundary

Kroma is a **local, single-user** coding-agent runtime. This document states what it protects, what
it explicitly does not, and which deployment shapes are supported. It is written to match the code;
where a mitigation is partial it says so.

## Supported deployment

* `kroma repl` / `kroma run` on a developer machine, one OS user.
* `kroma serve` on a Unix domain socket (recommended) or loopback TCP.
* `kroma acp` over stdio for an IDE client on the same machine.

Not supported and not claimed:

* multi-user isolation,
* daemon-to-daemon forwarding or peer discovery,
* exposing a daemon to an untrusted network,
* sandboxing untrusted repositories or untrusted MCP servers.

## Assets

Workspace source and uncommitted changes; the session WAL and revision store; provider credentials;
the host filesystem and process table; the operator's terminal.

## Trust levels

1. **Operator** — trusted; may grant `--allow-shell`, configure MCP servers, install hooks.
2. **Kernel binary** — trusted, but must fail closed on bad input.
3. **Workspace content** — untrusted. This includes `.kroma.toml`, `AGENTS.md`, skills, and any test
   or build script the agent may run.
4. **Model output** — untrusted.
5. **MCP servers and hook subprocesses** — untrusted by default, but currently executed with the
   daemon user's privileges (see below).
6. **Other local users and local network clients** — untrusted.

## Mitigations that exist

* File access goes through `WorkspaceCap` with `openat2`/`O_NOFOLLOW` and file-descriptor-relative
  operations; symlink escapes and special files (FIFO/socket/device) are refused.
* Writes are exclusive-create + `fsync` + rename; parent directories are synced before a recovery
  intent is cleared; sensitive files are created `0600` and private directories `0700`.
* `bash` and the completion gate run in their own process group (Unix) or Job Object (Windows) with
  bounded output, timeouts and descendant cleanup; the child environment is filtered of secrets.
* One shared token ledger for the parent run and every child; consumption that cannot be proven is
  held conservatively rather than refunded.
* Terminal output escapes untrusted text (model output, event payloads, diagnostics).
* `serve` refuses to bind a non-loopback address without an explicit token, rejects HTTP/Origin-shaped
  requests, compares the token in constant time, and creates the Unix socket `0600` in a parent
  directory it only restricts when it created it.
* A workspace has at most one writer: a process-local registry plus a cross-process advisory lock in
  `/tmp/kroma-writer-locks-<uid>` (symlink- and foreign-owner-refusing).
* Under the default `ask` policy, writes and shell execution require an approval decision, and the
  REPL reads that decision from the same stdin owner that reads prompts.

## Not mitigated (must appear in release notes)

1. A non-loopback TCP bind is **plaintext**: no TLS, and the auth token travels in JSON-RPC
   parameters, so it can appear in logs or client history. Kroma prints a startup warning and the
   token value itself is never logged.
2. MCP servers and policy/observer hooks run as the daemon user with no OS sandbox. A server's
   self-declared `read_only` flag is a hint, not a security proof, and a server's internal side
   effects cannot be expressed in the tool's effect class.
3. `bash` and repository test scripts run with host privileges. Approval is not a sandbox.
4. Workspace rollback covers files only. Network calls, databases, deployments and any other external
   side effect are not recoverable, and Kroma never claims otherwise.
5. Windows console/PTY behaviour and the named-pipe equivalent of the Unix socket have not been
   verified on a real Windows host.
6. There is no per-session CPU, memory or disk quota.

## Evidence

* `docs/` test suites and `crates/kroma-core/tests/filesystem_security.rs`,
  `tests/shell_supervisor.rs`, `tests/gate6_multi_surface_runtime.rs`, `tests/multi_session_daemon.rs`.
* Release acceptance manifest: the `accepted-source.json`, `binary-fingerprints.json`,
  `test-manifest.json`, `runtime-capabilities.json` and `known-limitations.md` produced alongside this
  document.
