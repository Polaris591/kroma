"""Offline controls for the live prompt contest; no paid model requests."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from types import SimpleNamespace

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('philosophy', HERE / 'prompt_philosophy_ablation.py')
contest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contest)
suite = contest.load_module('suite_for_test', HERE / 'agent_task_suite.py')


class PromptContestTests(unittest.TestCase):
    def test_baseline_is_exact_rust_guidance(self):
        text = (HERE.parent / 'src/prompt.rs').read_text(encoding='utf-8')
        guidance = contest.extract_baseline(text)
        self.assertTrue(guidance.startswith('\nFor coding tasks,'))
        self.assertEqual(guidance.count('For coding tasks,'), 1)
        self.assertEqual(guidance.count('Give a brief user-facing progress update'), 1)
        self.assertTrue(guidance.endswith('Investigate tooling internals only when the evidence points there.'))
        self.assertEqual(text.count(contest.DEFAULT_ADDITIONS), 1)
        self.assertEqual(text.count(contest.PROMPT_ANCHOR), 1)

    def test_plan_is_reproducible_and_balanced(self):
        plan = contest.make_plan(contest.TASKS, 3)
        self.assertEqual(plan, contest.make_plan(contest.TASKS, 3))
        self.assertEqual(len(plan), 15)
        for task in contest.TASKS:
            blocks = [block for block in plan if block['task'] == task]
            self.assertEqual(sorted(block['repeat'] for block in blocks), [1, 2, 3])
            for position in range(3):
                self.assertEqual(sorted(block['order'][position] for block in blocks), ['A', 'B', 'C'])

    def test_quality_and_validation_identical_between_b_c(self):
        b = (contest.ASSETS / 'B.txt').read_text(encoding='utf-8').splitlines()
        c = (contest.ASSETS / 'C.txt').read_text(encoding='utf-8').splitlines()
        self.assertEqual(b[1:3], c[1:3])
        self.assertNotEqual(b[0], c[0])
        self.assertNotEqual(b[3], c[3])
        for version in (b, c):
            self.assertIn('Do not omit required verification', version[2])

    def test_unknown_usage_remains_unknown(self):
        result = contest.metrics([], '', suite)
        self.assertIsNone(result['usage']['input'])
        self.assertIsNone(result['usage']['cached'])
        self.assertIsNone(result['usage']['uncached'])
        self.assertIsNone(result['monetary_cost'])

    def test_combined_usage_is_not_double_counted(self):
        log = ('[usage] task: 1 request(s) | input: 200 | output: 20 | cached: 50\n'
               '[usage] combined: 2 model request(s) | input: 1200 | output: 40 | cached: 60')
        result = contest.metrics([{'type': 'tool_started'},
                                  {'type': 'tool_finished', 'is_error': True}], log, suite)
        self.assertEqual(result['usage']['requests'], 2)
        self.assertEqual(result['usage']['input'], 1200)
        self.assertEqual(result['usage']['uncached'], 1140)
        self.assertEqual(result['tool_calls'], 1)
        self.assertEqual(result['tool_errors'], 1)

    def test_control_audit_rejects_schema_changes(self):
        with tempfile.TemporaryDirectory() as temp:
            lab = Path(temp)
            (lab / 'prompts').mkdir()
            trial = lab / 'trial'
            trial.mkdir()
            for arm in contest.ARMS:
                (lab / f'prompts/{arm}.txt').write_text('\n' + arm, encoding='utf-8')
            base = 'You are Kroma, an AI coding agent. frozen base'
            (trial / 'system.txt').write_text(base + '\nA', encoding='utf-8')
            (trial / 'tools.json').write_text('[]', encoding='utf-8')
            contest.audit(lab, trial, 'A')
            (trial / 'system.txt').write_text(base + '\nB', encoding='utf-8')
            contest.audit(lab, trial, 'B')
            (trial / 'tools.json').write_text('[{}]', encoding='utf-8')
            with self.assertRaisesRegex(RuntimeError, 'tools_sha256'):
                contest.audit(lab, trial, 'B')

    def test_all_coding_baselines_fail_independent_oracle(self):
        import ast
        tree = ast.parse((HERE / 'pi_kroma_complexity_matrix.py').read_text(encoding='utf-8'))
        constants = {}
        for statement in tree.body:
            if isinstance(statement, ast.Assign) and len(statement.targets) == 1:
                name = getattr(statement.targets[0], 'id', None)
                if name in ('PY_PROMPT', 'FILES', 'HIDDEN_CHECK'):
                    constants[name] = ast.literal_eval(statement.value)
        complex_tasks = SimpleNamespace(**constants)
        for task in contest.TASKS:
            with self.subTest(task=task), tempfile.TemporaryDirectory() as temp:
                work = Path(temp) / 'work'
                contest.prepare(work, task, suite, complex_tasks)
                self.assertFalse(contest.oracle(work, task, '', suite, complex_tasks))
                self.assertEqual(list(work.rglob('__pycache__')), [])

    def test_external_acceptance_cannot_be_weakened_by_editing_public_tests(self):
        with tempfile.TemporaryDirectory() as temp:
            work = Path(temp) / 'work'
            contest.prepare(work, 'fix_arithmetic', suite, None)
            (work / 'test_math_ops.py').write_text('', encoding='utf-8')
            self.assertFalse(contest.oracle(work, 'fix_arithmetic', '', suite, None))
            (work / 'math_ops.py').write_text(
                'def average(values):\n    return sum(values) / len(values) if values else 0\n', encoding='utf-8')
            self.assertTrue(contest.oracle(work, 'fix_arithmetic', '', suite, None))

    def test_setup_snapshots_build_script_and_oracles_without_credentials(self):
        import os
        import subprocess
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            config = base / 'config.json'
            config.write_text(json.dumps({'profiles': {'deepseek': {
                'model': contest.MODEL, 'base_url': 'https://proxy.example/v1',
                'api_key': 'OFFLINE_DUMMY_CREDENTIAL_NOT_REAL',
                'api_protocol': 'chat', 'reasoning_effort': 'high',
                'deepseek_thinking': 'high'}}}), encoding='utf-8')
            lab = base / 'experiment'
            contest.setup(SimpleNamespace(lab=lab, config=config, profile='deepseek',
                                          repeats=3, timeout=None))
            self.assertTrue((lab / 'runtime/build.rs').is_file())
            self.assertEqual((lab / 'runtime/build.rs').read_bytes(),
                             (HERE.parent / 'build.rs').read_bytes())
            metadata = json.loads((lab / 'metadata.json').read_text())
            self.assertIn('runner_sha256', metadata)
            self.assertNotIn('OFFLINE_DUMMY_CREDENTIAL_NOT_REAL',
                             (lab / 'metadata.json').read_text())
            fixture_suite, complex_tasks = contest.modules(lab)
            self.assertEqual(len(fixture_suite.TASKS), 6)
            self.assertIn('tracker/service.py', complex_tasks.FILES)
            env = contest.child_env(lab, metadata)
            interpreter = lab / 'python-bin' / ('python.exe' if os.name == 'nt' else 'python')
            result = subprocess.run([str(interpreter), '-c', 'import unittest,json'],
                                    env=env, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr.decode(errors='replace'))

    def test_audit_rejects_line_ending_changes(self):
        with tempfile.TemporaryDirectory() as temp:
            lab = Path(temp)
            (lab / 'prompts').mkdir()
            trial = lab / 'trial'
            trial.mkdir()
            guidance = chr(10) + 'A' + chr(10) + 'Z'
            base = 'You are Kroma, an AI coding agent. frozen base'
            (lab / 'prompts/A.txt').write_bytes(guidance.encode('utf-8'))
            (trial / 'system.txt').write_bytes((base + guidance).encode('utf-8'))
            (trial / 'tools.json').write_bytes(b'[]')
            contest.audit(lab, trial, 'A')
            altered = base + guidance.replace(chr(10), chr(13) + chr(10))
            (trial / 'system.txt').write_bytes(altered.encode('utf-8'))
            with self.assertRaisesRegex(RuntimeError, 'missing frozen arm guidance'):
                contest.audit(lab, trial, 'A')

    def test_two_arm_plan_is_balanced_for_each_task(self):
        plan = contest.make_plan(contest.TASKS, 4, arms=('A', 'B'))
        self.assertEqual(plan, contest.make_plan(contest.TASKS, 4, arms=('A', 'B')))
        self.assertEqual(len(plan), 20)
        for task in contest.TASKS:
            blocks = [block for block in plan if block['task'] == task]
            self.assertEqual(sorted(block['repeat'] for block in blocks), [1, 2, 3, 4])
            for position in range(2):
                self.assertEqual(sorted(block['order'][position] for block in blocks),
                                 ['A', 'A', 'B', 'B'])

    def test_appended_sentence_changes_only_b_and_report_has_two_arms(self):
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            config = base / 'config.json'
            config.write_text(json.dumps({'profiles': {'deepseek': {
                'model': contest.MODEL, 'base_url': 'https://proxy.example/v1',
                'api_key': 'OFFLINE_DUMMY_CREDENTIAL_NOT_REAL',
                'api_protocol': 'chat', 'reasoning_effort': 'high',
                'deepseek_thinking': 'high'}}}), encoding='utf-8')
            sentence_path = HERE / 'read_batching/append.txt'
            sentence = sentence_path.read_text(encoding='utf-8').strip()
            lab = base / 'experiment'
            contest.setup(SimpleNamespace(lab=lab, config=config, profile='deepseek',
                          repeats=4, timeout=None, arms='A,B', append_guidance=sentence_path))
            a = (lab / 'prompts/A.txt').read_text(encoding='utf-8')
            b = (lab / 'prompts/B.txt').read_text(encoding='utf-8')
            original = contest.extract_baseline((HERE.parent / 'src/prompt.rs').read_text(encoding='utf-8'))
            self.assertEqual(a, original)
            self.assertEqual((lab / 'prompts/A.txt').read_bytes(), original.encode('utf-8'))
            self.assertEqual((lab / 'prompts/B.txt').read_bytes(),
                             original.encode('utf-8') + bytes([10]) + sentence.encode('utf-8'))
            self.assertEqual(b, a + '\n' + sentence)
            self.assertFalse((lab / 'prompts/C.txt').exists())
            metadata = json.loads((lab / 'metadata.json').read_text())
            self.assertEqual(metadata['arms'], ['A', 'B'])
            self.assertEqual(set(metadata['variants']), {'A', 'B'})
            self.assertEqual(metadata['append_guidance'], sentence)
            self.assertEqual(metadata['plan_sha256'], contest.sha((lab / 'plan.json').read_bytes()))
            self.assertEqual(set(contest.report(lab)), {'A', 'B'})

    def test_append_guidance_rejects_a_three_arm_plan(self):
        with tempfile.TemporaryDirectory() as temp:
            args = SimpleNamespace(lab=Path(temp) / 'experiment', repeats=4,
                                   arms='A,B,C', append_guidance=HERE / 'read_batching/append.txt')
            with self.assertRaisesRegex(RuntimeError, 'requires exactly A,B'):
                contest.setup(args)
            self.assertFalse(args.lab.exists())

    def test_refuse_to_overwrite_existing_lab(self):
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaisesRegex(RuntimeError, 'refusing to overwrite'):
                contest.setup(SimpleNamespace(lab=Path(temp)))


if __name__ == '__main__':
    unittest.main(verbosity=2)
