#!/usr/bin/env python3
"""Four-arm, zero-quota Kroma system-prompt ablation on isolated code tasks.

Runs the identical Rust harness binary and tool schema for every arm; only
the system prompt changes via a TEST-ONLY env flag in an isolated Rust copy.
No changes to the production binary, installed provider profiles, or main
repository source. No agent time limit or token limit.

Usage:
    python3 benchmarks/prompt_ablation.py setup
    python3 benchmarks/prompt_ablation.py calibrate
    python3 benchmarks/prompt_ablation.py run
    python3 benchmarks/prompt_ablation.py report
"""
from __future__ import annotations

import argparse
import hashlib
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
LAB = Path('/tmp/kroma-prompt-ablation')
RUNTIME = LAB / 'runtime'
SOURCE = Path('/tmp/kroma-p1-sources/boltons')
TARGET = LAB / 'cargo-target'
BINARY = TARGET / 'release' / 'kroma'
RESULTS = LAB / 'results.json'
VARIANTS = ('A', 'B', 'C', 'D')
TASKS = ('bug', 'feature')

spec = importlib.util.spec_from_file_location('p1_tasks', ROOT / 'benchmarks' / 'p0p1_fair.py')
p1 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p1)

INJECT_GUARD = 'fn add_batch_guidance(view: &mut [Message]) {'
INJECT_HISTORY = 'fn initial_history(workdir: &std::path::Path) -> Vec<Message> {'
SOURCE_ITEMS = ['Cargo.toml', 'Cargo.lock', 'README.md', '.env.example', 'src', '.cargo']

INJECT_BEFORE_GUIDANCE = """fn add_batch_guidance(view: &mut [Message]) {
    // The test-only ablation changes only the outgoing system prompt.
    // The production run with the flag absent or set to A is untouched.
    if matches!(
        std::env::var("KROMA_PROMPT_ABLATION").ok().as_deref(),
        Some("B" | "C" | "D")
    ) {
        return;
    }
"""
INJECT_BEFORE_HISTORY = """fn initial_history(workdir: &std::path::Path) -> Vec<Message> {
    let base = format!(
        "You are Kroma, an AI coding agent. Use the shell tool to inspect and modify files in {}. Report command results accurately.",
        workdir.display()
    );
    let variant = std::env::var("KROMA_PROMPT_ABLATION").unwrap_or_default();
    let custom = match variant.as_str() {
        "B" => Some(base),
        "C" => Some(format!("{base} Inspect relevant code, implement the requested change, run the requested tests and relevant regressions, and finish when they pass. Combine independent inspections when useful.")),
        "D" => Some(format!("{base} Do not expand the requested scope, repeat already successful checks without new changes, or continue investigating unrelated issues after verification. Do not skip required tests or claim unverified work.")),
        _ => None,
    };
    if let Some(prompt) = custom {
        return vec![Message::new("system", prompt)];
    }
"""

def atomic_json(path, value):
    tmp = path.with_suffix('.json.new')
    tmp.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')
    tmp.replace(path)


def setup():
    if LAB.exists():
        raise RuntimeError(f'{LAB} already exists; refuse overwrite')
    LAB.mkdir(parents=True, mode=0o700)
    RUNTIME.mkdir(mode=0o700)
    for item in SOURCE_ITEMS:
        src = ROOT / item
        if src.is_dir():
            shutil.copytree(src, RUNTIME / item)
        elif src.is_file():
            shutil.copy2(src, RUNTIME / item)
    src = RUNTIME / 'src' / 'agent.rs'
    old = src.read_text()
    assert old.count(INJECT_GUARD) == 1
    assert old.count(INJECT_HISTORY) == 1
    updated = old.replace(INJECT_GUARD, INJECT_BEFORE_GUIDANCE, 1)
    updated = updated.replace(INJECT_HISTORY, INJECT_BEFORE_HISTORY, 1)
    src.write_text(updated)
    meta = {
        'source_sha': subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
        'main_agent_sha256': hashlib.sha256(old.encode()).hexdigest(),
        'test_agent_sha256': hashlib.sha256(updated.encode()).hexdigest(),
        'model': 'configured workbuddy/deepseek with High thinking; no credentials copied',
        'variants': {
            'A': 'exact production baseline initial_history + EXECUTION_GUIDANCE',
            'B': 'minimal identity/tool/cwd/accurate reporting',
            'C': 'B + concise positive workflow and stop rule',
            'D': 'B + negative guardrails against unnecessary work and skip tests',
        },
        'tasks': list(TASKS),
        'repeats': 2,
        'token_quota': None,
        'time_limit': None,
        'start_timestamp': time.time(),
    }
    atomic_json(LAB/'metadata.json', meta)
    atomic_json(RESULTS, {'metadata': meta, 'trials': []})
    env = os.environ.copy()
    env['CARGO_TARGET_DIR'] = str(TARGET)
    print('BUILD_START test-only instrumented runtime; no source changes in main checkout',flush=True)
    t = time.monotonic()
    p = subprocess.run(['cargo','build','--release','--offline','--locked','--quiet'], cwd=RUNTIME, env=env)
    if p.returncode:
        raise RuntimeError(f'experimental build failed with exit code {p.returncode}')
    print('BUILD_OK',round(time.monotonic()-t,2),'seconds', 'binary',BINARY,flush=True)
    assert BINARY.is_file()


def run_trial(variant, task, repeat, calibration=False):
    slug = f'{task}-{repeat}-{variant}'
    trial = LAB / 'trials' / slug
    if trial.exists():
        raise RuntimeError(f'trial already exists: {slug}')
    trial.mkdir(parents=True, mode=0o700)
    if calibration:
        work = trial/'source'
        work.mkdir()
        (work/'fixture.txt').write_text('MARKER=ABLAT_TEST_OK\n')
        prompt = 'Read fixture.txt using the shell tool and respond with the exact marker value only.'
    else:
        source = p1.prepare(task, f'ablation-{repeat}-{variant}')
        work = trial/'source'
        shutil.move(str(source),work)
        prompt = p1.TASKS[task]
        assert not p1.verify(task,work), 'baseline task acceptance unexpectedly passes'
    env = os.environ.copy()
    for k in ('KROMA_MODEL','KROMA_API_KEY','KROMA_BASE_URL','KROMA_CONFIG_FILE',
              'KROMA_PROVIDER_PROFILE','KROMA_DEEPSEEK_THINKING'):
        env.pop(k,None)
    env.update(KROMA_WORKDIR=str(work),
               KROMA_SESSION_FILE=str(trial/'conversation.json'),
               KROMA_PROFILE='1',
               KROMA_PROMPT_ABLATION=variant)
    out = trial/'stdout.log'
    err = trial/'stderr.log'
    started = time.monotonic()
    print('TRIAL_START',slug,flush=True)
    with out.open('wb') as so, err.open('wb') as se:
        os.chmod(out,0o600);os.chmod(err,0o600)
        proc = subprocess.Popen([str(BINARY),prompt], cwd=work,env=env,
                                stdin=subprocess.DEVNULL,stdout=so,stderr=se)
        code = proc.wait() # NO AGENT TIME LIMIT, NO TOKEN QUOTA.
    elapsed = round(time.monotonic()-started,3)
    text = err.read_text(errors='replace')
    usage = []
    for line in text.splitlines():
        if not line.startswith('[usage] request:'):
            continue
        row = {}
        for key in ('input','cached','output'):
            hit = re.search(r'\b'+key+r': (\d+|n/a)',line)
            row[key] = int(hit.group(1)) if hit and hit.group(1)!='n/a' else None
        if all(v is not None for v in row.values()):
            row['uncached'] = row['input'] - row['cached']
        usage.append(row)
    profile = next((line for line in text.splitlines() if line.startswith('[profile] wall_ms=')),None)
    profile_values = {}
    if profile:
        for field,val in re.findall(r'(\w+)=([0-9.]+)',profile):
            if field in ('context_ms','model_ms','shell_ms','journal_ms',
                         'apec_finish_ms','model_requests','tool_calls'):
                profile_values[field] = float(val) if field.endswith('_ms') else int(val)
    marker_ok = 'ABLAT_TEST_OK' in out.read_text(errors='replace') if calibration else None
    if calibration:
        verified=marker_ok
        upstream=None
    else:
        verified=p1.verify(task,work)
        upstream = subprocess.run(['python3','-m','pytest','-q',
                                   'tests/test_urlutils.py' if task=='bug' else 'tests/test_strutils.py'],
                                  cwd=work,capture_output=True,text=True).returncode==0
        # Verify the hidden acceptance file wasn't altered.
        acceptance=work/'tests'/('test_p1_regression.py' if task=='bug' else 'test_p1_feature.py')
        hidden=p1.BUG_TEST if task=='bug' else p1.FEATURE_TEST
        verified = verified and acceptance.read_text()==hidden
    result = {
        'task':task,'repeat':repeat,'variant':variant,'elapsed_s':elapsed,
        'exit_code':code,'functional_pass':verified,
        'upstream_pass':upstream,'completed_correctly':code==0 and verified and (upstream is not False),
        'requests':len(usage),'model_starts':text.count('[profile] model_start'),
        'model_done':text.count('[profile] model_done'),
        'tool_calls':profile_values.get('tool_calls'),
        'model_time_s':round(profile_values.get('model_ms',0)/1000,3),
        'shell_time_s':round(profile_values.get('shell_ms',0)/1000,3),
        'context_time_s':round(profile_values.get('context_ms',0)/1000,4),
        'apec_errors':text.count('[apec] summary failed'),
        'model_timeout_retries':text.count('[model] request timed out'),
        'uncached_input':sum(x.get('uncached') or 0 for x in usage),
        'cached_input':sum(x.get('cached') or 0 for x in usage),
        'output_tokens':sum(x.get('output') or 0 for x in usage),
        'usage_unknown':any(any(v is None for v in r.values()) for r in usage),
        'trial_dir':str(trial),
    }
    print('TRIAL_END',slug,'pass',result['completed_correctly'],'elapsed_s',elapsed,
          'requests',len(usage),'tools',result['tool_calls'],
          'model_timeout_retries',result['model_timeout_retries'],flush=True)
    return result


def run():
    assert BINARY.is_file(), 'run setup first'
    data = json.loads(RESULTS.read_text())
    schedules = []
    for repeat in (1,2):
        for task in (TASKS if repeat==1 else tuple(reversed(TASKS))):
            offset=(TASKS.index(task)+repeat-1)%4
            if repeat==1:
                arm_order=VARIANTS[offset:]+VARIANTS[:offset]
            else:
                arm_order=tuple(reversed(VARIANTS[offset:]+VARIANTS[:offset]))
            schedules.extend((var,task,repeat) for var in arm_order)
    for variant,task,repeat in schedules:
        if any(r['variant']==variant and r['task']==task and r['repeat']==repeat
               for r in data['trials']):
            continue
        row=run_trial(variant,task,repeat)
        data['trials'].append(row)
        atomic_json(RESULTS,data)
    report()


def calibrate():
    assert BINARY.is_file(), 'run setup first'
    all_rows=[]
    for variant in VARIANTS:
        row=run_trial(variant,'calibrate',0,True)
        all_rows.append(row)
    atomic_json(LAB/'calibration.json',all_rows)
    if not all(row['completed_correctly'] for row in all_rows):
        raise RuntimeError('Calibration failed in at least one variant')


def report():
    data=json.loads(RESULTS.read_text())
    print('RESULTS',RESULTS,'finished',len(data['trials']),flush=True)
    for v in VARIANTS:
        arr=[x for x in data['trials'] if x['variant']==v]
        print('SUMMARY',v,'n',len(arr),
              'passed',sum(x['completed_correctly'] for x in arr),
              'mean_seconds',round(statistics.mean(x['elapsed_s'] for x in arr),2) if arr else None,
              'model_requests',sum(x['requests'] for x in arr),
              'tools',sum(x['tool_calls'] or 0 for x in arr),
              'uncached',sum(x['uncached_input'] for x in arr),
              'cached',sum(x['cached_input'] for x in arr),
              'output',sum(x['output_tokens'] for x in arr),
              flush=True)


if __name__ == '__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('action',choices=('setup','calibrate','run','report'))
    args=parser.parse_args()
    {'setup':setup,'calibrate':calibrate,'run':run,'report':report}[args.action]()
