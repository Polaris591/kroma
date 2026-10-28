#!/usr/bin/env python3
"""Live three-arm ablation: Kroma shell / shell+edit+write / Pi tools.

High effort, same WorkBuddy DeepSeek model, same source fixtures, independent
acceptance and complete per-arm usage. Production checkout is never edited.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time
import uuid


ROOT = Path(__file__).resolve().parents[1]
LAB = Path('/tmp') / ('kroma-tool-ablation-' + uuid.uuid4().hex[:10])
KROMA = (ROOT/'target/debug/examples/tool_ablation').resolve()
PI = ['pi', '--offline', '--no-extensions', '--no-skills', '--no-mcp',
      '--no-context-files', '--no-approve', '--provider', 'workbuddy',
      '--model', 'deepseek', '--thinking', 'high', '--mode', 'json', '--print',
      '--no-session']


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT/'benchmarks'/path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


suite = module('tasks', 'agent_task_suite.py')
engineering = module('eng', 'engineering_compare.py')

TASKS = {
    'records': next(t for t in suite.TASKS if t['id'] == 'implement_records'),
    'cross_file': next(t for t in suite.TASKS if t['id'] == 'cross_file_fix'),
    'rust_regression': None,
}


def event(line):
    try:
        return json.loads(line)
    except (ValueError, TypeError):
        return {}


def parse_pi(out):
    metrics = {'requests':0, 'input':0, 'cached':0, 'uncached':0,
               'cache_write':0, 'output':0, 'tools':0,
               'tool_types':{}, 'missing_usage':False, 'apec_requests':0}
    messages = []
    for line in out.splitlines():
        x = event(line)
        kind = x.get('type')
        if kind == 'tool_execution_start':
            tool = x.get('toolName', '?')
            metrics['tool_types'][tool] = metrics['tool_types'].get(tool, 0) + 1
        if kind == 'tool_execution_end':
            metrics['tools'] += 1
        if kind == 'message_end' and x.get('message', {}).get('role') == 'assistant':
            message = x['message']; messages.append(message)
            usage = message.get('usage') or {}
            if not usage:
                metrics['missing_usage'] = True
                continue
            metrics['requests'] += 1
            for dest, key in [('uncached','input'), ('cached','cacheRead'),
                              ('cache_write','cacheWrite'), ('output','output')]:
                value = usage.get(key)
                if value is None:
                    metrics['missing_usage'] = True
                else:
                    metrics[dest] += value
    metrics['input'] = metrics['uncached'] + metrics['cached'] + metrics['cache_write']
    answer = '\n'.join(
        part.get('text', '') for msg in messages
        for part in msg.get('content', []) if part.get('type') == 'text'
    )
    return metrics, answer


def parse_kroma(err):
    metrics = {'requests':0, 'input':0, 'cached':0, 'uncached':0,
               'cache_write':None, 'output':0, 'tools':0,
               'tool_types':{}, 'missing_usage':False, 'apec_requests':0}
    for line in err.splitlines():
        if line.startswith('[usage] request:'):
            metrics['requests'] += 1
            for field in ('input','cached','output'):
                m = re.search(r'\b' + field + r':\s*(\d+|n/a)', line)
                if not m or m.group(1) == 'n/a':
                    metrics['missing_usage'] = True
                else:
                    metrics[field] += int(m.group(1))
        if line.startswith('[profile] tool_start'):
            m = re.search(r'\btool=([\w-]+)', line)
            if m:
                tool=m.group(1)
                metrics['tool_types'][tool]=metrics['tool_types'].get(tool,0)+1
        if line.startswith('[profile] tool_done'):
            metrics['tools'] += 1
        if line.startswith('[usage] APEC:'):
            m = re.search(r'APEC:\s*(\d+) request',line)
            if m:metrics['apec_requests'] += int(m.group(1))
    metrics['uncached']=metrics['input']-metrics['cached']
    return metrics


def make_fixture(task, dest):
    dest.mkdir(parents=True, mode=0o700)
    if task == 'rust_regression':
        engineering.make_copy(dest, 'A', 1)
        # Preserve independently supplied regression, not agent self-tests.
        regression=(dest/'src/sessions.rs').read_text()
        assert 'fn wal_append_controls_recent_session_order()' in regression
    else:
        suite.create_fixture(dest, TASKS[task])


def verify(task, dest):
    if task == 'rust_regression':
        source = (dest/'src/sessions.rs').read_text()
        if source.count('fn wal_append_controls_recent_session_order()') != 1:
            return False, 'regression removed'
        try:
            ok = engineering.verify(dest,'A',1)
            return bool(ok), 'independent WAL regression test'
        except Exception as e:
            return False, type(e).__name__
    try:
        ok = suite.verify(dest, TASKS[task]['id'], '')
        return bool(ok), 'independent Python unit tests'
    except Exception as e:
        return False, type(e).__name__


def run(task, arm, repeat, timeout):
    outdir = LAB/f'{task}-{repeat}'/arm
    work = outdir/'source'
    make_fixture(task, work)
    prompt = (engineering.A_PROMPT if task == 'rust_regression'
              else TASKS[task]['prompt'])
    prompt += ('\nUse only the isolated checkout. Do not modify hidden benchmark '
               'tests, commit, or access real credentials. Complete all '
               'requested validation before finishing.')
    env = {k:v for k,v in os.environ.items() if not k.startswith('KROMA_')}
    env['CARGO_TARGET_DIR'] = str(ROOT/'target')
    env['CARGO_INCREMENTAL'] = '0'
    if arm == 'pi':
        cmd = PI + [prompt]
    else:
        env.update(KROMA_PROVIDER_PROFILE='workbuddy',
                   KROMA_PROFILE='1',
                   KROMA_WORKDIR=str(work),
                   KROMA_SESSION_FILE=str(outdir/'session.json'))
        cmd = [str(KROMA), 'shell' if arm == 'shell' else 'editwrite', prompt]
    print('START',task,repeat,arm,flush=True)
    started=time.monotonic()
    proc = subprocess.Popen(cmd,cwd=work,env=env,stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                            start_new_session=True)
    timed_out=False
    try:out,err=proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out=True
        try:os.killpg(proc.pid,signal.SIGKILL)
        except ProcessLookupError:pass
        out,err=proc.communicate(timeout=10)
    elapsed=round(time.monotonic()-started,3)
    outdir.mkdir(exist_ok=True)
    (outdir/'stdout.log').write_bytes(out)
    (outdir/'stderr.log').write_bytes(err)
    (outdir/'stdout.log').chmod(0o600)
    (outdir/'stderr.log').chmod(0o600)
    text=out.decode(errors='replace')
    if arm == 'pi':metrics,answer=parse_pi(text)
    else:metrics=parse_kroma(err.decode(errors='replace'));answer=text
    accepted,evidence=verify(task,work)
    row={'task':task,'repeat':repeat,'arm':arm,
         'elapsed_s':elapsed,'exit_code':proc.returncode,'timeout':timed_out,
         'functional':accepted,'passed':bool(accepted and proc.returncode==0 and not timed_out),
         'evidence':evidence,'metrics':metrics,'outdir':str(outdir),
         'answer_tail':answer[-350:]}
    print('END',task,repeat,arm,'PASS',row['passed'],'WALL',elapsed,
          'REQUESTS',metrics['requests'],'TOOLS',metrics['tools'],
          'IN',metrics['input'],'CACHED',metrics['cached'],
          'OUT',metrics['output'],flush=True)
    return row


def main():
    ap=argparse.ArgumentParser()
    ap.add_argument('--tasks',default='records,cross_file,rust_regression')
    ap.add_argument('--python-repeats',type=int,default=2)
    ap.add_argument('--rust-repeats',type=int,default=1)
    ap.add_argument('--timeout',type=int,default=240)
    args=ap.parse_args()
    assert KROMA.is_file(), 'cargo build --locked --offline --example tool_ablation first'
    tasks=args.tasks.split(',')
    assert all(task in TASKS for task in tasks)
    LAB.mkdir(mode=0o700)
    report={'experiment':'Kroma Shell vs Kroma Shell+Edit/Write vs Pi built-ins',
            'model':'workbuddy/deepseek','thinking':'high',
            'code_sha':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
            'runner_binary':str(KROMA),'cases':[],'timeout_s':args.timeout,
            'method':'same Kroma runner, only edit/write tool registration changes; Pi built-in tools; independent fixtures; order alternated'}
    print('LAB',LAB,flush=True)
    plan=[]
    for index,task in enumerate(tasks):
        repeats=(args.rust_repeats if task=='rust_regression' else args.python_repeats)
        for repeat in range(1,repeats+1):
            # All three arms take different positions across consecutive blocks.
            order=['pi','shell','editwrite']
            offset=(index+repeat-1)%3
            order=order[offset:]+order[:offset]
            plan.extend((task,repeat,arm) for arm in order)
    for task,repeat,arm in plan:
        try:row=run(task,arm,repeat,args.timeout)
        except Exception as err:
            row={'task':task,'repeat':repeat,'arm':arm,'passed':False,
                 'error_type':type(err).__name__, 'error':str(err)[:200]}
            print('ERROR',task,repeat,arm,type(err).__name__,flush=True)
        report['cases'].append(row)
        temp=LAB/'results.tmp'
        temp.write_text(json.dumps(report,indent=2,ensure_ascii=False)+'\n')
        temp.replace(LAB/'results.json')
    print('REPORT',LAB/'results.json',flush=True)
    for arm in ('pi','shell','editwrite'):
        group=[x for x in report['cases'] if x['arm']==arm]
        m=[x['metrics'] for x in group if 'metrics' in x]
        print('TOTAL',arm,'passed',sum(x.get('passed',False) for x in group),
              'n',len(group),'time',round(sum(x.get('elapsed_s',0) for x in group),2),
              'input',sum(x['input'] for x in m),'cached',sum(x['cached'] for x in m),
              'output',sum(x['output'] for x in m),'requests',sum(x['requests'] for x in m),
              'tools',sum(x['tools'] for x in m),flush=True)


if __name__=='__main__':main()
