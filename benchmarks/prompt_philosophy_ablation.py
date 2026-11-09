#!/usr/bin/env python3
"""Two- or three-arm prompt-only live contest using an isolated copy of the real CLI.

No production Rust source, saved configuration, or installed plugin is changed.
setup/self-test do not call the model; run explicitly spends API credits.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import random
import re
import shutil
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / 'benchmarks/prompt_philosophy'
ARMS = ('A', 'B', 'C')
TASKS = ('fix_arithmetic', 'implement_records', 'cross_file_fix',
         'diagnose_large_log', 'python_complex')
EXTRA_TASKS = ('reuse_existing', 'complete_change')
SEED = 20261106
MODEL = 'deepseek-v4.1-flash'
GUARD = ('\nWork only in this isolated project. Do not access credentials, change '
         'model configuration, use network operations, or make commits. Do not '
         'edit supplied test files; put additional tests in new files. Use python for Python commands; '
         'it is configured on PATH. Finish with what changed and what you verified.')
DEFAULT_ADDITIONS = 'additions: vec![EXECUTION_GUIDANCE.to_owned(), V7_GUIDANCE.to_owned()],'
EXPERIMENT_ADDITIONS = '''additions: match std::env::var("KROMA_ABLATION_GUIDANCE_FILE") {
                Ok(path) => vec![std::fs::read_to_string(path)
                    .expect("cannot read experimental guidance")],
                Err(_) => vec![EXECUTION_GUIDANCE.to_owned(), V7_GUIDANCE.to_owned()],
            },'''
AUDIT_ANCHOR = '        let tool_schema_bytes = serde_json::to_vec(&tool_definitions)?.len();'
AUDIT_REPLACEMENT = '''        if let Ok(path) = std::env::var("KROMA_ABLATION_TOOLS_FILE") {
            std::fs::write(path, serde_json::to_vec(&tool_definitions)?)?;
        }
''' + AUDIT_ANCHOR
PROMPT_ANCHOR = '''            if !additions.is_empty() && !system.ends_with(&additions) {
                system.push_str(&additions);
            }'''
PROMPT_REPLACEMENT = PROMPT_ANCHOR + '''
            if let Ok(path) = std::env::var("KROMA_ABLATION_SYSTEM_FILE") {
                std::fs::write(path, system.as_bytes())
                    .expect("cannot write experimental prompt audit");
            }'''


def sha(data):
    return hashlib.sha256(data).hexdigest()


def atomic_json(path, value):
    tmp = path.with_suffix(path.suffix + '.new')
    tmp.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    tmp.replace(path)


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def extract_baseline(text):
    literal = r'".*"'
    execution = re.search(r'pub const EXECUTION_GUIDANCE: &str = (' + literal + r');', text)
    v7 = re.search(r'pub const V7_GUIDANCE: &str = concat!\((.*?)\n\);', text, re.S)
    if not execution or not v7:
        raise RuntimeError('prompt fixture drift: cannot extract production guidance')
    return json.loads(execution[1]) + ''.join(json.loads(x) for x in re.findall(literal, v7[1]))


def quality_variants(baseline, directory, arms):
    paragraphs = [line for line in baseline.splitlines()
                  if line.startswith('Prefer the simplest correct implementation')]
    if len(paragraphs) != 1:
        raise RuntimeError('expected exactly one production quality paragraph')
    quality = paragraphs[0]
    variants = {}
    for arm in arms:
        replacement = (directory / f'{arm}.txt').read_text(encoding='utf-8').strip()
        if not replacement or any(char in replacement for char in (chr(10), chr(13))):
            raise RuntimeError('quality variants must be nonempty single paragraphs')
        variants[arm] = baseline.replace(quality, replacement, 1)
    native = [arm for arm, guidance in variants.items() if guidance == baseline]
    if len(native) != 1:
        raise RuntimeError('exactly one quality variant must equal production guidance')
    return variants, native[0]


def make_plan(tasks, repeats, seed=SEED, arms=ARMS):
    rng = random.Random(seed)
    rotations = {}
    for task in tasks:
        order = rng.sample(list(arms), len(arms))
        rotations[task] = [order[i:] + order[:i] for i in range(len(arms))]
    blocks = [(task, repeat) for task in tasks for repeat in range(1, repeats + 1)]
    rng.shuffle(blocks)
    return [dict(task=task, repeat=repeat, order=rotations[task][(repeat - 1) % len(arms)])
            for task, repeat in blocks]


def inspect_profile(config_path, name):
    config = json.loads(config_path.read_text(encoding='utf-8-sig'))
    profile = config['profiles'][name]
    if profile['model'] != MODEL:
        raise RuntimeError(f'profile model is not the requested {MODEL}')
    if profile.get('api_protocol', 'chat') != 'chat':
        raise RuntimeError('this experiment expects the configured chat protocol')
    if not profile.get('api_key') and not os.environ.get(profile.get('api_key_env', '')):
        raise RuntimeError('provider credential is not configured')
    # Never return credentials or persist the full provider profile.
    keys = ('model', 'api_protocol', 'reasoning_effort', 'deepseek_thinking',
            'context_window', 'max_output_tokens', 'catalog_model', 'api_key_env')
    from urllib.parse import urlsplit
    return {**{key: profile.get(key) for key in keys},
            'endpoint_host': urlsplit(profile['base_url']).netloc,
            'endpoint_sha256': sha(profile['base_url'].encode()),
            'identity_note': 'Configured proxy model ID; upstream identity not independently attested.'}


def setup(args):
    lab = args.lab.resolve()
    if lab.exists():
        raise RuntimeError(f'refusing to overwrite existing experiment: {lab}')
    if not 1 <= args.repeats <= 9:
        raise RuntimeError('repeats must be between 1 and 9')
    arms = getattr(args, 'arms', 'A,B,C')
    arms = tuple(arms.split(',')) if isinstance(arms, str) else tuple(arms)
    if arms not in (('A', 'B'), ARMS):
        raise RuntimeError('supported arms are A,B or A,B,C')
    tasks = tuple(getattr(args, 'tasks', None) or TASKS)
    if len(tasks) != len(set(tasks)) or not set(tasks) <= set(TASKS + EXTRA_TASKS):
        raise RuntimeError('tasks must be distinct supported task IDs')
    quality_dir = getattr(args, 'quality_variants', None)
    append_path = getattr(args, 'append_guidance', None)
    if quality_dir is not None and append_path is not None:
        raise RuntimeError('quality-variants and append-guidance are mutually exclusive')
    if append_path is not None and arms != ('A', 'B'):
        raise RuntimeError('append-guidance requires exactly A,B')
    append_text = append_path.read_text(encoding='utf-8').strip() if append_path else None
    if append_text is not None and not append_text:
        raise RuntimeError('append-guidance must not be empty')
    config_path = args.config.resolve()
    profile = inspect_profile(config_path, args.profile)
    lab.mkdir(parents=True)
    runtime = lab / 'runtime'
    runtime.mkdir()
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    tracked = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard', 'src', 'Cargo.toml',
                                       'Cargo.lock', 'build.rs', 'README.md', '.cargo'], cwd=ROOT).decode().split('\0')
    original_hashes = {}
    for relative in filter(None, tracked):
        source = ROOT / relative
        destination = runtime / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
        original_hashes[relative] = sha(source.read_bytes())
    prompt_path = runtime / 'src/prompt.rs'
    original = prompt_path.read_text(encoding='utf-8')
    baseline = extract_baseline(original)
    quality_prompts, native_arm = (quality_variants(baseline, quality_dir, arms)
                                  if quality_dir is not None else ({}, 'A'))
    for anchor in (DEFAULT_ADDITIONS, PROMPT_ANCHOR):
        if original.count(anchor) != 1:
            raise RuntimeError('prompt instrumentation anchor drift')
    prompt_path.write_text(original.replace(DEFAULT_ADDITIONS, EXPERIMENT_ADDITIONS)
                          .replace(PROMPT_ANCHOR, PROMPT_REPLACEMENT), encoding='utf-8')
    agent_path = runtime / 'src/agent.rs'
    original_agent = agent_path.read_text(encoding='utf-8')
    if original_agent.count(AUDIT_ANCHOR) != 1:
        raise RuntimeError('tool audit anchor drift')
    agent_path.write_text(original_agent.replace(AUDIT_ANCHOR, AUDIT_REPLACEMENT), encoding='utf-8')
    (lab / 'prompts').mkdir()
    (lab / 'prompts/A.txt').write_text(baseline, encoding='utf-8', newline='\n')
    for arm in arms if quality_dir is not None else arms[1:]:
        if quality_dir is not None:
            guidance = quality_prompts[arm]
        elif append_text is not None:
            guidance = baseline + '\n' + append_text
        else:
            guidance = '\n' + (ASSETS / f'{arm}.txt').read_text(encoding='utf-8').strip()
        (lab / f'prompts/{arm}.txt').write_text(guidance, encoding='utf-8', newline='\n')
    oracles = lab / 'oracles'
    oracles.mkdir()
    oracle_names = ['agent_task_suite.py', 'pi_kroma_complexity_matrix.py']
    if set(tasks) & set(EXTRA_TASKS):
        oracle_names.append('ponytail_tasks.py')
    for name in oracle_names:
        shutil.copy2(ROOT / 'benchmarks' / name, oracles / name)
    (lab / 'home/.kroma/plugins').mkdir(parents=True)
    (lab / 'home/.kroma/plugins.toml').write_text('enabled = []\n', encoding='utf-8')
    (lab / 'python-bin').mkdir()
    if os.name == 'nt':
        shutil.copy2(sys.executable, lab / 'python-bin/python.exe')
        (lab / 'python-bin/pyvenv.cfg').write_text(
            f'home = {Path(sys.executable).parent}\ninclude-system-site-packages = false\n'
            f'version = {sys.version.split()[0]}\n', encoding='utf-8')
        for dll in Path(sys.executable).parent.glob('python*.dll'):
            shutil.copy2(dll, lab / 'python-bin' / dll.name)
    else:
        (lab / 'python-bin/python').symlink_to(sys.executable)
    prompts = {arm: {'sha256': sha((lab / f'prompts/{arm}.txt').read_bytes()),
                     'bytes': (lab / f'prompts/{arm}.txt').stat().st_size} for arm in arms}
    metadata = dict(schema=1, created_utc=datetime.now(timezone.utc).isoformat(),
                    source_commit=commit, source_root=str(ROOT), source_file_hashes=original_hashes,
                    profile=profile, config_file=str(config_path), profile_name=args.profile,
                    seed=SEED, repeats=args.repeats, tasks=list(tasks), arms=list(arms), prompts=prompts,
                    native_arm=native_arm,
                    plugins='disabled equally; core-prompt experiment, not plugin ablation',
                    controls='same binary, schemas, endpoint, frozen profile settings, fixtures, working paths; sequential',
                    timeout_s=args.timeout, task_token_quota=None,
                    pricing=None, cache='observed shared endpoint cache; no cold-cache claim',
                    variants={'A': 'exact production default guidance',
                              'B': 'consolidated wording, workflow requirements retained',
                              'C': 'B with inspection order and progress timing discretionary'},
                    limitations=['Synthetic Python tasks, not a general coding-agent leaderboard.',
                                 'B changes wording and length together, not pure token-length ablation.',
                                 'C retains tool-description workflow hints.',
                                 'No proxy billing tariff supplied; actual monetary cost unknown.',
                                 'No plugin or long-context compaction tasks in this first round.'])
    metadata['variants'] = {arm: metadata['variants'][arm] for arm in arms}
    if append_text is not None:
        metadata['variants']['B'] = 'exact A guidance plus one independent-reading sentence'
        metadata['append_guidance'] = append_text
        metadata['limitations'] = [
            'Synthetic Python/log tasks, not a general coding-agent leaderboard.',
            'Single appended sentence tests emphasis, not a new execution mechanism.',
            'No proxy billing tariff supplied; actual monetary cost unknown.',
            'Shared endpoint cache is observed, not independently cleared.',
            'No plugin or long-context compaction tasks in this round.']
        (lab / 'append-guidance.txt').write_text(append_text, encoding='utf-8')
    if quality_dir is not None:
        metadata['variants'] = {arm: f'quality paragraph {arm}' for arm in arms}
        metadata['quality_variants'] = True
        metadata['limitations'] = [
            'Nested quality-paragraph ablation; wording and length change together.',
            'Synthetic Python/log tasks; no general Rust, UI, plugin or long-context claim.',
            'Configured proxy model identity is not independently attested.',
            'Shared endpoint cache is observed, not independently cleared.',
            'No proxy billing tariff supplied; actual monetary cost unknown.']
    shutil.copy2(Path(__file__), lab / 'runner.py')
    metadata['runner_sha256'] = sha((lab / 'runner.py').read_bytes())
    metadata['oracle_hashes'] = {p.name: sha(p.read_bytes()) for p in oracles.glob('*.py')}
    metadata['instrumented_hashes'] = {name: sha((runtime / name).read_bytes())
                                     for name in ('src/prompt.rs', 'src/agent.rs')}
    plan = make_plan(tasks, args.repeats, arms=arms)
    atomic_json(lab / 'plan.json', plan)
    metadata['plan_sha256'] = sha((lab / 'plan.json').read_bytes())
    atomic_json(lab / 'metadata.json', metadata)

    atomic_json(lab / 'results.json', {'status': 'prepared', 'calibration': [], 'trials': []})
    print('SETUP_OK', lab, 'scored_trials', len(tasks) * args.repeats * len(arms), flush=True)


def child_env(lab, metadata, arm=None, trial=None):
    env = {k: v for k, v in os.environ.items() if not k.startswith('KROMA_')}
    home = str(lab / 'home')
    env.update(HOME=home, USERPROFILE=home, PYTHONUTF8='1', PYTHONDONTWRITEBYTECODE='1',
               PYTHONPATH='', PYTHONSTARTUP='',
               PATH=str(lab / 'python-bin') + os.pathsep + os.environ['PATH'],
               KROMA_CONFIG_FILE=metadata['config_file'],
               KROMA_PROVIDER_PROFILE=metadata['profile_name'],
               KROMA_MODEL=MODEL,
               KROMA_NO_MCP='1', KROMA_PROFILE='1', KROMA_NO_COLOR='1',
               KROMA_PLUGIN_DIR=str(lab / 'home/.kroma/plugins'))
    for field, variable in (('reasoning_effort', 'KROMA_REASONING_EFFORT'),
                            ('deepseek_thinking', 'KROMA_DEEPSEEK_THINKING')):
        value = metadata.get('profile', {}).get(field)
        if value is not None:
            env[variable] = str(value)
    credential_env = metadata.get('profile', {}).get('api_key_env')
    if credential_env and credential_env in os.environ:
        env[credential_env] = os.environ[credential_env]
    env.pop('PYTHONHOME', None)
    if arm is not None:
        env.update(KROMA_WORKDIR=str(lab / 'work'),
                   KROMA_SESSION_FILE=str(trial / 'session.json'),
                   KROMA_ABLATION_TOOLS_FILE=str(trial / 'tools.json'),
                   KROMA_ABLATION_SYSTEM_FILE=str(trial / 'system.txt'))
        if arm != metadata.get('native_arm', 'A'):
            env['KROMA_ABLATION_GUIDANCE_FILE'] = str(lab / f'prompts/{arm}.txt')
    return env


def build(lab):
    binary = lab / 'target/release' / ('kroma.exe' if os.name == 'nt' else 'kroma')
    if binary.exists():
        raise RuntimeError('build already exists; use run instead of silently rebuilding')
    env = os.environ.copy()
    env['CARGO_TARGET_DIR'] = str(lab / 'target')
    env['CARGO_INCREMENTAL'] = '0'
    print('BUILD_START', flush=True)
    with (lab / 'build.log').open('wb') as log:
        proc = subprocess.run(['cargo', 'build', '--release', '--locked', '--bin', 'kroma'],
                              cwd=lab / 'runtime', env=env, stdout=log, stderr=subprocess.STDOUT)
    if proc.returncode:
        raise RuntimeError(f'build failed: exit {proc.returncode}; see {lab / "build.log"}')
    atomic_json(lab / 'build.json', {'binary': str(binary), 'sha256': sha(binary.read_bytes()),
                                    'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip()})
    print('BUILD_OK', binary, flush=True)


def modules(lab):
    import ast
    from types import SimpleNamespace
    tree = ast.parse((lab / 'oracles/pi_kroma_complexity_matrix.py').read_text(encoding='utf-8'))
    constants = {}
    for statement in tree.body:
        if isinstance(statement, ast.Assign) and len(statement.targets) == 1:
            name = getattr(statement.targets[0], 'id', None)
            if name in ('PY_PROMPT', 'FILES', 'HIDDEN_CHECK'):
                constants[name] = ast.literal_eval(statement.value)
    if len(constants) != 3:
        raise RuntimeError('complex fixture constants changed')
    suite = load_module('philosophy_suite', lab / 'oracles/agent_task_suite.py')
    ponytail_path = lab / 'oracles/ponytail_tasks.py'
    if ponytail_path.exists():
        suite.ponytail_tasks = load_module('ponytail_tasks', ponytail_path)
    return suite, SimpleNamespace(**constants)


def prepare(work, task, suite, complex_tasks):
    work.mkdir()
    if task == 'calibration':
        (work / 'fixture.txt').write_text('MARKER=PHILOSOPHY_CAL_OK\n', encoding='utf-8')
        return 'Use shell to read fixture.txt and report the exact MARKER value. Do not edit files.' + GUARD
    if task in EXTRA_TASKS:
        return suite.ponytail_tasks.create_fixture(work, task) + GUARD
    if task == 'python_complex':
        for relative, text in complex_tasks.FILES.items():
            path = work / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding='utf-8')
        return complex_tasks.PY_PROMPT + GUARD
    item = next(task_item for task_item in suite.TASKS if task_item['id'] == task)
    suite.create_fixture(work, item)
    return item['prompt'] + GUARD


def oracle(work, task, answer, suite, complex_tasks):
    if task == 'calibration':
        return 'PHILOSOPHY_CAL_OK' in answer
    if task in EXTRA_TASKS:
        return suite.ponytail_tasks.verify(work, task)
    if task == 'python_complex':
        code = 'import sys; sys.path.insert(0, sys.argv[1]);\n' + complex_tasks.HIDDEN_CHECK
        proc = subprocess.run([sys.executable, '-I', '-B', '-c', code, str(work)], cwd=work,
                              capture_output=True, timeout=30)
        return proc.returncode == 0 and b'HIDDEN_PASS' in proc.stdout
    return bool(suite.verify(work, task, answer))


def parse_events(path):
    events = []
    for line in path.read_text(encoding='utf-8', errors='replace').splitlines():
        try:
            value = json.loads(line)
            if isinstance(value, dict):
                events.append(value)
        except json.JSONDecodeError:
            continue
    return events


def metrics(events, stderr, suite):
    usage = suite.parse_usage(stderr)
    profile = suite.parse_profile(stderr)
    for key in ('requests', 'input', 'output', 'cached'):
        usage.setdefault(key, None)
    usage['uncached'] = (usage['input'] - usage['cached']
                         if usage['input'] is not None and usage['cached'] is not None else None)
    return dict(usage=usage, profile=profile,
                model_started_events=sum(e.get('type') == 'model_started' for e in events),
                tool_calls=sum(e.get('type') == 'tool_started' for e in events),
                tool_errors=sum(e.get('type') == 'tool_finished' and e.get('is_error', False) for e in events),
                compactions=sum(e.get('type') == 'compaction_started' for e in events),
                monetary_cost=None)


def audit(lab, trial, arm):
    expected = (lab / f'prompts/{arm}.txt').read_bytes().decode('utf-8')
    system = (trial / 'system.txt').read_bytes().decode('utf-8')
    if not system.endswith(expected):
        raise RuntimeError('effective prompt is missing frozen arm guidance')
    base = system[:-len(expected)]
    if not base.startswith('You are Kroma, an AI coding agent.'):
        raise RuntimeError('unexpected system identity')
    hashes = {'base_sha256': sha(base.encode()),
              'guidance_sha256': sha(expected.encode()),
              'tools_sha256': sha((trial / 'tools.json').read_bytes())}
    control_path = lab / 'controls.json'
    if control_path.exists():
        controls = json.loads(control_path.read_text(encoding='utf-8'))
        for key in ('base_sha256', 'tools_sha256'):
            if hashes[key] != controls[key]:
                raise RuntimeError(f'experimental control changed: {key}')
    else:
        atomic_json(control_path, hashes)
    return hashes


def invoke(lab, metadata, arm, task, repeat, suite, complex_tasks):
    slug = f'{task}-{repeat}-{arm}'
    trial = lab / 'trials' / slug
    if trial.exists():
        raise RuntimeError(f'trial already exists; refusing to rerun: {slug}')
    trial.mkdir(parents=True)
    work = lab / 'work'
    if work.exists():
        raise RuntimeError('work directory exists; inspect interrupted trial before continuing')
    prompt = prepare(work, task, suite, complex_tasks)
    before_files = {str(p.relative_to(work)): sha(p.read_bytes()) for p in work.rglob('*') if p.is_file()}
    if task != 'calibration' and oracle(work, task, '', suite, complex_tasks):
        raise RuntimeError(f'task baseline unexpectedly passes: {task}')
    atomic_json(trial / 'input.json', {'task': task, 'repeat': repeat, 'arm': arm,
                                      'prompt': prompt, 'fixture_hashes': before_files})
    env = child_env(lab, metadata, arm, trial)
    binary = json.loads((lab / 'build.json').read_text(encoding='utf-8'))['binary']
    print('TRIAL_START', slug, flush=True)
    atomic_json(lab / 'active.json', {'slug': slug, 'started_utc': datetime.now(timezone.utc).isoformat()})
    start = time.monotonic()
    timed_out = False
    with (trial / 'events.jsonl').open('wb') as out, (trial / 'stderr.log').open('wb') as err:
        proc = subprocess.Popen([binary, '--json', '--no-mcp', '--new', prompt], cwd=work,
                                env=env, stdin=subprocess.DEVNULL, stdout=out, stderr=err)
        atomic_json(trial / 'process.json', {'pid': proc.pid})
        try:
            proc.wait(timeout=metadata['timeout_s'])
        except subprocess.TimeoutExpired:
            timed_out = True
            if os.name == 'nt':
                subprocess.run(['taskkill', '/PID', str(proc.pid), '/T', '/F'], capture_output=True)
            else:
                proc.kill()
            proc.wait()
    elapsed = round(time.monotonic() - start, 3)
    events = parse_events(trial / 'events.jsonl')
    answer = '\n'.join(e.get('text', '') for e in events if e.get('type') == 'result')
    stderr = (trial / 'stderr.log').read_text(encoding='utf-8', errors='replace')
    hashes = None
    audit_error = None
    try:
        hashes = audit(lab, trial, arm)
    except (OSError, RuntimeError) as error:
        audit_error = str(error)
    verified = oracle(work, task, answer, suite, complex_tasks)
    protected = {relative: digest for relative, digest in before_files.items()
                 if Path(relative).name.startswith('test_') or task in ('calibration', 'diagnose_large_log')}
    protected_ok = all((work / relative).is_file() and sha((work / relative).read_bytes()) == digest
                       for relative, digest in protected.items())
    completed = any(e.get('type') == 'task_finished' and e.get('status') == 'completed' for e in events)
    row = dict(task=task, repeat=repeat, arm=arm, elapsed_s=elapsed,
               exit_code=proc.returncode, timed_out=timed_out,
               oracle_pass=verified, tests_unchanged=protected_ok, audit=hashes,
               audit_error=audit_error, task_completed=completed,
               passed=bool(proc.returncode == 0 and not timed_out and verified and protected_ok
                           and completed and audit_error is None),
               artifacts=str(trial), **metrics(events, stderr, suite))
    atomic_json(trial / 'result.json', row)
    shutil.move(str(work), str(trial / 'source'))
    print('TRIAL_END', slug, 'pass', row['passed'], 'seconds', elapsed,
          'usage', json.dumps(row['usage']), 'tools', row['tool_calls'], flush=True)
    return row


def report(lab):
    result = json.loads((lab / 'results.json').read_text(encoding='utf-8'))
    summary = {}
    metadata = json.loads((lab / 'metadata.json').read_text(encoding='utf-8'))
    for arm in metadata.get('arms', ARMS):
        selected = [row for row in result['trials'] if row['arm'] == arm]
        def total(key):
            values = [row['usage'].get(key) for row in selected]
            return sum(values) if values and all(value is not None for value in values) else None
        summary[arm] = {'completed_trials': len(selected),
                        'passed': sum(row['passed'] for row in selected),
                        'total_s': round(sum(row['elapsed_s'] for row in selected), 3),
                        'median_s': statistics.median([row['elapsed_s'] for row in selected]) if selected else None,
                        'tokens': {key: total(key) for key in ('input', 'output', 'cached', 'uncached')},
                        'model_requests': total('requests'),
                        'tool_calls': sum(row['tool_calls'] for row in selected),
                        'monetary_cost': None}
    atomic_json(lab / 'summary.json', {'status': result['status'], 'arms': summary,
                                     'warning': 'Do not rank incomplete/unmatched trials; proxy monetary cost unknown.'})
    print('SUMMARY', json.dumps(summary, ensure_ascii=False), flush=True)
    return summary


def run(lab):
    metadata = json.loads((lab / 'metadata.json').read_text(encoding='utf-8'))
    arms = tuple(metadata.get('arms', ARMS))
    if metadata.get('plan_sha256') and sha((lab / 'plan.json').read_bytes()) != metadata['plan_sha256']:
        raise RuntimeError('frozen execution plan changed')
    if inspect_profile(Path(metadata['config_file']), metadata['profile_name']) != metadata['profile']:
        raise RuntimeError('provider settings changed since setup')
    build_info = json.loads((lab / 'build.json').read_text(encoding='utf-8'))
    if sha(Path(build_info['binary']).read_bytes()) != build_info['sha256']:
        raise RuntimeError('experimental binary changed')
    if sha((lab / 'runner.py').read_bytes()) != metadata['runner_sha256']:
        raise RuntimeError('frozen runner changed')
    for arm in arms:
        if sha((lab / f'prompts/{arm}.txt').read_bytes()) != metadata['prompts'][arm]['sha256']:
            raise RuntimeError('frozen prompts changed')
    result = json.loads((lab / 'results.json').read_text(encoding='utf-8'))
    for name, digest in metadata['oracle_hashes'].items():
        if sha((lab / 'oracles' / name).read_bytes()) != digest:
            raise RuntimeError('frozen task or acceptance code changed')
    suite, complex_tasks = modules(lab)
    for category in ('calibration', 'trials'):
        recovered = {(row['task'], row['repeat'], row['arm']) for row in result[category]}
        for path in (lab / 'trials').glob('*/result.json'):
            row = json.loads(path.read_text(encoding='utf-8'))
            if (row['task'] == 'calibration') == (category == 'calibration'):
                key = row['task'], row['repeat'], row['arm']
                if key not in recovered:
                    result[category].append(row)
                    recovered.add(key)
    result['status'] = 'calibrating'
    atomic_json(lab / 'results.json', result)
    for arm in arms:
        if not any(row['arm'] == arm for row in result['calibration']):
            result['calibration'].append(invoke(lab, metadata, arm, 'calibration', 0, suite, complex_tasks))
            atomic_json(lab / 'results.json', result)
    if not all(row['passed'] for row in result['calibration']):
        result['status'] = 'calibration_failed'
        atomic_json(lab / 'results.json', result)
        raise RuntimeError('calibration failed; scored contest was not started')
    result['status'] = 'running'
    atomic_json(lab / 'results.json', result)
    plan = json.loads((lab / 'plan.json').read_text(encoding='utf-8'))
    for block in plan:
        for arm in block['order']:
            key = block['task'], block['repeat'], arm
            if any((row['task'], row['repeat'], row['arm']) == key for row in result['trials']):
                continue
            row = invoke(lab, metadata, arm, block['task'], block['repeat'], suite, complex_tasks)
            result['trials'].append(row)
            atomic_json(lab / 'results.json', result)
            report(lab)
            if row['audit_error']:
                raise RuntimeError('control audit failed; paused without discarding trial')
    result['status'] = 'completed'
    atomic_json(lab / 'results.json', result)
    report(lab)
    print('CONTEST_COMPLETE', lab, flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=('setup', 'build', 'run', 'report', 'all'))
    parser.add_argument('--lab', type=Path, required=True)
    parser.add_argument('--config', type=Path,
                        default=Path(os.environ.get('USERPROFILE', str(Path.home()))) / '.kroma/config.json')
    parser.add_argument('--profile', default='deepseek')
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--arms', choices=('A,B', 'A,B,C'), default='A,B,C')
    parser.add_argument('--tasks', nargs='+', choices=TASKS + EXTRA_TASKS,
                        help='task IDs to freeze; defaults to the original five tasks')
    parser.add_argument('--quality-variants', type=Path,
                        help='directory of A/B[/C].txt quality paragraphs; one must match production')
    parser.add_argument('--append-guidance', type=Path,
                        help='append only this text to A to form B; requires --arms A,B')
    parser.add_argument('--timeout', type=int, default=None,
                        help='optional trial watchdog; default unlimited, frozen at setup')
    args = parser.parse_args()
    lab = args.lab.resolve()
    if args.action in ('setup', 'all'):
        setup(args)
    if args.action in ('build', 'all'):
        build(lab)
    if args.action in ('run', 'all'):
        run(lab)
    if args.action == 'report':
        report(lab)


if __name__ == '__main__':
    main()
