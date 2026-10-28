#!/usr/bin/env python3
"""Randomized paired live DeepSeek High contest, isolated tasks and external verification."""
import argparse,importlib.util,json,os,pathlib,random,signal,statistics,subprocess,time,uuid
ROOT=pathlib.Path(__file__).resolve().parents[1]
def load(name,path):
 s=importlib.util.spec_from_file_location(name,ROOT/'benchmarks'/path);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
eng=load('eng','engineering_compare.py');suite=load('suite','agent_task_suite.py');usage=load('usage','tool_ablation_live.py')
PI=['pi','--offline','--no-extensions','--no-skills','--no-mcp','--no-context-files','--no-approve','--provider','workbuddy','--model','deepseek','--thinking','high','--mode','json','--print','--no-session']
KROMA=str((ROOT/'target/debug/examples/tool_ablation').resolve())
TASKS=('records','cross_file','rust_regression')
FROZEN_SEED=20261028
def prepare(task,root):
 root.mkdir(parents=True,exist_ok=True)
 if task=='rust_regression':eng.make_copy(root,'A',1);return eng.A_PROMPT+'\nWork only in this isolated checkout. Do not modify the injected regression test. Do not commit or access credentials.'
 item=next(x for x in suite.TASKS if x['id']=={'records':'implement_records','cross_file':'cross_file_fix'}[task]);suite.create_fixture(root,item)
 return item['prompt']+'\nWork only in this isolated directory. Do not modify the supplied tests or access credentials.'
def verify(task,root):
 try:
  if task=='rust_regression':
   assert 'fn wal_append_controls_recent_session_order()' in (root/'src/sessions.rs').read_text()
   return bool(eng.verify(root,'A',1))
  return bool(suite.verify(root,{'records':'implement_records','cross_file':'cross_file_fix'}[task],''))
 except Exception:return False
def invoke(lab,task,rep,runner,timeout):
 dest=lab/task/str(rep)/runner;work=dest/'source';prompt=prepare(task,work)
 env={k:v for k,v in os.environ.items() if not k.startswith('KROMA_')};env['CARGO_TARGET_DIR']=str(ROOT/'target');env['CARGO_INCREMENTAL']='0'
 if runner=='pi':cmd=PI+[prompt]
 else:
  env.update(KROMA_PROVIDER_PROFILE='workbuddy',KROMA_PROFILE='1',KROMA_WORKDIR=str(work),KROMA_SESSION_FILE=str(dest/'session.json'),KROMA_REASONING_EFFORT='high',KROMA_DEEPSEEK_THINKING='off')
  cmd=[KROMA,'aligned',prompt]
 begin=time.monotonic();p=subprocess.Popen(cmd,cwd=work,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
 timed_out=False
 try:o,e=p.communicate(timeout=timeout)
 except subprocess.TimeoutExpired:
  timed_out=True
  try:os.killpg(p.pid,signal.SIGKILL)
  except ProcessLookupError:pass
  o,e=p.communicate(timeout=10)
 elapsed=round(time.monotonic()-begin,3)
 dest.mkdir(parents=True,exist_ok=True)
 for name,data in [('stdout.log',o),('stderr.log',e)]:
  path=dest/name;path.write_bytes(data);path.chmod(0o600)
 metrics,_=usage.parse_pi(o.decode(errors='replace')) if runner=='pi' else (usage.parse_kroma(e.decode(errors='replace')),None)
 ok=verify(task,work)
 return dict(task=task,repeat=rep,runner=runner,elapsed_s=elapsed,passed=bool(ok and p.returncode==0 and not timed_out),verified=ok,exit=p.returncode,timeout=timed_out,metrics=metrics,logs=str(dest))
def main():
 p=argparse.ArgumentParser();p.add_argument('--repeats',type=int,default=3);p.add_argument('--timeout-python',type=int,default=80);p.add_argument('--timeout-rust',type=int,default=175);p.add_argument('--tasks',default=','.join(TASKS));a=p.parse_args()
 tasks=a.tasks.split(',');assert 1<=a.repeats<=8 and all(t in TASKS for t in tasks)
 lab=pathlib.Path('/tmp')/('kroma-pi-crossover-'+uuid.uuid4().hex[:8]);lab.mkdir(mode=0o700)
 rng=random.Random(FROZEN_SEED);pairs=[(t,i) for t in tasks for i in range(1,a.repeats+1)];rng.shuffle(pairs)
 plan=[{'task':t,'repeat':i,'order':rng.sample(['pi','kroma'],2)} for t,i in pairs]
 report={'seed':FROZEN_SEED,'model':'workbuddy/deepseek','thinking':'high','note':'Kroma forced reasoning_effort=high and thinking.type omitted; Pi native High','source_commit':eng.SOURCE_SHA,'plan':plan,'results':[]}
 (lab/'results.json').write_text(json.dumps(report,indent=2)+'\n');print('LAB',lab,'PLAN',plan,flush=True)
 for item in plan:
  for runner in item['order']:
   task=item['task'];rep=item['repeat'];print('START',task,rep,runner,flush=True)
   try:row=invoke(lab,task,rep,runner,a.timeout_rust if task=='rust_regression' else a.timeout_python)
   except Exception as x:row={'task':task,'repeat':rep,'runner':runner,'passed':False,'error':type(x).__name__+': '+str(x)[:150]}
   report['results'].append(row);tmp=lab/'results.tmp';tmp.write_text(json.dumps(report,indent=2)+'\n');tmp.replace(lab/'results.json')
   m=row.get('metrics',{});print('END',task,rep,runner,'pass',row['passed'],'sec',row.get('elapsed_s'),'in',m.get('input'),'cached',m.get('cached'),'output',m.get('output'),'requests',m.get('requests'),flush=True)
 print('REPORT',lab/'results.json',flush=True)
if __name__=='__main__':main()
