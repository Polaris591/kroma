#!/usr/bin/env python3
"""Round17 post-run independent oracle recheck on every original source snapshot."""
from pathlib import Path
import hashlib,json
import round17_itoa_ab as b
out=[]
data=json.loads((b.LAB/"results.json").read_text(encoding="utf-8"))["results"]
for row in data:
    trial=b.LAB/str(row["repeat"])/row["arm"]
    project=trial/"project"
    env=b.env_for(project,trial,row["arm"])
    frozen=hashlib.sha256(b.src.PUBLIC.encode()).hexdigest()
    valid,detail=b.oracle(project,env,frozen,True)
    record={
        "repeat":row["repeat"],"arm":row["arm"],
        "original_verified":row["verified"],"recheck_verified":valid,
        "original_strict":row["strict_pass"],"original_tested":row["tested"],
        "details":detail,
    }
    out.append(record)
    print("RECHECK",row["repeat"],row["arm"],valid,flush=True)
target=b.LAB/"independent_recheck.json"
target.write_text(json.dumps({"schema":1,"results":out},indent=2,ensure_ascii=False)+"\n",encoding="utf-8")
assert len(out)==8 and all(x["original_verified"]==x["recheck_verified"] for x in out)
print("ROUND17_RECHECK_8_OF_8_MATCH",flush=True)
