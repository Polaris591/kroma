#!/usr/bin/env python3
"""Paired opt-in plugin overhead run using a sandbox of installed plugin files.

The original user plugin files, enabled state, settings and memory databases are
never modified. Each run copies the installed plugin source into an isolated
data root and enables only the four known plugin IDs, using manifest defaults.
No non-synthetic input files and no user settings are copied.
"""
from __future__ import annotations
import argparse, collections, hashlib, json, os, pathlib, random, shutil, subprocess, sys, time
import agent_task_suite as suite
import round4_windows as shared

BIN = pathlib.Path("D:/Kroma/target-kroma-check/release/kroma.exe")
LAB = pathlib.Path("D:/Kroma/experiments/kroma-round5-plugins")
INSTALLED = pathlib.Path.home()/".kroma"/"plugins"
PLUGINS = ("goal","project-memory","subagents","web")
SEED = 20261110

def one(lab, task, rep, arm, timeout):
    root=lab/task["id"]/str(rep)/arm
    workdir=root/"project"
    workdir.mkdir(parents=True,exist_ok=False)
    suite.create_fixture(workdir,task)
    plugin_root=root/"plugin-sandbox"
    plugin_dir=plugin_root/"plugins"
    plugin_dir.mkdir(parents=True,exist_ok=True)
    if arm=="enabled":
        for name in PLUGINS:
            source=INSTALLED/name
            if not source.is_dir():
                raise RuntimeError(f"installed plugin missing: {name}")
            shutil.copytree(source,plugin_dir/name)
        (plugin_root/"plugins.toml").write_text(
            "enabled = [\"goal\", \"project-memory\", \"subagents\", \"web\"]\n",encoding="utf-8")
    env={k:v for k,v in os.environ.items() if not k.startswith("KROMA_")}
    env.update({
        "KROMA_PROVIDER_PROFILE":"deepseek",
        "KROMA_DEEPSEEK_THINKING":"off",
        "KROMA_REASONING_EFFORT":"high",
        "KROMA_WORKDIR":str(workdir),
        "KROMA_PLUGIN_DIR":str(plugin_dir),
        "KROMA_SESSION_FILE":str(root/"session.json"),
        "KROMA_PROFILE":"1",
    })
    command=[str(BIN),"--print","--no-mcp",task["prompt"]]
    start=time.monotonic()
    proc=subprocess.Popen(command,cwd=workdir,env=env,stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,stderr=subprocess.PIPE,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if sys.platform=="win32" else 0)
    timed_out=False
    try:
        out,err=proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out=True
        if sys.platform=="win32":
            subprocess.run(["taskkill","/PID",str(proc.pid),"/T","/F"],capture_output=True,timeout=20)
        else:proc.kill()
        out,err=proc.communicate(timeout=15)
    wall=round(time.monotonic()-start,3)
    (root/"stdout.log").write_bytes(out)
    (root/"stderr.log").write_bytes(err)
    answer=out.decode("utf-8","replace")
    log=err.decode("utf-8","replace")
    metrics,_,tools,profile=shared.parse_kroma(answer,log,root/"session.json")
    try:verified=bool(suite.verify(workdir,task["id"],answer))
    except Exception:verified=False
    return dict(task=task["id"],repeat=rep,arm=arm,passed=verified and proc.returncode==0 and not timed_out,
                verified=verified,wall_s=wall,timeout=timed_out,exit=proc.returncode,
                metrics=metrics,profile=profile,tools=len(tools),
                tool_types=dict(collections.Counter(x["tool"] for x in tools)),
                plugin_warnings=sum("[plugin]" in line and ("warning" in line or "failed" in line)
                    for line in log.splitlines()),
                log=str(root/"stderr.log"))

def main():
    p=argparse.ArgumentParser()
    p.add_argument("--lab",type=pathlib.Path,default=LAB)
    p.add_argument("--tasks",default="exact_lookup,implement_records")
    p.add_argument("--repeats",type=int,default=2)
    p.add_argument("--repeat-start",type=int,default=1)
    p.add_argument("--max-runs",type=int,default=4)
    p.add_argument("--timeout",type=int,default=130)
    a=p.parse_args()
    if not (1<=a.repeats<=5 and 1<=a.max_runs<=25 and 30<=a.timeout<=300):
        p.error("invalid limits")
    ts=[x.strip() for x in a.tasks.split(",")]
    if not ts or any(x not in shared.TASKS for x in ts):
        p.error("unknown fixture")
    a.lab.mkdir(parents=True,exist_ok=True)
    report_path=a.lab/"results.json"
    metadata=dict(model="deepseek-v4.1-flash",thinking="high",
                  plugin_names=list(PLUGINS),seed=SEED,
                  kroma_sha256=hashlib.sha256(BIN.read_bytes()).hexdigest())
    if report_path.exists():
        report=json.loads(report_path.read_text(encoding="utf-8"))
        for key,val in metadata.items():
            if report.get(key)!=val:raise RuntimeError("existing lab mismatch: "+key)
    else:
        report={**metadata,"results":[]}
        report_path.write_text(json.dumps(report,indent=2)+"\n",encoding="utf-8")
    completed={(v["task"],v["repeat"],v["arm"]) for v in report["results"]}
    rng=random.Random(SEED)
    pairs=[(task,rep) for task in ts for rep in range(a.repeat_start,a.repeat_start+a.repeats)]
    rng.shuffle(pairs)
    count=0
    for task,rep in pairs:
        arms=["enabled","disabled"]
        rng.shuffle(arms)
        for arm in arms:
            if (task,rep,arm) in completed:continue
            if count>=a.max_runs:break
            print("START",task,rep,arm,flush=True)
            try: row=one(a.lab,shared.TASKS[task],rep,arm,a.timeout)
            except Exception as e:
                row=dict(task=task,repeat=rep,arm=arm,passed=False,
                         error=type(e).__name__+": "+str(e)[:180])
            report["results"].append(row)
            report_path.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
            count+=1
            m=row.get("metrics") or {}
            print("END",task,rep,arm,"pass",row.get("passed"),"sec",row.get("wall_s"),
                  "requests",m.get("requests"),"input",m.get("input"),
                  "cached",m.get("cached"),"output",m.get("output"),
                  "tools",row.get("tools"),flush=True)
        if count>=a.max_runs:break
    print("REPORT",report_path)
if __name__=="__main__": main()
