#!/usr/bin/env python3
"""Negatives for scripts/verify-prereg.py (M2-01).

The fixture is hermetic: it copies the preregistrations, creates the suites and the out-of-workspace
verifier they reference, and rewrites the paths to point inside the temporary tree. A fixture that
borrowed the real acceptance site would pass only on the machine that happens to have it, which is
exactly the defect this file exists to prevent.
"""
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VALIDATOR = ROOT / "scripts" / "verify-prereg.py"
failures = []


def check(name, condition, detail=""):
    print(f"ok   {name}" if condition else f"FAIL {name} {detail}")
    if not condition:
        failures.append(name)


def run(root):
    # Run the *copy*: it derives its root from its own location, so validating the original
    # script would silently check the real repository instead of the mutated copy.
    return subprocess.run(
        [sys.executable, str(root / "scripts" / "verify-prereg.py")],
        cwd=root,
        capture_output=True,
        text=True,
    )


def build_fixture(tmp: Path) -> tuple[Path, int]:
    """A repo whose preregistrations reference files that exist inside the fixture."""
    root = tmp / "repo"
    (root / "scripts").mkdir(parents=True)
    shutil.copy(VALIDATOR, root / "scripts" / "verify-prereg.py")
    prereg_dir = root / "experiments" / "prereg"
    prereg_dir.mkdir(parents=True)
    suites = root / "experiments" / "suites"
    suites.mkdir(parents=True)
    (suites / "dev.json").write_text('[{"id": "t1"}]', encoding="utf-8")
    (suites / "holdout.json").write_text('[{"id": "t2"}]', encoding="utf-8")
    # The verifier must sit outside the workspace, so it is a sibling of the repo directory.
    verifier = tmp / "verifier.py"
    verifier.write_text("#!/usr/bin/env python3\nraise SystemExit(0)\n", encoding="utf-8")

    copied = 0
    for source in sorted((ROOT / "experiments" / "prereg").glob("*.json")):
        doc = json.loads(source.read_text(encoding="utf-8"))
        cluster = doc.setdefault("task_cluster_definition", {})
        cluster["dev_suite"] = "experiments/suites/dev.json"
        cluster["holdout_suite"] = "experiments/suites/holdout.json"
        doc.setdefault("holdout_policy", {})["verifier_path"] = str(verifier)
        (prereg_dir / source.name).write_text(
            json.dumps(doc, ensure_ascii=False), encoding="utf-8"
        )
        copied += 1
    return root, copied


with tempfile.TemporaryDirectory() as tmp:
    root, expected = build_fixture(Path(tmp))

    ok = run(root)
    check("baseline passes", ok.returncode == 0, ok.stdout)
    check(
        "baseline validates every preregistration, not only the E-prefixed ones",
        f"{expected} preregistration(s), 0 problem(s), 0 environment item(s)" in ok.stdout,
        ok.stdout,
    )

    target = root / "experiments" / "prereg" / "E01.json"
    original = json.loads(target.read_text(encoding="utf-8"))

    def rewrite(mutate):
        doc = json.loads(json.dumps(original))
        mutate(doc)
        target.write_text(json.dumps(doc, ensure_ascii=False), encoding="utf-8")

    rewrite(lambda doc: doc.pop("stopping_rule"))
    missing = run(root)
    check("missing field is refused", missing.returncode == 1, missing.stdout)
    check("missing field is named", "missing `stopping_rule`" in missing.stdout, missing.stdout)

    rewrite(lambda doc: doc["holdout_policy"].update(verifier_path=str(root / "verifier.py")))
    inside = run(root)
    check("verifier inside the workspace is refused", inside.returncode == 1, inside.stdout)
    check("the reason names the workspace", "outside the workspace" in inside.stdout, inside.stdout)

    rewrite(lambda doc: doc["task_cluster_definition"].update(holdout_suite="experiments/suites/dev.json"))
    same = run(root)
    check("one suite as both dev and holdout is refused", same.returncode == 1, same.stdout)
    check("the reason names the collision", "must not be the same suite" in same.stdout, same.stdout)

    # The two-level split: an absent acceptance site is reported, but it is not a structural failure.
    rewrite(lambda doc: None)
    (root / "experiments" / "suites" / "holdout.json").unlink()
    absent = run(root)
    check(
        "a missing oracle does not fail the structure check",
        absent.returncode == 0,
        absent.stdout,
    )
    check(
        "a missing oracle is reported as ENV_MISSING",
        "ENV_MISSING" in absent.stdout,
        absent.stdout,
    )

if failures:
    print(f"\n{len(failures)} failure(s): {', '.join(failures)}")
    raise SystemExit(1)
print("\nall preregistration fixtures behaved")
