#!/usr/bin/env python3
"""Post-P4 Kroma versus Pi, 2 independent OSS engineering tasks x 2 repeats.

No whole-agent timeouts, no token quota, no direct access to API credentials.
"""
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

ROOT=Path(__file__).resolve().parents[1]
HERE=Path('/tmp/kroma-p4-pi-compare')
REPORT=HERE/'results.json'
spec=importlib.util.spec_from_file_location('benchp1',ROOT/'benchmarks/p0p1_fair.py')
p1=importlib.util.module_from_spec(spec)
spec.loader.exec_module(p1)
KROMA=ROOT/'target/release/kroma'
PI=p1.PI

def save(d):
    tmp=REPORT.with_suffix('.tmp')
    tmp.write_text(json.dumps(d,ensure_ascii=False,indent=2)+'\n')
    tmp.replace(REPORT)

def run(which,task,repeat):
    ident=f'{task}-{repeat}-{which}'
    destination=HERE/ident
    if destination.exists():
        raise RuntimeError('Refusing to overwrite existing trial')
    destination.mkdir(parents=True,mode=0o700)
    source=p1.prepare(task,f'post-p4-{which}-{repeat}')
    work=destination/'source'
    shutil.move(str(source),work)
    env=os.environ.copy()
    for name in ('KROMA_MODEL','KROMA_API_KEY','KROMA_BASE_URL',
                 'KROMA_CONFIG_FILE','KROMA_PROVIDER_PROFILE','KROMA_DEEPSEEK_THINKING'):
        env.pop(name,None)
    if which=='pi':
        command=PI+['--no-session',p1.TASKS[task]]
        for name in ('KROMA_WORKDIR','KROMA_SESSION_FILE','KROMA_PROFILE'):
            env.pop(name,None)
    else:
        command=[str(KROMA),p1.TASKS[task]]
        env['KROMA_WORKDIR']=str(work)
        env['KROMA_SESSION_FILE']=str(destination/'session.json')
        env['KROMA_PROFILE']='1'
    out=destination/'stdout.log';err=destination/'stderr.log'
    print('P4_COMPARE_START',ident,flush=True)
    begin=time.monotonic()
    with out.open('wb') as o,err.open('wb') as e:
        os.chmod(out,0o600);os.chmod(err,0o600)
        p=subprocess.Popen(command,cwd=work,env=env,stdin=subprocess.DEVNULL,
                           stdout=o,stderr=e)
        code=p.wait() # NO task timeout or token quota
    elapsed=round(time.monotonic()-begin,3)
    acceptance=p1.verify(task,work)
    expected=p1.BUG_TEST if task=='bug' else p1.FEATURE_TEST
    test_file=work/'tests'/('test_p1_regression.py' if task=='bug' else 'test_p1_feature.py')
    acceptance=bool(acceptance and test_file.read_text()==expected)
    upstream=subprocess.run(
        ['python3','-m','pytest','-q','tests/test_urlutils.py' if task=='bug' else 'tests/test_strutils.py'],
        cwd=work,capture_output=True,text=True).returncode==0
    stats={'requests':0,'tool_calls':0,'cached_input':0,'uncached_input':0,
           'output_tokens':0,'usage_unknown':False}
    if which=='pi':
        for line in out.open(errors='replace'):
            try:e=json.loads(line)
            except ValueError:continue
            typ=e.get('type')
            if typ=='tool_execution_end':stats['tool_calls']+=1
            if typ=='message_end' and e.get('message',{}).get('role')=='assistant':
                usage=e['message'].get('usage')
                if usage:
                    stats['requests']+=1
                    stats['cached_input']+=usage.get('cacheRead',0) or 0
                    stats['uncached_input']+=usage.get('input',0) or 0
                    stats['output_tokens']+=usage.get('output',0) or 0
    else:
        s=err.read_text(errors='replace')
        stats['tool_calls']=len(re.findall(r'^\[profile\] tool_done',s,re.M))
        for line in s.splitlines():
            if not line.startswith('[usage] request:'):continue
            stats['requests']+=1
            values={}
            for key in ('input','cached','output'):
                m=re.search(r'\b'+key+r': (\d+|n/a)',line)
                values[key]=int(m.group(1)) if m and m.group(1)!='n/a' else None
            if None in values.values():stats['usage_unknown']=True
            else:
                stats['cached_input']+=values['cached']
                stats['uncached_input']+=values['input']-values['cached']
                stats['output_tokens']+=values['output']
        stats['model_timeouts']=s.count('[model] request timed out')
        stats['apec_errors']=s.count('[apec] summary failed')
    res={'runner':which,'task':task,'repeat':repeat,'elapsed_s':elapsed,
         'exit':code,'functional':acceptance,'upstream_pass':upstream,
         'pass':code==0 and acceptance and upstream,'metrics':stats,
         'artifact_dir':str(destination)}
    print('P4_COMPARE_END',ident,'pass',res['pass'],'seconds',elapsed,
          'requests',stats['requests'],'tools',stats['tool_calls'],flush=True)
    return res

def main():
    HERE.mkdir(parents=True,exist_ok=True)
    if REPORT.is_file():report=json.loads(REPORT.read_text())
    else:
        report={'meta':{'model':'same workbuddy/deepseek high',
                        'task_timeout':None,'token_quota':None,
                        'code':'post-P4 release build with Bash fallback and compact tool output'},
                'trials':[]}
    for repeat in (1,2):
        for task in (('bug','feature') if repeat==1 else ('feature','bug')):
            for which in (('pi','kroma') if (repeat+['bug','feature'].index(task))%2 else ('kroma','pi')):
                if any(x['task']==task and x['repeat']==repeat and x['runner']==which for x in report['trials']):
                    continue
                report['trials'].append(run(which,task,repeat))
                save(report)
    for runner in ('pi','kroma'):
        cases=[x for x in report['trials'] if x['runner']==runner]
        print('P4_COMPARE_TOTAL',runner,'n',len(cases),
              'pass',sum(x['pass'] for x in cases),
              'seconds',round(sum(x['elapsed_s'] for x in cases),3),
              'requests',sum(x['metrics']['requests'] for x in cases),
              'tools',sum(x['metrics']['tool_calls'] for x in cases),
              'uncached',sum(x['metrics']['uncached_input'] for x in cases),
              'cached',sum(x['metrics']['cached_input'] for x in cases),flush=True)
    print('P4_COMPARE_REPORT',REPORT,flush=True)

if __name__=='__main__':main()
