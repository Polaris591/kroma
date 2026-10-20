#!/usr/bin/env python3
"""KR-C01: classify every production process-spawn point.

The point is not to produce a nice table -- it is to make an *unclassified* spawn point fail the
gate. An isolation claim ("untrusted work runs in a sandbox") is only as strong as its weakest spawn
path, so a new `Command::new` that nobody classified must break the build of this audit rather than
silently inherit whatever trust the caller happened to have.

Classes are the ones the runtime already names (`sandbox::ExecutionClass`) plus the worker/lock
cases that never go through it.

Output: docs/nextgen/spawn_points.json
Exit:   0 every production spawn is classified, 1 at least one is not.
"""
import json
import os
import re
import sys

# file -> (class, policy, owner, evidence)
# "policy" records whether the path is wrapped in sandbox::ExecutionPolicy today. "none" is honest:
# it means the process runs on the host with the caller's privileges.
CLASSES = {
    "crates/kroma-core/src/tools/bash.rs": (
        "AgentBash",
        "ExecutionPolicy (guest workspace root, opt-in)",
        "run-scoped; cancelled by the run's watch channel",
        "t09_execution_boundary, t31_codemode_execution, b13_sandbox_isolation",
    ),
    "crates/kroma-core/src/tool/codemode.rs": (
        "AgentBash",
        "ExecutionPolicy (code mode runs through the supervised bash tool)",
        "run-scoped",
        "t31_codemode_execution",
    ),
    "crates/kroma-core/src/completion_gate.rs": (
        "Verifier",
        "ExecutionPolicy (probe + wrap_argv)",
        "verification step; bounded by the gate deadline",
        "t35_surface_execution_boundary, t36_external_verifier_delivery",
    ),
    "crates/kroma-core/src/extensions.rs": (
        "Hook",
        "none (ProcessLaunch only: credential scrub + process group)",
        "hook invocation; bounded by the shared hard-stop deadline",
        "none for isolation -- see KR-C02",
    ),
    "crates/kroma-core/src/mcp.rs": (
        "McpServer",
        "none (ProcessLaunch; sidecars are operator-configured infrastructure)",
        "MCP host lifetime; torn down on unmount",
        "test-mcp-*, t21/t22 protocol fixtures",
    ),
    "crates/kroma-core/src/provenance.rs": (
        "Probe",
        "sandbox::run_bounded (deadline + output budget)",
        "single bounded VCS probe",
        "b08c_git_metadata",
    ),
    "crates/kroma-core/src/job_scope.rs": (
        "Worker",
        "none (process-group bookkeeping for children the caller already owns)",
        "job scope; reclaims recorded groups",
        "job_scope unit tests",
    ),
    "crates/kroma-core/src/process_group.rs": (
        "Worker",
        "none (low-level group/signal primitive)",
        "caller-owned",
        "process_group unit tests",
    ),
    "crates/kroma-core/src/process_supervisor.rs": (
        "Worker",
        "none (supervision layer used by every caller above)",
        "caller-owned; deadline + output budget enforced here",
        "process_supervisor unit tests",
    ),
    "crates/kroma-core/src/process_launch.rs": (
        "Worker",
        "none (shared launch seam: scrub, group, fail-closed containment)",
        "caller-owned",
        "process_launch unit tests",
    ),
    "crates/kroma-core/src/sandbox.rs": (
        "Worker",
        "n/a (this *is* the policy/backend)",
        "n/a",
        "b13_sandbox_isolation",
    ),
    "crates/kroma-core/src/session_ownership.rs": (
        "Worker",
        "none (cross-process lock holder probe)",
        "lock test child",
        "session_ownership unit tests",
    ),
}

PATTERN = re.compile(
    r"\b(?:std::process::Command::new|Command::new|ProcessLaunch::new|tokio::process::Command::new)"
)
SKIP_DIRS = {".git", "target", "__pycache__"}


def scan(root):
    hits = []
    for base in ("crates/kroma-core/src", "crates/kroma-cli/src"):
        for dirpath, dirnames, filenames in os.walk(os.path.join(root, base)):
            dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
            for name in sorted(filenames):
                if not name.endswith(".rs"):
                    continue
                full = os.path.join(dirpath, name)
                rel = os.path.relpath(full, root).replace(os.sep, "/")
                with open(full, errors="replace") as fh:
                    for lineno, line in enumerate(fh, 1):
                        stripped = line.strip()
                        if stripped.startswith("//"):
                            continue
                        if PATTERN.search(line):
                            hits.append((rel, lineno, stripped[:160]))
    return hits


def main(argv):
    if len(argv) < 2:
        print("usage: nextgen_spawn_audit.py <source-root>", file=sys.stderr)
        return 1
    root = os.path.abspath(argv[1])
    hits = scan(root)

    unclassified = {}
    rows = []
    for rel, lineno, text in hits:
        entry = CLASSES.get(rel)
        if entry is None:
            unclassified.setdefault(rel, []).append(lineno)
            continue
        cls, policy, owner, evidence = entry
        rows.append({
            "file": rel,
            "line": lineno,
            "class": cls,
            "policy": policy,
            "owner": owner,
            "evidence": evidence,
            "snippet": text,
        })

    by_class = {}
    for row in rows:
        by_class.setdefault(row["class"], []).append(row["file"])

    doc = {
        "task": "KR-C01",
        "spawn_points": len(hits),
        "classified": len(rows),
        "unclassified": unclassified,
        "by_class": {k: sorted(set(v)) for k, v in sorted(by_class.items())},
        "classes_without_isolation": sorted(
            {r["class"] for r in rows if r["policy"].startswith("none")}
        ),
        "gate_passed": not unclassified,
        "note": (
            "Classes listed in classes_without_isolation run on the host today. That is a stated "
            "boundary, not an accident: docs/CAPABILITIES.md says the sandbox covers the bash "
            "execution class only. KR-C02 is the task that decides whether hooks need wrapping."
        ),
        "rows": rows,
    }

    out_dir = os.path.join(root, "docs", "nextgen")
    os.makedirs(out_dir, exist_ok=True)
    out = os.path.join(out_dir, "spawn_points.json")
    with open(out, "w") as fh:
        json.dump(doc, fh, indent=2)
        fh.write("\n")

    print(json.dumps({
        "written": os.path.relpath(out, root),
        "spawn_points": doc["spawn_points"],
        "classified": doc["classified"],
        "unclassified": doc["unclassified"],
        "by_class": {k: len(v) for k, v in doc["by_class"].items()},
        "classes_without_isolation": doc["classes_without_isolation"],
        "gate_passed": doc["gate_passed"],
    }, indent=2))
    return 0 if doc["gate_passed"] else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
