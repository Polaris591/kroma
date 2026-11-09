#!/usr/bin/env python3
"""DeepSeek High paired edit_file vs benchmark-only unified diff apply_patch.
The only changed runtime capability is the editing tool; no product defaults change.
"""
import argparse, collections, hashlib, json, os, pathlib, random, subprocess, sys, time
import agent_task_suite as suite
import round4_windows as shared

LAB = pathlib.Path("D:/Kroma/experiments/kroma-round5-edit-patch")
BINARY = pathlib.Path("D:/Kroma/target-kroma-check/release/examples/tool_ablation.exe")
SEED = 20261110
ARMS = ("native_edit", "native_patch")
GUIDANCE = (
    "\nUse the dedicated editing tool available in this task (edit_file or "
    "apply_patch) for all file modifications. Shell is for inspecting and "
    "testing only, not modifying files. Keep tests unchanged and do not commit."
)

def trial(lab, task, rep, arm, timeout):
    outdir = lab / task["id"] / str(rep) / arm
    root = outdir / "project"
    root.mkdir(parents=True, exist_ok=False)
    suite.create_fixture(root, task)
    session = outdir / "session.json"
    env = {k:v for k,v in os.environ.items() if not k.startswith("KROMA_")}
    env.update(KROMA_PROVIDER_PROFILE="deepseek", KROMA_DEEPSEEK_THINKING="off",
               KROMA_REASONING_EFFORT="high", KROMA_PROFILE="1",
               KROMA_PLUGIN_DIR=str(outdir / "disabled-global-plugins" / "plugins"),
               KROMA_WORKDIR=str(root), KROMA_SESSION_FILE=str(session))
    if task["id"] == "rust_multifile":
        env["CARGO_TARGET_DIR"] = str(lab / "rust-target" / arm)
    command = [str(BINARY), arm, task["prompt"] + GUIDANCE]
    start = time.monotonic()
    proc = subprocess.Popen(command, env=env, cwd=root, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if sys.platform=="win32" else 0)
    timed_out = False
    try:
        out, err = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        if sys.platform == "win32":
            subprocess.run(["taskkill","/PID",str(proc.pid),"/T","/F"], capture_output=True,timeout=20)
        else:
            proc.kill()
        out, err = proc.communicate(timeout=15)
    elapsed = round(time.monotonic() - start, 3)
    (outdir/"stdout.log").write_bytes(out)
    (outdir/"stderr.log").write_bytes(err)
    answer = out.decode("utf-8","replace")
    metrics, _, tools, profile = shared.parse_kroma(answer,err.decode("utf-8","replace"),session)
    try:
        if task["id"] == "rust_multifile":
            (root/"tests"/"hidden_round4.rs").write_text(shared.HIDDEN_RUST_TEST,encoding="utf-8")
            p=subprocess.run(["cargo","test","--locked","--offline","--quiet"],cwd=root,
                             env=env,capture_output=True,text=True,timeout=90)
            verified=p.returncode==0
        else:
            verified=bool(suite.verify(root,task["id"],answer))
    except Exception:
        verified=False
    return dict(task=task["id"],repeat=rep,arm=arm,
                passed=verified and proc.returncode==0 and not timed_out,
                verified=verified,wall_s=elapsed,exit=proc.returncode,
                timeout=timed_out,metrics=metrics,profile=profile,
                tools=len(tools),edit_tool_calls=sum(t["tool"] in ("edit_file","apply_patch") for t in tools),
                tool_types=dict(collections.Counter(t["tool"] for t in tools)),
                shell_modifications=sum(t["tool"]=="shell" and t["operation"]=="edit" for t in tools),
                logs=str(outdir))

def main():
    p=argparse.ArgumentParser()
    p.add_argument("--lab",type=pathlib.Path,default=LAB)
    p.add_argument("--tasks",default="fix_arithmetic,implement_records,cross_file_fix,rust_multifile")
    p.add_argument("--repeats",type=int,default=3)
    p.add_argument("--repeat-start",type=int,default=1)
    p.add_argument("--timeout",type=int,default=130)
    p.add_argument("--max-runs",type=int,default=8)
    p.add_argument("--self-test",action="store_true")
    a=p.parse_args()
    if a.self_test:
        assert all(t in shared.TASKS for t in ("fix_arithmetic","rust_multifile"))
        print("round5 edit/patch self-test OK")
        return
    tasks=[t.strip() for t in a.tasks.split(",") if t.strip()]
    if not tasks or any(t not in shared.TASKS for t in tasks):
        p.error("unknown task")
    if not (1<=a.repeats<=8 and 30<=a.timeout<=300 and 1<=a.max_runs<=48):
        p.error("invalid limits")
    if not BINARY.is_file():
        raise RuntimeError("build release example tool_ablation first")
    lab=a.lab.resolve()
    lab.mkdir(parents=True,exist_ok=True)
    path=lab/"results.json"
    metadata=dict(seed=SEED,arms=list(ARMS),model="deepseek-v4.1-flash",thinking="high",
                  endpoint="https://cobblemon.asia/v1",
                  executable_sha256=hashlib.sha256(BINARY.read_bytes()).hexdigest())
    if path.exists():
        report=json.loads(path.read_text(encoding="utf-8"))
        for k,v in metadata.items():
            if report.get(k)!=v: raise RuntimeError("incompatible existing report: "+k)
    else:
        report={**metadata,"results":[]}
        path.write_text(json.dumps(report,indent=2)+"\n",encoding="utf-8")
    completed={(r["task"],r["repeat"],r["arm"]) for r in report["results"]}
    rng=random.Random(SEED)
    pairs=[(t,rep) for t in tasks for rep in range(a.repeat_start,a.repeat_start+a.repeats)]
    rng.shuffle(pairs)
    count=0
    for t,rep in pairs:
        order=list(ARMS)
        rng.shuffle(order)
        for arm in order:
            if (t,rep,arm) in completed: continue
            if count>=a.max_runs: break
            print("START",t,rep,arm,flush=True)
            try:
                row=trial(lab,shared.TASKS[t],rep,arm,a.timeout)
            except Exception as e:
                row=dict(task=t,repeat=rep,arm=arm,passed=False,error=type(e).__name__+": "+str(e)[:180])
            report["results"].append(row)
            path.write_text(json.dumps(report,indent=2,ensure_ascii=False)+"\n",encoding="utf-8")
            completed.add((t,rep,arm));count+=1
            m=row.get("metrics") or {}
            print("END",t,rep,arm,"pass",row.get("passed"),"sec",row.get("wall_s"),
                  "req",m.get("requests"),"input",m.get("input"),
                  "output",m.get("output"),"edits",row.get("edit_tool_calls"),flush=True)
        if count>=a.max_runs: break
    print("REPORT",path,"rows",len(report["results"]),flush=True)

if __name__=="__main__":
    main()
