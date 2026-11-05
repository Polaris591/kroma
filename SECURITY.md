# Security policy

Kroma is a public Beta of the CLI and TUI. This file is the vulnerability
reporting policy; the runtime safety model and its limits are in
[docs/security.md](docs/security.md).

## Supported versions

Only the latest published Beta release (once one exists) and the current `main`
branch receive security fixes. Older versions and prereleases on the `next` channel may be
abandoned without a backport. The version is currently `0.4.0`; see
[docs/releasing.md](docs/releasing.md).

## Reporting a vulnerability

Use GitHub's private vulnerability reporting for
[Polaris591/kroma](https://github.com/Polaris591/kroma/security/advisories/new)
if it is enabled. If it is not, contact the maintainer through the address on
their GitHub profile. Do not open a public issue with exploit details, and do not
include API keys, tokens, session files or proprietary code.

Include the Kroma version (`kroma --version`), operating system and architecture,
installation method (npm, Cargo or a source build), reproduction steps and the
observed impact. Tell us if the issue is already public.

Enable private reporting in the repository settings before announcing a release;
without it this channel does not exist.

There is no bug bounty and no guaranteed response time. Reports are triaged and
fixed on a best-effort basis, and credits are given only with permission.

## In scope

- The CLI, TUI, JSONL events and session/checkpoint handling.
- The npm launcher, platform package resolution and the manual `kroma update`
  path: it must use the official HTTPS npm registry and the npm package manager,
  must not contact the network before an explicit update request, and must not
  overwrite a source/Cargo binary.
- Trust decisions for project MCP files and project plugins, including approval
  binding and the documented scan bounds.
- Leaks of credentials or session data through diagnostics, logs or tool output
  handling.

## Out of scope

- Model providers, third-party MCP servers and third-party plugins' own behavior
  or policies, and provider retention/training policies.
- Effects of commands the agent was asked to run, including network requests,
  database writes and changes outside the worktree. Kroma is not a sandbox.
- The optional Desktop preview unless a finding is in code shared with the CLI.

## Safety limits to keep in mind

Kroma executes tools with your user permissions and asks for no per-command
approval. It is not a sandbox. Git-worktree snapshots cannot undo `git push`,
database or network side effects, files ignored by `.gitignore`, or changes
outside the worktree. Run untrusted tasks in a disposable worktree, container or
VM. Updates are manual only: there is no startup check and no background
download.
