#!/usr/bin/env python3
"""Rust decision-turn ablation: unchanged Kroma vs minimal evidence-first task guidance.

Same Kroma executable, model, High reasoning, endpoints and original task
fixtures. Only a generic final user instruction changes; this is exploratory
and is NOT evidence that changing the production system prompt helps.
Independent verification comes from round4_windows.run_one.
"""
from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import random

import round4_windows as base
from round6_rust_trace_diagnosis import summarize_kroma

BINARY=Path("D:/Kroma/experiments/kroma-release-ce0b386/kroma.exe")
LAB=Path("D:/Kroma/experiments/kroma-round6-rust-guidance")
SEED=20261110
ARMS=("baseline","focused")
GUIDANCE=(
    "\nExecution guidance for this repair (keep correctness and verification): "
    "In the first investigation round, batch the search for the named failure "
    "and the code that implements that behavior; then read bounded relevant "
    "snippets. If the cause is sufficiently supported, make the minimal edit "
    "without exploring unrelated files. If uncertainty remains, inspect the "
    "specific missing evidence, not the entire repository. Do not perform an "
    "expensive Cargo test before the edit unless reproducing first is needed "
    "to disambiguate the cause. After the edit, run the exact requested targeted "
    "test; if it passes and requirements are met, finish. "
    "Never skip required tests, never modify tests, and never guess at changes."
)

def run_one(lab, task_id, rep, arm, timeout):
    task=dict(base.TASKS[task_id])
    if arm=="focused":
        task["prompt"]+=GUIDANCE
    root=lab/arm
    row=base.run_one(root,BINARY,task,rep,"kroma",timeout)
    trace_file=root/task_id/str(rep)/"kroma"/"stderr.log"
    trace=summarize_kroma(trace_file) if trace_file.is_file() else None
    # A task explicitly requiring a targeted Cargo test is not fully
    # compliant merely because the independent post-run oracle passes.
    row["agent_ran_cargo_test"] = bool(trace and trace.get("tests", 0) > 0)
    row["strict_pass"] = bool(row.get("passed") and row["agent_ran_cargo_test"])
    row.update(arm=arm,trace={
        key:trace.get(key) for key in ("model_replies","tool_calls","reported_tool_ms",
            "model_ms","inspections","tests","edits","first_edit_number",
            "inspections_before_first_edit","tests_before_first_edit")
    } if trace else None)
    return row

def main():
    p=argparse.ArgumentParser()
    p.add_argument("--lab",type=Path,default=LAB)
    p.add_argument("--tasks",default="rust_multifile,rust_wal_regression")
    p.add_argument("--repeat-start",type=int,default=1)
    p.add_argument("--repeats",type=int,default=1)
    p.add_argument("--max-runs",type=int,default=4)
    p.add_argument("--timeout",type=int,default=180)
    p.add_argument("--self-test",action="store_true")
    a=p.parse_args()
    if a.self_test:
        assert "Run the" not in GUIDANCE and "Cargo" in GUIDANCE
        assert "rust_wal_regression" in base.TASKS
        print("Rust guidance ablation self-test OK",flush=True)
        return
    tasks=[x.strip() for x in a.tasks.split(",") if x.strip()]
    if any(x not in base.TASKS for x in tasks) or not tasks:
        p.error("unknown task")
    if not 1<=a.repeats<=6 or not 1<=a.max_runs<=48 or not 30<=a.timeout<=300:
        p.error("invalid limits")
    a.lab.mkdir(parents=True,exist_ok=True)
    output=a.lab/"results.json"
    metadata=dict(schema=1,seed=SEED,arms=list(ARMS),
        model="deepseek-v4.1-flash",thinking="high",
        kroma_sha256=hashlib.sha256(BINARY.read_bytes()).hexdigest(),
        focus_guidance_sha256=hashlib.sha256(GUIDANCE.encode()).hexdigest())
    if output.exists():
        report=json.loads(output.read_text(encoding="utf-8"))
        for k,v in metadata.items():
            if report.get(k)!=v: raise RuntimeError(f"lab mismatch {k}")
    else:
        report={**metadata,"results":[]}
        output.write_text(json.dumps(report,indent=2)+"\n",encoding="utf-8")
    done={(v["task"],v["repeat"],v["arm"]) for v in report["results"]}
    rng=random.Random(SEED)
    items=[(task,rep) for task in tasks for rep in range(a.repeat_start,a.repeat_start+a.repeats)]
    rng.shuffle(items)
    count=0
    for task,rep in items:
        # Stable pair order even when runs are resumed one at a time. A prior
        # implementation reset RNG for every invocation, unintentionally
        # running baseline first in every long-task pair and favoring the
        # second arm with potentially warmer Cargo build artifacts.
        order=list(ARMS)
        coin=hashlib.sha256(f"{SEED}:{task}:{rep}".encode()).digest()[0] & 1
        if coin:
            order.reverse()
        for arm in order:
            if (task,rep,arm) in done:continue
            if count>=a.max_runs:break
            print("START",task,rep,arm,flush=True)
            try:
                row=run_one(a.lab,task,rep,arm,a.timeout)
            except Exception as error:
                row=dict(task=task,repeat=rep,arm=arm,passed=False,
                         error=type(error).__name__+": "+str(error)[:180])
            report["results"].append(row)
            output.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
            count+=1
            m=row.get("metrics") or {}
            t=row.get("trace") or {}
            print("END",task,rep,arm,"PASS",row.get("passed"),
                "wall",row.get("wall_s"),"requests",m.get("requests"),
                "input",m.get("input"),"output",m.get("output"),
                "tools",row.get("tools"),"pre_edit_reads",t.get("inspections_before_first_edit"),
                "tests",t.get("tests"),flush=True)
        if count>=a.max_runs:break
    print("REPORT",output,"rows",len(report["results"]),flush=True)
if __name__=="__main__":main()
