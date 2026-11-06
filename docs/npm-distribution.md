# npm distribution

This document describes the repository's npm packaging, installation testing and
update path: how the tarballs are built, what CI verifies, how `kroma update`
behaves, and what is deliberately not done yet. The packaging stage stops at
reviewable artifacts. **None of these packages is published to an npm registry**,
and the release workflow described at the end has not been executed.

## Status and package names

The npm packages do not exist publicly yet. The names are provisional and are not
reserved by this repository:

- `kroma`
- `kroma-win32-x64`
- `kroma-linux-x64`
- `kroma-linux-arm64`
- `kroma-darwin-x64`
- `kroma-darwin-arm64`

The only registry evidence available so far is that the public registry returns
404 for `kroma`. A 404 does not reserve a name, establish ownership, or guarantee
that the name will be available for publication. Name ownership and the final
package name must be confirmed by a human before any future release. If a name
changes, the launcher, platform table and generated manifests must be changed
together.

The npm CI workflow (`.github/workflows/npm.yml`) has read-only repository access
and stops after uploading tarballs as workflow artifacts; it has no registry
login, token, secret or publish step. A separate, manually dispatched release
workflow is the intended publication path, but it has not been run: no package is
published and no trusted publisher is confirmed here. The update path
that ships with Kroma is described in [Update path](#update-path).

## Packages and versions

| Package | Role |
| --- | --- |
| `kroma` | Main package. Contains `launcher.cjs`, `platforms.cjs`, package metadata, `LICENSE` and the package README. Its `bin` entry is the launcher, not a native binary. |
| `kroma-win32-x64` | Windows x64 native package containing `bin/kroma.exe`. |
| `kroma-linux-x64` | glibc Linux x64 native package containing `bin/kroma`. |
| `kroma-linux-arm64` | glibc Linux ARM64 native package containing `bin/kroma`. |
| `kroma-darwin-x64` | macOS Intel native package containing `bin/kroma`. |
| `kroma-darwin-arm64` | macOS Apple Silicon native package containing `bin/kroma`. |

Every package gets the exact version from `[package] version` in the repository's
`Cargo.toml`. There is no separate npm version, version range or independent
platform versioning. The main package's `optionalDependencies` point at all five
native packages at that exact version. The staging tool also executes the binary
with `--version` and refuses to package it unless it reports the same `kroma`
version.

The main tarball contains only `package.json`, `launcher.cjs`, `platforms.cjs`,
`LICENSE` and `README.md`. A native tarball contains only `package.json`,
`LICENSE`, `README.md` and `bin/kroma` (or `bin/kroma.exe`). Tests, source files
and repository tooling are not part of either package; the smoke test validates
this allowlist.

## Runtime and installation requirements

- **Node.js 22 or newer.** The packages declare `"engines": {"node": ">=22"}`;
  the launcher and packaging tools are tested with Node 22.
- **Optional dependencies must be installed.** npm optional dependencies are how
  the matching native package is selected. Do not install with
  `--omit=optional`/`--no-optional`, and do not prune optional dependencies. If
  the platform package is missing or has the wrong version, the launcher exits
  with an installation error instead of downloading a binary.
- **No install scripts.** The generated manifests define no `preinstall`,
  `install` or `postinstall` script. The native executable is already inside the
  selected platform package, so an install script or runtime download is
  unnecessary. `--ignore-scripts` does not supply a missing optional dependency.
- **Supported hosts are explicit.** The launcher checks the host
  `process.platform` and `process.arch`, and on Linux it checks for glibc. The
  native packages declare matching `os`, `cpu` and (on Linux) `libc` metadata.
  Alpine/musl, Windows ARM64, 32-bit systems and other architectures are not
  supported by this npm distribution. Source builds remain available for those
  hosts.
- **Updates are manual.** The launcher does not check for or install updates on
  its own; it acts only when you run `kroma update`, and then only through the
  npm package manager against the official HTTPS registry. See
  [Update path](#update-path).

## Update path

Updates are user-initiated in every installation:

| Command | Behavior |
| --- | --- |
| `kroma update --check` | Read-only. Reads the channel version from the official npm registry over HTTPS and prints it; installs nothing. |
| `kroma update` | Updates to the `latest` channel when the installation is an unambiguous npm install. |
| `kroma update --channel next` | Same, for the `next` prerelease dist-tag while one is published. |

There is no startup check, background poll, update notification or automatic
download. Nothing about an update is scheduled by the TUI, the line CLI or the
launcher.

For an npm installation the launcher handles `kroma update` before the native
executable starts: it runs your npm package manager against the official HTTPS
registry (`https://registry.npmjs.org`). It does not implement a second
downloader and does not consult another registry or mirror. Global installs stay
global; local direct dependencies update their owning project and may change its
package.json/lockfile. Linked, transitive and other-manager installations are
refused. It does not scan PATH or delete other installations. Missing platform
packages can be repaired by npm; success requires both installed versions to match.

A Cargo or source binary is never overwritten by Kroma. `kroma update --check`
still reads the channel version from the registry and reports it, and `kroma
update` prints the manual rebuild or `cargo install` command for you to run.
Profiles, sessions and checkpoints under `~/.kroma` are not rewritten by an
update; project-local installs may change package.json/lockfile. The channel is chosen per
command: `latest` is the default, and `next` resolves only while that dist-tag exists.

## CI build and install baselines

Each native package is built and installation-tested on a specific GitHub-hosted
runner. These five runner baselines are build and install CI targets. They are
not a locally verified support matrix and not a promise that every older OS,
distribution or glibc version works. No other native baseline is tested here.

| Package | CI runner | Rust target built natively | Binary | Native baseline |
| --- | --- | --- | --- | --- |
| `kroma-win32-x64` | `windows-latest` | `x86_64-pc-windows-msvc` | `target/release/kroma.exe` | Windows x64, MSVC toolchain |
| `kroma-linux-x64` | `ubuntu-22.04` | `x86_64-unknown-linux-gnu` | `target/release/kroma` | glibc, Ubuntu 22.04 x64 baseline |
| `kroma-linux-arm64` | `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` | `target/release/kroma` | glibc, Ubuntu 24.04 ARM64 baseline |
| `kroma-darwin-x64` | `macos-15-intel` | `x86_64-apple-darwin` | `target/release/kroma` | macOS Intel runner |
| `kroma-darwin-arm64` | `macos-15` | `aarch64-apple-darwin` | `target/release/kroma` | macOS Apple Silicon runner |

Linux packages are glibc-only: x64 has an Ubuntu 22.04 (glibc 2.35) build/install
baseline and ARM64 has an Ubuntu 24.04 (glibc 2.39) baseline. There is no musl
package. The macOS rows describe the CI build environments, not an independently
verified minimum macOS release. The workflow explicitly verifies the Node host
platform/architecture and the `rustc` host before building so a runner that is
actually the wrong operating system or architecture cannot produce a
false-positive package.

## Official plugins bundled with the native package

Every supported native npm release builds and bundles the three official
plugins as **disabled, read-only inventory** under
`share/kroma/plugins/`: Web Search, Subagents and Project Memory. Kroma's npm
launcher passes that directory through `KROMA_OFFICIAL_PLUGIN_DIR`; the normal
plugin catalog lists these manifests but never enables them automatically.
User-installed plugins with the same ID take priority. Enable only the plugin
you want with `kroma plugins enable web`, `... subagents`, or
`... project-memory`; there is no extra `plugins install`, `cargo install`,
postinstall script or runtime network download. The memory worker is separately
compiled by the platform release workflow and packaged alongside Kroma under
`bin/kroma-memory[.exe]` on the same process PATH. A source-built Kroma can use
the same generic `KROMA_OFFICIAL_PLUGIN_DIR` opt-in to an explicit bundle.

The npm release smoke test verifies that installed packages contain all three
manifests and the memory executable and that `plugins list` discovers them. A
CI staging success is not a substitute for a published-release validation on
each supported OS and architecture.

## Credential-free commands and working directories

All commands below are run from the repository root and require no npm login,
token or publishing credential. None of them publishes anything. Tests, staging
and smoke checks do not contact an external npm registry. Smoke serves the native
tarball through a temporary loopback-only registry to test real optional-dependency
resolution, using isolated npm configuration and cache. The Cargo build uses the existing Rust toolchain and Cargo
configuration, not npm credentials.

### Package tooling tests

Working directory: repository root.

```sh
npm --prefix npm test
```

The `npm/` package is private, has no external JavaScript dependencies and uses
Node's built-in test runner. No `npm install` is required. npm runs the script
with its working directory set to `npm/`, which is why `--prefix npm` is used.

### Build, stage and smoke

Working directory: repository root for every command. Build on the same host
platform and architecture that will run the binary; this packaging stage does not
cross-compile. Replace the target and binary with the matching row from the table
above.

```sh
cargo build --release --locked --bin kroma
node npm/stage.cjs --target x86_64-unknown-linux-gnu --binary target/release/kroma --out .npm-dist
npm --prefix npm run smoke -- ../.npm-dist
```

Windows x64 uses the same working directory and commands with the MSVC target and
the `.exe` binary:

```sh
cargo build --release --locked --bin kroma
node npm/stage.cjs --target x86_64-pc-windows-msvc --binary target/release/kroma.exe --out .npm-dist
npm --prefix npm run smoke -- ../.npm-dist
```

Other native targets use the same shape, for example
`--target aarch64-unknown-linux-gnu` or `--target aarch64-apple-darwin`, always
with `target/release/kroma`. The `--binary` and `--out` arguments are resolved
against the shell's working directory, so run staging from the repository root.

### Fresh staging directories are required

`.npm-dist` is a staging area generated from source and build output, not a cache
or a release record. The staging command refuses to reuse an existing
`.npm-dist/kroma` or `.npm-dist/kroma-<platform>` directory; it does not merge,
overwrite or repair old output. Each attempt therefore needs a fresh/empty output
directory. If reusing the path, remove the old staging directory first. Do not
stage multiple targets into the same directory: use a separate output directory
per target (the CI matrix uses a fresh workspace per job).

A successful stage creates only these package source directories:

```text
.npm-dist/
  kroma/                 # main package and generated package.json
  kroma-<platform>/      # native package, binary under bin/
```

It does not create tarballs. The smoke command performs packaging and writes the
tarballs to `.npm-dist/tarballs/`.

### Smoke verification

Working directory: repository root.

```sh
npm --prefix npm run smoke -- ../.npm-dist
```

npm runs the smoke script with `npm/` as its working directory, so the relative
argument `../.npm-dist` resolves to the repository-root staging directory. Run the
command exactly from the repository root; if invoking the script from elsewhere,
pass an absolute path to `.npm-dist` instead.

The smoke test packs both staged packages and installs only the main tarball into
a temporary directory whose path contains spaces. npm must resolve the native
optional dependency from a temporary loopback-only registry fixture; other platform
packages return 404. No external registry or credentials are involved. It executes the
installed CLI for `--version`, `--help` and an invalid flag, including the npm
local bin shim. It also installs globally into an isolated temporary prefix,
executes the global bin shim, reinstalls and uninstalls without touching your real
global installation. Uninstalling only `kroma` must also remove its native
optional dependency. A separate `--omit=optional` install must fail with actionable
launcher guidance. It compares the expected and actual exit codes, validates
each tarball against the allowlist above, and leaves the tarballs in
`.npm-dist/tarballs/`. This catches packaging
and quoting mistakes without registry access and without changing your normal
global installation. Unit tests exercise literal argument forwarding, inherited
stdin/stdout/stderr, working directories, environment, numeric exit codes and
installation failures. POSIX runners also test signal forwarding; Windows console
Ctrl+C and the interactive TUI still require a real-terminal smoke test. The
Windows launcher deliberately does not emulate Ctrl+C using `child.kill`, which
would force-terminate the native process instead of cancelling cooperatively.

## CI workflow

`.github/workflows/npm.yml` runs on pushes and pull requests that change `src/**`,
`Cargo.toml`, `Cargo.lock`, `build.rs`, `npm/**` or the workflow itself, and can
also be started manually with `workflow_dispatch`. It declares only
`permissions: contents: read`.

The matrix contains exactly the five runner/target rows in the baseline table.
Each job performs the following sequence on its native runner:

1. Check out the repository and install Node 22 plus the stable Rust toolchain.
2. Verify `process.platform`, `process.arch` and the `rustc` host match the
   expected native target, rejecting an incorrectly labelled or emulated runner.
3. Run `npm --prefix npm test` for the package tooling.
4. Build with `cargo build --release --locked --bin kroma` for the runner's own
   host target.
5. Stage with the exact target and binary for that runner into a fresh
   `.npm-dist`.
6. Install and exercise the tarballs with `npm --prefix npm run smoke --
   ../.npm-dist`.
7. Upload `.npm-dist/tarballs/*.tgz` as artifact `kroma-npm-<platform>`, failing
   if no tarball is found.

Each platform gets its own artifact name, and each artifact contains both that
platform's native tarball and a copy of the main `kroma` tarball. The main package
has the same contents and version in every job; a release collector should use one
copy plus all five native tarballs. Preserve and publish the exact `.tgz` files;
do not extract and repack POSIX packages on Windows, which can lose executable
permission bits. POSIX smoke tests verify the native tarball executable mode. The workflow has no publish, registry-login, token,
secret or provenance step, and it never runs an update command. Existing
`core.yml` and `desktop.yml` gates are not modified or replaced by this
workflow. Publishing lives in a separate release workflow.

## Release workflow (not executed)

Publishing has not been performed. Publication is meant to go through a separate
release workflow for maintainers, started only by manual `workflow_dispatch`; it
is not triggered by pushes and has not been executed for these packages. Confirm
the actual file and inputs in the repository before any dispatch. Its intended
order is:

1. Confirm package-name availability, registry ownership and maintainer access.
2. Review the exact tarball contents and confirm every package has the same
   version derived from `Cargo.toml`.
3. Review the commit and tag it before distributing artifacts.
4. Publish all five platform tarballs at that exact version first.
5. Publish the main `kroma` tarball last.

Platform packages go first because the main package's `optionalDependencies`
specify exact native versions. A main package published first would temporarily
make installation fail while its native dependencies are unavailable.

The workflow is expected to check a `v<version>` tag against `Cargo.toml`,
default to the `next` channel, default `publish` to false, require an explicit
ownership confirmation through the protected `npm-release` environment, validate
the five native artifacts, publish the platform packages before the main package
with OIDC provenance, and attach checksums to the GitHub release. Those defaults
and the exact workflow file are for the maintainer to confirm; see
[releasing](releasing.md) for the checklist. The CI artifacts in the packaging
workflow remain build outputs for review, not a published release.

## Cargo and manual builds

The existing Cargo/manual build path is unaffected. `cargo build --release` and
the instructions in `docs/quickstart.md` continue to produce a standalone binary
without npm, and this packaging infrastructure does not modify `Cargo.toml`,
`Cargo.lock`, `build.rs` or Cargo build output. npm staging only copies an
already-built release binary into generated package directories. The npm
workflow does not replace the existing core or desktop CI gates.

Not done or not verified in this stage: an executed npm publication, confirmed
registry ownership or trusted-publisher configuration, npm tokens/secrets,
version-bump automation, publishing from ordinary CI, musl builds, Windows ARM64,
32-bit platforms, and locally verified support beyond the five CI baselines.
Update checks are never automatic: `kroma update` is user-initiated, and no
package has been installed from a public registry yet.
