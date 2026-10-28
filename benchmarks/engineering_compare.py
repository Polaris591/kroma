#!/usr/bin/env python3
"""Real-code engineering quality comparison, never modifying the live checkout.

Each trial starts from the exact committed Rust Kroma source archive. The A
fixture adds a real regression to session ordering. B and C request CLI
features on a separate copy. D checks conversation persistence across process
restarts. The model and endpoint are selected externally, not by this script.

Use --runner pi for the existing Pi WorkBuddy setup; --runner kroma requires
Kroma provider configuration already available to Kroma at startup.
"""

import argparse
import io
import json
import os
from pathlib import Path
import re
import selectors
import signal
import statistics
import subprocess
import tarfile
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
SOURCE_SHA = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
MODEL = 'deepseek'
PI = ['pi', '--offline', '--no-extensions', '--no-skills', '--no-mcp',
      '--no-context-files', '--no-approve', '--provider', 'workbuddy', '--model', MODEL,
      '--thinking', 'high', '--mode', 'json', '--print']
KROMA = str((ROOT / 'target/release/kroma').resolve())
KEY_TEST = 'BENCHMARK_DUMMY_KEY_NOT_REAL'

A_PROMPT = (
    'This is a real Rust codebase. The session selector now has a regression: '
    'a conversation updated only in its WAL does not move to the top of /sessions. '
    'Reproduce and fix the root cause, without editing or removing the regression '
    'test. Run cargo test --locked --offline and report what you verified. '
    'Only modify this isolated checkout; do not commit or access API secrets.'
)
B_PROMPT = (
    'Implement `kroma config remove <name>` across this Rust application. '
    'Remove only the named profile; reject missing profiles without touching '
    'the file; if the active profile is removed, select the first remaining '
    'profile in alphabetical order, or no active profile if empty. Keep other '
    'entries and secrets unchanged. Update CLI help and README documentation. '
    'Use existing atomic private profile saving. Run cargo test --locked --offline '
    'and verify the command. Do not commit or access real API secrets.'
)
C_PROMPTS = (
    'Stage 1 of a continuous multi-step Rust change. Implement '
    '`kroma config show --json` alongside the existing human-readable output. '
    'Print only a single JSON object with string fields model, host, thinking '
    'and boolean key_configured. Respect current environment overrides, and '
    'never print the API key. Existing text commands must remain unchanged. '
    'Run an appropriate verification. Do not commit or read real API secrets.',
    'Stage 2: continue the same work. Implement `kroma config list --json` '
    'with a single JSON array sorted by profile name. Each object needs name, '
    'active (bool), model, host, thinking, and key_configured (bool). Keep '
    'legacy text output unchanged. Never print stored secret values. Verify '
    'both JSON commands work with two profiles. Do not commit.',
    'Stage 3: finish the continuous feature. Update CLI --help and README for '
    'both new --json commands, add useful Rust tests for JSON output / key '
    'masking if feasible, run cargo test --locked --offline and fix failures. '
    'Do not commit. Report which checks passed.',
)
D_FIRST = (
    'Read continuation_note.txt using your tools, and remember its exact '
    'MARKER value for the next message. Reply with the marker and confirm it '
    'will be recalled after a process restart. Do not modify files.'
)
D_NEXT = (
    'The tool process was restarted and continuation_note.txt was deleted. '
    'From the previous conversation only, reply with the exact MARKER value; '
    'do not read files, use tools, or guess. Reply with the marker alone.'
)


def make_copy(destination, task, repeat):
    destination.mkdir(parents=True, exist_ok=True)
    archive = subprocess.check_output(['git', 'archive', 'HEAD'], cwd=ROOT)
    with tarfile.open(fileobj=io.BytesIO(archive), mode='r:') as tar:
        tar.extractall(destination, filter='data')
    # The server's Rust offline registry override is deliberately untracked.
    # Copy only this non-secret build setting so identical source copies can
    # resolve already-cached dependencies without network access.
    cargo_config = ROOT / '.cargo/config.toml'
    if cargo_config.is_file():
        (destination / '.cargo').mkdir(exist_ok=True)
        (destination / '.cargo/config.toml').write_bytes(cargo_config.read_bytes())
    if task == 'A':
        path = destination / 'src/sessions.rs'
        text = path.read_text()
        original = '''                // The JSON checkpoint can be old while the WAL contains the
                // latest turns; order by the most recent durable write.
                let wal = path.with_extension("json.wal");
                let modified = entry
                    .metadata()?
                    .modified()
                    .ok()
                    .into_iter()
                    .chain(fs::metadata(wal).and_then(|meta| meta.modified()).ok())
                    .max();'''
        broken = '''                // Regression: ordering currently looks only at the checkpoint.
                let modified = entry.metadata()?.modified().ok();'''
        assert text.count(original) == 1, 'fixture drift: session ordering code changed'
        regression = '''
#[cfg(test)]
mod engineering_regression {
    use super::*;
    #[test]
    fn wal_append_controls_recent_session_order() {
        let root = std::env::temp_dir().join(format!(
            "kroma-benchmark-wal-{}-{}", std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let catalog = SessionCatalog::from_default(root.join("sample.json")).unwrap();
        let alpha = root.join("sample--alpha.json");
        let bravo = root.join("sample--bravo.json");
        fs::write(&alpha, "checkpoint").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(&bravo, "checkpoint").unwrap();
        assert_eq!(catalog.list().unwrap()[0].id, "bravo");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(alpha.with_extension("json.wal"), "newer WAL entry").unwrap();
        assert_eq!(catalog.list().unwrap()[0].id, "alpha");
        fs::remove_dir_all(root).unwrap();
    }
}
'''
        path.write_text(text.replace(original, broken) + regression)
    else:
        if task in ('B', 'C'):
            (destination / 'bench-profile.json').write_text(json.dumps({
                'active': 'alpha',
                'profiles': {
                    'alpha': {'base_url': 'https://alpha.example/v1',
                              'model': 'alpha-model', 'api_key': KEY_TEST,
                              'deepseek_thinking': 'high'},
                    'beta': {'base_url': 'https://beta.example/v1',
                             'model': 'beta-model'},
                },
            }, indent=2) + '\n')
            os.chmod(destination / 'bench-profile.json', 0o600)
        elif task == 'D':
            (destination / 'continuation_note.txt').write_text(
                f'MARKER=RECOVERY_BENCH_{repeat}_R9H4T2\n'
            )


def invoke(root, runner, prompt, session, timeout, persist_session=False):
    env = os.environ.copy()
    env.update({'CARGO_TARGET_DIR': str(ROOT / 'target'),
                'KROMA_WORKDIR': str(root),
                'KROMA_SESSION_FILE': str(session),
                'KROMA_PROFILE': '1'})
    if runner == 'pi':
        cmd = PI + (['--session', str(session)] if persist_session else ['--no-session']) + [prompt]
    else:
        cmd = [KROMA, prompt]
    # Pi must not inherit Kroma-only instrumentation or session overrides.
    # Keep both runners' model settings unchanged.
    if runner == 'pi':
        for key in ('KROMA_WORKDIR', 'KROMA_SESSION_FILE', 'KROMA_PROFILE',
                    'KROMA_CONFIG_FILE', 'KROMA_DEEPSEEK_THINKING'):
            env.pop(key, None)
    started = time.monotonic()
    proc = subprocess.Popen(cmd, cwd=root, env=env, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            start_new_session=True)
    timed_out = False
    streams = {'stdout': bytearray(), 'stderr': bytearray()}
    json_line = bytearray()
    events = {}
    tools_started = 0
    tools_finished = 0
    full_messages = 0
    updates = 0
    last_event = 'spawn'
    last_event_at = 0.
    event_counts = {}
    selector = selectors.DefaultSelector()
    selector.register(proc.stdout, selectors.EVENT_READ, 'stdout')
    selector.register(proc.stderr, selectors.EVENT_READ, 'stderr')
    def see_event(raw):
        nonlocal last_event, last_event_at, tools_started, tools_finished, full_messages, updates
        try: event = json.loads(raw)
        except (json.JSONDecodeError, UnicodeDecodeError):
            return
        kind = event.get('type')
        if not kind:
            return
        now = round(time.monotonic() - started, 3)
        event_counts[kind] = event_counts.get(kind, 0) + 1
        if kind == 'message_end':
            if event.get('message', {}).get('role') == 'assistant':
                full_messages += 1
                events.setdefault('first_assistant_message_s', now)
        elif kind == 'message_update':
            updates += 1
            events.setdefault('first_message_update_s', now)
        elif kind == 'tool_execution_start':
            tools_started += 1
            events.setdefault('first_tool_start_s', now)
        elif kind == 'tool_execution_end':
            tools_finished += 1
        if kind in ('agent_start', 'turn_start', 'message_start',
                    'message_update', 'message_end', 'tool_execution_start',
                    'tool_execution_end', 'agent_end'):
            last_event = kind
            last_event_at = now
    try:
        while selector.get_map():
            remaining = timeout - (time.monotonic() - started)
            if remaining <= 0:
                timed_out = True
                break
            ready = selector.select(timeout=min(0.6, remaining))
            if not ready and proc.poll() is not None:
                # The process exited; continue draining any remaining pipe data.
                continue
            for key, _ in ready:
                kind = key.data
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                streams[kind].extend(chunk)
                if runner == 'pi' and kind == 'stdout':
                    json_line.extend(chunk)
                    while b'\n' in json_line:
                        line, _, remainder = json_line.partition(b'\n')
                        json_line = bytearray(remainder)
                        see_event(line)
        if timed_out and proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=10)
    finally:
        selector.close()
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait(timeout=10)
    stdout = streams['stdout'].decode('utf-8', errors='replace')
    stderr = streams['stderr'].decode('utf-8', errors='replace')
    elapsed = round(time.monotonic() - started, 3)
    result = {'exit_code': proc.returncode, 'elapsed_s': elapsed,
              'timeout': timed_out, 'requests': None, 'tools': None,
              'input': None, 'cached': None, 'output': None, 'answer': '',
              'stream_diagnostic': {
                  'events': events, 'last_event': last_event,
                  'last_event_at_s': last_event_at,
                  'event_counts': event_counts,
                  'tool_starts': tools_started, 'tool_finishes': tools_finished,
                  'assistant_messages': full_messages, 'message_updates': updates,
                  'stdout_bytes': len(streams['stdout']), 'stderr_bytes': len(streams['stderr']),
              }}
    if runner == 'pi':
        usage = []; tool_calls = 0; answer = ''
        for line in stdout.splitlines():
            try: event = json.loads(line)
            except json.JSONDecodeError: continue
            if event.get('type') != 'message_end': continue
            message = event.get('message', {})
            if message.get('role') != 'assistant': continue
            if message.get('usage'): usage.append(message['usage'])
            tool_calls += sum(p.get('type') == 'toolCall'
                              for p in message.get('content', []))
            text = ''.join(p.get('text', '') for p in message.get('content', [])
                           if p.get('type') == 'text')
            if text: answer = text
        result.update(requests=len(usage), tools=tool_calls, answer=answer[:2000])
        for src, key in [('input', 'input'), ('cacheRead', 'cached'),
                         ('output', 'output')]:
            result[key] = sum(u.get(src, 0) or 0 for u in usage) if usage else None
    else:
        usage = re.search(r'\[usage\] task: (\d+) request\(s\).*?input: (\d+|n/a).*?output: (\d+|n/a).*?cached: (\d+|n/a)', stderr)
        profile = re.search(r'\[profile\] wall_ms=[^\n]*?\btool_calls=(\d+)', stderr)
        if usage:
            result['requests'] = int(usage.group(1))
            for key, idx in [('input', 2), ('output', 3), ('cached', 4)]:
                result[key] = int(usage.group(idx)) if usage.group(idx) != 'n/a' else None
        if profile: result['tools'] = int(profile.group(1))
        result['answer'] = stdout[-2000:]
    # Error summaries deliberately omit raw stderr to avoid recording API
    # secrets that a provider or shell might have echoed unexpectedly.
    result['failure_kind'] = ('timeout' if timed_out else
                              'nonzero_exit' if proc.returncode else None)
    return result


def cli(root, *args, extra=None):
    env = os.environ.copy()
    env['CARGO_TARGET_DIR'] = str(ROOT / 'target')
    env['KROMA_CONFIG_FILE'] = str(root / 'bench-profile.json')
    if extra: env.update(extra)
    return subprocess.run(['cargo', 'run', '--quiet', '--locked', '--offline', '--',
                           'config', *args], cwd=root, env=env, capture_output=True,
                          text=True, timeout=75)


def verify(root, task, repeat):
    if task == 'A':
        p = subprocess.run(['cargo', 'test', '--quiet', '--locked', '--offline',
                            'wal_append_controls_recent_session_order'], cwd=root,
                           env={**os.environ, 'CARGO_TARGET_DIR': str(ROOT / 'target')},
                           capture_output=True, text=True, timeout=75)
        return p.returncode == 0
    if task == 'B':
        before = json.loads((root / 'bench-profile.json').read_text())
        assert list(before['profiles']) == ['alpha', 'beta']
        a = cli(root, 'remove', 'alpha')
        after = json.loads((root / 'bench-profile.json').read_text())
        ok = a.returncode == 0 and after['active'] == 'beta'
        ok &= list(after['profiles']) == ['beta']
        ok &= 'api_key' not in a.stdout
        current = (root / 'bench-profile.json').read_bytes()
        missing = cli(root, 'remove', 'not-found')
        ok &= missing.returncode != 0 and current == (root / 'bench-profile.json').read_bytes()
        last = cli(root, 'remove', 'beta')
        after = json.loads((root / 'bench-profile.json').read_text())
        ok &= last.returncode == 0 and not after['profiles'] and after.get('active') is None
        ok &= 'config remove' in (root / 'README.md').read_text()
        ok &= 'config remove' in (root / 'src/main.rs').read_text()
        return bool(ok)
    if task == 'C':
        show = cli(root, 'show', '--json', extra={'KROMA_MODEL': 'overridden-model'})
        listing = cli(root, 'list', '--json')
        if show.returncode or listing.returncode: return False
        if KEY_TEST in show.stdout or KEY_TEST in listing.stdout: return False
        try:
            item = json.loads(show.stdout)
            entries = json.loads(listing.stdout)
        except (json.JSONDecodeError, ValueError): return False
        if not isinstance(item, dict) or not isinstance(entries, list): return False
        if item.get('model') != 'overridden-model' or item.get('host') != 'alpha.example': return False
        if item.get('thinking') != 'high' or item.get('key_configured') is not True: return False
        if [e.get('name') for e in entries] != ['alpha', 'beta']: return False
        if entries[0].get('active') is not True or entries[1].get('active') is not False: return False
        if entries[0].get('key_configured') is not True or entries[1].get('key_configured') is not False: return False
        if entries[1].get('host') != 'beta.example': return False
        if 'show --json' not in (root / 'README.md').read_text(): return False
        if 'list --json' not in (root / 'README.md').read_text(): return False
        tests = subprocess.run(['cargo','test','--quiet','--locked','--offline'], cwd=root,
                               env={**os.environ,'CARGO_TARGET_DIR':str(ROOT/'target')},
                               capture_output=True,text=True,timeout=90)
        return tests.returncode == 0
    raise ValueError('unknown source task')


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--runner', choices=('pi', 'kroma'), required=True)
    ap.add_argument('--task', choices=('A', 'B', 'C', 'D'), action='append')
    ap.add_argument('--repeat', type=int, default=2)
    ap.add_argument('--timeout', type=int, default=210)
    ap.add_argument('--output', type=Path)
    ap.add_argument('--self-test', action='store_true')
    args = ap.parse_args()
    if not 1 <= args.repeat <= 3: ap.error('repeat range is 1..3')
    if args.runner == 'kroma' and not (os.getenv('KROMA_MODEL') or
                                        (Path.home()/'.kroma/config.json').is_file()):
        ap.error('Kroma provider config absent; set it in the shell that runs this suite')
    out = args.output or Path(f'/tmp/kroma-engineering-{args.runner}.json')
    root = Path(tempfile.mkdtemp(prefix=f'kroma-engineering-{args.runner}-'))
    report = {'schema': 1, 'source_sha': SOURCE_SHA, 'runner': args.runner,
              'model_alias': MODEL, 'case_results': [], 'root': str(root)}
    cases = args.task or ['A','B','C','D']

    def save():
        out.write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')

    for task in cases:
        repeats = 1 if task == 'D' else args.repeat
        for repeat in range(1, repeats + 1):
            work = root / f'{task}-{repeat}'
            make_copy(work, task, repeat)
            session = root / f'{task}-{repeat}-session.json'
            prompts = [A_PROMPT] if task == 'A' else [B_PROMPT] if task == 'B' else list(C_PROMPTS) if task == 'C' else [D_FIRST, D_NEXT]
            steps = []
            for index, prompt in enumerate(prompts):
                if task == 'D' and index == 1:
                    (work / 'continuation_note.txt').unlink()
                result = invoke(work, args.runner, prompt, session, args.timeout,
                                persist_session=task in ('C', 'D'))
                steps.append(result)
                print(f'{args.runner} {task}-{repeat} stage={index+1}/{len(prompts)} '
                      f'elapsed={result["elapsed_s"]}s requests={result["requests"]} '
                      f'exit={result["exit_code"]}', flush=True)
                if result['exit_code'] != 0: break
            if task == 'D':
                passed = len(steps) == 2 and all(s['exit_code'] == 0 for s in steps)
                passed &= f'RECOVERY_BENCH_{repeat}_R9H4T2' in steps[-1]['answer']
            else:
                passed = len(steps) == len(prompts) and all(s['exit_code'] == 0 for s in steps)
                if passed: passed = verify(work, task, repeat)
            row = {'task': task, 'repeat': repeat, 'passed': bool(passed),
                   'wall_s': round(sum(s['elapsed_s'] for s in steps),3),
                   'requests': sum(s['requests'] for s in steps) if all(s['requests'] is not None for s in steps) else None,
                   'tools': sum(s['tools'] for s in steps) if all(s['tools'] is not None for s in steps) else None,
                   'steps': [{k:v for k,v in step.items() if k != 'answer'} for step in steps],
                   'workdir': str(work)}
            report['case_results'].append(row)
            save()
            print(f'CASE {args.runner} {task}-{repeat} pass={row["passed"]} wall={row["wall_s"]}s',flush=True)
    rows = report['case_results']
    report['summary'] = {'count':len(rows),'passed':sum(x['passed'] for x in rows),
                         'total_wall_s':round(sum(x['wall_s'] for x in rows),2),
                         'median_wall_s':statistics.median(x['wall_s'] for x in rows)}
    save()
    print('SUMMARY',json.dumps(report['summary']), 'OUTPUT',out,flush=True)


if __name__ == '__main__':
    main()
