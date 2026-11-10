#!/usr/bin/env python3
"""Recheck frozen public tests and hidden behavioral oracles on all VALID Round14 runs.

The first p-limit-109 run (v1) is invalid because its oracle had an
unhandled rejection timing mistake; only v2 is eligible.
"""
from __future__ import annotations
from pathlib import Path
import json
import round14_real_pr_cases as map_bench
MAP_TASKS=dict(map_bench.TASKS)
import round14_real_plimit_cases as limit_bench
LIMIT_TASKS=dict(limit_bench.runner.TASKS)

LABS=[
    (Path("D:/Kroma/experiments/kroma-round14-real-prs"),map_bench,MAP_TASKS),
    (Path("D:/Kroma/experiments/kroma-round14-real-plimit-v2"),limit_bench.runner,LIMIT_TASKS),
]

def main():
    checked=[]
    for lab,mod,task_map in LABS:
        mod.TASKS.clear()
        mod.TASKS.update(task_map)
        data=json.loads((lab/"results.json").read_text(encoding="utf-8"))
        for row in data["results"]:
            task=row["task"]
            project=lab/task/str(row["repeat"])/row["arm"]/"project"
            original=mod.TASKS[task]["public"]
            source=mod.TASKS[task]["source"]
            provenance=json.loads(source.read_text(encoding="utf-8"))
            expected=provenance["files"]["index.js"]["source"]
            is_historical=bool(expected and provenance["base_sha"])
            unchanged=(project/"tests/public.test.mjs").read_text(encoding="utf-8")==original
            import hashlib
            test_sha=hashlib.sha256(original.encode("utf-8")).hexdigest()
            verified,detail=mod.oracle(task,project,test_sha,True)
            result={
                "task":task,"repeat":row["repeat"],"arm":row["arm"],
                "historical_source_confirmed":is_historical,
                "public_test_unchanged":unchanged,
                "first_verified":row["verified"],
                "recheck_verified":verified,
                "strict_pass":row["strict_pass"],
                "source_baseline_sha":provenance["base_sha"],
                "upstream":provenance["repo"],
                "detail":detail[-360:] if not verified else "independent behavior checks passed",
            }
            checked.append(result)
            print("RECHECK",task,row["repeat"],row["arm"],
                  "initial",row["verified"],"current",verified,
                  "public_untouched",unchanged,flush=True)
    out=Path("D:/Kroma/experiments/kroma-round14-real-prs/combined_recheck.json")
    out.write_text(json.dumps({"schema":1,"checks":checked},
                   ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
    match=sum(x["first_verified"]==x["recheck_verified"] and x["public_test_unchanged"] for x in checked)
    print("RECHECK_TOTAL",len(checked),"MATCH",match,flush=True)
    if match!=len(checked):raise RuntimeError("independent recheck mismatch")

if __name__=="__main__":main()
