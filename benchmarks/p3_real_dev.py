#!/usr/bin/env python3
"""P3: unlimited-duration real Rust development, Kroma against Pi.

The benchmark does not impose token quotas or elapsed-time cutoffs. Each
step runs in its own process and resumes a persistent conversation. It uses
only copies of the current source tree, never the live project checkout.

Each external acceptance test uses dummy credentials in a private temp file.
The real provider key is never loaded, inspected, or printed by this script.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
P3 = Path('/tmp/kroma-p3-realdev')
RUNNERS = ('pi', 'kroma')
KROMA_BIN = ROOT / 'target/release/kroma'
CARGO_TARGET = ROOT / 'target'
FAKE_KEY = 'P3_DUMMY_CREDENTIAL_NO_REAL_SECRET_7R4X'
PI_CMD = ['pi', '--offline', '--no-extensions', '--no-skills', '--no-mcp',
          '--no-context-files', '--no-approve', '--provider', 'workbuddy',
          '--model', 'deepseek', '--thinking', 'high', '--mode', 'json', '--print']

GUARD = (
    'You work ONLY in the current isolated copy of the Rust Kroma codebase. '
    'Do not access real API credentials, change ~/.kroma, modify /root/kroma-new, '
    'use network operations, or make Git commits. The Kroma model configuration '
    'is already set up in the runner; you must not change it. '
    'Focus on the implementation and tests; do not cut corners on correctness. '
    'All requirements for this stage must be met. '
    'All three tasks form one continuous project, and your process will be '
    'restarted after each stage while retaining its conversation.'
)

STAGES = [
  ('remove', '''Stage 1: Implement a real configuration-management feature:

`kroma config remove <name>` deletes exactly one saved provider profile.
Reject unknown names or invalid names without touching the saved config file.
For a non-active removal preserve the active profile. For removal of the
active profile, choose the first remaining name in sorted order as the new
active profile, or no active profile when the map becomes empty. Preserve
other profiles including optional API keys and thinking mode. Keep private
file permissions and existing atomic save behavior. Update the root CLI help,
config usage, README, and include Rust tests for success and failure cases.
Use `cargo test --locked --offline` and relevant CLI smoke tests. Do not
edit unrelated features. Finish with a short verified status.'''),
  ('rename', '''Stage 2: Continue the previous work in this SAME project:

Implement `kroma config rename <old> <new>` to rename one saved profile.
It must preserve its complete configuration including the API key and
thinking mode, and update the active profile if it was renamed. Reject
missing source, invalid new name, and an existing destination without
changing config bytes. Renaming a profile to its own identical name is a
safe no-op. Keep private atomic saving. Update CLI help, config usage,
README, and add focused Rust tests. Re-test the remove behavior from stage
1 and ensure `cargo test --locked --offline` passes. Do not commit.'''),
  ('json', '''Stage 3: Continue the same code, implementing machine-readable
outputs for the configuration commands:

`kroma config show --json` prints ONE valid JSON object with exactly these
fields: model (string), host (string), thinking (string), key_configured
(boolean). Respect model/thinking/environment overrides just as regular
`config show` does. Never expose the API key. `kroma config list --json`
prints ONE JSON array, sorted by profile name; each item must have name,
active (boolean), model, host, thinking, key_configured (boolean). Preserve
the behavior and output of existing human-readable commands. Update CLI help
and README. Add Rust tests for JSON correctness and key secrecy, and re-run
`cargo test --locked --offline`. Do not commit.'''),
  ('finish', '''Stage 4: Final integrated engineering review. This stage is
not a new feature request; make the three previous features production-ready.
Review failure atomicity, profile selection, JSON correctness, permission
preservation, credential secrecy, CLI ergonomics and docs. Run
`cargo fmt --check`, `cargo clippy --locked --offline --all-targets -- -D warnings`,
and `cargo test --locked --offline`. Fix real problems, add any missing focused
unit tests, and report evidence. Avoid unrelated changes or Git commits.'''),
]


def source_files():
    paths = [ROOT/'Cargo.toml', ROOT/'Cargo.lock', ROOT/'README.md', ROOT/'.env.example']
    paths.extend(sorted((ROOT/'src').rglob('*')))
    paths.append(ROOT/'.cargo/config.toml')
    return [p for p in paths if p.is_file()]


def snapshot_digest(paths,base=ROOT):
    digest = hashlib.sha256()
    for path in paths:
        rel = path.relative_to(base)
        digest.update(str(rel).encode())
        digest.update(b'\0')
        digest.update(path.read_bytes())
        digest.update(b'\0')
    return digest.hexdigest()


def init():
    if P3.exists():
        raise RuntimeError(f'{P3} already exists; refusing to overwrite experiment')
    P3.mkdir(mode=0o700)
    paths=source_files()
    hash_value = snapshot_digest(paths)
    for who in RUNNERS:
        dest=P3/who/'source'
        dest.mkdir(parents=True,mode=0o700)
        for path in paths:
            target=dest/path.relative_to(ROOT)
            target.parent.mkdir(parents=True,exist_ok=True)
            shutil.copy2(path,target)
    meta={'source':str(ROOT),'source_sha':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
          'source_tree_sha256':hash_value,'source_files':len(paths),
          'tasks':[name for name,_ in STAGES],
          'test_method':'same source, model and requirements; serial alternating stage order; independent project copies',
          'time_limit':None,'token_limit':None}
    (P3/'metadata.json').write_text(json.dumps(meta,indent=2,ensure_ascii=False))
    (P3/'results.json').write_text(json.dumps({'metadata':meta,'runs':[]},indent=2))
    print('P3_INIT',hash_value,'files',len(paths),'root',P3,flush=True)
    for who in RUNNERS:
        assert snapshot_digest([P3/who/'source'/p.relative_to(ROOT) for p in paths], P3/who/'source') == hash_value, 'source copies differ'


def run_command(args,cwd,env=None):
    t=time.monotonic()
    p=subprocess.run(args,cwd=cwd,env=env,capture_output=True,text=True)
    # No timeout: this evaluates a real task without elapsed-time caps.
    return {'exit':p.returncode,'wall_s':round(time.monotonic()-t,3),
            'stdout':p.stdout,'stderr':p.stderr}


def fixture(path,profiles=None,active='alpha'):
    data={'active':active,'profiles':profiles or {
        'alpha':{'base_url':'https://alpha.example/v1','model':'alpha-model',
                 'api_key':FAKE_KEY,'deepseek_thinking':'high'},
        'beta':{'base_url':'https://beta.example/v1','model':'beta-model',
                'deepseek_thinking':'enabled'},
        'charlie':{'base_url':'https://charlie.example/v1','model':'charlie-model'},
    }}
    path.write_text(json.dumps(data,indent=2)+'\n')
    path.chmod(0o600)
    return data


def acceptance(who,stage_index):
    work=P3/who/'source'
    validation=P3/who/'validation'/f'stage-{stage_index+1}'
    validation.mkdir(parents=True,exist_ok=True)
    env=os.environ.copy()
    for k in ('KROMA_CONFIG_FILE','KROMA_API_KEY','KROMA_MODEL',
              'KROMA_BASE_URL','KROMA_PROVIDER_PROFILE','KROMA_DEEPSEEK_THINKING'):
        env.pop(k,None)
    env['CARGO_TARGET_DIR']=str(CARGO_TARGET)
    results={}
    build=run_command(['cargo','build','--locked','--offline','--quiet'],work,env)
    results['cargo_build']=build['exit']==0
    if not results['cargo_build']:
        results['build_error_type']='compile_failed'
        return results
    binary=CARGO_TARGET/'debug/kroma'

    def call(name,*args,extra=None):
        e=env.copy()
        e['KROMA_CONFIG_FILE']=str(validation/name)
        if extra:e.update(extra)
        return run_command([str(binary),'config',*args],work,e)

    if stage_index >= 0:
        file=validation/'remove_active.json';before=fixture(file)
        r=call(file.name,'remove','alpha')
        after=json.loads(file.read_text())
        results['remove_active']=r['exit']==0 and after.get('active')=='beta' and list(after['profiles'])==['beta','charlie'] and after['profiles']['beta']==before['profiles']['beta']
        results['remove_private_perms']=stat.S_IMODE(file.stat().st_mode)==0o600
        fresh=validation/'remove_nonactive.json';before=fixture(fresh)
        r=call(fresh.name,'remove','charlie');after=json.loads(fresh.read_text())
        results['remove_nonactive']=r['exit']==0 and after.get('active')=='alpha' and 'charlie' not in after['profiles'] and after['profiles']['alpha']==before['profiles']['alpha']
        fresh=validation/'remove_missing.json';fixture(fresh)
        raw=fresh.read_bytes();r=call(fresh.name,'remove','absent')
        results['remove_missing_atomic']=r['exit']!=0 and raw==fresh.read_bytes()
        fresh=validation/'remove_all.json';fixture(fresh,profiles={'alpha':before['profiles']['alpha']})
        r=call(fresh.name,'remove','alpha');after=json.loads(fresh.read_text())
        results['remove_last']=r['exit']==0 and not after['profiles'] and after['active'] is None
        results['remove_secret_not_echoed']=FAKE_KEY not in (r['stdout']+r['stderr'])
    if stage_index >= 1:
        fresh=validation/'rename_active.json';before=fixture(fresh)
        r=call(fresh.name,'rename','alpha','delta');after=json.loads(fresh.read_text())
        results['rename_active']=r['exit']==0 and after['active']=='delta' and 'alpha' not in after['profiles'] and after['profiles']['delta']==before['profiles']['alpha']
        results['rename_private_perms']=stat.S_IMODE(fresh.stat().st_mode)==0o600
        fresh=validation/'rename_nonactive.json';before=fixture(fresh)
        r=call(fresh.name,'rename','beta','delta');after=json.loads(fresh.read_text())
        results['rename_nonactive']=r['exit']==0 and after['active']=='alpha' and after['profiles']['delta']==before['profiles']['beta']
        for key,args in (('rename_missing',('missing','delta')),
                         ('rename_collision',('alpha','beta')),
                         ('rename_invalid',('alpha','bad/name'))):
            fresh=validation/(key+'.json');fixture(fresh)
            raw=fresh.read_bytes();r=call(fresh.name,'rename',*args)
            results[key+'_atomic']=r['exit']!=0 and raw==fresh.read_bytes()
        fresh=validation/'rename_self.json';fixture(fresh)
        raw=fresh.read_bytes();r=call(fresh.name,'rename','alpha','alpha')
        results['rename_self']=r['exit']==0 and raw==fresh.read_bytes()
    if stage_index >= 2:
        fresh=validation/'json.json';fixture(fresh)
        r=call(fresh.name,'list','--json')
        try:
            items=json.loads(r['stdout'])
            results['json_list']=r['exit']==0 and isinstance(items,list) and [x['name'] for x in items]==['alpha','beta','charlie']
            results['json_flags']=items[0]['active'] is True and items[0]['key_configured'] is True and items[1]['active'] is False and items[1]['key_configured'] is False and items[0]['host']=='alpha.example' and items[1]['thinking']=='enabled'
        except (ValueError,KeyError,TypeError):
            results['json_list']=False;results['json_flags']=False
        results['json_list_secret_safe']=FAKE_KEY not in (r['stdout']+r['stderr'])
        r=call(fresh.name,'show','--json',extra={'KROMA_MODEL':'override-model',
                      'KROMA_DEEPSEEK_THINKING':'high'})
        try:
            item=json.loads(r['stdout'])
            results['json_show']=r['exit']==0 and item['model']=='override-model' and item['host']=='alpha.example' and item['thinking']=='high' and item['key_configured'] is True
        except (ValueError,KeyError,TypeError):results['json_show']=False
        results['json_show_secret_safe']=FAKE_KEY not in (r['stdout']+r['stderr'])
        results['text_output_works']=call(fresh.name,'list')['exit']==0 and call(fresh.name,'show')['exit']==0
    test=run_command(['cargo','test','--locked','--offline','--quiet'],work,env)
    results['cargo_test']=test['exit']==0
    if stage_index==3:
        fmt=run_command(['cargo','fmt','--check'],work,env)
        clippy=run_command(['cargo','clippy','--locked','--offline','--all-targets','--','-D','warnings'],work,env)
        results['cargo_fmt']=fmt['exit']==0
        results['cargo_clippy']=clippy['exit']==0
    if stage_index>=1:
        helptext=(work/'src/main.rs').read_text()
        readme=(work/'README.md').read_text()
        results['help_documentation']=all(x in helptext and x in readme for x in ('config remove','config rename'))
    if stage_index>=2:
        helptext=(work/'src/main.rs').read_text()
        readme=(work/'README.md').read_text()
        results['json_documentation']='--json' in helptext and '--json' in readme
    return results


def usage_for(who,log_dir):
    stdout=(log_dir/'stdout.jsonl').read_text(errors='replace')
    stderr=(log_dir/'stderr.log').read_text(errors='replace')
    if who=='pi':
        messages=[];types={};tools_started=0;tools_done=0
        for line in stdout.splitlines():
            try:ev=json.loads(line)
            except json.JSONDecodeError:continue
            typ=ev.get('type','unknown');types[typ]=types.get(typ,0)+1
            if typ=='message_end' and ev.get('message',{}).get('role')=='assistant':
                messages.append(ev['message'])
            if typ=='tool_execution_start':tools_started+=1
            if typ=='tool_execution_end':tools_done+=1
        stats={'complete_model_requests':len([m for m in messages if m.get('usage')]),
               'tool_starts':tools_started,'tool_finishes':tools_done,
               'event_types':types}
        for key,dest in [('input','input'),('cacheRead','cached'),('output','output')]:
            stats[dest]=sum((m.get('usage',{}).get(key,0) or 0) for m in messages)
        stats['uncached_input']=stats['input']
        stats['total_prompt']=stats['input']+stats['cached']
        return stats
    matched=re.search(r'^\[usage\] task: (\d+) request\(s\),[^\n]+?input: (\d+|n/a) \| output: (\d+|n/a) \| cached: (\d+|n/a)',stderr,re.M)
    stats={'complete_model_requests':None,'tool_finishes':None}
    if matched:
        stats['complete_model_requests']=int(matched.group(1))
        for dest,i in [('total_prompt',2),('output',3),('cached',4)]:
            stats[dest]=int(matched.group(i)) if matched.group(i)!='n/a' else None
        stats['uncached_input']=(stats['total_prompt']-stats['cached']) if stats.get('total_prompt') is not None and stats.get('cached') is not None else None
    profile=re.search(r'^\[profile\] wall_ms=[^\n]*?\btool_calls=(\d+)',stderr,re.M)
    if profile:stats['tool_finishes']=int(profile.group(1))
    stats['model_starts']=len(re.findall(r'^\[profile\] model_start',stderr,re.M))
    stats['model_finishes']=len(re.findall(r'^\[profile\] model_done',stderr,re.M))
    stats['tool_starts']=len(re.findall(r'^\[profile\] tool_start',stderr,re.M))
    return stats


def stage_run(who,stage_index):
    work=P3/who/'source'
    logs=P3/who/'logs'/f'{stage_index+1:02}-{STAGES[stage_index][0]}'
    logs.mkdir(parents=True,exist_ok=True)
    env=os.environ.copy()
    for k in ('KROMA_MODEL','KROMA_API_KEY','KROMA_BASE_URL','KROMA_CONFIG_FILE',
              'KROMA_PROVIDER_PROFILE','KROMA_DEEPSEEK_THINKING'):
        env.pop(k,None)
    env['CARGO_TARGET_DIR']=str(CARGO_TARGET)
    prompt=STAGES[stage_index][1]+'\n\n'+GUARD
    if who=='kroma':
        env.update(KROMA_WORKDIR=str(work),KROMA_SESSION_FILE=str(P3/who/'session.json'),KROMA_PROFILE='1')
        command=[str(KROMA_BIN),prompt]
    else:
        for k in ('KROMA_WORKDIR','KROMA_SESSION_FILE','KROMA_PROFILE'):
            env.pop(k,None)
        command=PI_CMD+['--session',str(P3/who/'session.json'),prompt]
    started=time.monotonic()
    # Both runners are provided with closed stdin: Pi otherwise sometimes
    # waits for input forever even when the prompt is a CLI argument.
    with (logs/'stdout.jsonl').open('wb') as output,(logs/'stderr.log').open('wb') as error:
        os.chmod(logs/'stdout.jsonl',0o600)
        os.chmod(logs/'stderr.log',0o600)
        print('P3_START',STAGES[stage_index][0],who,'working_directory',work,flush=True)
        process=subprocess.Popen(command,cwd=work,env=env,stdin=subprocess.DEVNULL,
                                 stdout=output,stderr=error)
        # Deliberately NO timeout and NO token quota.
        return_code=process.wait()
    duration=round(time.monotonic()-started,3)
    stats=usage_for(who,logs)
    validation=acceptance(who,stage_index)
    row={'stage':STAGES[stage_index][0],'stage_number':stage_index+1,
         'runner':who,'exit_code':return_code,'elapsed_s':duration,
         'session_exists':(P3/who/'session.json').exists(),
         'verification':validation,'validation_pass':all(validation.values()),
         'usage':stats,'log_dir':str(logs),'source_dir':str(work)}
    return row


def run():
    report=P3/'results.json'
    data=json.loads(report.read_text())
    for stage_index in range(len(STAGES)):
        # Alternate who goes first to limit time-of-day / proxy-load bias.
        for who in (('pi','kroma') if stage_index%2==0 else ('kroma','pi')):
            if any(x['stage_number']==stage_index+1 and x['runner']==who for x in data['runs']):
                continue
            result=stage_run(who,stage_index)
            data['runs'].append(result)
            report.write_text(json.dumps(data,indent=2,ensure_ascii=False))
            print('P3_END',result['stage'],who,'exit',result['exit_code'],
                  'seconds',result['elapsed_s'],'pass',result['validation_pass'],
                  'failed_checks',[k for k,v in result['verification'].items() if not v],flush=True)
    print('P3_DONE',report,flush=True)


if __name__=='__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('action',choices=('init','run','verify'))
    args=parser.parse_args()
    if args.action=='init':init()
    elif args.action=='run':run()
    else:
        r=json.loads((P3/'results.json').read_text())
        print(json.dumps([{k:x[k] for k in ('stage','runner','exit_code','elapsed_s','validation_pass')}
                          for x in r['runs']],indent=2))
