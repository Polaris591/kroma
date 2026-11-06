# Repository maintenance

This is the small set of GitHub repository-level tasks that source commits alone
cannot perform. The CLI/TUI is a **public Beta**, Desktop remains a separate
preview, and npm distribution is **prepared but not published**. Do not call
a release production-ready simply because compilation or mock tests pass.

## GitHub repository settings

The repository owner should verify these manually in **Settings**. A file-only
PR cannot guarantee that repository metadata, protections or security settings
have been applied.

| Setting | Recommended value | Why |
| --- | --- | --- |
| About / Description | Minimal, observable Rust coding-agent harness with TUI, MCP, Skills and opt-in plugins | Makes the repository discoverable without overclaiming |
| Topics | rust, ai-agent, coding-agent, terminal-ui, mcp, agent-harness, developer-tools | Searchability; adjust if GitHub rejects a topic |
| Default branch | main | Single current integration branch |
| Issues | Enabled | Use the bug and feature templates |
| Wiki / Discussions | Optional | Avoid empty surfaces until there is an actual need |
| Private vulnerability reporting | Enable before a public release | A private reporting path for disclosures |
| Branch protection/ruleset | Require reviewed PRs and applicable passing checks, once CI is stable | Prevent bypassing verification |
| Actions permissions | Least privilege | Only publishing needs elevated rights |
| Environment: npm-release | Required reviewers and appropriate trusted publishing settings | Explicit human-controlled publishing |

**Do not enable required status checks until their exact check names and
availability have been confirmed.** CI can be queued, conditional or unavailable;
a permanently required non-running check would block all maintenance.

## Changes and triage

1. Keep `main` as the current documented integration branch. Feature branches may remain
   temporarily for audit and comparison; delete only after confirming they
   contain no unique work and are not being used by another worktree.
2. Issue templates are for reproducible bugs or scoped features. Security
   vulnerabilities belong in [private reporting](../SECURITY.md), not a public issue.
3. Keep core changes focused and testable. Follow
   [CONTRIBUTING.md](../CONTRIBUTING.md), check the affected workflow and
   verify context/session compatibility.
4. Maintain both READMEs when the installation, support or status story changes.
   Run `python3 scripts/check_doc_links.py` before proposing documentation changes.
5. Retain old [reviews](../reviews/README.md) and
   [benchmarks](../benchmarks/README.md) as archival evidence rather than mixing
   them with current user documentation.

## CI and release ownership

- `core.yml`: Rust checks and JavaScript plugin contracts on Linux/macOS/Windows.
- `project-memory.yml`: independent memory crate and host E2E checks.
- `desktop.yml`: Desktop host checks for relevant code paths.
- `npm.yml`: build/package/install test matrix; produces artifacts, **never publishes**.
- `docs.yml`: maintained Markdown local-link check.
- `release.yml`: manual, tagged release staging or publication after
  explicit ownership confirmation and environment approvals.
- `project-memory-release.yml`: separate memory-worker release packaging.

Use [the complete release procedure](releasing.md) and
[distribution guide](npm-distribution.md). Tag creation, registry ownership,
release-notes approval, signing, hardware acceptance and actual publishing
remain deliberate maintainer actions.

Before approving a release, inspect CI from the **release commit and tag**,
test a clean installation, verify key model/provider flows with real endpoints,
review security reporting, and explicitly list unverified hosts or capabilities.
Publishing should never be triggered as an incidental effect of merging docs.
