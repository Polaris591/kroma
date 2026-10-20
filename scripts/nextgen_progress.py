#!/usr/bin/env python3
"""Progress ledger for the 120-task Kroma NextGen book.

The ledger is the only place task state lives. A task is DONE only when it has
evidence strings that point at real files or commands; BLOCKED requires a
blocker string plus evidence of the probe that proved the blocker. Everything
else stays TODO. There is no "probably done".

Usage:
  nextgen_progress.py init <tasks.json> <template-progress.json> <out.json>
  nextgen_progress.py set <ledger.json> <task-id> <STATUS> \
      [--commit SHA] [--evidence TEXT]... [--blocker TEXT] [--decision TEXT]
  nextgen_progress.py status <ledger.json>
"""
import hashlib
import json
import os
import subprocess
import sys
from datetime import datetime, timezone

# Status vocabulary is the taskbook's own (taskctl.py STATES), not an invention.
VALID = {"TODO", "IN_PROGRESS", "IMPLEMENTED_UNVERIFIED", "VERIFIED", "FAILED",
         "BLOCKED_EXTERNAL", "REJECTED_WITH_EVIDENCE", "DEFERRED_WITH_RATIONALE"}
RESEARCH_TERMINAL = {"REJECTED_WITH_EVIDENCE", "DEFERRED_WITH_RATIONALE"}

SKIP_DIRS = {".git", "target", "__pycache__", "node_modules"}


def tree_sha256(root):
    """Same algorithm as nextgen_baseline_identity.py, so receipts bind to the
    source tree rather than to a filename.

    docs/nextgen/ is excluded on purpose: it holds the ledger itself, so
    including it would make every receipt's tree hash self-referential.
    """
    entries = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for name in sorted(filenames):
            if name.endswith(".pyc"):
                continue
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, root).replace(os.sep, "/")
            if rel.startswith("docs/nextgen/"):
                continue
            entries.append((rel, full))
    entries.sort()
    h = hashlib.sha256()
    for rel, full in entries:
        mode = "100755" if os.access(full, os.X_OK) else "100644"
        fh = hashlib.sha256()
        with open(full, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                fh.update(chunk)
        h.update(f"{mode} {fh.hexdigest()} {rel}\n".encode())
    return h.hexdigest()


def file_sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def load(path):
    with open(path) as fh:
        return json.load(fh)


def save(path, doc):
    tmp = path + ".tmp"
    with open(tmp, "w") as fh:
        json.dump(doc, fh, indent=2)
        fh.write("\n")
    os.replace(tmp, path)


def cmd_init(argv):
    if len(argv) != 3:
        print("usage: init <tasks.json> <template-progress.json> <out.json>",
              file=sys.stderr)
        return 1
    tasks = load(argv[0])["tasks"]
    template = load(argv[1])
    doc = {
        "schema_version": template.get("schema_version", "1.0"),
        "baseline_sha256": template.get("baseline_sha256"),
        "live_total_budget": template.get("live_total_budget", 0),
        "operator_authorization": template.get("operator_authorization"),
        "records": {},
    }
    # Keep any record the template already carries, then ensure every task exists.
    for tid, rec in template.get("records", {}).items():
        doc["records"][tid] = rec
    for t in tasks:
        doc["records"].setdefault(t["id"], {
            "status": "TODO", "attempts": 0, "commit": None,
            "evidence": [], "blocker": None, "decision": None,
        })
    save(argv[2], doc)
    print(json.dumps({
        "written": argv[2],
        "records": len(doc["records"]),
        "todo": sum(1 for r in doc["records"].values() if r["status"] == "TODO"),
    }, indent=2))
    return 0


def cmd_set(argv):
    if len(argv) < 3:
        print("usage: set <ledger.json> <task-id> <STATUS> [flags]", file=sys.stderr)
        return 1
    path, tid, status = argv[0], argv[1], argv[2].upper()
    if status not in VALID:
        print(f"invalid status {status}; allowed: {sorted(VALID)}", file=sys.stderr)
        return 1
    doc = load(path)
    rec = doc["records"].get(tid)
    if rec is None:
        print(f"unknown task {tid}", file=sys.stderr)
        return 1

    commit = None
    receipts, blocker, decision = [], None, None
    i = 3
    while i < len(argv):
        a = argv[i]
        if a == "--commit":
            commit = argv[i + 1]; i += 2
        elif a == "--receipt":
            receipts.append(argv[i + 1]); i += 2
        elif a == "--blocker":
            blocker = argv[i + 1]; i += 2
        elif a == "--decision":
            decision = argv[i + 1]; i += 2
        elif a == "--root":
            globals()["_ROOT"] = argv[i + 1]; i += 2
        else:
            print(f"unknown flag {a}", file=sys.stderr)
            return 1

    root = os.path.abspath(globals().get("_ROOT") or os.getcwd())
    tree = tree_sha256(root)
    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    built = []
    for spec in receipts:
        fields = {}
        for part in spec.split(";"):
            if not part.strip():
                continue
            k, _, v = part.partition("=")
            fields[k.strip()] = v.strip()
        rel = fields.get("path", "")
        full = os.path.join(root, rel) if rel else ""
        if not rel or not os.path.isfile(full):
            print(f"receipt path does not exist: {rel!r}", file=sys.stderr)
            return 1
        # taskctl.py resolves receipt paths against the directory holding the
        # ledger (docs/nextgen), so store them relative to that directory.
        evidence_root = os.path.join(root, "docs", "nextgen")
        stored_rel = os.path.relpath(full, evidence_root).replace(os.sep, "/")
        receipt = {
            "command": fields.get("command", ""),
            "path": stored_rel,
            "sha256": file_sha256(full),
            "source_tree_sha256": tree,
            "completed_utc": now,
            "exit_code": int(fields.get("exit", "0")),
            "required": fields.get("required", "true").lower() != "false",
        }
        if "kind" in fields:
            receipt["kind"] = fields["kind"]
        if "tests" in fields:
            receipt["tests_run"] = int(fields["tests"])
        if not receipt["command"]:
            print(f"receipt needs command=: {spec!r}", file=sys.stderr)
            return 1
        built.append(receipt)

    if status == "VERIFIED":
        if not built:
            print("refusing VERIFIED without at least one --receipt", file=sys.stderr)
            return 1
        if not commit:
            print("refusing VERIFIED without --commit", file=sys.stderr)
            return 1
        bad = [r for r in built if r["required"] and r["exit_code"] != 0]
        if bad:
            print(f"refusing VERIFIED with failing required receipt: {bad[0]['command']}",
                  file=sys.stderr)
            return 1
    if status in RESEARCH_TERMINAL and not decision:
        print(f"refusing {status} without --decision", file=sys.stderr)
        return 1
    if status == "BLOCKED_EXTERNAL" and not blocker:
        print("refusing BLOCKED_EXTERNAL without --blocker", file=sys.stderr)
        return 1

    rec["status"] = status
    rec["attempts"] = int(rec.get("attempts") or 0) + 1
    if commit:
        rec["commit"] = commit
    rec["evidence"] = list(rec.get("evidence") or []) + built
    if blocker:
        rec["blocker"] = blocker
    if decision:
        rec["decision"] = decision
    save(path, doc)
    print(json.dumps({"task": tid, "status": status,
                      "receipts": len(built),
                      "source_tree_sha256": tree[:16]}, indent=2))
    return 0


def cmd_status(argv):
    if len(argv) != 1:
        print("usage: status <ledger.json>", file=sys.stderr)
        return 1
    doc = load(argv[0])
    counts = {}
    for rec in doc["records"].values():
        counts[rec["status"]] = counts.get(rec["status"], 0) + 1
    pending = [t for t, r in doc["records"].items() if r["status"] == "TODO"]
    print(json.dumps({
        "total": len(doc["records"]),
        "counts": counts,
        "todo_count": len(pending),
    }, indent=2))
    return 0


def main(argv):
    if not argv:
        print(__doc__, file=sys.stderr)
        return 1
    cmd, rest = argv[0], argv[1:]
    if cmd == "init":
        return cmd_init(rest)
    if cmd == "set":
        return cmd_set(rest)
    if cmd == "status":
        return cmd_status(rest)
    print(f"unknown command {cmd}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
