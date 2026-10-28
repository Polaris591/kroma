#!/usr/bin/env python3
"""P4 tool-interface ablation with no token quota or task deadlines.

This script alters a *disposable copy* of the production Rust sources.
Only the shell language, shell tool description, and redundant tool-result
metadata vary; all experiments use the same binary, model, prompt and tasks.
The production checkout's sources/configuration are never edited.
"""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
DIR = Path('/tmp/kroma-p4-tools')
SOURCE = DIR / 'rust-runtime'
BINARY = DIR / 'kroma-p4-ablation'
TARGET = Path('/tmp/kroma-prompt-ablation/cargo-target')
REPORT = DIR / 'results.json'
ARMS = ('A', 'B', 'C', 'D')
TASKS = ('bug', 'feature')
REPEATS = 2

spec = importlib.util.spec_from_file_location('p4_p1', ROOT/'benchmarks/p0p1_fair.py')
p1 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p1)


def atomic_save(path, data):
    tmp = path.with_suffix('.tmp')
    tmp.write_text(json.dumps(data, ensure_ascii=False, indent=2) + '\n')
    tmp.replace(path)


def replace_once(path, old, new):
    text = path.read_text()
    assert text.count(old) == 1, f'unexpected reference in {path}: {old[:60]}'
    path.write_text(text.replace(old, new, 1))


def setup():
    if DIR.exists():
        raise RuntimeError('P4 tool lab already exists; refusing to overwrite')
    DIR.mkdir(mode=0o700)
    SOURCE.mkdir()
    for item in ('Cargo.toml','Cargo.lock','README.md','.env.example','src','.cargo'):
        path = ROOT/item
        if path.is_dir():
            shutil.copytree(path, SOURCE/item)
        elif path.is_file():
            shutil.copy2(path, SOURCE/item)
    shell = SOURCE/'src/shell.rs'
    replace_once(shell, 'let (shell, switch) = ("sh", "-lc");',
        '''let (shell, switch) = if matches!(
        std::env::var("KROMA_P4_TOOL_VARIANT").ok().as_deref(),
        Some("B" | "C" | "D")
    ) && std::path::Path::new("/bin/bash").is_file() {
        ("bash", "-lc")
    } else {
        ("sh", "-lc")
    };''')
    tools = SOURCE/'src/tools.rs'
    replace_once(tools,
        '"description": "Execute a non-interactive shell command in the current working directory. Returns stdout, stderr and exit code. Only run commands needed for the user\'s task.",',
        '''"description": if matches!(
                std::env::var("KROMA_P4_TOOL_VARIANT").ok().as_deref(),
                Some("C" | "D")
            ) {
                "Run a Bash command in the project working directory. Supports normal Bash syntax including pipes and pipefail. Return exit_code, stdout and stderr. Combine independent read/search inspections into a single concise command when useful. Keep checks relevant, avoid duplicating successful checks without code changes, and verify required tests."
            } else {
                "Execute a non-interactive shell command in the current working directory. Returns stdout, stderr and exit code. Only run commands needed for the user's task."
            },''')
    replace_once(tools,
        'Ok(serde_json::to_string(&output)?)',
        '''if std::env::var("KROMA_P4_TOOL_VARIANT").ok().as_deref() == Some("D") {
        // Only omit redundant byte counts when the complete output is inline.
        // Preserve file-backed output references, status and all visible text.
        let mut payload = serde_json::to_value(&output)?;
        let object = payload.as_object_mut().expect("ShellOutput is a JSON object");
        if !object.contains_key("stdout_file") {
            object.remove("stdout_bytes");
        }
        if !object.contains_key("stderr_file") {
            object.remove("stderr_bytes");
        }
        Ok(payload.to_string())
    } else {
        Ok(serde_json::to_string(&output)?)
    }''')
    env=os.environ.copy()
    env['CARGO_TARGET_DIR']=str(TARGET)
    print('P4_BUILD_START',flush=True)
    t=time.monotonic()
    p=subprocess.run(['cargo','build','--release','--offline','--locked','--quiet'],
                     cwd=SOURCE,env=env)
    if p.returncode:raise RuntimeError(f'P4 build exit={p.returncode}')
    shutil.copy2(TARGET/'release/kroma',BINARY)
    print('P4_BUILD_OK','elapsed',round(time.monotonic()-t,2),'binary',BINARY,flush=True)
    meta={
        'source_repository':str(ROOT),
        'isolation':'all tool variants in one disposable compiled binary',
        'model':'same configured workbuddy DeepSeek High',
        'arms':{
          'A':'original POSIX sh + original tool description + full JSON result',
          'B':'Bash on Unix + original description + full JSON',
          'C':'Bash + precise tool description + full JSON',
          'D':'Bash + precise tool description + omit redundant inline output byte counters'
        },
        'tasks':list(TASKS),'repeats_per_task':REPEATS,
        'benchmark_timeout_seconds':None,'token_quota':None,
        'baseline': 'compiled source copy includes latest P2/P3-C fixes',
    }
    atomic_save(REPORT,{'metadata':meta,'trials':[]})


def command(cmd,cwd,env=None):
    t=time.monotonic()
    p=subprocess.run(cmd,cwd=cwd,env=env,capture_output=True,text=True)
    return p, round(time.monotonic()-t,3)


def test_trial(arm,task,repeat,calibrate=False):
    ident=f'{task}-{repeat}-{arm}'
    workdir=DIR/'trials'/ident
    workdir.mkdir(parents=True)
    source=workdir/'source'
    if calibrate:
        source.mkdir()
        (source/'marker.txt').write_text('MARKER=P4_TOOL_OK\n')
        prompt='Read marker.txt via the shell tool and reply with its marker exactly.'
    else:
        created=p1.prepare(task,f'p4-{repeat}-{arm}')
        shutil.move(str(created),source)
        prompt=p1.TASKS[task]
    env=os.environ.copy()
    for key in ('KROMA_MODEL','KROMA_BASE_URL','KROMA_API_KEY','KROMA_CONFIG_FILE',
                'KROMA_PROVIDER_PROFILE','KROMA_DEEPSEEK_THINKING'):
        env.pop(key,None)
    env.update(KROMA_WORKDIR=str(source),
               KROMA_SESSION_FILE=str(workdir/'conversation.json'),
               KROMA_PROFILE='1',KROMA_P4_TOOL_VARIANT=arm)
    stdout=workdir/'stdout.log';stderr=workdir/'stderr.log'
    print('P4_START',ident,flush=True)
    t=time.monotonic()
    with stdout.open('wb') as o,stderr.open('wb') as e:
        os.chmod(stdout,0o600);os.chmod(stderr,0o600)
        proc=subprocess.Popen([str(BINARY),prompt],cwd=source,env=env,
            stdin=subprocess.DEVNULL,stdout=o,stderr=e)
        rc=proc.wait()  # No Benchmark timeout or token quota.
    elapsed=round(time.monotonic()-t,3)
    err=stderr.read_text(errors='replace')
    lines=err.splitlines()
    usage=[]
    for line in lines:
        if not line.startswith('[usage] request:'):continue
        fields={}
        for key in ('input','cached','output'):
            m=re.search(r'\b'+key+r': (\d+|n/a)',line)
            fields[key]=int(m.group(1)) if m and m.group(1)!='n/a' else None
        usage.append(fields)
    m=re.search(r'^\[profile\] wall_ms=[^\n]*?\btool_calls=(\d+)',err,re.M)
    finished_calls=int(m.group(1)) if m else len(re.findall(r'^\[profile\] tool_done',err,re.M))
    if calibrate:
        functional='P4_TOOL_OK' in stdout.read_text(errors='replace')
        upstream=None
    else:
        target='test_p1_regression.py' if task=='bug' else 'test_p1_feature.py'
        accept,elapsed_accept=command(['python3','-m','pytest','-q',f'tests/{target}'],source)
        fixture=(source/'tests'/target).read_text()
        reference=p1.BUG_TEST if task=='bug' else p1.FEATURE_TEST
        functional=accept.returncode==0 and fixture==reference
        upstream_test='tests/test_urlutils.py' if task=='bug' else 'tests/test_strutils.py'
        upstream_result,_=command(['python3','-m','pytest','-q',upstream_test],source)
        upstream=upstream_result.returncode==0
    result={
        'arm':arm,'task':task,'repeat':repeat,'wall_s':elapsed,
        'exit':rc,'functional':functional,'upstream':upstream,
        'pass':rc==0 and functional and upstream is not False,
        'model_requests':len(usage),'tool_calls':finished_calls,
        'model_timeouts':err.count('[model] request timed out'),
        'apec_errors':err.count('[apec] summary failed'),
        'cached_input':sum(row.get('cached') or 0 for row in usage),
        'uncached_input':sum((row['input']-row['cached']) if
                             row.get('input') is not None and row.get('cached') is not None
                             else 0 for row in usage),
        'output_tokens':sum(row.get('output') or 0 for row in usage),
        'usage_unknown':any(None in row.values() for row in usage),
        'case_dir':str(workdir)
    }
    print('P4_END',ident,'pass',result['pass'],'time',elapsed,
          'model_requests',result['model_requests'],'tools',finished_calls,
          'model_timeouts',result['model_timeouts'],flush=True)
    return result


def calibrate():
    data=[]
    for arm in ARMS:
        row=test_trial(arm,'calibration',0,True)
        data.append(row)
    atomic_save(DIR/'calibration.json',data)
    if not all(x['pass'] for x in data):
        raise RuntimeError('P4 tool interface calibration failed')


def schedule():
    # Interleave order to avoid assigning early/late proxy conditions to
    # one arm across all tasks.
    for task in TASKS:
        for repeat in (1,2):
            order=ARMS if (TASKS.index(task)+repeat)%2 else tuple(reversed(ARMS))
            for arm in order:
                yield arm,task,repeat


def run():
    d=json.loads(REPORT.read_text())
    for arm,task,repeat in schedule():
        if any(x['arm']==arm and x['task']==task and x['repeat']==repeat for x in d['trials']):
            continue
        row=test_trial(arm,task,repeat)
        d['trials'].append(row)
        atomic_save(REPORT,d)
    report()


def report():
    d=json.loads(REPORT.read_text())
    print('P4_RESULTS',str(REPORT),'completed',len(d['trials']),flush=True)
    for arm in ARMS:
        cases=[x for x in d['trials'] if x['arm']==arm]
        print('P4_SUMMARY',arm,'n',len(cases),
            'pass',sum(x['pass'] for x in cases),
            'mean_s',round(statistics.mean(x['wall_s'] for x in cases),2) if cases else None,
            'model_calls',sum(x['model_requests'] for x in cases),
            'tools',sum(x['tool_calls'] for x in cases),
            'cached',sum(x['cached_input'] for x in cases),
            'uncached',sum(x['uncached_input'] for x in cases),
            'output',sum(x['output_tokens'] for x in cases),flush=True)


if __name__=='__main__':
    import argparse
    p=argparse.ArgumentParser()
    p.add_argument('action',choices=('setup','calibrate','run','report'))
    a=p.parse_args()
    {'setup':setup,'calibrate':calibrate,'run':run,'report':report}[a.action]()
