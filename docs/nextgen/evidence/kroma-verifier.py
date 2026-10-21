#!/usr/bin/env python3
"""Independent held-out verifier for EXP-00 (KR-F01/F02/F03).

Reads the arm result JSON on stdin and prints a verdict line. It never sees the agent's reasoning
and it is not part of the workspace, so the agent cannot edit its own judge.
"""
import json, sys

def main():
    try:
        doc = json.load(sys.stdin)
    except Exception:
        print("verdict=unscorable reason=unparsable-result")
        return 2
    resolved = doc.get("resolved")
    task = doc.get("task_id", "?")
    if resolved is True:
        print(f"verdict=pass task={task}")
        return 0
    print(f"verdict=fail task={task}")
    return 1

if __name__ == "__main__":
    sys.exit(main())
