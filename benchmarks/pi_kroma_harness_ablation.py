#!/usr/bin/env python3
"""Live DeepSeek High ablation of payload parity, scoped search, batching, cache prefix."""
import argparse, importlib.util, json, os, pathlib, re, signal, subprocess, time, uuid
ROOT=pathlib.Path(__file__).resolve().parents[1]
LAB=pathlib.Path('/tmp')/('kroma-harness-ablation-'+uuid.uuid4().hex[:8])
spec=importlib.util.spec_from_file_location('eng',ROOT/'benchmarks/engineering_compare.py')
eng=importlib.util.module_from_spec(spec);spec.loader.exec_module(eng)
RUNNER=str((ROOT/'target/debug/examples/tool_ablation').resolve())
ARMS=['shell','aligned','scoped','batch','cache_prefix']
def go(arm,repeat,timeout):
    work=LAB/f'repeat-{repeat}'/arm/'source';eng.make_copy(work,'A',repeat)
    prompt=eng.A_PROMPT+'\nWork only in this isolated checkout. Do not commit or access real credentials. Do not change the injected regression test.'
    env={k:v for k,v in os.environ.items() if not k.startswith('KROMA_')}
    env.update(KROMA_PROVIDER_PROFILE='workbuddy',KROMA_PROFILE='1',KROMA_WORKDIR=str(work),KROMA_SESSION_FILE=str(work.parent/'session.json'),CARGO_TARGET_DIR=str(ROOT/'target'),CARGO_INCREMENTAL='0')
    cmd=[RUNNER,arm,prompt]
    start=time.monotonic();p=subprocess.Popen(cmd,cwd=work,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    timeout_hit=False
    try:out,err=p.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timeout_hit=True
        try:os.killpg(p.pid,signal.SIGKILL)
        except ProcessLookupError:pass
        out,err=p.communicate(timeout=10)
    wall=round(time.monotonic()-start,3)
    (work.parent/'stdout.log').write_bytes(out);(work.parent/'stderr.log').write_bytes(err)
    for name in ['stdout.log','stderr.log']:(work.parent/name).chmod(0o600)
    e=err.decode(errors='replace');metrics={'requests':0,'input':0,'cached':0,'output':0,'tools':0,'batch_turns':0,'max_tools_per_turn':0,'model_ms':None,'tool_ms':None,'trc':0,'apec':0}
    for line in e.splitlines():
        if line.startswith('[usage] request:'):
            metrics['requests']+=1
            for f in ('input','cached','output'):
                m=re.search(r'\b'+f+r':\s*(\d+)',line)
                if m:metrics[f]+=int(m.group(1))
        if line.startswith('[profile] tool_done'):metrics['tools']+=1
        if line.startswith('[profile] model_done'):
            m=re.search(r'\breturned_tools=(\d+)',line)
            if m:
                n=int(m.group(1));metrics['max_tools_per_turn']=max(metrics['max_tools_per_turn'],n)
                metrics['batch_turns']+=int(n>1)
        if line.startswith('[profile] wall_ms='):
            for field,target in [('model_ms','model_ms'),('shell_ms','tool_ms')]:
                m=re.search(r'\b'+field+r'=([\d.]+)',line)
                if m:metrics[target]=float(m.group(1))
        if line.startswith('[trc]'):metrics['trc']+=1
        if line.startswith('[apec]'):metrics['apec']+=1
    metrics['uncached']=metrics['input']-metrics['cached']
    try:verified=bool(eng.verify(work,'A',repeat))
    except Exception as x:verified=False
    row=dict(arm=arm,repeat=repeat,wall_s=wall,exit=p.returncode,timeout=timeout_hit,verified=verified,passed=verified and p.returncode==0 and not timeout_hit,metrics=metrics,logdir=str(work.parent))
    print('END',repeat,arm,'pass',row['passed'],'s',wall,'in',metrics['input'],'cached',metrics['cached'],'calls',metrics['requests'],'tools',metrics['tools'],'batches',metrics['batch_turns'],flush=True)
    return row

def main():
    ap=argparse.ArgumentParser();ap.add_argument('--repeats',type=int,default=1);ap.add_argument('--timeout',type=int,default=195);ap.add_argument('--arms',default=','.join(ARMS));ap.add_argument('--task',choices=['A','B'],default='A');a=ap.parse_args()
    arms=a.arms.split(',');assert all(s in ARMS for s in arms)
    LAB.mkdir(mode=0o700);r={'model':'workbuddy/deepseek','thinking':'high','source_commit':eng.SOURCE_SHA,'arms':arms,'task':a.task,'results':[]}
    print('LAB',LAB,flush=True)
    for repeat in range(1,a.repeats+1):
        order=arms[(repeat-1)%len(arms):]+arms[:(repeat-1)%len(arms)]
        for arm in order:
            print('START',repeat,arm,flush=True)
            try:row=go(arm,repeat,a.timeout)
            except Exception as e:
                row={'arm':arm,'repeat':repeat,'passed':False,'error':type(e).__name__+': '+str(e)[:200]}
                print('ERROR',repeat,arm,row['error'],flush=True)
            r['results'].append(row)
            (LAB/'results.json').write_text(json.dumps(r,indent=2)+'\n')
    print('REPORT',LAB/'results.json',flush=True)
if __name__=='__main__':main()
