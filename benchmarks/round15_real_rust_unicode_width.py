#!/usr/bin/env python3
"""Real Rust historical issue: unicode-width PR #89.

Pins real upstream pre-fix source from SHA d8654db...; public test
passes, hidden post-run test detects half-width katakana voiced marks
and asserts the generator source of truth was also corrected.
"""
from __future__ import annotations
import argparse, hashlib, json, os, random, shutil, subprocess, sys, time
from pathlib import Path
from round4_windows import parse_kroma
from round6_rust_trace_diagnosis import summarize_kroma
from round13_fresh_portfolio import agent_tested

ROOT=Path(__file__).resolve().parents[1]
SOURCES=ROOT/"benchmarks/round15_unicode_sources"
LAB=Path("D:/Kroma/experiments/kroma-round15-rust-real-issues-v6")
BINARY=Path("D:/Kroma/target-kroma-check/release/kroma.exe")
SOURCE_SHA="d8654db078bf9f93b5bfe1e6294a477cc4addc28"
TASK="unicode-width-89"
ARMS=("baseline","finish")
PUBLIC=r'''use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[test]
fn ordinary_latin_and_fullwidth_are_preserved() {
    assert_eq!('a'.width(), Some(1));
    assert_eq!("hello".width(), 5);
    assert_eq!('Ｗ'.width(), Some(2));
}
#[test]
fn newline_and_nonspacing_combining_remain_consistent() {
    assert_eq!("\r\n".width(), 1);
    assert_eq!('\u{0300}'.width(), Some(0));
}
'''
HIDDEN=r'''use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[test]
fn halfwidth_kana_marks_are_one_column_in_both_modes() {
    for c in ['\u{FF9E}', '\u{FF9F}'] {
        assert_eq!(c.width(), Some(1), "regular width for {c:?}");
        assert_eq!(c.width_cjk(), Some(1), "CJK width for {c:?}");
    }
}
#[test]
fn joined_halfwidth_kana_sequence_remains_four_columns() {
    assert_eq!("ﾊﾟｸﾞ".width(), 4);
    assert_eq!("ﾊﾟｸﾞ".width_cjk(), 4);
}
'''
PROMPT=(
    "Fix a real historical regression in the unicode-width Rust crate: "
    "halfwidth Katakana voiced / semi-voiced marks U+FF9E and U+FF9F "
    "incorrectly have zero display width, but they occupy one terminal "
    "column, including in CJK mode and sequences such as ﾊﾟｸﾞ. "
    "Keep the original API and other Unicode width behavior intact. "
    "This project includes a Unicode data generation script in "
    "scripts/unicode.py and generated Rust tables; update the source "
    "rule and usable generated output consistently. Do not modify tests. "
    "Run cargo test --offline and suitable relevant checks. "
    "Do not commit, install packages, or access credentials."
)
FILES={
    "Cargo.toml":"Cargo.toml",
    "src/lib.rs":"src__lib.rs",
    "src/lookup.rs":"src__lookup.rs",
    "src/props.rs":"src__props.rs",
    "src/width_info.rs":"src__width_info.rs",
    "src/test.rs":"src__test.rs",
    "src/gen/lookup.rs":"src__gen__lookup.rs",
    "src/gen/props.rs":"src__gen__props.rs",
    "src/gen/width_info.rs":"src__gen__width_info.rs",
    "src/gen/tables.rs":"src__gen__tables.rs",
    "scripts/unicode.py":"scripts__unicode.py",
}
def env_for(project:Path,trial:Path,arm:str)->dict:
    env={k:v for k,v in os.environ.items() if not k.startswith("KROMA_")}
    env.update({
        "KROMA_PROVIDER_PROFILE":"deepseek",
        "KROMA_DEEPSEEK_THINKING":"off",
        "KROMA_REASONING_EFFORT":"high",
        "KROMA_PROFILE":"1",
        "KROMA_PLUGIN_DIR":str(trial/"disabled-plugins"/"plugins"),
        "KROMA_WORKDIR":str(project),
        "KROMA_SESSION_FILE":str(trial/"session.json"),
        "CARGO_TARGET_DIR":str(trial/"cargo-target"),
        "CARGO_INCREMENTAL":"0",
    })
    env["KROMA_EXPERIMENT_PROMPT_ARM"]="finish" if arm=="finish" else "baseline"
    return env
def fixture(project:Path)->str:
    for path,asset in FILES.items():
        dest=project/path
        dest.parent.mkdir(parents=True,exist_ok=True)
        dest.write_bytes((SOURCES/asset).read_bytes())
    # Historical manifest has optional rustc-dep-of-std workspace crates absent
    # from this Windows host offline Cargo index. Remove only those opt-in
    # dependencies in the test fixture, not any runtime source or default flag.
    manifest=project/"Cargo.toml"
    text=manifest.read_text(encoding="utf-8")
    expected=[
        'std = { version = "1.0", package = "rustc-std-workspace-std", optional = true }\n',
        'core = { version = "1.0", package = "rustc-std-workspace-core", optional = true }\n',
        "rustc-dep-of-std = ['std', 'core']",
    ]
    if any(s not in text for s in expected):
        raise RuntimeError("historical manifest drift")
    text=text.replace(expected[0],"").replace(expected[1],"")
    text=text.replace(expected[2],"rustc-dep-of-std = []")
    manifest.write_text(text,encoding="utf-8",newline="\n")
    # Upstream generated unit fixture tables_test.rs is 1.36 MB, unavailable
    # via connector. Disable only its test-module inclusion for this reduced
    # offline benchmark; no production code, generated tables, or public API
    # is changed. Test behavior through separate integration test files.
    lib=project/"src/lib.rs"
    source=lib.read_text(encoding="utf-8")
    testmod="\n#[cfg(test)]\nmod test;\n"
    if source.count(testmod)!=1:
        raise RuntimeError("original lib test module changed")
    lib.write_text(source.replace(testmod,"\n// Benchmark only: original generated internal unit suite omitted.\n"),encoding="utf-8",newline="\n")
    (project/"tests").mkdir(exist_ok=True)
    (project/"tests/public.rs").write_text(PUBLIC,encoding="utf-8",newline="\n")
    # Add an out-of-tree target dir; do not modify source crate dependencies.
    return hashlib.sha256(PUBLIC.encode()).hexdigest()
def test_command():
    return ["cargo","test","--offline","--quiet"]
def oracle(project:Path,env:dict,hash_:str,hidden:bool)->tuple[bool,str]:
    p=project/"tests/public.rs"
    if hashlib.sha256(p.read_text(encoding="utf-8").encode()).hexdigest()!=hash_:
        return False,"public tests modified"
    if hidden:
        (project/"tests/round15_holdout.rs").write_text(HIDDEN,encoding="utf-8",newline="\n")
    try:
        run=subprocess.run(test_command(),cwd=project,env=env,
            capture_output=True,text=True,encoding="utf-8",errors="replace",timeout=105)
    except Exception as e:
        return False,f"{type(e).__name__}: {str(e)[:200]}"
    if run.returncode:
        return False,(run.stdout[-700:]+"\n"+run.stderr[-1000:])[-1700:]
    if hidden:
        code=(project/"scripts/unicode.py").read_text(encoding="utf-8")
        required=("zw_map[0xFF9E]", "zw_map[0xFF9F]")
        # The exact spelling can vary. Require a meaningful source change
        # relative to the upstream baseline, in addition to running behavior.
        old=(SOURCES/"scripts__unicode.py").read_text(encoding="utf-8")
        if code==old or not any(c in code for c in required):
            return False,"generator source-of-truth not updated"
    return True,"cargo tests passed; generator rule updated" if hidden else "public tests passed"
def run_one(arm:str,repeat:int,cap:int)->dict:
    trial=LAB/TASK/str(repeat)/arm
    project=trial/"project"
    project.mkdir(parents=True,exist_ok=False)
    frozen=fixture(project)
    env=env_for(project,trial,arm)
    # Prewarm each arm's isolated Cargo cache on the public tests, before
    # timing the agent. If this fails, benchmark trial is invalid.
    first,details=oracle(project,env,frozen,False)
    if not first: raise RuntimeError("prewarm failed: "+details[:700])
    start=time.monotonic()
    proc=subprocess.Popen([str(BINARY),"--print","--no-mcp",PROMPT],
        cwd=project,env=env,stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,stderr=subprocess.PIPE,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name=="nt" else 0)
    timedout=False
    try:out,err=proc.communicate(timeout=cap)
    except subprocess.TimeoutExpired:
        timedout=True
        if os.name=="nt":
            subprocess.run(["taskkill","/PID",str(proc.pid),"/T","/F"],
                capture_output=True,timeout=20)
        else:proc.kill()
        out,err=proc.communicate(timeout=15)
    wall=round(time.monotonic()-start,3)
    (trial/"stdout.log").write_bytes(out);(trial/"stderr.log").write_bytes(err)
    metrics,_,tools,profile=parse_kroma(out.decode(errors="replace"),err.decode(errors="replace"),trial/"session.json")
    verified,detail=oracle(project,env,frozen,True)
    ran=agent_tested(trial/"session.json.wal")
    finished=proc.returncode==0 and not timedout
    return {
        "task":TASK,"arm":arm,"repeat":repeat,"wall_s":wall,
        "timeout":timedout,"finished":finished,"exit":proc.returncode,
        "verified":verified,"detail":detail,"agent_tested":ran,
        "strict_pass":bool(verified and ran and finished),
        "metrics":metrics,"profile":profile,"tools":len(tools),
        "trace":summarize_kroma(trial/"stderr.log"),
        "logs":str(trial),
    }

def main():
    p=argparse.ArgumentParser()
    p.add_argument("--preflight",action="store_true")
    p.add_argument("--self-test",action="store_true")
    p.add_argument("--repeat-start",type=int,default=1)
    p.add_argument("--repeats",type=int,default=2)
    p.add_argument("--max-runs",type=int,default=2)
    p.add_argument("--timeout",type=int,default=185)
    a=p.parse_args()
    if a.self_test:
        assert SOURCES.joinpath("src__gen__tables.rs").stat().st_size>70000
        assert set(FILES)==set(str(x).replace("__","/") for x in FILES.values())
        assert all(SOURCES.joinpath(s).is_file() for s in FILES.values())
        print("rust historical PR driver self-test OK")
        return
    LAB.mkdir(parents=True,exist_ok=True)
    if a.preflight:
        trial=LAB/"preflight"
        project=trial/"project"
        project.mkdir(parents=True,exist_ok=False)
        frozen=fixture(project)
        env=env_for(project,trial,"baseline")
        original,log=oracle(project,env,frozen,False)
        original_hidden,hiddenlog=oracle(project,env,frozen,True)
        print("NEGATIVE_CONTROL","public",original,"holdout",original_hidden,
              "error",ascii(hiddenlog[-1150:]),flush=True)
        if not original or original_hidden:
            raise RuntimeError("bad original Rust fixture: "+log[-180:])
        return
    if not (60<=a.timeout<=260 and 1<=a.repeats<=4 and 1<=a.max_runs<=8):
        p.error("invalid limits")
    report_file=LAB/"results.json"
    meta={
        "schema":1,"source_sha":SOURCE_SHA,"arms":list(ARMS),
        "model":"deepseek-v4.1-flash","thinking":"high",
        "fixture_sha256":hashlib.sha256(
            "".join((SOURCES / f).read_text(encoding="utf-8") for f in FILES.values()).encode()
        ).hexdigest(),
        "binary_sha256":hashlib.sha256(BINARY.read_bytes()).hexdigest(),
    }
    doc=json.loads(report_file.read_text(encoding="utf-8")) if report_file.exists() else {**meta,"results":[]}
    for k,v in meta.items():
        if doc.get(k)!=v: raise RuntimeError("metadata changed: "+k)
    done={(x["repeat"],x["arm"]) for x in doc["results"]}
    count=0
    for rep in range(a.repeat_start,a.repeat_start+a.repeats):
        order=list(ARMS if rep%2 else ARMS[::-1])
        for arm in order:
            if (rep,arm) in done or count>=a.max_runs:continue
            print("START",rep,arm,flush=True)
            try:row=run_one(arm,rep,a.timeout)
            except Exception as error:
                row={"task":TASK,"repeat":rep,"arm":arm,"strict_pass":False,
                     "runner_error":type(error).__name__+": "+str(error)[:230]}
            doc["results"].append(row)
            report_file.write_text(json.dumps(doc,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
            count+=1
            m=row.get("metrics") or {}
            print("END",rep,arm,"strict",row.get("strict_pass"),
                  "verified",row.get("verified"),"wall",row.get("wall_s"),
                  "requests",m.get("requests"),"tools",row.get("tools"),
                  "issue",ascii(row.get("runner_error") or str(row.get("detail"))[:300]),flush=True)
    print("REPORT",report_file,len(doc["results"]),flush=True)
if __name__=="__main__":main()
