#!/usr/bin/env python3
"""M2-01: refuse to run an experiment that is not preregistered.

The blueprint's rule is that a pre-registration lands before the first paid request, and that the
holdout cannot be tuned against. This checks the second half mechanically: every experiment has
all eight required fields filled in, dev and holdout are different suites, and the verifier lives
outside the workspace.

Two levels, because they fail for different reasons:

* **structure** - the eight required blocks are filled in, dev and holdout differ, the verifier path
  is absolute and outside the workspace. This is the repository's own contract and must always hold.
* **oracle** - the suite files and the verifier actually exist. They live in the acceptance site
  (`/opt/kroma-exp`), which a clean checkout does not have, so a missing oracle is reported as
  `ENV_MISSING` and does not fail the structure check. The gate that must not be fooled - `kroma
  bench --executor provider` refusing a run its preregistration did not admit - resolves the same
  paths at the moment of a paid request, where they are present by construction.

Exit 0 = structure holds; 1 = at least one structural problem, named on stdout.
"""
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REQUIRED = (
    "task_cluster_definition",
    "model_identity_observability",
    "quality_margin",
    "noise_and_power_plan",
    "cache_order_design",
    "budget",
    "stopping_rule",
    "holdout_policy",
)
NESTED = {
    "task_cluster_definition": ("cluster", "dev_suite", "holdout_suite"),
    "quality_margin": ("primary_metric", "delta", "rule"),
    "noise_and_power_plan": ("plan", "interval"),
    "cache_order_design": ("order",),
    "budget": ("plan",),
    "holdout_policy": ("queries", "verifier_path"),
}


def check(path: Path) -> tuple[list[str], list[str]]:
    """Return (structural problems, environment problems)."""
    problems = []
    environment = []
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        return [f"{path.name}: unreadable ({exc})"], []

    for field in REQUIRED:
        value = doc.get(field)
        if value in (None, "", {}, []):
            problems.append(f"{path.name}: missing `{field}`")
    for field, keys in NESTED.items():
        block = doc.get(field) or {}
        for key in keys:
            if block.get(key) in (None, ""):
                problems.append(f"{path.name}: missing `{field}.{key}`")

    cluster = doc.get("task_cluster_definition") or {}
    dev, holdout = cluster.get("dev_suite"), cluster.get("holdout_suite")
    if dev and holdout and dev == holdout:
        problems.append(f"{path.name}: dev and holdout must not be the same suite")

    # Pointing at a suite that is not there used to pass this gate: fields were
    # filled in, the file they named was not. Check existence, not intention.
    for field, rel in (("dev_suite", dev), ("holdout_suite", holdout)):
        if not rel:
            continue
        suite = (ROOT / rel) if not str(rel).startswith("/") else Path(rel)
        if not suite.is_file():
            environment.append(f"ENV_MISSING {path.name}: {field} does not exist ({rel})")
            continue
        try:
            json.loads(suite.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            problems.append(f"{path.name}: {field} is not readable JSON ({exc})")

    verifier = (doc.get("holdout_policy") or {}).get("verifier_path") or ""
    if not verifier.startswith("/"):
        problems.append(f"{path.name}: verifier_path must be absolute")
    elif str(ROOT) in verifier:
        problems.append(f"{path.name}: the verifier must live outside the workspace")
    elif not Path(verifier).is_file():
        environment.append(f"ENV_MISSING {path.name}: verifier_path does not exist ({verifier})")

    if doc.get("status") != "PREREGISTERED_NOT_RUN" and not doc.get("results"):
        problems.append(f"{path.name}: claims {doc.get('status')} without results")

    return problems, environment


def main() -> int:
    # Every preregistration, not only the ones named after an experiment id: M2-02, M2-04 and M3-03
    # carry the same contract and were silently skipped by an `E*` glob.
    files = sorted((ROOT / "experiments" / "prereg").glob("*.json"))
    if not files:
        print("no preregistrations found")
        return 1
    structural, environment = [], []
    for path in files:
        found, missing = check(path)
        structural.extend(found)
        environment.extend(missing)
    for problem in structural + environment:
        print(problem)
    print(
        f"{len(files)} preregistration(s), {len(structural)} problem(s), "
        f"{len(environment)} environment item(s)"
    )
    return 1 if structural else 0


if __name__ == "__main__":
    raise SystemExit(main())
