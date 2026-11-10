#!/usr/bin/env python3
"""Discover real historical regression commits in small open-source libraries.

Never use task-source repairs as scoring or as model prompt material.
Repositories are cloned ONLY under the isolated experiment cache.
"""
from __future__ import annotations
from pathlib import Path
import subprocess

ROOT = Path("D:/Kroma/experiments/kroma-round14-real-issues/upstream")
REPOS = {
    "p-map": "https://github.com/sindresorhus/p-map.git",
    "p-limit": "https://github.com/sindresorhus/p-limit.git",
    "itoa": "https://github.com/dtolnay/itoa.git",
    "unicode-width": "https://github.com/unicode-rs/unicode-width.git",
}
for name, remote in REPOS.items():
    repo = ROOT / name
    if not repo.joinpath(".git").exists():
        repo.parent.mkdir(parents=True, exist_ok=True)
        print("CLONE", name, flush=True)
        result = subprocess.run(
            ["git", "clone", "--filter=blob:none", "--depth", "120",
             "--single-branch", "--quiet", remote, str(repo)],
            text=True, capture_output=True, timeout=135,
        )
        if result.returncode:
            print("CLONE_FAILED", name, result.stderr[-250:], flush=True)
            continue
    cmd = ["git", "log", "--format=%h|%s", "--regexp-ignore-case", "--extended-regexp",
           "--grep=fix|bug|regress|incorrect|wrong|edge.case|handle", "-35"]
    report = subprocess.run(cmd,cwd=repo,text=True,capture_output=True,timeout=20)
    print("==", name, "HEAD",
          subprocess.check_output(["git","rev-parse","--short","HEAD"],cwd=repo,text=True).strip(),
          "==", flush=True)
    print(report.stdout[:4800],flush=True)
