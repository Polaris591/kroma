#!/usr/bin/env python3
"""Pick the test harnesses out of `cargo test --no-run --message-format=json`.

`cargo test --no-run` also builds the workspace's ordinary binaries and examples, and those
artifacts carry an `executable` too. That is normal Cargo behaviour (an integration test needs the
crate to compile), but it means "run every artifact that has an executable" runs the product entry
point and the examples as if they were tests: `examples/paired_smoke` exits non-zero without its
required arguments, and `kroma` starts a real run.

The rule here is structural, not textual: a test harness is a `compiler-artifact` message whose
`profile.test` is true. Target names that need a real sandbox backend are excluded by exact name,
because a substring match on the path also drops unrelated binaries.

Reads Cargo's JSON messages on stdin, writes one executable path per line to stdout, and prints what
it skipped to stderr.

    cargo test --workspace --no-run --message-format=json | scripts/select-test-binaries.py
"""

import json
import os
import sys

# Suites that refuse to pass without a usable sandbox backend. scripts/run-sandbox-tests.sh runs
# them; a workspace run that silently skipped them would be reporting a weaker gate than it claims.
EXCLUDED_TARGETS = frozenset(
    {
        "b13_sandbox_isolation",
        "t09_execution_boundary",
        "t31_codemode_execution",
        "t33_bench_execution_boundary",
        "t35_surface_execution_boundary",
        "t36_external_verifier_delivery",
        "t38_prereg_oracle_authority",
    }
)


def select(stream, out, err):
    """Write the selected harness paths to `out`; return a process exit code."""
    selected = []
    seen = set()
    skipped_non_test = []
    excluded = []

    for number, line in enumerate(stream, start=1):
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError as error:
            print(
                "select-test-binaries: line %d is not a Cargo JSON message (%s): %s"
                % (number, error.msg, line[:200]),
                file=err,
            )
            return 1
        if message.get("reason") != "compiler-artifact":
            continue
        executable = message.get("executable")
        if not executable:
            continue

        target = message.get("target") or {}
        name = target.get("name") or "?"
        profile = message.get("profile") or {}
        if profile.get("test") is not True:
            skipped_non_test.append(name)
            continue
        if name in EXCLUDED_TARGETS:
            excluded.append(name)
            continue
        if executable in seen:
            continue
        if not os.path.isfile(executable):
            print(
                "select-test-binaries: Cargo reported harness '%s' at '%s', which does not exist"
                % (name, executable),
                file=err,
            )
            return 1
        seen.add(executable)
        selected.append(executable)

    if not selected:
        print(
            "select-test-binaries: Cargo reported no test-profile artifacts with an executable; "
            "refusing to report success",
            file=err,
        )
        return 1

    print(
        "select-test-binaries: %d test harnesses; %d non-test artifacts not run (%s); "
        "%d sandbox-dependent harnesses excluded (%s)"
        % (
            len(selected),
            len(skipped_non_test),
            ", ".join(skipped_non_test) or "none",
            len(excluded),
            ", ".join(excluded) or "none",
        ),
        file=err,
    )
    for path in selected:
        print(path, file=out)
    return 0


def main():
    return select(sys.stdin, sys.stdout, sys.stderr)


if __name__ == "__main__":
    sys.exit(main())
