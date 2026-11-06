#!/usr/bin/env python3
"""Small native-CLI comparison. --run explicitly makes paid API calls.

Uses installed binaries unchanged, stock prompts/default builtin tools,
clean user context, one run per task, and external fixture acceptance.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import sys
import time
import prompt_philosophy_ablation as fixtures

TASKS = ('implement_records', 'cross_file_fix', 'python_complex')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def pi_metrics(events):
    messages = [e['message'] for e in events if e.get('type') == 'message_end'
                and e.get('message', {}).get('role') == 'assistant']
    def total(field):
        values = [m.get('usage', {}).get(field) for m in messages]
        return sum(values) if values and all(isinstance(v, (int, float)) for v in values) else None
    uncached, cached, written = total('input'), total('cacheRead'), total('cacheWrite')
    usage = {'requests': len(messages), 'uncached': uncached, 'cached': cached,
             'cache_write': written, 'output': total('output'),
             'input': uncached + cached + written if None not in (uncached, cached, written) else None}
    return {'usage': usage,
            'tool_calls': sum(e.get('type') == 'tool_execution_start' for e in events),
            'tool_errors': sum(e.get('type') == 'tool_execution_end' and bool(e.get('isError')) for e in events),
            'monetary_cost': None}


def self_test():
    event = {'type': 'message_end', 'message': {'role': 'assistant',
             'usage': {'input': 100, 'output': 20, 'cacheRead': 50, 'cacheWrite': 0}}}
    metrics = pi_metrics([event, {'type': 'tool_execution_start'}])
    assert metrics['usage']['input'] == 150
    assert metrics['usage']['uncached'] == 100
    assert metrics['tool_calls'] == 1
    assert pi_metrics([])['usage']['input'] is None
    assert pi_metrics([{'type': 'message_end', 'message': {'role': 'assistant'}}])['usage']['output'] is None
    print('SELF_TEST_OK: cached-input normalization, missing usage, tool counts')


def summary(lab, results):
    groups = {}
    for runner in ('kroma', 'pi'):
        rows = [r for r in results['trials'] if r['runner'] == runner]
        def total(key):
            values = [r['usage'].get(key) for r in rows]
            return sum(values) if values and all(v is not None for v in values) else None
        groups[runner] = {'completed': len(rows), 'passed': sum(r['passed'] for r in rows),
                          'wall_s': round(sum(r['wall_s'] for r in rows), 3),
                          'input': total('input'), 'output': total('output'),
                          'uncached': total('uncached'), 'cached': total('cached'),
                          'requests': total('requests'), 'tools': sum(r['tool_calls'] for r in rows),
                          'monetary_cost': None}
    fixtures.atomic_json(lab / 'summary.json', {'status': results['status'], 'groups': groups})
    return groups


def setup(lab, fixture_lab):
    if lab.exists():
        raise RuntimeError(f'refusing to overwrite {lab}')
    home = Path(os.environ['USERPROFILE'])
    config_path = home / '.kroma/config.json'
    profile = json.loads(config_path.read_text(encoding='utf-8-sig'))['profiles']['deepseek']
    pi_source = json.loads((home / '.pi/agent/models.json').read_text(encoding='utf-8-sig'))
    candidates = [(name, provider, model) for name, provider in pi_source['providers'].items()
                  for model in provider.get('models', []) if model['id'] == fixtures.MODEL
                  and provider.get('baseUrl', '').rstrip('/') == profile['base_url'].rstrip('/')]
    if profile['model'] != fixtures.MODEL or len(candidates) != 1:
        raise RuntimeError('native endpoint/model matching failed')
    if profile.get('reasoning_effort') != 'high' or profile.get('deepseek_thinking') != 'high':
        raise RuntimeError('expected the existing DeepSeek High profile')
    secret = profile.get('api_key') or os.environ.get(profile.get('api_key_env', ''))
    if not secret:
        raise RuntimeError('provider credential is unavailable')
    provider_name, provider, model = candidates[0]
    install = home / '.pi/agent/install'
    release = (install / 'current-version').read_text().strip()
    pi_package = install / 'releases' / release / 'node_modules/@earendil-works/pi-coding-agent'
    pi_cli = pi_package / 'dist/bundle/cli.js'
    pi_launcher = home / '.pi/agent/bin/pi-launcher.js'
    kroma = Path(shutil.which('kroma') or '')
    node = shutil.which('node')
    if not kroma.is_file() or not node or not pi_cli.is_file():
        raise RuntimeError('installed native CLI is unavailable')
    lab.mkdir(parents=True)
    (lab / 'home/.kroma/plugins').mkdir(parents=True)
    (lab / 'home/.kroma/plugins.toml').write_text('enabled = []\n', encoding='utf-8')
    (lab / 'pi-config').mkdir()
    # Both runners use the same credential in memory; never persist its value.
    provider = {key: provider[key] for key in ('baseUrl', 'api', 'compat') if key in provider}
    model = {key: model[key] for key in ('id', 'name', 'reasoning', 'input', 'cost',
             'contextWindow', 'maxTokens', 'compat', 'thinkingLevelMap',
             'samplingParams', 'samplingParamsByThinkingLevel') if key in model}
    provider.update(apiKey='${BENCH_DEEPSEEK_API_KEY}', models=[model])
    fixtures.atomic_json(lab / 'pi-config/models.json', {'providers': {provider_name: provider}})
    fixtures.atomic_json(lab / 'pi-config/settings.json', {})
    commands = {'kroma': [str(kroma), '--json', '--no-mcp'],
                'pi': [node, str(pi_launcher), '--offline', '--no-extensions', '--no-skills',
                       '--no-mcp', '--no-context-files', '--no-prompt-templates', '--no-approve',
                       '--provider', provider_name, '--model', fixtures.MODEL, '--thinking', 'high',
                       '--mode', 'json', '--print']}
    metadata = {'model': fixtures.MODEL, 'thinking': 'high', 'endpoint': profile['base_url'],
                'kroma_version': subprocess.check_output([str(kroma), '--build-info'], text=True).strip(),
                'pi_version': subprocess.check_output([node, str(pi_launcher), '--version'], text=True).strip(),
                'kroma_sha256': digest(kroma), 'pi_cli_sha256': digest(pi_cli),
                'tasks': list(TASKS), 'repeats': 1, 'prompts': 'native defaults; no system overrides',
                'tools': 'each native builtin default; user extensions/skills/MCP/global rules excluded',
                'payload': 'native High paths; Kroma deepseek_thinking=high retained, no payload rewrite',
                'limits': 'existing equal context/output model configuration; no additional task quota',
                'pricing': None, 'fixture_lab': str(fixture_lab),
                'note': 'Single-run smoke comparison, not a statistical ranking or upstream identity attestation.'}
    fixtures.atomic_json(lab / 'metadata.json', metadata)
    return metadata, commands, profile, secret, kroma, pi_cli


def invoke(lab, name, task, command, env, suite, complex_tasks):
    trial = lab / 'trials' / f'{task}-{name}'
    trial.mkdir(parents=True)
    work = lab / 'work'
    prompt = fixtures.prepare(work, task, suite, complex_tasks)
    if fixtures.oracle(work, task, '', suite, complex_tasks):
        raise RuntimeError('fixture baseline unexpectedly passes')
    initial = {str(p.relative_to(work)): digest(p) for p in work.rglob('*') if p.is_file()}
    fixtures.atomic_json(trial / 'input.json', {'prompt': prompt, 'files': initial})
    env = dict(env)
    if name == 'kroma':
        env.update(KROMA_WORKDIR=str(work), KROMA_SESSION_FILE=str(trial / 'session.json'))
    else:
        command = command + ['--session', str(trial / 'session.jsonl')]
    print('START',task,name,flush=True)
    fixtures.atomic_json(lab / 'active.json', {'task':task,'runner':name})
    started = time.monotonic()
    with (trial / 'events.jsonl').open('wb') as out, (trial / 'stderr.log').open('wb') as err:
        process = subprocess.run(command + [prompt], cwd=work, env=env,
                                 stdin=subprocess.DEVNULL, stdout=out, stderr=err)
    wall = round(time.monotonic() - started, 3)
    events = fixtures.parse_events(trial / 'events.jsonl')
    text = (trial / 'stderr.log').read_text(encoding='utf-8', errors='replace')
    if name == 'kroma':
        telemetry = fixtures.metrics(events, text, suite)
        completed = any(e.get('type') == 'task_finished' and e.get('status') == 'completed' for e in events)
    else:
        telemetry = pi_metrics(events)
        completed = any(e.get('type') == 'agent_end' for e in events) and not any(
            e.get('type') == 'message_end' and e.get('message', {}).get('stopReason') in ('error','aborted')
            for e in events)
    accepted = fixtures.oracle(work, task, '', suite, complex_tasks)
    tests_unchanged = all((work / rel).is_file() and digest(work / rel) == value
                          for rel,value in initial.items() if Path(rel).name.startswith('test_'))
    row = {'task':task,'runner':name,'wall_s':wall,'exit_code':process.returncode,
           'accepted':accepted,'tests_unchanged':tests_unchanged,'completed':completed,
           'passed':process.returncode == 0 and completed and accepted and tests_unchanged,
           'artifacts':str(trial),**telemetry}
    fixtures.atomic_json(trial / 'result.json',row)
    shutil.move(str(work),str(trial / 'source'))
    print('END',task,name,'pass',row['passed'],'seconds',wall,'usage',json.dumps(row['usage']),flush=True)
    return row


def run(args):
    lab=args.lab.resolve()
    fixture_lab=args.fixtures.resolve()
    metadata,commands,profile,secret,kroma,pi_cli=setup(lab,fixture_lab)
    suite,complex_tasks=fixtures.modules(fixture_lab)
    env={k:v for k,v in os.environ.items() if not k.startswith(('KROMA_','PI_'))}
    env.update(HOME=str(lab/'home'),USERPROFILE=str(lab/'home'),
               PATH=str(Path(sys.executable).parent)+os.pathsep+os.environ['PATH'],
               PYTHONUTF8='1',PYTHONDONTWRITEBYTECODE='1',
               BENCH_DEEPSEEK_API_KEY=secret,PI_CODING_AGENT_DIR=str(lab/'pi-config'),
               PI_OFFLINE='1',PI_TELEMETRY='0',
               KROMA_MODEL=fixtures.MODEL,KROMA_BASE_URL=profile['base_url'],
               KROMA_API_KEY=secret,KROMA_API_PROTOCOL=profile.get('api_protocol','chat'),
               KROMA_REASONING_EFFORT='high',KROMA_DEEPSEEK_THINKING='high',
               KROMA_PROFILE='1',KROMA_NO_MCP='1',KROMA_NO_COLOR='1',
               KROMA_PLUGIN_DIR=str(lab/'home/.kroma/plugins'))
    for source,dest in [('context_window','KROMA_CONTEXT_WINDOW'),('max_output_tokens','KROMA_MAX_OUTPUT_TOKENS')]:
        if profile.get(source):env[dest]=str(profile[source])
    for key in ('PYTHONHOME','PYTHONPATH','PYTHONSTARTUP'):env.pop(key,None)
    results={'status':'running','metadata':metadata,'trials':[]}
    fixtures.atomic_json(lab/'results.json',results)
    shutil.copy2(Path(__file__),lab/'runner.py')
    plan=[{'task':task,'order':['kroma','pi'] if i%2==0 else ['pi','kroma']}
          for i,task in enumerate(TASKS)]
    fixtures.atomic_json(lab/'plan.json',plan)
    for block in plan:
        for name in block['order']:
            if digest(kroma)!=metadata['kroma_sha256'] or digest(pi_cli)!=metadata['pi_cli_sha256']:
                raise RuntimeError('installed binary changed during comparison')
            row=invoke(lab,name,block['task'],commands[name],env,suite,complex_tasks)
            results['trials'].append(row)
            fixtures.atomic_json(lab/'results.json',results)
            summary(lab,results)
    results['status']='completed'
    fixtures.atomic_json(lab/'results.json',results)
    print('COMPLETE',json.dumps(summary(lab,results)),flush=True)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    choice=parser.add_mutually_exclusive_group(required=True)
    choice.add_argument('--run',action='store_true')
    choice.add_argument('--self-test',action='store_true')
    parser.add_argument('--lab',type=Path,default=Path('D:/Kroma/experiments/native-kroma-pi-smoke-v1'))
    parser.add_argument('--fixtures',type=Path,default=Path('D:/Kroma/experiments/prompt-philosophy-v1'))
    args=parser.parse_args()
    if args.self_test:self_test()
    else:run(args)


if __name__=='__main__':
    main()
