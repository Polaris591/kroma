#!/usr/bin/env bash
# Run the suites that prove real sandbox isolation: b13_sandbox_isolation, t09_execution_boundary,
# t31_codemode_execution, t33_bench_execution_boundary, t35_surface_execution_boundary and
# t36_external_verifier_delivery and t38_prereg_oracle_authority. They are
# excluded from scripts/run-workspace-tests.sh by exact target name, so this script is the only place
# that runs them and a host without a usable sandbox must not report them as passed.
#
# These are deliberately hard-failing: a host without a usable bwrap cannot prove the isolation claim, and
# turning that into a skip would make the claim unfalsifiable. The caller decides what an unavailable
# sandbox means for its own gate -- a hosted CI runner cannot create unprivileged user namespaces, so the
# release workflow records HOST_CAPABILITY_BLOCKED instead of pretending the suite passed.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

if ! command -v bwrap >/dev/null 2>&1; then
  echo "bwrap is not installed on this host" >&2
  exit 2
fi
if ! bwrap --ro-bind / / --dev /dev --proc /proc --unshare-all --die-with-parent true 2>/tmp/bwrap-probe.err; then
  echo "bwrap cannot create a namespace on this host: $(head -1 /tmp/bwrap-probe.err)" >&2
  exit 2
fi

exec cargo test -p kroma-core \
  --test b13_sandbox_isolation \
  --test t09_execution_boundary \
  --test t31_codemode_execution \
  --test t33_bench_execution_boundary \
  --test t35_surface_execution_boundary \
  --test t36_external_verifier_delivery \
  --test t38_prereg_oracle_authority
