#!/usr/bin/env bash
# Run every workspace test binary except the two that need a real sandbox backend.
#
# Why this exists: `cargo test -- --skip NAME` filters *test names*, not binaries, so naming
# `b13_sandbox_isolation` there silently skips nothing. The two sandbox-dependent suites also refuse to
# pass without a usable bwrap, which is the right behaviour and must not be weakened -- they are run by
# scripts/run-sandbox-tests.sh instead, and their result is reported separately.
#
# Both ci.yml and release.yml call this script so the quality gate cannot drift between the two
# workflows: a green CI run and a green release run mean the same thing.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

binlist="$(mktemp)"
trap 'rm -f "$binlist"' EXIT

if ! cargo test --workspace --no-run --message-format=json 2>/dev/null \
  | grep -o '"executable":"[^"]*"' | sed 's/"executable":"//; s/"$//' \
  | grep -v -e b13_sandbox_isolation -e t09_execution_boundary > "$binlist"; then
  echo "could not enumerate test binaries" >&2
  exit 1
fi

count="$(wc -l < "$binlist")"
if [ "$count" -lt 1 ]; then
  echo "no test binaries were built -- refusing to report success" >&2
  exit 1
fi
echo "running $count test binaries (2 sandbox-dependent suites excluded, see run-sandbox-tests.sh)"

fail=0
while read -r bin; do
  echo "== $(basename "$bin")"
  if ! "$bin"; then
    fail=1
  fi
done < "$binlist"

if [ "$fail" -ne 0 ]; then
  echo "at least one test binary failed" >&2
fi
exit "$fail"
