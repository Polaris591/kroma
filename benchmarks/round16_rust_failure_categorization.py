#!/usr/bin/env python3
"""Round16 forensic breakdown from Round15 WAL, with categorized failure counts.

Only tool calls/results are analyzed. Shell commands that create scratch
scripts are not counted as edits of application source. Separate source edits,
test execution, scratch work and missing-environment errors.
"""
from __future__ import annotations
from collections import Counter
import json
from pathlib import Path
import re
from round16_rust_trace_audit import LAB,TASK,parse

def categorize(call:dict) -> str:
    name=call["name"]
    c=call["command"].replace("\\","/").lower()
    if name=="edit_file":
        if "scripts/unicode.py" in c or "src/gen/tables.rs" in c:
            return "project_edit"
        return "scratch_edit"
    if re.search(r"\bcargo\s+(test|run|build|check)\b",c):
        return "cargo"
    if any(x in c for x in ["> /tmp/",">/tmp/","cat >","mkdir -p /tmp","_tmp_decode.py", "/tmp/decode.py", "examples/probe.rs"]):
        return "scratch"
    if "scripts/unicode.py" in c:
        return "generator_inspection"
    return "other_inspection"

def main():
    report=[]
    for repeat in (1,2):
        for arm in ("baseline","finish"):
            wal=LAB/TASK/str(repeat)/arm/"session.json.wal"
            calls,replies=parse(wal)
            stats=Counter(categorize(c) for c in calls)
            exit_fail=[c for c in calls if isinstance(c["exit"],int) and c["exit"]!=0]
            failures=Counter()
            for c in exit_fail:
                message=(c["stderr"] or "")+" "+(c["out"] or "")
                if "Python was not found" in message or "python: command not found" in message:
                    failures["python_unavailable"]+=1
                elif "re.error" in message or "ValueError" in message or "AttributeError" in message:
                    failures["scratch_decoder_exception"]+=1
                elif "grep" in c["command"]:
                    failures["grep_failed_or_no_match"]+=1
                else:
                    failures["other"]+=1
            project_edits=[c["n"] for c in calls if categorize(c)=="project_edit"]
            row={
                "repeat":repeat,"arm":arm,"replies":replies,
                "tools_total":len(calls),"categories":dict(stats),
                "failures":dict(failures),"failed_tools":len(exit_fail),
                "first_project_edit":next(iter(project_edits),None),
                "project_edit_count":len(project_edits),
                "cargo_command_count":stats["cargo"],
                "scratch_command_count":stats["scratch"]+stats["scratch_edit"],
            }
            report.append(row)
            print(json.dumps(row,ensure_ascii=True),flush=True)
    print("SUMMARY",len(report),"total_tools",sum(x["tools_total"] for x in report),
          "total_failed_tools",sum(x["failed_tools"] for x in report),
          "total_scratch",sum(x["scratch_command_count"] for x in report),flush=True)
if __name__=="__main__":
    main()
