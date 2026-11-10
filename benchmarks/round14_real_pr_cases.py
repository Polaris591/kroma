#!/usr/bin/env python3
"""Round14 REAL historical GitHub PR regression cases (p-map #88, #91).

Sources are byte-for-byte public GitHub blob content from the historical
PR's base commit, pinned by SHA and source Git blob SHA. Each task only adds
a public Node built-in test and hidden independent oracle injected AFTER
agent exit. These are historical real patches, not synthetic injected bugs.
The original upstream npm test uses AVA/XO/TSD; only the targeted Node built-in
functional tests run in this Windows benchmark.
"""
from __future__ import annotations
import argparse, hashlib, json, os, random, re, subprocess, sys, time
from pathlib import Path

from round4_windows import parse_kroma
from round6_rust_trace_diagnosis import summarize_kroma

ROOT = Path(__file__).resolve().parents[1]
LAB = Path("D:/Kroma/experiments/kroma-round14-real-prs")
BINARY = Path("D:/Kroma/target-kroma-check/release/kroma.exe")
ARMS = ("baseline", "finish")
SEED = 20261110

TASKS = {
    "p-map-88": {
        "repo": "sindresorhus/p-map", "pr": 88,
        "source": ROOT / "benchmarks/round14_fixture_p-map-88.json",
        "cap": 120,
        "prompt": (
            "Fix a real p-map library regression in index.js: pMapIterable "
            "passes a mapper index based on promise settlement timing when "
            "the input contains promises resolving out of order. The index "
            "must always mean the source iterable position, as in pMap, "
            "without altering the output order, backpressure or concurrency. "
            "Keep the existing public API, run node --test tests/public.test.mjs "
            "and relevant checks. Do not change tests, install dependencies, "
            "commit, or access credentials."
        ),
        "public": '''import test from "node:test";
import assert from "node:assert/strict";
import pMap, {pMapIterable} from "../index.js";
test("pMap returns results in source order", async () => {
  assert.deepEqual(await pMap([2, 3, 4], x => x * 2, {concurrency: 2}), [4, 6, 8]);
});
test("pMapIterable simple index", async () => {
  const out = [];
  for await (const value of pMapIterable(["a","b","c"], (v, i) => [v,i], {concurrency: 2})) out.push(value);
  assert.deepEqual(out, [["a",0],["b",1],["c",2]]);
});
''',
        "holdout": '''import test from "node:test";
import assert from "node:assert/strict";
import {pMapIterable} from "../index.js";
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
test("indices refer to original promise positions, not resolution order", async () => {
  const input = [
    sleep(65).then(() => "a"),
    sleep(5).then(() => "b"),
    sleep(30).then(() => "c"),
  ];
  const out = [];
  for await (const item of pMapIterable(input, async (v,i) => [v,i], {concurrency: 3})) out.push(item);
  assert.deepEqual(out, [["a",0],["b",1],["c",2]]);
});
test("index remains stable for a delayed first item with backpressure", async () => {
  const input = [
    sleep(45).then(() => 10),
    sleep(3).then(() => 20),
    sleep(10).then(() => 30),
    sleep(1).then(() => 40),
  ];
  const out = [];
  for await (const item of pMapIterable(input, async (v,i) => [v,i], {concurrency: 3, backpressure: 4})) out.push(item);
  assert.deepEqual(out, [[10,0],[20,1],[30,2],[40,3]]);
});
''',
    },
    "p-map-91": {
        "repo": "sindresorhus/p-map", "pr": 91,
        "source": ROOT / "benchmarks/round14_fixture_p-map-91.json",
        "cap": 120,
        "prompt": (
            "Fix a real p-map library regression in index.js: if pMap is "
            "called with a signal that was already aborted, it rejects but "
            "still attaches an abort event listener after cleanup, leaking "
            "the listener on a reusable signal. Reject with the original "
            "reason and leave no listener attached, preserving active-signal "
            "abort behavior. Keep the public API, run "
            "node --test tests/public.test.mjs and relevant checks. "
            "Do not modify tests, install packages, commit, or access credentials."
        ),
        "public": '''import test from "node:test";
import assert from "node:assert/strict";
import pMap from "../index.js";
test("normal mapping still works", async () => {
  assert.deepEqual(await pMap([1,2,3], x => x*2, {concurrency:2}), [2,4,6]);
});
test("already aborted signal rejects", async () => {
  const ctrl = new AbortController();
  ctrl.abort(new Error("already aborted"));
  await assert.rejects(pMap([1], x => x, {signal:ctrl.signal}), /already aborted/);
});
''',
        "holdout": '''import test from "node:test";
import assert from "node:assert/strict";
import {getEventListeners} from "node:events";
import pMap from "../index.js";
test("already aborted signals retain no listener", async () => {
  const ctrl = new AbortController();
  const reason = new Error("prior cancellation");
  ctrl.abort(reason);
  await assert.rejects(pMap([1,2], async x => x, {signal:ctrl.signal}), e => e === reason);
  assert.equal(getEventListeners(ctrl.signal, "abort").length, 0);
});
test("abort while running still cleans listener", async () => {
  const ctrl = new AbortController();
  const pending = pMap([1,2,3], async x => {
    await new Promise(resolve => setTimeout(resolve, 25));
    return x;
  }, {signal:ctrl.signal, concurrency:1});
  ctrl.abort(new Error("cancel now"));
  await assert.rejects(pending, /cancel now/);
  assert.equal(getEventListeners(ctrl.signal, "abort").length, 0);
});
''',
    },
}

def fingerprint(task: dict) -> str:
    return hashlib.sha256((task["prompt"] + task["public"] + task["holdout"]
                          + task["source"].read_text(encoding="utf-8")).encode("utf-8")).hexdigest()

def environment(project: Path, trial: Path, arm: str) -> dict:
    env={k:v for k,v in os.environ.items() if not k.startswith("KROMA_")}
    env.update({
        "KROMA_PROVIDER_PROFILE":"deepseek",
        "KROMA_DEEPSEEK_THINKING":"off",
        "KROMA_REASONING_EFFORT":"high",
        "KROMA_PROFILE":"1",
        "KROMA_PLUGIN_DIR":str(trial/"disable-plugins"/"plugins"),
        "KROMA_WORKDIR":str(project),
        "KROMA_SESSION_FILE":str(trial/"session.json"),
    })
    env["KROMA_EXPERIMENT_PROMPT_ARM"]="finish" if arm=="finish" else "baseline"
    return env

def make_fixture(project: Path, task_name: str) -> str:
    task=TASKS[task_name]
    data=json.loads(task["source"].read_text(encoding="utf-8"))
    for rel, item in data["files"].items():
        p=project/rel
        p.parent.mkdir(parents=True,exist_ok=True)
        p.write_text(item["source"],encoding="utf-8",newline="\n")
    tests=project/"tests"
    tests.mkdir(exist_ok=True)
    (tests/"public.test.mjs").write_text(task["public"],encoding="utf-8",newline="\n")
    source=(project/"index.js").read_text(encoding="utf-8")
    if source != data["files"]["index.js"]["source"]:
        raise RuntimeError("source mismatch to GitHub blob")
    return hashlib.sha256(task["public"].encode("utf-8")).hexdigest()

def oracle(task_name: str, project: Path, testhash: str, hidden: bool) -> tuple[bool,str]:
    public=(project/"tests/public.test.mjs").read_text(encoding="utf-8")
    if hashlib.sha256(public.encode()).hexdigest()!=testhash:
        return False,"agent modified public tests"
    task=TASKS[task_name]
    testlist=["tests/public.test.mjs"]
    if hidden:
        (project/"tests/holdout.test.mjs").write_text(task["holdout"],encoding="utf-8",newline="\n")
        testlist.append("tests/holdout.test.mjs")
    run=subprocess.run(["node","--test",*testlist],cwd=project,text=True,
        capture_output=True,timeout=45)
    return run.returncode==0,(run.stdout[-1450:]+" "+run.stderr[-300:])[-1700:]

def test_called(wal: Path)->bool:
    if not wal.is_file():return False
    for ln in wal.read_text(encoding="utf-8",errors="replace").splitlines():
        try: msg=json.loads(ln.split("\t",1)[0])["message"]
        except (ValueError,KeyError):continue
        if msg.get("role")!="assistant":continue
        for call in msg.get("tool_calls") or []:
            if call["function"]["name"]!="shell":continue
            try: cmd=json.loads(call["function"]["arguments"]).get("command","")
            except ValueError:continue
            if "node --test" in cmd or "npm test" in cmd: return True
    return False

def run_one(task_name: str,arm:str,repeat:int,cap:int)->dict:
    trial=LAB/task_name/str(repeat)/arm
    project=trial/"project"
    project.mkdir(parents=True,exist_ok=False)
    hash_=make_fixture(project,task_name)
    env=environment(project,trial,arm)
    started=time.monotonic()
    proc=subprocess.Popen([str(BINARY),"--print","--no-mcp",TASKS[task_name]["prompt"]],
        cwd=project,env=env,stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,stderr=subprocess.PIPE,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=="nt" else 0)
    timeout=False
    try: out,err=proc.communicate(timeout=cap)
    except subprocess.TimeoutExpired:
        timeout=True
        if os.name=="nt":
            subprocess.run(["taskkill","/PID",str(proc.pid),"/T","/F"],
                capture_output=True,timeout=20)
        else:proc.kill()
        out,err=proc.communicate(timeout=15)
    elapsed=round(time.monotonic()-started,3)
    (trial/"stdout.log").write_bytes(out)
    (trial/"stderr.log").write_bytes(err)
    metrics,_,tools,profile=parse_kroma(out.decode(errors="replace"),err.decode(errors="replace"),trial/"session.json")
    verified,details=oracle(task_name,project,hash_,True)
    tested=test_called(trial/"session.json.wal")
    finished=proc.returncode==0 and not timeout
    return {
        "task":task_name,"repeat":repeat,"arm":arm,"wall_s":elapsed,
        "timeout":timeout,"exit":proc.returncode,"verified":verified,
        "tested":tested,"finished":finished,
        "strict_pass":bool(verified and tested and finished),
        "metrics":metrics,"tools":len(tools),"profile":profile,
        "details":details,"trace":summarize_kroma(trial/"stderr.log"),
        "logs":str(trial),
    }

def order_for(task:str,repeat:int)->list[str]:
    order=list(ARMS)
    random.Random(int.from_bytes(hashlib.sha256(f"{SEED}:{task}".encode()).digest()[:8])).shuffle(order)
    if repeat%2==0:order.reverse()
    return order

def main():
    p=argparse.ArgumentParser()
    p.add_argument("--tasks",default=",".join(TASKS))
    p.add_argument("--repeat-start",type=int,default=1)
    p.add_argument("--repeats",type=int,default=2)
    p.add_argument("--max-runs",type=int,default=2)
    p.add_argument("--timeout",type=int)
    p.add_argument("--preflight",action="store_true")
    p.add_argument("--self-test",action="store_true")
    a=p.parse_args()
    names=[x.strip() for x in a.tasks.split(",") if x.strip()]
    if a.self_test:
        assert all(n in TASKS for n in names)
        assert all(len(t["source"].read_text())>5000 for t in TASKS.values())
        assert order_for("p-map-88",2)==order_for("p-map-88",1)[::-1]
        print("round14 historical-GitHub benchmark self-test OK",flush=True)
        return
    if any(n not in TASKS for n in names):p.error("unknown task")
    LAB.mkdir(parents=True,exist_ok=True)
    if a.preflight:
        for name in names:
            project=LAB/"preflight"/name
            project.mkdir(parents=True,exist_ok=False)
            expected=make_fixture(project,name)
            public,_=oracle(name,project,expected,False)
            hidden,details=oracle(name,project,expected,True)
            print("NEGATIVE_CONTROL",name,"public",public,"hidden",hidden,
                  "failure",details[-320:],flush=True)
            if not public or hidden:raise RuntimeError("invalid historical bug fixture")
        return
    if not BINARY.is_file():p.error("binary missing")
    file=LAB/"results.json"
    meta={
        "schema":1, "seed":SEED, "arms":list(ARMS),
        "tasks":{n:fingerprint(t) for n,t in TASKS.items()},
        "source_refs":{n:json.loads(t["source"].read_text())["base_sha"] for n,t in TASKS.items()},
        "binary_sha256":hashlib.sha256(BINARY.read_bytes()).hexdigest(),
        "model":"deepseek-v4.1-flash","thinking":"high",
    }
    doc=json.loads(file.read_text(encoding="utf-8")) if file.is_file() else {**meta,"results":[]}
    for key,value in meta.items():
        if doc.get(key)!=value:raise RuntimeError("lab metadata drift: "+key)
    done={(r["task"],r["repeat"],r["arm"]) for r in doc["results"]}
    count=0
    for rep in range(a.repeat_start,a.repeat_start+a.repeats):
        for name in names:
            for arm in order_for(name,rep):
                if (name,rep,arm) in done or count>=a.max_runs:continue
                print("START",rep,name,arm,flush=True)
                try: row=run_one(name,arm,rep,a.timeout or TASKS[name]["cap"])
                except Exception as exc:
                    row={"task":name,"repeat":rep,"arm":arm,"strict_pass":False,
                         "runner_error":type(exc).__name__+": "+str(exc)[:180]}
                doc["results"].append(row)
                file.write_text(json.dumps(doc,indent=2,ensure_ascii=False,default=str)+"\n",encoding="utf-8")
                m=row.get("metrics") or {}
                print("END",rep,name,arm,"strict",row.get("strict_pass"),"verified",row.get("verified"),
                      "tested",row.get("tested"),"wall",row.get("wall_s"),"req",m.get("requests"),
                      "tools",row.get("tools"),"error",row.get("runner_error"),flush=True)
                count+=1
            if count>=a.max_runs:break
        if count>=a.max_runs:break
    print("REPORT",file,len(doc["results"]),flush=True)

if __name__=="__main__":
    main()
