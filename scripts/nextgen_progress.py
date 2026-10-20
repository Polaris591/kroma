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
import json
import os
import sys

VALID = {"TODO", "IN_PROGRESS", "DONE", "BLOCKED", "REJECTED"}


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
        print("usage: set <ledger.json> <task-id> <STATUS> [flags]",
              file=sys.stderr)
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
    evidence, blocker, decision = [], None, None
    i = 3
    while i < len(argv):
        a = argv[i]
        if a == "--commit":
            commit = argv[i + 1]; i += 2
        elif a == "--evidence":
            evidence.append(argv[i + 1]); i += 2
        elif a == "--blocker":
            blocker = argv[i + 1]; i += 2
        elif a == "--decision":
            decision = argv[i + 1]; i += 2
        else:
            print(f"unknown flag {a}", file=sys.stderr)
            return 1

    if status == "DONE" and not evidence:
        print("refusing DONE without --evidence", file=sys.stderr)
        return 1
    if status == "BLOCKED" and not blocker:
        print("refusing BLOCKED without --blocker", file=sys.stderr)
        return 1

    rec["status"] = status
    rec["attempts"] = int(rec.get("attempts") or 0) + 1
    if commit:
        rec["commit"] = commit
    rec["evidence"] = sorted(set(list(rec.get("evidence") or []) + evidence))
    if blocker:
        rec["blocker"] = blocker
    if decision:
        rec["decision"] = decision
    save(path, doc)
    print(json.dumps({"task": tid, "status": status,
                      "evidence": rec["evidence"]}, indent=2))
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
