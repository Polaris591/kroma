#!/usr/bin/env python3
"""Independent post-run recheck with no shared Cargo target artifacts.

Inspects the 32 existing Round 13 results, reruns every hidden acceptance
with a distinct per-trial Cargo target directory, and records a read-only
recheck report. Does not invoke LLMs or mutate experimental source modules.
"""
from __future__ import annotations
import hashlib
import json
import os
from pathlib import Path

import round13_fresh_portfolio as round13

ROOT=round13.LAB
RESULTS=ROOT/"results.json"
OUT=ROOT/"independent_recheck.json"

def main():
    data=json.loads(RESULTS.read_text(encoding="utf-8"))
    checked=[]
    for row in data["results"]:
        task=row["task"]
        rep=row["repeat"]
        arm=row["arm"]
        trial=ROOT/task/str(rep)/arm
        project=trial/"project"
        test_name="tests/basic.test.mjs" if task.startswith("node_") else "tests/basic.rs"
        original=round13.TASKS[task]["files"][test_name]
        public_file = project / test_name
        # Compare normalized text to the immutable fixture source; Windows
        # creates CRLF on disk and byte-for-byte LF hashing is incorrect.
        text_unchanged = (public_file.read_text(encoding="utf-8") == original)
        checksum=hashlib.sha256(public_file.read_bytes()).hexdigest() if text_unchanged else "INVALID_PUBLIC_TEST"
        env=round13.environment(project,trial,arm)
        if task.startswith("rust_"):
            env["CARGO_TARGET_DIR"] = str(ROOT/"isolated-targets"/task/str(rep)/arm)
        passed,detail=round13.oracle(task,project,env,checksum,hidden=True)
        result={
            "task":task,"repeat":rep,"arm":arm,
            "original_verified":row.get("verified"),
            "isolated_verified":passed,
            "detail":detail,
        }
        checked.append(result)
        OUT.write_text(json.dumps({"schema":1,"checks":checked},ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
        print(task,rep,arm,"previous",row.get("verified"),"isolated",passed,flush=True)
    print("RECHECKED",len(checked),"MATCHES",sum(x["original_verified"]==x["isolated_verified"] for x in checked),flush=True)

if __name__=="__main__":
    main()
