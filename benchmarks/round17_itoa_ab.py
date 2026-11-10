#!/usr/bin/env python3
"""Round17 A/B: complete, offline genuine Rust itoa #7 historical bug.

Same DeepSeek High model + endpoint, same source, same Rust toolchain.
Only KROMA_EXPERIMENT_PROMPT_ARM=finish differs. Each run isolated; hidden
acceptance injected after agent exits. No changes to production Kroma.
"""
from __future__ import annotations
import argparse,hashlib,json,os,subprocess,sys,time
from pathlib import Path
import round17_itoa_preflight as src
from round4_windows import parse_kroma
from round6_rust_trace_diagnosis import summarize_kroma
from round13_fresh_portfolio import agent_tested
from round16_rust_trace_audit import parse as parse_wal

ROOT=Path(__file__).resolve().parents[1]
LAB=Path("D:/Kroma/experiments/kroma-round17-itoa-ab-v2")
BINARY=Path("D:/Kroma/target-kroma-check/release/kroma.exe")
ARMS=("baseline","finish")
SOURCE_SHA="b2445b6d0ef9cb5c6caf98dc2dcf31f644f705fd"
PROMPT=(
    "Fix the real historical bug in the itoa Rust crate (#7): the public "
    "itoa::write API currently returns Result<()> instead of the number "
    "of bytes written. Make it return the correct Result<usize> for both "
    "positive and negative signed integers, u64 extremes, and custom "
    "io::Write implementations. Preserve integer formatting and propagate "
    "actual writer errors. Keep the existing public API apart from the "
    "required return type change; adjust the related Integer trait. Do not "
    "edit, delete, or regenerate any existing or new test files. "
    "Run cargo test --offline, and report the checks "
    "you performed. Work within this isolated checkout, do not install "
    "dependencies, commit or read credentials."
)

def fixture(project:Path)->str:
    base=ROOT/"benchmarks/round17_itoa_base"
    for name,path in [("Cargo.toml","Cargo.toml"),("src__lib.rs","src/lib.rs"),("tests__test.rs","tests/test.rs")]:
        out=project/path
        out.parent.mkdir(parents=True,exist_ok=True)
        out.write_bytes((base/name).read_bytes())
    (project/"tests/public.rs").write_text(src.PUBLIC,encoding="utf-8",newline="\n")
    return hashlib.sha256(src.PUBLIC.encode()).hexdigest()

def env_for(project:Path,trial:Path,arm:str)->dict:
    env={k:v for k,v in os.environ.items() if not k.startswith("KROMA_")}
    env.update({
        "KROMA_PROVIDER_PROFILE":"deepseek",
        "KROMA_DEEPSEEK_THINKING":"off",
        "KROMA_REASONING_EFFORT":"high",
        "KROMA_PROFILE":"1",
        "KROMA_PLUGIN_DIR":str(trial/"no-plugins"/"plugins"),
        "KROMA_WORKDIR":str(project),
        "KROMA_SESSION_FILE":str(trial/"session.json"),
        "CARGO_TARGET_DIR":str(trial/"cargo-target"),
        "CARGO_INCREMENTAL":"0",
    })
    env["KROMA_EXPERIMENT_PROMPT_ARM"]="finish" if arm=="finish" else "baseline"
    return env

def oracle(project:Path,env:dict,hash_:str,hidden:bool):
    pub=project/"tests/public.rs"
    if not pub.exists() or hashlib.sha256(pub.read_text(encoding="utf-8").encode()).hexdigest()!=hash_:
        return False,"public tests modified"
    if hidden:
        (project/"tests/holdout.rs").write_text(src.HIDDEN,encoding="utf-8",newline="\n")
    run=subprocess.run(["cargo","test","--offline","--quiet"],cwd=project,env=env,
        capture_output=True,text=True,encoding="utf-8",errors="replace",timeout=90)
    return run.returncode==0,("test suite passed" if run.returncode==0 else (run.stdout+run.stderr)[-950:])

def edits(wal:Path):
    if not wal.is_file():return {"first_project_edit":None,"project_edits":[],"scratch_calls":0}
    actions,_=parse_wal(wal)
    nums=[]
    scratch=0
    for call in actions:
        name=call["name"]
        command=call["command"].lower().replace("\\","/")
        if name=="edit_file" and ("/src/lib.rs" in command or command.endswith("src/lib.rs")):
            nums.append(call["n"])
        elif name=="shell":
            if "src/lib.rs" in command and any(x in command for x in [" > src/lib.rs","sed -i","python3 -","apply_patch"]):
                nums.append(call["n"])
            if any(x in command for x in ["/tmp/","scratch","cat >","_tmp_"]):scratch+=1
    return {"first_project_edit":min(nums,default=None),"project_edits":nums,"scratch_calls":scratch}

def run_one(rep:int,arm:str,cap:int):
    trial=LAB/str(rep)/arm
    project=trial/"project"
    project.mkdir(parents=True,exist_ok=False)
    frozen=fixture(project)
    env=env_for(project,trial,arm)
    public_ok,detail=oracle(project,env,frozen,False)
    if not public_ok: raise RuntimeError("public prewarm failed: "+detail[-400:])
    start=time.monotonic()
    proc=subprocess.Popen([str(BINARY),"--print","--no-mcp",PROMPT],
        cwd=project,env=env,stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,stderr=subprocess.PIPE,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=="nt" else 0)
    timeout=False
    try:out,err=proc.communicate(timeout=cap)
    except subprocess.TimeoutExpired:
        timeout=True
        if os.name=="nt":
            subprocess.run(["taskkill","/PID",str(proc.pid),"/T","/F"],
                           capture_output=True,timeout=20)
        else:proc.kill()
        out,err=proc.communicate(timeout=15)
    elapsed=round(time.monotonic()-start,3)
    (trial/"stdout.log").write_bytes(out)
    (trial/"stderr.log").write_bytes(err)
    metrics,_,tools,profile=parse_kroma(out.decode(errors="replace"),err.decode(errors="replace"),trial/"session.json")
    valid,note=oracle(project,env,frozen,True)
    tested=agent_tested(trial/"session.json.wal")
    completed=proc.returncode==0 and not timeout
    return {
        "repeat":rep,"arm":arm,"wall_s":elapsed,"timeout":timeout,
        "finished":completed,"verified":valid,"tested":tested,
        "strict_pass":bool(completed and valid and tested),
        "error":note if not valid else None,
        "metrics":metrics,"profile":profile,"tools":len(tools),
        "edits":edits(trial/"session.json.wal"),
        "logs":str(trial),
    }

def main():
    p=argparse.ArgumentParser()
    p.add_argument("--repeat-start",type=int,default=1)
    p.add_argument("--repeats",type=int,default=2)
    p.add_argument("--max-runs",type=int,default=2)
    p.add_argument("--cap",type=int,default=130)
    p.add_argument("--self-test",action="store_true")
    a=p.parse_args()
    if a.self_test:
        assert SOURCE_SHA and "Result<usize>" in PROMPT
        assert len(src.PUBLIC)>150 and len(src.HIDDEN)>250
        print("Round17 valid",flush=True);return
    if not (60<=a.cap<=260 and 1<=a.repeats<=4 and 1<=a.max_runs<=8):
        p.error("invalid parameters")
    LAB.mkdir(parents=True,exist_ok=True)
    target=LAB/"results.json"
    meta={
        "schema":1,"model":"deepseek-v4.1-flash","thinking":"high",
        "source_sha":SOURCE_SHA,
        "prompt_sha":hashlib.sha256(PROMPT.encode()).hexdigest(),
        "binary_sha":hashlib.sha256(BINARY.read_bytes()).hexdigest(),
        "arms":list(ARMS)
    }
    data=json.loads(target.read_text(encoding="utf-8")) if target.exists() else {**meta,"results":[]}
    for k,v in meta.items():
        if data.get(k)!=v:raise RuntimeError("metadata drift "+k)
    done={(r["repeat"],r["arm"]) for r in data["results"]}
    num=0
    for rep in range(a.repeat_start,a.repeat_start+a.repeats):
        order=ARMS if rep%2 else ARMS[::-1]
        for arm in order:
            if (rep,arm) in done or num>=a.max_runs:continue
            print("START",rep,arm,flush=True)
            try:row=run_one(rep,arm,a.cap)
            except Exception as e:
                row={"repeat":rep,"arm":arm,"strict_pass":False,
                     "runner_error":type(e).__name__+": "+str(e)[:170]}
            data["results"].append(row)
            target.write_text(json.dumps(data,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
            num+=1
            print("END",rep,arm,"pass",row.get("strict_pass"),"verified",row.get("verified"),
                  "tested",row.get("tested"),"wall",row.get("wall_s"),
                  "tools",row.get("tools"),"first_src_edit",(row.get("edits") or {}).get("first_project_edit"),
                  "error",ascii(row.get("error") or row.get("runner_error")),flush=True)
    print("REPORT",target,len(data["results"]),flush=True)

if __name__=="__main__":main()
