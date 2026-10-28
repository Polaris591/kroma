#!/usr/bin/env python3
"""P0/P1: isolated, sequential, externally verified coding benchmark.

Only writes under /tmp, and never reads or prints provider credentials.
Requires preconfigured Kroma and Pi; each trial starts from a fresh copy.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
KROMA = ROOT / 'target/release/kroma'
PI = ['pi','--offline','--no-extensions','--no-skills','--no-mcp',
      '--no-context-files','--no-approve','--provider','workbuddy',
      '--model','deepseek','--thinking','high','--mode','json','--print']
SOURCE = Path('/tmp/kroma-p1-sources/boltons')
REPORT = Path('/tmp/kroma-p0p1-report.json')

TASKS = {
  'calibrate': 'Read fixture.txt with a tool, then respond with its exact MARKER value only. Do not modify anything.',
  'bug': (
    'This is a real third-party Python repository (boltons). A regression causes '
    'boltons.urlutils.parse_host to misclassify bracketed IPv6 addresses. '
    'Find the cause and fix the implementation, preserving IPv4/hostname behavior. '
    'Do not modify tests. Run python3 -m pytest -q tests/test_p1_regression.py '
    'before declaring success. Work only inside this checkout. Do not commit.'),
  'feature': (
    'In this real third-party Python repository (boltons), implement '
    'boltons.strutils.mask_digits(text, replacement="X"). Return a new string '
    'with every ASCII digit 0-9 replaced by the provided single-character '
    'replacement. Preserve non-ASCII digits and all other characters. '
    'Validate that input is str and replacement is a single-character str, '
    'raising TypeError and ValueError respectively. Add meaningful tests to '
    'tests/test_strutils.py and document the new function in docs/strutils.rst '
    'if present (otherwise use the module docstring). '
    'Run python3 -m pytest -q tests/test_p1_feature.py before declaring success. '
    'Do not edit the hidden benchmark acceptance test or commit.'),
}

BUG_TEST = '''from boltons.urlutils import parse_host
import socket

def test_bracketed_ipv6_regression():
    assert parse_host('[::1]') == (socket.AF_INET6, '::1')
    assert parse_host('[2001:db8::1]') == (socket.AF_INET6, '2001:db8::1')
    assert parse_host('127.0.0.1') == (socket.AF_INET, '127.0.0.1')
    assert parse_host('example.com') == (None, 'example.com')
'''
FEATURE_TEST = '''import pytest
from boltons.strutils import mask_digits

@pytest.mark.parametrize('value,repl,expected', [
    ('abc123-045', 'X', 'abcXXX-XXX'),
    ('9\\u0664\\u06f5', '*', '*\\u0664\\u06f5'),
    ('unchanged', '*', 'unchanged'),
    ('0123456789', '#', '##########'),
    ('', '*', ''),
])
def test_mask_digits(value,repl,expected):
    assert mask_digits(value,repl) == expected

def test_mask_digits_rejects_invalid_types():
    with pytest.raises(TypeError): mask_digits(22)
    with pytest.raises(ValueError): mask_digits('123', '**')
    with pytest.raises(ValueError): mask_digits('123', 2)
'''


def prepare(task, run_id):
    dest=Path(tempfile.mkdtemp(prefix=f'kroma-p1-{task}-{run_id}-'))
    if task=='calibrate':
        (dest/'fixture.txt').write_text('MARKER=CALIBRATE_P0_OK\n')
        return dest
    shutil.copytree(SOURCE,dest,dirs_exist_ok=True,ignore=shutil.ignore_patterns('.git','__pycache__','.pytest_cache'))
    if task=='bug':
        p=dest/'boltons/urlutils.py';s=p.read_text()
        needle="if ':' in host and '[' == host[0] and ']' == host[-1]:"
        assert s.count(needle)==1
        p.write_text(s.replace(needle,"if False and ':' in host and '[' == host[0] and ']' == host[-1]:"))
        (dest/'tests/test_p1_regression.py').write_text(BUG_TEST)
    elif task=='feature':
        (dest/'tests/test_p1_feature.py').write_text(FEATURE_TEST)
    return dest


def verify(task,path,timeout=40):
    if task=='calibrate':return None
    target='test_p1_regression.py' if task=='bug' else 'test_p1_feature.py'
    p=subprocess.run(['python3','-m','pytest','-q','tests/'+target],cwd=path,
                     capture_output=True,text=True,timeout=timeout)
    return p.returncode==0


def execute(runner,task,repeat,timeout):
    path=prepare(task,f'{runner}-{repeat}')
    env=os.environ.copy()
    for key in ('KROMA_MODEL','KROMA_BASE_URL','KROMA_API_KEY','KROMA_CONFIG_FILE',
                'KROMA_PROVIDER_PROFILE'):
        env.pop(key,None)
    if runner=='kroma':
        env['KROMA_WORKDIR']=str(path)
        env['KROMA_SESSION_FILE']=str(path/'session.json')
        env['KROMA_PROFILE']='1'
        cmd=[str(KROMA),TASKS[task]]
    else:
        for key in ('KROMA_WORKDIR','KROMA_SESSION_FILE','KROMA_PROFILE',
                    'KROMA_DEEPSEEK_THINKING'):
            env.pop(key,None)
        cmd=PI+['--no-session',TASKS[task]]
    started=time.monotonic()
    # Pi 1.3.0 can stall before the first JSON event when launched with a
    # detached process session and regular-file stdout. Use communicate() to
    # drain pipes continuously (including while model/tool work is ongoing).
    # Keep partial output from TimeoutExpired for stage diagnosis.
    out=path/'runner-stdout.log';err=path/'runner-stderr.log'
    # Critical for Pi --print: an inherited open stdin from an MCP execution
    # session can make Pi wait indefinitely for prompt input before JSON starts.
    proc=subprocess.Popen(cmd,cwd=path,env=env,stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    timed_out=False
    try:
        stdout,stderr=proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired as ex:
        timed_out=True
        proc.kill()
        stdout,stderr=proc.communicate(timeout=10)
    out.write_bytes(stdout);err.write_bytes(stderr)
    os.chmod(out,0o600);os.chmod(err,0o600)
    elapsed=round(time.monotonic()-started,3)
    summary={'runner':runner,'task':task,'repeat':repeat,'elapsed_s':elapsed,
             'exit_code':proc.returncode,'timeout':timed_out,'workdir':str(path),
             'stdout_bytes':out.stat().st_size,'stderr_bytes':err.stat().st_size}
    data=out.read_text(errors='replace')
    if runner=='pi':
        kinds={};msg=[];toolstart=0;toolend=0;first_ts=None
        for line in data.splitlines():
            try:e=json.loads(line)
            except (json.JSONDecodeError,ValueError):continue
            kind=e.get('type','?');kinds[kind]=kinds.get(kind,0)+1
            if first_ts is None:first_ts=kind
            if kind=='tool_execution_start':toolstart+=1
            if kind=='tool_execution_end':toolend+=1
            if kind=='message_end' and e.get('message',{}).get('role')=='assistant':msg.append(e['message'])
        summary['events']=kinds
        summary['requests']=len([x for x in msg if x.get('usage')])
        summary['tools_started']=toolstart;summary['tools_completed']=toolend
        for dst,key in [('input','input'),('cached','cacheRead'),('output','output')]:
            summary[dst]=sum(x.get('usage',{}).get(key,0) or 0 for x in msg)
        answer=''.join(item.get('text','') for m in msg for item in m.get('content',[]) if item.get('type')=='text')
    else:
        stderr=err.read_text(errors='replace')
        usage=re.search(r'\[usage\] task: (\d+) request\(s\).*?input: (\d+|n/a).*?output: (\d+|n/a).*?cached: (\d+|n/a)',stderr)
        summary['requests']=int(usage.group(1)) if usage else None
        if usage:
            for dst,index in [('input',2),('output',3),('cached',4)]:
                summary[dst]=int(usage.group(index)) if usage.group(index)!='n/a' else None
        # Per-request telemetry also logs returned tools. Only the final
        # wall_ms profile line carries the *task-total* tool count.
        profile=re.search(r'\[profile\] wall_ms=[^\n]*?\btool_calls=(\d+)',stderr)
        summary['tools_completed']=int(profile.group(1)) if profile else None
        answer=data
    if task=='calibrate':
        functional_complete='CALIBRATE_P0_OK' in answer
    else:
        # A timed-out agent may have already written a correct solution, but
        # failed to finish its agent loop. Record these separately.
        functional_complete=verify(task,path)
    passed=not timed_out and proc.returncode==0 and functional_complete
    summary['functional_complete']=bool(functional_complete)
    summary['passed']=bool(passed)
    summary['outcome']=('pass' if passed else 'no_events' if runner=='pi' and not summary.get('events')
                       else 'timeout_functional' if timed_out and functional_complete
                       else 'timeout' if timed_out else 'failed_acceptance')
    return summary


def main():
    ap=argparse.ArgumentParser()
    ap.add_argument('--phase',choices=('p0','p1'),required=True)
    ap.add_argument('--repeat',type=int,default=2)
    ap.add_argument('--timeout',type=int,default=90)
    args=ap.parse_args()
    if not (1<=args.repeat<=3):ap.error('repeat must be 1..3')
    if not SOURCE.is_dir() and args.phase=='p1':ap.error('external project source missing')
    tasks=['calibrate'] if args.phase=='p0' else ['bug','feature']
    rows=[]
    for task in tasks:
        for repeat in range(1,args.repeat+1):
            for runner in ('pi','kroma') if repeat%2 else ('kroma','pi'):
                print('START',task,repeat,runner,flush=True)
                try:r=execute(runner,task,repeat,args.timeout)
                except Exception as e:r={'runner':runner,'task':task,'repeat':repeat,
                                        'passed':False,'outcome':'harness_exception','error_type':type(e).__name__}
                rows.append(r)
                print('END',task,repeat,runner,r.get('outcome'),'seconds',r.get('elapsed_s'),flush=True)
                REPORT.write_text(json.dumps({'phase':args.phase,'source_sha':
                    subprocess.check_output(['git','-C',str(SOURCE),'rev-parse','HEAD'],text=True).strip() if SOURCE.is_dir() else None,
                    'cases':rows},indent=2))
    print('SUMMARY',json.dumps([{'runner':runner,'tested':sum(x['runner']==runner for x in rows),
        'passed':sum(x['runner']==runner and x.get('passed',False) for x in rows)} for runner in ('pi','kroma')]))
    print('REPORT',REPORT)

if __name__=='__main__':main()
