#!/usr/bin/env python3
"""P1 continued-session engineering test in independent boltons copies.

Each runner implements one function, then receives a follow-up requirement in
the *same* saved conversation after a process restart. Tests are external.
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

SPEC=importlib.util.spec_from_file_location('fair',Path(__file__).with_name('p0p1_fair.py'))
fair=importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fair)
OUTPUT=Path('/tmp/kroma-p1-continuous-report.json')

PROMPTS=(
  'Stage 1/2: in this existing third-party Python project boltons, implement '
  'boltons.strutils.redact_ascii_digits(text, replacement="*"). It must return '
  'a str with ASCII digits 0-9 replaced by the single-character replacement, '
  'leave Unicode digits unchanged, and accept only str inputs. Check '
  'python3 -m pytest -q tests/test_p1_cont_stage1.py. Do not change the '
  'benchmark tests or commit. Next turn will continue this task.',
  'Stage 2/2: continue the existing implementation from the previous stage. '
  'Add optional keep_last=0 to redact_ascii_digits; when keep_last is positive, '
  'preserve that many ASCII digits at the END of the text (count only ASCII '
  'digits), masking earlier ASCII digits. Validate keep_last is a non-negative '
  'integer, reject bool. Document this new behavior in the module docstring '
  'and add regression tests to tests/test_strutils.py. Run both external '
  'test files tests/test_p1_cont_stage1.py and tests/test_p1_cont_stage2.py '
  'and ensure existing tests pass. Do not change benchmark tests or commit.'
)

TEST1='''from boltons.strutils import redact_ascii_digits
import pytest

def test_stage_one():
    assert redact_ascii_digits('A12-b3') == 'A**-b*'
    assert redact_ascii_digits('7\\u0664\\u06f5', '#') == '#\\u0664\\u06f5'
    assert redact_ascii_digits('text') == 'text'
    with pytest.raises(TypeError): redact_ascii_digits(123)
'''
TEST2='''from boltons.strutils import redact_ascii_digits
import pytest

def test_keep_last():
    assert redact_ascii_digits('12-34-56', '#', keep_last=2) == '##-##-56'
    assert redact_ascii_digits('12-34-56', '#', keep_last=99) == '12-34-56'
    assert redact_ascii_digits('12-34-56', '#', keep_last=0) == '##-##-##'
    assert redact_ascii_digits('abc', '#', keep_last=4) == 'abc'
    assert redact_ascii_digits('9\\u0664 8', '#', keep_last=1) == '#\\u0664 8'

@pytest.mark.parametrize('bad', [-1, 1.5, True, '2'])
def test_invalid_keep_last(bad):
    with pytest.raises((TypeError, ValueError)):
        redact_ascii_digits('12', keep_last=bad)
'''

def run(runner,repeat,timeout):
    base=Path(tempfile.mkdtemp(prefix=f'kroma-p1-cont-{runner}-{repeat}-'))
    work=base/'source'
    shutil.copytree(fair.SOURCE,work,ignore=shutil.ignore_patterns('.git','__pycache__','.pytest_cache'))
    (work/'tests/test_p1_cont_stage1.py').write_text(TEST1)
    (work/'tests/test_p1_cont_stage2.py').write_text(TEST2)
    session=base/'session.json'
    env=os.environ.copy()
    for key in ('KROMA_MODEL','KROMA_BASE_URL','KROMA_API_KEY',
                'KROMA_CONFIG_FILE','KROMA_PROVIDER_PROFILE'):
        env.pop(key,None)
    if runner=='kroma':
        env.update(KROMA_WORKDIR=str(work),KROMA_SESSION_FILE=str(session),KROMA_PROFILE='1')
    else:
        for key in ('KROMA_WORKDIR','KROMA_SESSION_FILE','KROMA_PROFILE',
                    'KROMA_DEEPSEEK_THINKING'):
            env.pop(key,None)
    steps=[]
    for stage,prompt in enumerate(PROMPTS,1):
        cmd=([str(fair.KROMA),prompt] if runner=='kroma'
             else fair.PI+['--session',str(session),prompt])
        started=time.monotonic()
        proc=subprocess.Popen(cmd,cwd=work,env=env,stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        timed_out=False
        try:out,err=proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out=True
            proc.kill();out,err=proc.communicate(timeout=8)
        elapsed=round(time.monotonic()-started,3)
        # The raw outputs stay private in the isolated temporary folder.
        op=base/f'stage{stage}-{runner}-stdout.log';ep=base/f'stage{stage}-{runner}-stderr.log'
        op.write_bytes(out);ep.write_bytes(err)
        os.chmod(op,0o600);os.chmod(ep,0o600)
        step={'stage':stage,'exit_code':proc.returncode,'timeout':timed_out,
              'wall_s':elapsed,'stdout_bytes':len(out),'stderr_bytes':len(err)}
        if runner=='pi':
            ev={};msgs=[]
            for line in out.decode(errors='replace').splitlines():
                try:e=json.loads(line)
                except (ValueError,TypeError):continue
                k=e.get('type');ev[k]=ev.get(k,0)+1
                if k=='message_end' and e.get('message',{}).get('role')=='assistant':
                    msgs.append(e['message'])
            step.update(event_counts=ev,requests=len([m for m in msgs if m.get('usage')]),
                        tools_started=ev.get('tool_execution_start',0),
                        tools_finished=ev.get('tool_execution_end',0))
        else:
            import re
            stderr=err.decode(errors='replace')
            match=re.search(r'\[usage\] task: (\d+) request\(s\)',stderr)
            step['requests']=int(match.group(1)) if match else None
        steps.append(step)
        print('STAGE',runner,repeat,stage,'exit',proc.returncode,
              'timeout',timed_out,'seconds',elapsed,flush=True)
        if proc.returncode!=0:break
    intact=((work/'tests/test_p1_cont_stage1.py').read_text()==TEST1 and
            (work/'tests/test_p1_cont_stage2.py').read_text()==TEST2)
    checks={}
    for key,filename in [('stage1','test_p1_cont_stage1.py'),
                         ('stage2','test_p1_cont_stage2.py'),
                         ('upstream','test_strutils.py')]:
        p=subprocess.run(['python3','-m','pytest','-q','tests/'+filename],
                         cwd=work,capture_output=True,text=True,timeout=35)
        checks[key]={'passed':p.returncode==0,
                     'summary':p.stdout.strip().splitlines()[-1] if p.stdout.strip() else ''}
    complete=all(v['passed'] for v in checks.values()) and intact
    return {'runner':runner,'repeat':repeat,'workdir':str(work),
            'steps':steps,'checks':checks,'integrity':intact,
            'functional_complete':complete,
            'finished_on_time':complete and len(steps)==2 and all(not x['timeout'] and x['exit_code']==0 for x in steps),
            'total_wall_s':round(sum(x['wall_s'] for x in steps),3)}


def main():
    import argparse
    p=argparse.ArgumentParser();p.add_argument('--repeat',type=int,default=2)
    p.add_argument('--timeout',type=int,default=75);a=p.parse_args()
    rows=[]
    for repeat in range(1,a.repeat+1):
        for runner in ('pi','kroma') if repeat%2 else ('kroma','pi'):
            print('CONT_START',repeat,runner,flush=True)
            r=run(runner,repeat,a.timeout)
            rows.append(r)
            OUTPUT.write_text(json.dumps({'cases':rows},indent=2))
            print('CONT_END',repeat,runner,'functional',r['functional_complete'],
                  'complete_on_time',r['finished_on_time'],'wall_s',r['total_wall_s'],flush=True)

if __name__=='__main__':main()
