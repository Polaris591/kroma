#!/usr/bin/env python3
"""No third-party YAML dependency: enforce release/CI gate invariants before publishing.

Regression mutations prove the checker fails if checkout, tag Python gates, or mandatory sandbox
isolation is removed. The job/step indentation is a part of our checked-in workflow contract.
"""
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
PYTHON_GATES = (
    "scripts/test-analyze-paired.py",
    "scripts/test-ci-artifact-ingest.py",
    "scripts/test-trusted-gate.py",
    "scripts/test-run-workspace-tests.py",
    "scripts/test-verify-prereg.py",
    "scripts/verify-prereg.py",
)


def jobs(text):
    section = re.search(r"^jobs:\s*$", text, re.M)
    if section is None:
        raise AssertionError("no jobs stanza")
    result = {}
    current = None
    for line in text[section.end():].splitlines():
        job = re.match(r"^  ([a-z][a-z0-9_-]*):\s*$", line)
        if job:
            current = job.group(1)
            result[current] = []
        elif current is not None:
            result[current].append(line)
    return {key: "\n".join(lines) for key, lines in result.items()}


def steps(job):
    start = re.search(r"^    steps:\s*$", job, re.M)
    if not start:
        raise AssertionError("job has no steps")
    return [block for block in re.split(r"(?=^      - (?:uses:|name:|run:))", job[start.end():], flags=re.M) if block.strip()]


def validate_release(text):
    all_jobs = jobs(text)
    dispatch = all_jobs["validate-dispatch"]
    quality = all_jobs["quality"]
    verdict = all_jobs["release-verdict"]
    assert "workflow_dispatch" in dispatch.split("\n", 2)[0], "manual input validator must execute on manual runs"
    assert "github.event_name == 'push'" in quality, "tag push must run quality gates"
    assert "validate-dispatch" in verdict, "second-pass verdict must require validated inputs"
    for label, job in (("validate-dispatch", dispatch), ("quality", quality)):
        sequence = steps(job)
        checkout_at = next((i for i, step in enumerate(sequence) if "actions/checkout@" in step), None)
        if checkout_at is None:
            raise AssertionError(f"{label}: source checkout missing")
        for gate in PYTHON_GATES:
            gate_at = next((i for i, step in enumerate(sequence) if gate in step), None)
            if gate_at is None or gate_at <= checkout_at:
                raise AssertionError(f"{label}: {gate} must run after checkout")
    assert "scripts/test-updater-faults.js" in quality, "tag build must fault-test the updater"


def validate_ci(text):
    all_jobs = jobs(text)
    sandbox = all_jobs["sandbox-isolation"]
    assert not re.search(r"^    continue-on-error:\s*true\s*$", sandbox, re.M), (
        "sandbox test failure must not turn CI green"
    )
    assert "scripts/run-sandbox-tests.sh" in sandbox, "CI must run a real sandbox suite"
    assert "scripts/test-updater-faults.js" in all_jobs["lint-and-test"], "CI must fault-test the updater"


def rejects(fn, broken, name):
    try:
        fn(broken)
    except (AssertionError, KeyError):
        return
    raise AssertionError(f"negative fixture unexpectedly passed: {name}")


def main():
    release = (ROOT / ".github/workflows/release.yml").read_text()
    ci = (ROOT / ".github/workflows/ci.yml").read_text()
    validate_release(release)
    validate_ci(ci)
    rejects(validate_release, release.replace(
        "      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4\n      # Validate source gates",
        "      # checkout accidentally removed\n      # Validate source gates", 1), "no manual checkout")
    rejects(validate_release, release.replace("          python3 scripts/test-verify-prereg.py\n", "", 1), "missing gate")
    rejects(validate_ci, ci.replace("  sandbox-isolation:\n", "  sandbox-isolation:\n    continue-on-error: true\n", 1), "fail-open sandbox")
    print("workflow contract PASS: tag/manual gates, checkout ordering, sandbox hard fail, adversarial mutations")


if __name__ == "__main__":
    main()
