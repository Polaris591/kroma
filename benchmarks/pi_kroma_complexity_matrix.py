#!/usr/bin/env python3
"""Fresh complex Python vs different Rust feature, randomized paired Pi/Kroma.

No live checkout edits; each task uses independent hidden acceptance.
"""
from __future__ import annotations
import argparse, importlib.util, json, os, pathlib, random, signal, statistics, subprocess, sys, time, uuid

ROOT = pathlib.Path(__file__).resolve().parents[1]
def load_module(name, path):
    spec=importlib.util.spec_from_file_location(name, ROOT/'benchmarks'/path)
    mod=importlib.util.module_from_spec(spec);spec.loader.exec_module(mod);return mod
eng=load_module('eng_complexity', 'engineering_compare.py')
metrics=load_module('metrics_complexity', 'tool_ablation_live.py')
PI=['pi','--offline','--no-extensions','--no-skills','--no-mcp','--no-context-files','--no-approve',
    '--provider','workbuddy','--model','deepseek','--thinking','high','--mode','json','--print','--no-session']
KROMA=str((ROOT/'target/debug/examples/tool_ablation').resolve())
PY_PROMPT=(
    'Implement and verify a small real multi-module Python workqueue system. Existing '
    'tracker/models.py, storage.py, service.py and report.py contain bugs/TODOs. '
    'Required public APIs: models.normalize_title(text) returns strip+casefold nonempty '
    'key (raise ValueError for empty); models.normalize_tags(tags) returns sorted '
    'unique nonempty strip+casefold tags; storage.load(path) returns state with next_id, '
    'cursor, tasks; storage.save(path,state) persists JSON using atomic replacement; '
    'service.add_task(path,title,tags=()) creates record with integer incremental id, '
    'original stripped title, normalized unique tags, status="pending", assignee=None; '
    'duplicate normalized titles, including closed tasks, raise ValueError. '
    'service.claim_next(path,assignees) chooses oldest unassigned pending task, assigns '
    'normalized nonempty assignee from supplied ordered list, rotates across successful '
    'claims using persisted cursor; return claimed task, or None when none pending; empty '
    'assignees raises ValueError. service.complete(path,task_id) returns True only on '
    'first transition to done, False if missing/already done. report.open_report(path) '
    'returns all not-done tasks as {id,title,assignee,tags}, sorted ascending by id. '
    'Functions must work across new processes on the same JSON file. Fix tests in source '
    'only if they are demonstrably wrong; do not remove tests. Run tests/smoke checks. '
    'Work only in this isolated source directory; do not commit or access credentials.'
)

FILES={
    'tracker/__init__.py': '',
    'tracker/models.py': '''def normalize_title(text):\n    return str(text).lower()\n\ndef normalize_tags(tags):\n    return list(tags)\n''',
    'tracker/storage.py': '''import json\nfrom pathlib import Path\n\ndef load(path):\n    path=Path(path)\n    if path.exists():\n        return json.loads(path.read_text())\n    return {"next_id": 1, "cursor": 0, "tasks": []}\n\ndef save(path,state):\n    Path(path).write_text(json.dumps(state))\n''',
    'tracker/service.py': '''from . import models, storage\n\ndef add_task(path,title,tags=()):\n    state=storage.load(path)\n    task={"id": 1, "title": title, "tags": list(tags), "status": "pending", "assignee": None}\n    state["tasks"].append(task)\n    storage.save(path,state)\n    return task\n\ndef claim_next(path,assignees):\n    state=storage.load(path)\n    if not state["tasks"]: return None\n    task=state["tasks"][0]\n    task["assignee"]=next(iter(assignees), None)\n    storage.save(path,state)\n    return task\n\ndef complete(path, task_id):\n    return False\n''',
    'tracker/report.py': '''from . import storage\n\ndef open_report(path):\n    return storage.load(path)["tasks"]\n''',
    'tests/test_public.py': '''import tempfile\nimport unittest\nfrom pathlib import Path\nfrom tracker import service, report\n\nclass Smoke(unittest.TestCase):\n    def test_create_and_report(self):\n        with tempfile.TemporaryDirectory() as d:\n            path=Path(d)/"state.json"\n            task=service.add_task(path," First ",[" OPS "])\n            self.assertEqual(task["id"],1)\n            self.assertEqual(report.open_report(path)[0]["title"],"First")\n\nif __name__ == "__main__": unittest.main()\n''',
}

HIDDEN_CHECK = r'''import tempfile
from pathlib import Path
from tracker import models, storage, service, report
with tempfile.TemporaryDirectory() as d:
    path=Path(d)/'db.json'
    assert models.normalize_title('  Straße  ') == 'strasse'
    assert models.normalize_tags([' OPS ', 'ops', '  ', 'Erlebnis']) == ['erlebnis','ops']
    a=service.add_task(path,'  Straße  ',[' OPS ', 'ops'])
    b=service.add_task(path,'Other', [' X ', 'Y'])
    c=service.add_task(path,'Later', [])
    assert [a['id'],b['id'],c['id']] == [1,2,3]
    assert a['title']=='Straße' and a['tags']==['ops']
    try: service.add_task(path,'STRASSE')
    except ValueError: pass
    else: raise AssertionError('normalized duplicate permitted')
    x=service.claim_next(path,[' Alice ','Bob'])
    y=service.claim_next(path,[' Alice ','Bob'])
    z=service.claim_next(path,[' Alice ','Bob'])
    assert [x['id'],y['id'],z['id']]==[1,2,3]
    assert [x['assignee'],y['assignee'],z['assignee']]==['alice','bob','alice']
    assert service.claim_next(path,['Alice','Bob']) is None
    assert service.complete(path,2) is True
    assert service.complete(path,2) is False
    assert service.complete(path,2000) is False
    view=report.open_report(path)
    assert [r['id'] for r in view] == [1,3]
    assert all(sorted(r)==['assignee','id','tags','title'] for r in view)
    # Rotation semantics matter; the cursor may be stored modulo assignee count.
    assert isinstance(storage.load(path)['cursor'], int)
    assert storage.load(path)['next_id']==4
    assert storage.load(path)['tasks'][1]['status']=='done'
    try: service.claim_next(path,[])
    except ValueError: pass
    else: raise AssertionError('missing assignee error')
    try: service.add_task(path, '   ')
    except ValueError: pass
    else: raise AssertionError('empty title error')
print('HIDDEN_PASS')
'''

def prepare(task, root):
    root.mkdir(parents=True, exist_ok=True)
    if task=='rust_config':
        eng.make_copy(root,'B',1)
        return eng.B_PROMPT + ' Work only in this isolated checkout. There is no .git metadata here; do not commit.'
    for name, content in FILES.items():
        p=root/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(content)
    return PY_PROMPT

def verify(task, root, output_dir):
    if task=='rust_config':
        try:return bool(eng.verify(root,'B',1))
        except (Exception, SystemExit):return False
    p=output_dir/'hidden_check.py';p.write_text(HIDDEN_CHECK);p.chmod(0o600)
    env=dict(os.environ);env['PYTHONPATH']=str(root)
    try:
        result=subprocess.run([sys.executable,str(p)],cwd=root,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=18)
        return result.returncode==0 and b'HIDDEN_PASS' in result.stdout
    except (subprocess.TimeoutExpired, OSError):return False

def run(lab,task,rep,who,timeout):
    directory=lab/task/str(rep)/who;source=directory/'source';prompt=prepare(task,source)
    env={k:v for k,v in os.environ.items() if not k.startswith('KROMA_')}
    env.update(CARGO_TARGET_DIR=str(ROOT/'target'), CARGO_INCREMENTAL='0',TMPDIR='/tmp')
    if who=='pi':command=PI+[prompt]
    else:
        env.update(KROMA_PROVIDER_PROFILE='workbuddy',KROMA_PROFILE='1',KROMA_WORKDIR=str(source),
            KROMA_SESSION_FILE=str(directory/'session.json'), KROMA_REASONING_EFFORT='high',KROMA_DEEPSEEK_THINKING='off')
        command=[KROMA,'aligned',prompt]
    start=time.monotonic();proc=subprocess.Popen(command,cwd=source,env=env,stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    killed=False
    try:o,e=proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        killed=True
        try:os.killpg(proc.pid,signal.SIGKILL)
        except ProcessLookupError:pass
        o,e=proc.communicate(timeout=10)
    wall=round(time.monotonic()-start,3)
    for name,buf in (('stdout.log',o),('stderr.log',e)):
        dest=directory/name;dest.write_bytes(buf);dest.chmod(0o600)
    m,_=metrics.parse_pi(o.decode(errors='replace')) if who=='pi' else (metrics.parse_kroma(e.decode(errors='replace')),None)
    ok=verify(task,source,directory)
    enospc=(b'No space left on device' in o or b'No space left on device' in e)
    return dict(task=task,repeat=rep,runner=who,wall_s=wall,passed=bool(ok and proc.returncode==0 and not killed),
        verified=ok,exit_code=proc.returncode,timeout=killed,disk_error=enospc,metrics=m,logs=str(directory))

def main():
    p=argparse.ArgumentParser();p.add_argument('--repeats',type=int,default=2);p.add_argument('--python-timeout',type=int,default=160);p.add_argument('--rust-timeout',type=int,default=230);a=p.parse_args()
    assert a.repeats in (1,2,3)
    lab=pathlib.Path('/tmp')/('kroma-pi-complexity-'+uuid.uuid4().hex[:8]);lab.mkdir(mode=0o700)
    randomizer=random.Random(20261028)
    tasks=['python_complex','rust_config'];pairs=[(x,r) for x in tasks for r in range(1,a.repeats+1)]
    randomizer.shuffle(pairs)
    plan=[]
    for task,rep in pairs:
        # Explicit counterbalancing within each task type when repeats=2.
        order=['pi','kroma'] if rep%2 else ['kroma','pi']
        plan.append(dict(task=task,repeat=rep,order=order))
    report=dict(seed=20261028,model='workbuddy/deepseek',thinking='high',commit=eng.SOURCE_SHA,plan=plan,runs=[])
    (lab/'results.json').write_text(json.dumps(report,indent=2)+'\n')
    print('LAB',lab,'PLAN',plan,flush=True)
    for step in plan:
        for runner in step['order']:
            task,rep=step['task'],step['repeat'];print('START',task,rep,runner,flush=True)
            try:row=run(lab,task,rep,runner,a.rust_timeout if task=='rust_config' else a.python_timeout)
            except Exception as error:
                row=dict(task=task,repeat=rep,runner=runner,passed=False,error=type(error).__name__+': '+str(error)[:160])
            report['runs'].append(row);tmp=lab/'results.tmp';tmp.write_text(json.dumps(report,indent=2)+'\n');tmp.replace(lab/'results.json')
            m=row.get('metrics',{});print('END',task,rep,runner,'PASS',row.get('passed'), 'WALL',row.get('wall_s'),
                'IN',m.get('input'),'UNCACHED',m.get('uncached'),'REQUESTS',m.get('requests'),'DISK_ERR',row.get('disk_error'),flush=True)
    print('REPORT',lab/'results.json',flush=True)

if __name__=='__main__':main()
