#!/usr/bin/env python3
"""P5 controlled 2x2 prompt/tool-schema ablation. Production code is untouched.

Arms: A=production, B=focused-inspection system clause, C=concise tool
description, D=B+C. Only a disposable source copy receives the test flags.
Each task starts from the same independent acceptance-test fixture. No agent
wall-time/token budget. Logs stay in a private directory outside the project.

Usage: python3 benchmarks/p5_prompt_tool_factorial.py setup|calibrate|run|report
"""
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
LAB = Path('/tmp/kroma-p5-prompt-tool')
RUNTIME = LAB / 'runtime'
TARGET = LAB / 'cargo-target'
BINARY = TARGET / 'release' / 'kroma'
RESULTS = LAB / 'results.json'
ARMS = ('A', 'B', 'C', 'D')
TASKS = ('bug', 'feature')

spec = importlib.util.spec_from_file_location('p1_tasks', ROOT / 'benchmarks' / 'p0p1_fair.py')
p1 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p1)


def replace_once(path, old, new):
    source = path.read_text()
    if source.count(old) != 1:
        raise ValueError(f'Expected one match in {path}: {old[:100]}')
    path.write_text(source.replace(old, new, 1))


def save(path, content):
    tmp = path.with_suffix('.tmp')
    tmp.write_text(json.dumps(content, ensure_ascii=False, indent=2) + '\n')
    tmp.replace(path)


def setup():
    if LAB.exists():
        raise RuntimeError('Experiment directory already exists; no overwrite')
    LAB.mkdir(mode=0o700)
    RUNTIME.mkdir(mode=0o700)
    for item in ('Cargo.toml', 'Cargo.lock', 'README.md', '.env.example', 'src', '.cargo'):
        src = ROOT / item
        if src.is_dir():
            shutil.copytree(src, RUNTIME / item)
        elif src.is_file():
            shutil.copy2(src, RUNTIME / item)

    agent = RUNTIME / 'src' / 'agent.rs'
    replace_once(agent,
        'system.push_str(EXECUTION_GUIDANCE);',
        '''let variant = std::env::var("KROMA_P5_ARM").unwrap_or_default();
        if variant == "B" || variant == "D" {
            system.push_str(&EXECUTION_GUIDANCE.replace(
                "For coding tasks, focus inspections on relevant files.",
                "For coding tasks, use focused searches of relevant code; once the needed evidence is found, edit rather than re-inspecting unchanged files."
            ));
        } else {
            system.push_str(EXECUTION_GUIDANCE);
        }''')

    tools = RUNTIME / 'src' / 'tools.rs'
    replace_once(tools,
        '"description": description,',
        '''"description": if matches!(
                std::env::var("KROMA_P5_ARM").ok().as_deref(),
                Some("C" | "D")
            ) && std::path::Path::new("/bin/bash").is_file() {
                "Run a Bash command in the project working directory. Supports normal Bash syntax including pipes and pipefail. Returns exit_code, stdout and stderr."
            } else {
                description
            },''')

    env = os.environ.copy()
    env['CARGO_TARGET_DIR'] = str(TARGET)
    print('P5_BUILD_START', flush=True)
    start = time.monotonic()
    subprocess.run(['cargo', 'build', '--release', '--offline', '--locked', '--quiet'],
                   cwd=RUNTIME, env=env, check=True)
    print('P5_BUILD_OK', round(time.monotonic() - start, 2), flush=True)

    meta = {
        'source_git_head': subprocess.check_output(['git', 'rev-parse', 'HEAD'],
                                                    cwd=ROOT, text=True).strip(),
        'production_agent_sha256': hashlib.sha256((ROOT / 'src/agent.rs').read_bytes()).hexdigest(),
        'production_tools_sha256': hashlib.sha256((ROOT / 'src/tools.rs').read_bytes()).hexdigest(),
        'arms': {
            'A': 'unchanged current P4 system prompt and tool description',
            'B': 'replace only first system guidance sentence with focused-search / stop-reinspection clause',
            'C': 'replace only Bash tool description with shorter interface contract (no duplicated workflow advice)',
            'D': 'both B and C (interaction test)',
        },
        'model': 'existing configured WorkBuddy DeepSeek High; no provider credentials copied',
        'tasks': list(TASKS), 'repeats': 2,
        'agent_time_budget': None, 'token_budget': None,
        'selection_policy': 'all acceptance/upstream checks; paired model/tool calls; then wall time; validate winner on an independent Rust task',
    }
    save(RESULTS, {'metadata': meta, 'trials': []})


def trial(arm, task, repeat, calibration=False):
    name = f'{task}-{repeat}-{arm}'
    trial_dir = LAB / 'trials' / name
    trial_dir.mkdir(parents=True, mode=0o700, exist_ok=False)
    work = trial_dir / 'source'
    if calibration:
        work.mkdir()
        (work / 'fixture.txt').write_text('MARKER=P5_OK\n')
        prompt = 'Use the shell tool to read fixture.txt and return exactly the marker value.'
    else:
        source = p1.prepare(task, f'p5-{arm}-{repeat}')
        shutil.move(str(source), work)
        prompt = p1.TASKS[task]
        if p1.verify(task, work):
            raise RuntimeError('Acceptance passes before task; invalid fixture')

    env = os.environ.copy()
    for key in ('KROMA_MODEL', 'KROMA_API_KEY', 'KROMA_BASE_URL',
                'KROMA_CONFIG_FILE', 'KROMA_PROVIDER_PROFILE',
                'KROMA_DEEPSEEK_THINKING'):
        env.pop(key, None)
    env.update(KROMA_WORKDIR=str(work), KROMA_SESSION_FILE=str(trial_dir / 'conversation.json'),
               KROMA_PROFILE='1', KROMA_P5_ARM=arm)
    stdout, stderr = trial_dir / 'stdout.log', trial_dir / 'stderr.log'
    print('P5_START', name, flush=True)
    started = time.monotonic()
    with stdout.open('wb') as out, stderr.open('wb') as err:
        os.chmod(stdout, 0o600)
        os.chmod(stderr, 0o600)
        process = subprocess.Popen([str(BINARY), prompt], cwd=work, env=env,
                                   stdin=subprocess.DEVNULL, stdout=out, stderr=err)
        exit_code = process.wait()  # No task-level deadline or token budget.
    wall = round(time.monotonic() - started, 3)
    logs = stderr.read_text(errors='replace')
    usage = []
    for line in logs.splitlines():
        if not line.startswith('[usage] request:'):
            continue
        values = {}
        for key in ('input', 'cached', 'output'):
            value = re.search(r'\b' + key + r': (\d+|n/a)', line)
            values[key] = int(value.group(1)) if value and value.group(1) != 'n/a' else None
        usage.append(values)

    if calibration:
        accepted = 'P5_OK' in stdout.read_text(errors='replace')
        upstream = True
        unchanged_fixture = True
    else:
        target = 'test_p1_regression.py' if task == 'bug' else 'test_p1_feature.py'
        unchanged_fixture = (work / 'tests' / target).read_text() == (
            p1.BUG_TEST if task == 'bug' else p1.FEATURE_TEST)
        accepted = p1.verify(task, work)
        upstream = subprocess.run(['python3', '-m', 'pytest', '-q',
                                   'tests/test_urlutils.py' if task == 'bug' else 'tests/test_strutils.py'],
                                  cwd=work, capture_output=True, text=True).returncode == 0

    profile = re.search(r'^\[profile\] wall_ms=[^\n]*?\btool_calls=(\d+)', logs, re.M)
    result = {
        'arm': arm, 'task': task, 'repeat': repeat, 'elapsed_s': wall,
        'exit': exit_code, 'accepted': accepted, 'unchanged_fixture': unchanged_fixture,
        'upstream': upstream, 'pass': bool(exit_code == 0 and accepted and unchanged_fixture and upstream),
        'requests': len(usage),
        'tools': int(profile.group(1)) if profile else len(re.findall(r'^\[profile\] tool_done', logs, re.M)),
        'cached': sum(v.get('cached') or 0 for v in usage),
        'uncached': sum(v['input'] - v['cached'] for v in usage
                        if v['input'] is not None and v['cached'] is not None),
        'output': sum(v.get('output') or 0 for v in usage),
        'usage_unknown': any(None in v.values() for v in usage),
        'model_timeout_retries': logs.count('[model] request timed out'),
        'apec_errors': logs.count('[apec] summary failed'),
        'artifacts': str(trial_dir),
    }
    print('P5_END', name, 'pass', result['pass'], 'seconds', wall,
          'requests', result['requests'], 'tools', result['tools'], flush=True)
    return result


def calibrate():
    rows = [trial(arm, 'calibration', 0, True) for arm in ARMS]
    save(LAB / 'calibration.json', rows)
    if not all(r['pass'] for r in rows):
        raise RuntimeError('At least one calibration failed')


def schedule():
    # Counterbalanced arm order for each task; all variants run sequentially.
    for repeat in (1, 2):
        for task in (TASKS if repeat == 1 else tuple(reversed(TASKS))):
            offset = (TASKS.index(task) + repeat - 1) % 4
            arms = ARMS[offset:] + ARMS[:offset]
            if repeat == 2:
                arms = tuple(reversed(arms))
            for arm in arms:
                yield arm, task, repeat


def run():
    data = json.loads(RESULTS.read_text())
    for arm, task, repeat in schedule():
        if any(x['arm'] == arm and x['task'] == task and x['repeat'] == repeat for x in data['trials']):
            continue
        data['trials'].append(trial(arm, task, repeat))
        save(RESULTS, data)
    report()


def report():
    data = json.loads(RESULTS.read_text())
    for arm in ARMS:
        cases = [x for x in data['trials'] if x['arm'] == arm]
        print('P5_SUMMARY', arm, 'n', len(cases),
              'pass', sum(x['pass'] for x in cases),
              'mean_s', round(statistics.mean(x['elapsed_s'] for x in cases), 2) if cases else None,
              'requests', sum(x['requests'] for x in cases),
              'tools', sum(x['tools'] for x in cases),
              'uncached', sum(x['uncached'] for x in cases),
              'cached', sum(x['cached'] for x in cases), flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=('setup', 'calibrate', 'run', 'report'))
    action = parser.parse_args().action
    {'setup': setup, 'calibrate': calibrate, 'run': run, 'report': report}[action]()
