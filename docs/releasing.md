# Releasing Kroma

Maintainer checklist for the public Beta of the CLI and TUI. It describes what to
verify and in which order; it is not evidence that a release happened. At the time
of writing, **no npm package is published**, the release workflow has not been
executed, and the five-platform CI results have not been checked from this
workspace.

## Current state and decisions

- `[package] version` in `Cargo.toml` is the only version source; it is currently
  `0.4.0`. Whether Beta 1 keeps that version or bumps it is a maintainer decision.
- Package names `kroma`, `kroma-win32-x64`, `kroma-linux-x64`,
  `kroma-linux-arm64`, `kroma-darwin-x64` and `kroma-darwin-arm64` are
  provisional until registry ownership is confirmed for all six.
- `.github/workflows/npm.yml` builds and installation-tests tarballs on five
  native runners with `contents: read` and never publishes. The publishing path
  is a separate release workflow started only by `workflow_dispatch`; it has not
  been executed. Confirm its file name, inputs and defaults in the repository
  before dispatching.
- The optional Desktop preview is not part of the npm installer. It has its own
  workflow and must not be a prerequisite for a CLI/TUI Beta release.

## 1. Secret, history and probe audit

Run from a clean checkout before tagging:

```sh
git status --porcelain          # must be empty; no generated keys or .env files
git ls-files -- .env '*.pem' '*.key' '*.p12'
# If installed, use a credential scanner with redacted output:
gitleaks git --redact .
gitleaks dir --redact .
```

- Check for keys in code, tests, docs, committed session files and review notes.
- Tests and the smoke harness must use loopback fixtures or mocks, not the public
  registry or a paid model endpoint. A real registry is only used by an explicit
  release verification step.
- If a credential was committed, rotate it first. Deleting the file is not enough;
  decide deliberately whether history has to be rewritten, since a rewritten tag
  can never be reused.
- Never export an npm token while running the credential-free packaging commands.

## 2. Name ownership and publishing access

- Confirm that all six names are owned by the project account, that 2FA is on, and
  that the people who will publish have the right organization role. A registry
  404 is not a reservation.
- Make the repository public only after the audit and enable private vulnerability reporting.
- Configure OIDC trusted publishing for the release workflow and protect the
  `npm-release` environment with a required reviewer. Prefer no long-lived npm
  token; this workflow does not accept one. Configure all six packages for
  `release.yml`, environment `npm-release`, with direct **Allow npm publish**
  enabled (new trusted publishers can default to stage-only). npm requires
  CLI 11.5.1+ / Node 22.14+; the publish job uses Node 24 and npm 11.
  See [npm trusted publishing](https://docs.npmjs.com/trusted-publishers/).
  Initial name registration/bootstrap, if required by npm, is a separate owner action;
  do not assume OIDC alone establishes ownership of unpublished names.
- If a package name changes, update `npm/platforms.cjs`, `npm/stage.cjs`, the
  launcher, generated manifests and these documents together.

## 3. Version, tag and artifacts

- Bump `Cargo.toml` once, build all five native targets from the same commit, and
  confirm each binary reports that version with `kroma --version`.
- Tag exactly `v<version>`. Do not re-tag or reuse a tag; bump instead.
- Rebuild and re-test the tagged commit. Do not publish tarballs from a dirty or
  different tree.
- Confirm the tagged commit contains the `kroma update [--check] [--channel
  latest|next]` implementation and the release workflow this checklist names,
  and that `docs/reference/cli.md` lists `update`. The public docs describe that
  contract; they are not evidence that either exists or ran.
- Preserve the exact `.tgz` files and their POSIX executable modes; verify the
  SHA256 sums that will be attached to the GitHub release.

## 4. Dispatch the release workflow

The workflow is intended to verify the `v<version>` tag against `Cargo.toml`,
default the channel to `next`, default `publish` to false, and require an explicit
ownership confirmation bound to the protected `npm-release` environment. Validate
the five native artifacts first, then publish the platform packages before the
main package, with OIDC provenance. Confirm those inputs and the environment name
in the actual workflow file before dispatch.

Dispatch from the same tagged commit, so npm provenance identifies the actual source:

```sh
gh workflow run release.yml --ref v0.4.0 -f tag=v0.4.0 -f channel=next -F publish=false
```

The publish job checks that the repository is public, that `npm-release` has required
reviewers, and that the remote tag still points at the approved commit. Ordinary
pushes and tag pushes do not publish anything.

A dispatch with `publish: false` is intended as the dry run: it should build,
validate and stage artifacts without registry writes. Confirm that behavior in the
workflow, and treat a publishing dispatch as a separate, reviewed decision.

## 5. Verify from the real registry

Local tarball smoke tests do not prove registry behavior. After publishing:

- On each of the five baselines, install into an isolated prefix and run
  `kroma --version`, `kroma --help` and an invalid-flag check through both the npm
  bin shim and the native executable.
- `--omit=optional` must still fail with actionable launcher guidance; a normal
  install must resolve the matching platform package at the exact version.
- `kroma update --check` prints the published channel version and changes nothing.
  `kroma update` on an npm install upgrades, and `npm uninstall --global kroma`
  removes the launcher and its platform package.
- Test an upgrade from the previous published version, not only a fresh install.
- Confirm the provenance attestation and the `latest`/`next` dist-tags point where
  intended. Normal launcher startup must make no update request; npm installation
  itself naturally downloads package tarballs. Beta installs use `kroma@next`
  and `kroma update --channel next` until a `latest` dist-tag is published.

## 6. Actual CI and terminal smoke

Record results from the real workflows rather than assuming them:

- All five `npm.yml` platform jobs green, including the native runner/host
  verification step and the smoke installs.
- TUI first launch with no profile, guided setup, a one-shot `--print`/`--json`
  run, Ctrl+C cancellation and Ctrl+Z draft restore.
- Windows console Ctrl+C (not emulated), macOS/Linux signal forwarding in a real
  terminal, and a JSONL run piped into `head`.
- Known local limits: Windows GNU Desktop library tests need the documented
  resource-manifest workaround, so the standard test command is not a clean
  pass there. Linux and macOS results have not been observed in this workspace.

## 7. Publish the GitHub release

- Release notes must state: public Beta scope (CLI/TUI), the five supported
  platform baselines, glibc floors, unsupported hosts (Alpine/musl, Windows ARM64,
  32-bit), manual-only updates and how to report security issues.
- Attach the SHA256 checksums and link [security](../SECURITY.md) and the
  [safety model](security.md).
- Do not claim adoption, community metrics or benchmark gains. Local planner
  measurements are a constrained microbenchmark, not an end-to-end task speedup.
- Mark a `next` release as a prerelease on GitHub to match its dist-tag.

If npm publication fails partway through, stop and audit which immutable versions
were published. There is no automatic rollback or blind retry; never rebuild a
different tarball under an already-published version.

## 8. After publishing

- Keep the release immutable. For a broken version, publish a fixed version and
  `npm deprecate` the old one; do not unpublish or reuse the tag.
- Update README status only after the registry install and update checks pass.
- If any gate above is skipped, say so in the release notes rather than implying
  it passed.
