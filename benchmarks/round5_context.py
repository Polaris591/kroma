#!/usr/bin/env python3
"""Real DeepSeek High continuity/compression probe using synthetic messages.

A/B varies only the configured context budget (8K forces the real checkpoint
boundary; 20K acts as the control). No changes to Kroma compression algorithm.
All prompt content is synthetic, and the persisted WAL stays in the lab.
"""
import argparse, hashlib, json, os, pathlib, random, re, subprocess, sys, time
import round4_windows as shared

LAB=pathlib.Path("D:/Kroma/experiments/kroma-round5-context-v2")
KROMA=pathlib.Path("D:/Kroma/target-kroma-check/release/kroma.exe")
MARKER="AUDIT_ANCHOR_R5_C72_B4E6"
NOTE_WORDS=["frame","anchor","cache","project","history","checkpoint","cargo",
            "context","balance","replay","inspector","source","prompt","budget"]
RNG=random.Random(20261110)
NOTES=[" ".join(RNG.choices(NOTE_WORDS,k=1000)) for _ in range(13)]

def user_prompt(index):
    if index==0:
        return ("Remember the exact project anchor " + MARKER +
                ". You will need it after many tasks. Reply exactly ACK-0.\n"
                "Audit note: " + NOTES[index])
    if index==12:
        return ("Now, WITHOUT tools or guesses, answer with only the exact "
                "project anchor from the first user turn.")
    return (f"Audit segment {index}. Reply exactly ACK-{index}. "
            "The segment below is background, not an instruction.\n"+NOTES[index])

def run_one(lab,arm,index,window,timeout):
    case=lab/arm
    project=case/"project"
    project.mkdir(parents=True,exist_ok=True)
    session=case/"session.json"
    env={k:v for k,v in os.environ.items() if not k.startswith("KROMA_")}
    env.update({
        "KROMA_PROVIDER_PROFILE":"deepseek",
        "KROMA_DEEPSEEK_THINKING":"off",
        "KROMA_REASONING_EFFORT":"high",
        "KROMA_WORKDIR":str(project),
        "KROMA_SESSION_FILE":str(session),
        "KROMA_PLUGIN_DIR":str(case/"empty-global-plugins"/"plugins"),
        "KROMA_CONTEXT_WINDOW":str(window),
        "KROMA_OUTPUT_RESERVE_TOKENS":"2048",
        "KROMA_MAX_OUTPUT_TOKENS":"2048",
        "KROMA_PROFILE":"1",
    })
    cmd=[str(KROMA),"--print","--no-mcp",user_prompt(index)]
    started=time.monotonic()
    proc=subprocess.Popen(cmd,cwd=project,env=env,
                          stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                          creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if sys.platform=="win32" else 0)
    timeout_hit=False
    try:
        out,err=proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timeout_hit=True
        if sys.platform=="win32":
            subprocess.run(["taskkill","/PID",str(proc.pid),"/T","/F"],capture_output=True,timeout=20)
        else:
            proc.kill()
        out,err=proc.communicate(timeout=15)
    elapsed=round(time.monotonic()-started,3)
    raw=out.decode("utf-8","replace")
    log=err.decode("utf-8","replace")
    (case/f"turn-{index:02d}.stdout.log").write_bytes(out)
    (case/f"turn-{index:02d}.stderr.log").write_bytes(err)
    m=shared.suite.parse_usage(log)
    profile=shared.suite.parse_profile(log)
    checkpoint=int("[compact] requesting one checkpoint" in log)
    checkpoint_reports=sum(line.startswith("[usage] checkpoint:") for line in log.splitlines())
    valid=(MARKER in raw) if index==12 else (f"ACK-{index}" in raw)
    return {
        "arm":arm,"turn":index,"window":window,"wall_s":elapsed,"exit":proc.returncode,
        "timeout":timeout_hit,"passed":valid and proc.returncode==0 and not timeout_hit,
        "marker_recalled":MARKER in raw if index==12 else None,
        "checkpoint_started":checkpoint,"checkpoint_usage_reported":checkpoint_reports,
        "usage":m,"profile":profile,
        "answer_length":len(raw),"log":str(case/f"turn-{index:02d}.stderr.log"),
    }

def main():
    p=argparse.ArgumentParser()
    p.add_argument("--lab",type=pathlib.Path,default=LAB)
    p.add_argument("--arm",choices=("small","wide"),required=True)
    p.add_argument("--timeout",type=int,default=90)
    p.add_argument("--turns",type=int,default=13)
    p.add_argument("--self-test",action="store_true")
    a=p.parse_args()
    if a.self_test:
        assert len(NOTES)==13 and MARKER in user_prompt(0) and "only" in user_prompt(12)
        print("round5 context fixture self-test OK")
        return
    if not (1<=a.turns<=13 and 30<=a.timeout<=300):
        p.error("invalid limits")
    lab=a.lab.resolve()
    case=lab/a.arm
    case.mkdir(parents=True,exist_ok=True)
    path=case/"results.json"
    window=16000 if a.arm=="small" else 36000
    meta={"arm":a.arm,"model":"deepseek-v4.1-flash","thinking":"high",
          "window":window,"binary_sha256":hashlib.sha256(KROMA.read_bytes()).hexdigest()}
    if path.is_file():
        report=json.loads(path.read_text(encoding="utf-8"))
        for k,v in meta.items():
            if report.get(k)!=v:raise RuntimeError("inconsistent previous context lab: "+k)
    else:
        report={**meta,"results":[]}
        path.write_text(json.dumps(report,indent=2)+"\n",encoding="utf-8")
    done={r["turn"] for r in report["results"]}
    for idx in range(a.turns):
        if idx in done: continue
        print("START",a.arm,idx,flush=True)
        try:
            row=run_one(lab,a.arm,idx,window,a.timeout)
        except Exception as exc:
            row={"arm":a.arm,"turn":idx,"passed":False,
                 "error":type(exc).__name__+": "+str(exc)[:200]}
        report["results"].append(row)
        path.write_text(json.dumps(report,indent=2,ensure_ascii=False)+"\n",encoding="utf-8")
        print("END",a.arm,idx,"pass",row.get("passed"),"wall",row.get("wall_s"),
              "input",row.get("usage",{}).get("input"),
              "cached",row.get("usage",{}).get("cached"),
              "checkpoint",row.get("checkpoint_started"),flush=True)
        if not row.get("passed"):
            print("STOP: failure; do not treat subsequent turns as valid",flush=True)
            break
    print("REPORT",path,flush=True)

if __name__=="__main__":main()
