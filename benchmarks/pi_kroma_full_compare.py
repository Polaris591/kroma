#!/usr/bin/env python3
"""Six matched live Pi/Kroma cases. Sources stay in isolated /tmp dirs."""
import argparse, importlib.util, json, os, pathlib, re, subprocess, sys, time, uuid
ROOT=pathlib.Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('suite',ROOT/'benchmarks/agent_task_suite.py')
suite=importlib.util.module_from_spec(spec);spec.loader.exec_module(suite)
PI=['pi','--offline','--no-extensions','--no-skills','--no-mcp','--no-context-files','--no-approve','--provider','workbuddy','--model','deepseek','--thinking','high','--mode','json','--print','--no-session']
KROMA=str((ROOT/'target/debug/kroma').resolve())
LAB=pathlib.Path('/tmp/kroma-pi-20261028-'+uuid.uuid4().hex[:8]); LAB.mkdir(mode=0o700)
REPORT=LAB/'results.json'; rows=[]

def usage_pi(raw):
    out={'requests':0,'input':0,'uncached':0,'cached':0,'output':0,'cache_write':0,'unknown':False,'tools':0}
    for line in raw.splitlines():
        try:e=json.loads(line)
        except (ValueError,TypeError):continue
        if e.get('type')=='tool_execution_end':out['tools']+=1
        if e.get('type')=='message_end' and e.get('message',{}).get('role')=='assistant':
            u=e['message'].get('usage') or {}
            if not u:out['unknown']=True;continue
            out['requests']+=1
            for dst,key in [('uncached','input'),('cached','cacheRead'),('cache_write','cacheWrite'),('output','output')]:
                v=u.get(key)
                if v is None:out['unknown']=True
                else:out[dst]+=v
    out['input']=out['uncached']+out['cached']+out['cache_write']
    return out

def usage_kroma(raw):
    result={'requests':0,'input':0,'uncached':0,'cached':0,'output':0,'cache_write':None,'unknown':False,'tools':0,'apec_requests':0}
    for line in raw.splitlines():
        if '[usage] request:' in line:
            result['requests']+=1
            for field in ('input','output','cached'):
                match=re.search(r'\b'+field+r':\s*(\d+|n/a)',line)
                if not match or match.group(1)=='n/a':result['unknown']=True
                else:result[field]+=int(match.group(1))
        if '[usage] APEC:' in line:
            m=re.search(r'APEC:\s*(\d+) request',line)
            if m:result['apec_requests']+=int(m.group(1))
    m=re.search(r'\[profile\] wall_ms=[^\n]*?\btool_calls=(\d+)',raw)
    if m:result['tools']=int(m.group(1))
    result['uncached']=result['input']-result['cached']
    return result

def run(task,which,timeout):
    dest=LAB/task['id']/which; dest.mkdir(parents=True,mode=0o700)
    suite.create_fixture(dest,task)
    env={k:v for k,v in os.environ.items() if not k.startswith('KROMA_')}
    if which=='pi':
        cmd=PI+[task['prompt']]
    else:
        env.update(KROMA_WORKDIR=str(dest),KROMA_SESSION_FILE=str(dest/'conversation.json'),KROMA_PROVIDER_PROFILE='workbuddy',KROMA_PROFILE='1')
        cmd=[KROMA,task['prompt']]
    print('START',task['id'],which,flush=True)
    begin=time.monotonic();timed_out=False
    proc=subprocess.Popen(cmd,cwd=dest,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    try:stdout,stderr=proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out=True
        import signal
        try:os.killpg(proc.pid,signal.SIGKILL)
        except ProcessLookupError:pass
        stdout,stderr=proc.communicate(timeout=8)
    elapsed=round(time.monotonic()-begin,3)
    (dest/'stdout.log').write_bytes(stdout);(dest/'stderr.log').write_bytes(stderr)
    os.chmod(dest/'stdout.log',0o600);os.chmod(dest/'stderr.log',0o600)
    s=stdout.decode(errors='replace');err=stderr.decode(errors='replace')
    if which=='pi':
        answer='\n'.join(part.get('text','') for line in s.splitlines() for e in [parse_line(line)] if e.get('type')=='message_end' and e.get('message',{}).get('role')=='assistant' for part in e['message'].get('content',[]) if part.get('type')=='text')
        metrics=usage_pi(s)
    else:answer=s;metrics=usage_kroma(err)
    ok=suite.verify(dest,task['id'],answer)
    row={'task':task['id'],'runner':which,'elapsed_s':elapsed,'passed':bool(ok and proc.returncode==0 and not timed_out),'verified':bool(ok),'returncode':proc.returncode,'timeout':timed_out,'metrics':metrics,'directory':str(dest)}
    print('END',task['id'],which,'passed',row['passed'],'wall',elapsed,'tokens',metrics['input'],metrics['output'],'cached',metrics['cached'],flush=True)
    return row

def parse_line(line):
    try:return json.loads(line)
    except (ValueError,TypeError):return {}

def main():
    ap=argparse.ArgumentParser();ap.add_argument('--timeout',type=int,default=150);ap.add_argument('--limit',type=int,default=6);a=ap.parse_args()
    for i,task in enumerate(suite.TASKS[:a.limit]):
        order=['pi','kroma'] if i%2==0 else ['kroma','pi']
        for runner in order:
            try:r=run(task,runner,a.timeout)
            except Exception as e:
                r={'task':task['id'],'runner':runner,'passed':False,'error':f'{type(e).__name__}: {e}'}
                print('ERROR',r['task'],runner,r['error'],flush=True)
            rows.append(r);REPORT.write_text(json.dumps({'model':'workbuddy/deepseek','thinking':'high','cases':rows},ensure_ascii=False,indent=2)+'\n')
    print('REPORT',REPORT,flush=True)
    for runner in ['pi','kroma']:
        group=[r for r in rows if r['runner']==runner]
        print('TOTAL',runner,'passed',sum(r.get('passed',False) for r in group),'n',len(group),'wall_s',round(sum(r.get('elapsed_s',0) for r in group),2), 'input',sum(r.get('metrics',{}).get('input',0) for r in group),'cached',sum(r.get('metrics',{}).get('cached',0) for r in group),'output',sum(r.get('metrics',{}).get('output',0) for r in group),flush=True)

if __name__=='__main__':main()
