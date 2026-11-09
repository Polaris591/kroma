"""Offline controls for the nested Ponytail quality-paragraph contest."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('ponytail_contest', HERE / 'prompt_philosophy_ablation.py')
contest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contest)
ASSETS = HERE / 'ponytail'


class PonytailAblationTests(unittest.TestCase):
    def test_variants_only_replace_quality_and_b_is_native_production(self):
        source = (HERE.parent / 'src/prompt.rs').read_text(encoding='utf-8')
        baseline = contest.extract_baseline(source)
        variants, native = contest.quality_variants(baseline, ASSETS, contest.ARMS)
        self.assertEqual(native, 'B')
        self.assertEqual(variants['B'], baseline)
        paragraphs = {arm: (ASSETS / f'{arm}.txt').read_text().strip() for arm in contest.ARMS}
        common = variants['C'].replace(paragraphs['C'], '<quality>', 1)
        for arm in contest.ARMS:
            self.assertEqual(variants[arm].replace(paragraphs[arm], '<quality>', 1), common)
        reuse = ('Before adding code, prefer suitable project implementations, then '
                 'standard-library or platform capabilities and existing dependencies, '
                 'before writing new code. ')
        complete = ('Identify affected callers, interfaces, configuration, and tests; '
                    'make the smallest complete change across that scope without weakening '
                    'readability, compatibility, security, or required verification, or '
                    'silently reducing the requested scope. ')
        self.assertEqual(paragraphs['B'].replace(reuse, ''), paragraphs['A'])
        self.assertEqual(paragraphs['C'].replace(complete, ''), paragraphs['B'])

    def test_variants_require_one_native_paragraph_and_single_line(self):
        baseline = contest.extract_baseline((HERE.parent / 'src/prompt.rs').read_text())
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            for arm in contest.ARMS:
                (directory / f'{arm}.txt').write_text('different quality')
            with self.assertRaisesRegex(RuntimeError, 'exactly one quality variant'):
                contest.quality_variants(baseline, directory, contest.ARMS)
            (directory / 'A.txt').write_text('bad\nparagraph')
            with self.assertRaisesRegex(RuntimeError, 'single paragraphs'):
                contest.quality_variants(baseline, directory, contest.ARMS)

    def test_setup_freezes_quality_tasks_untracked_modules_and_native_arm(self):
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            config = base / 'config.json'
            config.write_text(json.dumps({'profiles': {'deepseek': {
                'model': contest.MODEL, 'base_url': 'https://proxy.example/v1',
                'api_key': 'OFFLINE_DUMMY_CREDENTIAL_NOT_REAL', 'api_protocol': 'chat',
                'reasoning_effort': 'high', 'deepseek_thinking': 'high'}}}))
            lab = base / 'lab'
            tasks = list(contest.TASKS + contest.EXTRA_TASKS)
            contest.setup(SimpleNamespace(lab=lab, config=config, profile='deepseek',
                          repeats=3, timeout=300, arms='A,B,C', tasks=tasks,
                          quality_variants=ASSETS))
            metadata = json.loads((lab / 'metadata.json').read_text())
            self.assertEqual(metadata['native_arm'], 'B')
            self.assertEqual(metadata['tasks'], tasks)
            self.assertEqual(len(json.loads((lab / 'plan.json').read_text())), 21)
            self.assertTrue((lab / 'oracles/ponytail_tasks.py').is_file())
            self.assertNotIn('OFFLINE_DUMMY_CREDENTIAL_NOT_REAL', (lab / 'metadata.json').read_text())
            for module in (HERE.parent / 'src').glob('*.rs'):
                self.assertTrue((lab / 'runtime/src' / module.name).is_file(), module.name)
            for arm in contest.ARMS:
                trial = lab / 'trial'
                env = contest.child_env(lab, metadata, arm, trial)
                self.assertEqual('KROMA_ABLATION_GUIDANCE_FILE' in env, arm != 'B')
                self.assertNotIn(b'\r', (lab / f'prompts/{arm}.txt').read_bytes())
            self.assertEqual((lab / 'prompts/B.txt').read_bytes(),
                             contest.extract_baseline((HERE.parent / 'src/prompt.rs').read_text()).encode())
            suite, complex_tasks = contest.modules(lab)
            for task in contest.EXTRA_TASKS:
                work = lab / task
                prompt = contest.prepare(work, task, suite, complex_tasks)
                self.assertTrue(prompt.endswith(contest.GUARD))
                self.assertFalse(contest.oracle(work, task, '', suite, complex_tasks))

    def test_child_settings_match_frozen_profile_without_rewriting_configuration(self):
        metadata = {'config_file': 'unused', 'profile_name': 'deepseek',
                    'profile': {'reasoning_effort': 'max', 'deepseek_thinking': 'enabled'}}
        env = contest.child_env(Path('unused'), metadata)
        self.assertEqual(env['KROMA_REASONING_EFFORT'], 'max')
        self.assertEqual(env['KROMA_DEEPSEEK_THINKING'], 'enabled')

    def test_explicit_kroma_prefixed_credential_env_survives_sanitization(self):
        name = 'KROMA_OFFLINE_CONTEST_TEST_KEY'
        metadata = {'config_file': 'unused', 'profile_name': 'deepseek',
                    'profile': {'api_key_env': name}}
        with patch.dict(os.environ, {name: 'OFFLINE_DUMMY_NOT_REAL'}):
            env = contest.child_env(Path('unused'), metadata)
        self.assertEqual(env[name], 'OFFLINE_DUMMY_NOT_REAL')

    def test_guard_matches_immutable_supplied_test_contract(self):
        self.assertIn('Do not edit supplied test files', contest.GUARD)
        self.assertIn('put additional tests in new files', contest.GUARD)

    def test_quality_and_append_are_mutually_exclusive(self):
        with tempfile.TemporaryDirectory() as temp:
            args = SimpleNamespace(lab=Path(temp) / 'lab', repeats=3, arms='A,B',
                                   append_guidance=Path('unused'), quality_variants=ASSETS)
            with self.assertRaisesRegex(RuntimeError, 'mutually exclusive'):
                contest.setup(args)
            self.assertFalse(args.lab.exists())

    def test_duplicate_and_unknown_tasks_rejected_before_setup(self):
        for tasks in [('fix_arithmetic', 'fix_arithmetic'), ('not_a_task',)]:
            with self.subTest(tasks=tasks), tempfile.TemporaryDirectory() as temp:
                args = SimpleNamespace(lab=Path(temp) / 'lab', repeats=3, arms='A,B,C', tasks=tasks)
                with self.assertRaisesRegex(RuntimeError, 'distinct supported task'):
                    contest.setup(args)
                self.assertFalse(args.lab.exists())


if __name__ == '__main__':
    unittest.main(verbosity=2)
