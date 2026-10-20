#!/usr/bin/env bash
# Run every workspace test harness except those that need a real sandbox backend.
#
# Why this exists: `cargo test -- --skip NAME` filters *test names*, not binaries, so naming
# `b13_sandbox_isolation` there silently skips nothing. The sandbox-dependent suites also refuse to
# pass without a usable bwrap, which is the right behaviour and must not be weakened -- they are run by
# scripts/run-sandbox-tests.sh instead, and their result is reported separately.
#
# Which artifacts count as a harness is decided by Cargo's own message stream, not by grepping every
# `executable`: `cargo test --no-run` also builds ordinary binaries and examples, and running those
# starts the product entry point (and examples that need arguments). scripts/select-test-binaries.py
# keeps only `compiler-artifact` messages with `profile.test == true` and excludes the sandbox suites
# by exact target name.
#
# Both ci.yml and release.yml call this script so the quality gate cannot drift between the two
# workflows: a green CI run and a green release run mean the same thing.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

command -v python3 >/dev/null || {
  echo "error: python3 is required to read Cargo's JSON messages" >&2
  exit 1
}

binlist="$(mktemp)"
selectlog="$(mktemp)"
trap 'rm -f "$binlist" "$selectlog"' EXIT

if ! cargo test --workspace --no-run --message-format=json 2>/dev/null \
  | python3 scripts/select-test-binaries.py >"$binlist" 2>"$selectlog"; then
  cat "$selectlog" >&2
  echo "could not enumerate test binaries" >&2
  exit 1
fi
cat "$selectlog" >&2

count="$(wc -l <"$binlist")"
if [ "$count" -lt 1 ]; then
  echo "no test binaries were built -- refusing to report success" >&2
  exit 1
fi
echo "running $count test binaries (sandbox-dependent suites excluded, see run-sandbox-tests.sh)"

fail=0
while read -r bin; do
  echo "== $(basename "$bin")"
  if ! "$bin"; then
    fail=1
  fi
done <"$binlist"

if [ "$fail" -ne 0 ]; then
  echo "at least one test binary failed" >&2
fi
exit "$fail"
