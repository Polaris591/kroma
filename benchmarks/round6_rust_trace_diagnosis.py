#!/usr/bin/env python3
"""Inspect earlier synthetic Rust WAL harness traces without altering fixtures.

Compare model/tool wall time, exploration before edit, targeted vs broad tests,
and repeated inspections. Reads only previously generated lab files.
"""
from __future__ import annotations
import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import re
from statistics import median

ROOT=Path("D:/Kroma/experiments/kroma-round5-complex-session-fixed")
OLD=Path("D:/Kroma/experiments/kroma-round5-complex-current")

def category(tool,cmd):
    low=cmd.lower()
    if tool in ("edit","write","edit_file","apply_patch"):
        return "edit"
    if tool in ("read","grep","search","history_search","history_read"):
        return "inspect"
    if any(t in low for t in ("cargo test","cargo check","cargo build","rustc ")):
        return "test"
    if any(t in low for t in ("grep ","rg ","sed ","head ","tail ","ls ","find ","cat ","wc ")):
        return "inspect"
    if any(t in low for t in ("python -", "git apply"," git diff","git diff","echo "," > ","printf ")):
        return "potential_edit"
    return "other"

def summarize_kroma(path):
    if not path.exists():return None
    lines=path.read_text(encoding="utf-8",errors="replace").splitlines()
    phases=Counter()
    commands=[]
    name_by_number={}
    last_name=None
    for line in lines:
        m=re.search(r"^\[profile\] model_done request=(\d+) elapsed_ms=([\d.]+)",line)
        if m: phases["model_ms"]+=round(float(m.group(2))); phases["model_replies"]+=1
        m=re.search(r"^\[profile\] tool_start number=(\d+) after_request=(\d+) tool=(\S+)",line)
        if m:
            last_name=int(m.group(1))
            name_by_number[last_name]=m.group(3)
            commands.append({"number":last_name,"tool":m.group(3),"command":""})
        m=re.search(r"^\[shell\] (.*)",line)
        if m and last_name is not None and commands:
            commands[-1]["command"]=m.group(1)
        m=re.search(r"^\[profile\] tool_done number=(\d+) elapsed_ms=([\d.]+)",line)
        if m:
            phases["tool_ms"]+=round(float(m.group(2)))
            phases["tools_completed"]+=1
            for command in reversed(commands):
                if command["number"]==int(m.group(1)):
                    command["ms"]=round(float(m.group(2)))
                    break
    return normalize(phases,commands)

def summarize_pi(path):
    if not path.exists():return None
    phases=Counter()
    commands=[]
    in_flight={}
    text=path.read_text(encoding="utf-8",errors="replace")
    for line in text.splitlines():
        try:ev=json.loads(line)
        except ValueError:continue
        ty=ev.get("type")
        if ty=="message_end" and (ev.get("message") or {}).get("role")=="assistant":
            phases["model_replies"]+=1
        if ty=="tool_execution_start":
            args=ev.get("args") or {}
            if not isinstance(args,dict):args={}
            cmd=str(args.get("command") or args.get("path") or "")
            tool=ev.get("toolName") or "unknown"
            row={"number":len(commands)+1,"tool":tool,"command":cmd}
            commands.append(row)
            in_flight[ev.get("toolCallId")]=row
        if ty=="tool_execution_end":
            row=in_flight.get(ev.get("toolCallId"))
            if row:
                ms=ev.get("durationMs")
                if isinstance(ms,(int,float)):
                    row["ms"]=round(ms)
                    phases["tool_ms"]+=round(ms)
                phases["tools_completed"]+=1
    return normalize(phases,commands)

def normalize(phases,commands):
    grouped=Counter()
    examples={}
    first_edit=len(commands)+1
    first_test=len(commands)+1
    repeated=Counter()
    detailed=[]
    for row in commands:
        cat=category(row["tool"],row["command"])
        grouped[cat]+=1
        grouped[cat+"_ms"]+=row.get("ms",0)
        if cat=="edit" and first_edit>len(commands):
            first_edit=row["number"]
        if cat=="test" and first_test>len(commands):
            first_test=row["number"]
        # Sanitize user paths; same command after eliminating run-specific project prefix
        cmd=re.sub(r"D:/Kroma/experiments/[^ ]+/rust_wal_regression/\d+/(pi|kroma)/project",
                   "<project>",row["command"],flags=re.I).strip().lower()
        if cat=="inspect":
            repeated[cmd]+=1
        if cat=="test":
            examples[cmd[:140]]=examples.get(cmd[:140],0)+1
        detailed.append((row["number"],cat,row.get("ms"),row["command"][:190]))
    prefix=[x for x in commands if x["number"]<first_edit]
    return {
        "model_replies":phases["model_replies"],"tool_calls":len(commands),
        "reported_tool_ms":phases["tool_ms"],"model_ms":phases["model_ms"],
        "inspections":grouped["inspect"],"inspection_ms":grouped["inspect_ms"],
        "tests":grouped["test"],"test_ms":grouped["test_ms"],
        "edits":grouped["edit"],"first_edit_number":first_edit if first_edit<=len(commands) else None,
        "inspections_before_first_edit":sum(category(x["tool"],x["command"])=="inspect" for x in prefix),
        "tests_before_first_edit":sum(category(x["tool"],x["command"])=="test" for x in prefix),
        "duplicates_exact_inspections":sum(n-1 for n in repeated.values() if n>1),
        "tests_list":list(examples.items()),"commands":detailed,
    }

def main():
    p=argparse.ArgumentParser()
    p.add_argument("--details",action="store_true")
    a=p.parse_args()
    for i in (1,2,3):
        for arm in ("kroma","pi"):
            base=ROOT/"rust_wal_regression"/str(i)/arm
            if i==1 and arm=="pi":
                base=OLD/"rust_wal_regression"/"1"/"pi"
            if arm=="kroma":
                row=summarize_kroma(base/"stderr.log")
            else:
                row=summarize_pi(base/"stdout.log")
            if row is None:continue
            commands=row.pop("commands")
            print(json.dumps({"repeat":i,"harness":arm,**row},ensure_ascii=False))
            if a.details:
                for n,cat,ms,cmd in commands:
                    print(f"  {n:02d} {cat:14s} {ms if ms is not None else '-':>6} {cmd[:130]}")
if __name__=="__main__":main()
