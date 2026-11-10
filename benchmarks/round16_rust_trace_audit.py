#!/usr/bin/env python3
"""Read-only Round15 trace audit: aggregate tool calls and stage transitions.

Use no model-internal reasoning. Analyze only issued tool arguments and actual
tool results for observed behavior, and short assistant-visible statements.
"""
from __future__ import annotations
import argparse
import json
import re
from pathlib import Path
from collections import Counter

LAB=Path("D:/Kroma/experiments/kroma-round15-rust-real-issues-v6")
TASK="unicode-width-89"

def parse(wal:Path):
    rows=[]
    for line in wal.read_text(encoding="utf-8",errors="replace").splitlines():
        obj=json.loads(line.split("\t",1)[0])
        if "message" in obj:
            rows.append(obj["message"])
    pending={}
    actions=[]
    assistant_messages=0
    for msg in rows:
        if msg.get("role") == "assistant":
            assistant_messages+=1
            for call in msg.get("tool_calls") or []:
                f=call.get("function",{})
                try: args=json.loads(f.get("arguments","{}"))
                except json.JSONDecodeError: args={}
                command=args.get("command") or args.get("path") or ""
                action={
                    "n":len(actions)+1,
                    "reply":assistant_messages,
                    "name":f.get("name",""),
                    "command":str(command),
                    "exit":None,"out":"","stderr":"",
                }
                actions.append(action)
                pending[call["id"]]=action
        elif msg.get("role") == "tool":
            action=pending.get(msg.get("tool_call_id"))
            if not action: continue
            try:payload=json.loads(msg.get("content") or "{}")
            except (ValueError,TypeError):payload={}
            action["exit"]=payload.get("exit_code")
            action["out"]=str(payload.get("stdout",""))
            action["stderr"]=str(payload.get("stderr",""))
    return actions,assistant_messages

def kind(c):
    name=c["name"].lower()
    cmd=c["command"].lower()
    if name in ("edit_file","file_edit","write_file") or re.search(r"(^|\s)(python|perl|sed -i) ",cmd) or "apply_patch" in cmd or re.search(r"\bcat\s*>\s*",cmd):
        return "edit"
    if "cargo test" in cmd or "cargo build" in cmd or "cargo check" in cmd or "pytest" in cmd:
        return "test"
    if "scripts/unicode.py" in cmd or "generate" in cmd or "regenerate" in cmd:
        return "generator"
    if name == "shell": return "inspect"
    return "other"

def summarize(label,actions,verbose=True):
    print("CASE",label,"tools",len(actions),"kinds",dict(Counter(kind(c) for c in actions)),
          "firstedit",next((c["n"] for c in actions if kind(c)=="edit"),None),flush=True)
    if not verbose:return
    for c in actions:
        title=c["command"].replace("\r"," ").replace("\n"," ; ")
        title=title[:165]
        code=c["exit"]
        out=c["out"]
        stderr=c["stderr"]
        summary=[]
        if "test result: ok." in out: summary.append("TEST_PASS")
        if "test result: FAILED" in out or "test result: FAILED" in stderr: summary.append("TEST_FAIL")
        if "error:" in stderr.lower() or code not in (None,0):summary.append("ERROR")
        if c["name"]=="shell" and not out and not stderr and code is None:
            summary.append("NO_RESULT")
        error_line=next((s.strip()[:95] for s in stderr.splitlines()
                        if any(k in s.lower() for k in ("error:","failed","not found"))),"")
        if error_line:summary.append(error_line)
        print(f'{c["n"]:02d} R{c["reply"]:02d} {kind(c):9s} {c["name"]:12s} exit={str(code):4} cmd={ascii(title)} info={ascii(";".join(summary))}',flush=True)

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--summary",action="store_true")
    parser.add_argument("--repeat",type=int)
    parser.add_argument("--arm",choices=["baseline","finish"])
    args=parser.parse_args()
    for rep in (1,2):
        if args.repeat and args.repeat!=rep:continue
        for arm in ("baseline","finish"):
            if args.arm and args.arm!=arm:continue
            wal=LAB/TASK/str(rep)/arm/"session.json.wal"
            if not wal.exists(): continue
            actions,replies=parse(wal)
            summarize(f"repeat{rep}/{arm}, replies={replies}",actions,not args.summary)

if __name__=="__main__":main()
