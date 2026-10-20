#!/usr/bin/env python3
"""Directed checks for scripts/ci-artifact-ingest.py (M0-03-A3).

Builds a throwaway artifact bundle, ingests it, verifies it, then corrupts one byte
and requires verification to fail. No network, no cargo, standard library only.

    python3 scripts/test-ci-artifact-ingest.py
"""

import json
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "ci-artifact-ingest.py"
failures = []


def check(name, condition, detail=""):
    print(f"ok   {name}" if condition else f"FAIL {name} {detail}")
    if not condition:
        failures.append(name)


def call(*args):
    return subprocess.run([sys.executable, str(SCRIPT), *args],
                          capture_output=True, text=True)


with tempfile.TemporaryDirectory() as tmp:
    root = Path(tmp)

    # A downloaded artifact: one zip (two files) and one extracted directory.
    bundle_zip = root / "kroma-linux-x64.zip"
    with zipfile.ZipFile(bundle_zip, "w") as archive:
        archive.writestr("bin/kroma", b"#!/bin/sh\necho kroma\n")
        archive.writestr("SHA256SUMS", b"deadbeef  bin/kroma\n")

    extracted = root / "kroma-report"
    extracted.mkdir()
    (extracted / "report.json").write_text('{"jobs": 24}\n', encoding="utf-8")

    manifest_path = root / "manifest.json"
    build = call("build", "--run-id", "39106253179",
                 "--artifact", str(bundle_zip), "--artifact", str(extracted),
                 "--out", str(manifest_path))
    check("build exits 0", build.returncode == 0, build.stderr)
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    check("two artifacts recorded", manifest["artifact_count"] == 2)
    check("three files recorded",
          sum(artifact["file_count"] for artifact in manifest["artifacts"]) == 3,
          manifest)
    check("run id carried through", manifest["run_id"] == "39106253179")
    check("every file has a sha256",
          all(len(entry["sha256"]) == 64
              for artifact in manifest["artifacts"] for entry in artifact["files"]))

    verify = call("verify", "--manifest", str(manifest_path))
    check("verify accepts the untouched bundle", verify.returncode == 0, verify.stdout)

    # Corrupt one byte inside the extracted artifact: verification must fail.
    (extracted / "report.json").write_text('{"jobs": 25}\n', encoding="utf-8")
    tampered = call("verify", "--manifest", str(manifest_path))
    check("verify rejects a changed byte", tampered.returncode == 1, tampered.stdout)
    check("verify names the changed file", "changed kroma-report/report.json" in tampered.stdout,
          tampered.stdout)

    # A missing artifact is reported, not silently skipped.
    (extracted / "report.json").unlink()
    missing = call("verify", "--manifest", str(manifest_path))
    check("verify reports a missing file", missing.returncode == 1 and "missing" in missing.stdout,
          missing.stdout)

    # Usage: build without artifacts is a usage error, not an empty manifest.
    empty = call("build", "--run-id", "1", "--out", str(root / "empty.json"))
    check("build refuses with no artifact", empty.returncode == 2, empty.stdout)

print(f"\n{len(failures)} failure(s)")
sys.exit(1 if failures else 0)
