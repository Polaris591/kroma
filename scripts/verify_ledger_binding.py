#!/usr/bin/env python3
"""Verify that every ledger receipt's `source_tree_sha256` really describes its recorded commit.

The taskbook's own `taskctl.py` only checks that the field is 64 hex characters -- it never
recomputes it. That means a receipt could name any tree and still validate. This script closes that
gap: for each receipt it takes the receipt's `commit`, rebuilds the source tree identity from git
objects (excluding docs/nextgen, exactly like the writer does), and compares.

Usage: verify_ledger_binding.py <source-root> <ledger.json>
Exit:  0 every receipt binds, 1 at least one does not (details on stdout), 2 usage error.
"""
import hashlib
import json
import os
import subprocess
import sys

SKIP_DIRS = {".git", "target", "__pycache__", "node_modules"}


def git(root, *args):
    r = subprocess.run(["git", "-C", root, *args], capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {r.stderr.strip()}")
    return r.stdout


def tree_identity_for_commit(root, commit):
    """Rebuild the writer's tree identity from the commit's own blobs."""
    listing = git(root, "ls-tree", "-r", "-z", "--full-tree", commit)
    entries = []
    for chunk in listing.split("\0"):
        if not chunk:
            continue
        meta, path = chunk.split("\t", 1)
        mode, _kind, blob = meta.split()
        if path.startswith("docs/nextgen/"):
            continue
        if path.endswith(".pyc"):
            continue
        entries.append((path, mode, blob))
    entries.sort()
    h = hashlib.sha256()
    for path, mode, blob in entries:
        content = subprocess.run(
            ["git", "-C", root, "cat-file", "blob", blob],
            capture_output=True,
        ).stdout
        content_sha = hashlib.sha256(content).hexdigest()
        h.update(f"{mode} {content_sha} {path}\n".encode())
    return h.hexdigest(), len(entries)


def main(argv):
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    root, ledger_path = os.path.abspath(argv[1]), argv[2]
    doc = json.load(open(ledger_path))

    cache = {}
    failures = []
    checked = 0
    for tid, rec in doc["records"].items():
        commit = rec.get("commit")
        for receipt in rec.get("evidence") or []:
            claimed = receipt.get("source_tree_sha256")
            if not claimed:
                continue
            if not commit:
                failures.append((tid, "receipt has a tree hash but the record has no commit"))
                continue
            if commit not in cache:
                try:
                    cache[commit] = tree_identity_for_commit(root, commit)
                except RuntimeError as e:
                    cache[commit] = (None, 0)
                    failures.append((tid, f"cannot rebuild tree for {commit}: {e}"))
                    continue
            actual, _n = cache[commit]
            checked += 1
            if actual != claimed:
                failures.append(
                    (tid, f"tree mismatch at {commit}: claimed {claimed[:16]}… actual {actual[:16]}…")
                )

    print(json.dumps({
        "receipts_checked": checked,
        "distinct_commits": len(cache),
        "failures": failures,
        "bound": not failures,
    }, indent=2))
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
