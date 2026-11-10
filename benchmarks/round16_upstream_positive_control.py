#!/usr/bin/env python3
"""Positive control: historical upstream PR#89 fix against identical reduced offline Rust acceptance.

No model requests. The same fixture with pinned historical pre-fix sources,
and only the two files actually changed by the PR, overwritten with upstream
post-PR files. Negative and positive controls must differ as expected.
"""
from pathlib import Path
import hashlib
import subprocess
import round15_real_rust_unicode_width as base

ROOT=Path("D:/Kroma/experiments/kroma-round16-rust-positive-control")
FIXED=Path(__file__).resolve().parent/"round16_upstream_fixed"
project=ROOT/"project"
project.mkdir(parents=True,exist_ok=False)
frozen=base.fixture(project)
env=base.env_for(project,ROOT,"baseline")
negative,detail=base.oracle(project,env,frozen,True)
print("PRE_FIX",negative,ascii(detail[-350:]),flush=True)
if negative:
    raise RuntimeError("negative control unexpectedly passes")
for asset,dest in [("scripts__unicode.py","scripts/unicode.py"),("src__gen__tables.rs","src/gen/tables.rs")]:
    (project/dest).write_bytes((FIXED/asset).read_bytes())
fixed,detail=base.oracle(project,env,frozen,True)
print("UPSTREAM_FIXED",fixed,ascii(detail[-450:]),flush=True)
if not fixed:
    raise RuntimeError("upstream positive control failed")
print("POSITIVE_AND_NEGATIVE_CONTROLS_BOTH_VALID",flush=True)
