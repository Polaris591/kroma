#!/usr/bin/env python3
"""Single fresh-isolated P8 F coding regression with independent Python checks.

This does not re-run or overwrite P7. Actual provider credentials are never
printed or copied to the task directory. No agent task-level timeout.
"""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
LAB = Path("/root/kroma-p8-regression")
BINARY = ROOT / "target/release/kroma"


def load_p1():
    spec = importlib.util.spec_from_file_location("p8_p1", ROOT / "benchmarks/p0p1_fair.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def main():
    if LAB.exists():
        raise SystemExit(f"Cannot overwrite pre-existing regression: {LAB}")
    LAB.mkdir(mode=0o700)
    p1 = load_p1()
    original = p1.prepare("bug", "p8-f-final")
    source = LAB / "source"
    shutil.move(str(original), source)
    hidden_test = source / "tests/test_p1_regression.py"
    original_hidden_test = hidden_test.read_text()
    assert not p1.verify("bug", source), "fixture already passed before agent"

    env = os.environ.copy()
    for key in ("KROMA_MODEL", "KROMA_BASE_URL", "KROMA_API_KEY",
                "KROMA_CONFIG_FILE", "KROMA_PROVIDER_PROFILE"):
        env.pop(key, None)
    env.update(
        KROMA_WORKDIR=str(source),
        KROMA_SESSION_FILE=str(LAB / "session.json"),
        KROMA_PROFILE="1",
    )
    stdout, stderr = LAB / "stdout.log", LAB / "stderr.log"
    prompt = p1.TASKS["bug"] + " Work only in this isolated checkout. Do not change hidden tests or access credentials."
    start = time.monotonic()
    with stdout.open("wb") as out, stderr.open("wb") as err:
        os.chmod(stdout, 0o600)
        os.chmod(stderr, 0o600)
        process = subprocess.Popen(
            [str(BINARY), prompt], cwd=source, env=env,
            stdin=subprocess.DEVNULL, stdout=out, stderr=err
        )
        returncode = process.wait()
    elapsed = round(time.monotonic() - start, 3)
    accepted = p1.verify("bug", source)
    unchanged = hidden_test.read_text() == original_hidden_test
    upstream = subprocess.run(
        ["python3", "-m", "pytest", "-q", "tests/test_urlutils.py"],
        cwd=source, capture_output=True, text=True
    ).returncode == 0
    log = stderr.read_text(errors="replace")
    model = re.search(r"\[usage\] task: (\d+) request\(s\)", log)
    apec = re.search(r"\[usage\] APEC: (\d+) request\(s\)", log)
    result = {
        "suite": "p8_f_single_live_python",
        "elapsed_s": elapsed,
        "exit_code": returncode,
        "independent_acceptance": accepted,
        "hidden_test_unchanged": unchanged,
        "upstream_tests": upstream,
        "durable_session": (LAB / "session.json").is_file() and
        (LAB / "session.json.wal").is_file(),
        "model_completed_requests": int(model.group(1)) if model else None,
        "apec_requests": int(apec.group(1)) if apec else 0,
        "has_interruption_marker": "Kroma interrupted" in log,
        "tool_calls": len(re.findall(r"^\[shell\]", log, flags=re.M)),
    }
    (LAB / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print("P8_LIVE_REGRESSION", json.dumps(result, sort_keys=True), flush=True)
    if not (returncode == 0 and accepted and unchanged and upstream and result["durable_session"]):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
