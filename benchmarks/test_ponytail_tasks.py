"""Offline unittest coverage for the Ponytail synthetic tasks.

No model requests, no network access: every case builds the fixture in a
temporary directory and grades it with the hidden acceptance script.
"""

import hashlib
import importlib.util
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("ponytail_tasks", HERE / "ponytail_tasks.py")
tasks = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tasks)


def _fixture(testcase, task_id):
    directory = tempfile.TemporaryDirectory()
    testcase.addCleanup(directory.cleanup)
    work = Path(directory.name)
    prompt = tasks.create_fixture(work, task_id)
    return work, prompt


def _write(work, name, text):
    (work / name).write_text(text, encoding="utf-8")


def _empty_public_tests(work):
    for path in work.glob("test_*.py"):
        path.write_text("", encoding="utf-8")


def _digest(text):
    return hashlib.sha1(text.encode("utf-8")).hexdigest()[:7]


_RELEASE_CORRECT = (
    "from helpers import short_hash\n"
    "\n"
    "\n"
    "def release_tag(version):\n"
    '    return "rel-" + short_hash(version)\n'
)

_RELEASE_ALTERNATE = {
    "module attribute": (
        "import helpers\n"
        "\n"
        "\n"
        "def release_tag(version):\n"
        "    digest = helpers.short_hash(version)\n"
        '    return f"rel-{digest}"\n'
    ),
    "local import": (
        "def release_tag(version):\n"
        "    from helpers import short_hash\n"
        '    return "rel-" + short_hash(version)\n'
    ),
}

_RELEASE_INLINE_HASH = (
    "import hashlib\n"
    "\n"
    "\n"
    "def release_tag(version):\n"
    '    return "rel-" + hashlib.sha1(version.encode("utf-8")).hexdigest()[:7]\n'
)

_RELEASE_HARDCODED = (
    "TAGS = {\n"
    '    "v1.2.3": "rel-' + _digest("v1.2.3") + '",\n'
    '    "2.0.0-rc1": "rel-' + _digest("2.0.0-rc1") + '",\n'
    "}\n"
    "\n"
    "\n"
    "def release_tag(version):\n"
    "    return TAGS[version]\n"
)

_CHANGELOG_HARDCODED = (
    "def changelog_id(title):\n"
    '    return "chg-' + _digest("Fix crash") + '"\n'
)

_DURATIONS_CORRECT = (
    '"""Duration parsing helpers."""\n'
    "\n"
    '_MULTIPLIERS = {"s": 1, "m": 60, "h": 60 * 60, "d": 24 * 60 * 60}\n'
    "\n"
    "\n"
    "def seconds(text):\n"
    "    unit = text[-1]\n"
    "    try:\n"
    "        multiplier = _MULTIPLIERS[unit]\n"
    "    except KeyError:\n"
    "        raise ValueError(f\"unsupported unit: {unit}\") from None\n"
    "    return int(text[:-1]) * multiplier\n"
)

_SCHEDULE_CORRECT = (
    '"""Public scheduling helpers."""\n'
    "\n"
    "from durations import seconds\n"
    "\n"
    'SUPPORTED_UNITS = ("s", "m", "h", "d")\n'
    "\n"
    "\n"
    "def schedule_delay(text):\n"
    "    if not text or text[-1] not in SUPPORTED_UNITS:\n"
    "        raise ValueError(f\"unsupported duration: {text!r}\")\n"
    "    return seconds(text)\n"
)

_DURATIONS_DROPS_HOURS = (
    "_MULTIPLIERS = {\"s\": 1, \"m\": 60, \"d\": 24 * 60 * 60}\n"
    "\n"
    "\n"
    "def seconds(text):\n"
    "    unit = text[-1]\n"
    "    try:\n"
    "        multiplier = _MULTIPLIERS[unit]\n"
    "    except KeyError:\n"
    "        raise ValueError(f\"unsupported unit: {unit}\") from None\n"
    "    return int(text[:-1]) * multiplier\n"
)

_DURATIONS_ACCEPTS_ANY = (
    "_MULTIPLIERS = {\"s\": 1, \"m\": 60, \"h\": 60 * 60, \"d\": 24 * 60 * 60}\n"
    "\n"
    "\n"
    "def seconds(text):\n"
    "    return int(text[:-1]) * _MULTIPLIERS.get(text[-1], 1)\n"
)


class TaskContractTests(unittest.TestCase):
    def test_tasks_are_two_independent_synthetic_projects(self):
        self.assertEqual([task["id"] for task in tasks.TASKS], ["reuse_existing", "complete_change"])
        for task in tasks.TASKS:
            self.assertEqual(set(task), {"id", "prompt", "files"})
            self.assertTrue(task["prompt"].isascii())
            self.assertTrue(task["prompt"].strip())
            self.assertTrue(task["files"])
            for name, text in task["files"].items():
                self.assertIsInstance(name, str)
                self.assertIsInstance(text, str)
                self.assertTrue(text.endswith("\n"))

    def test_create_fixture_writes_only_the_task_files_and_returns_prompt(self):
        for task in tasks.TASKS:
            with self.subTest(task=task["id"]):
                work, prompt = _fixture(self, task["id"])
                self.assertEqual(prompt, task["prompt"])
                self.assertEqual(
                    sorted(path.name for path in work.iterdir()),
                    sorted(task["files"]),
                )
                self.assertFalse((work / "__pycache__").exists())

    def test_unknown_task_id_is_rejected(self):
        work, _ = _fixture(self, "reuse_existing")
        with self.assertRaises(ValueError):
            tasks.create_fixture(work, "not_a_task")
        with self.assertRaises(ValueError):
            tasks.verify(work, "not_a_task")

    def test_missing_work_dir_fails_gracefully(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        missing = Path(directory.name) / "missing"
        self.assertFalse(tasks.verify(missing, "reuse_existing"))
        self.assertFalse(tasks.verify(missing, "complete_change"))


class ReuseExistingTests(unittest.TestCase):
    def test_initial_fixture_fails(self):
        work, _ = _fixture(self, "reuse_existing")
        self.assertFalse(tasks.verify(work, "reuse_existing"))

    def test_correct_solution_passes(self):
        work, _ = _fixture(self, "reuse_existing")
        _write(work, "release.py", _RELEASE_CORRECT)
        self.assertTrue(tasks.verify(work, "reuse_existing"))

    def test_reasonable_implementations_all_pass(self):
        for label, source in _RELEASE_ALTERNATE.items():
            with self.subTest(style=label):
                work, _ = _fixture(self, "reuse_existing")
                _write(work, "release.py", source)
                self.assertTrue(tasks.verify(work, "reuse_existing"))

    def test_empty_public_tests_do_not_pass(self):
        work, _ = _fixture(self, "reuse_existing")
        _empty_public_tests(work)
        self.assertFalse(tasks.verify(work, "reuse_existing"))

    def test_empty_public_tests_do_not_block_correct_solution(self):
        work, _ = _fixture(self, "reuse_existing")
        _write(work, "release.py", _RELEASE_CORRECT)
        _empty_public_tests(work)
        self.assertTrue(tasks.verify(work, "reuse_existing"))

    def test_reimplemented_hash_is_rejected(self):
        work, _ = _fixture(self, "reuse_existing")
        _write(work, "release.py", _RELEASE_INLINE_HASH)
        self.assertFalse(tasks.verify(work, "reuse_existing"))

    def test_hardcoded_digests_are_rejected(self):
        work, _ = _fixture(self, "reuse_existing")
        _write(work, "release.py", _RELEASE_HARDCODED)
        self.assertFalse(tasks.verify(work, "reuse_existing"))

    def test_changelog_regression_is_rejected(self):
        work, _ = _fixture(self, "reuse_existing")
        _write(work, "release.py", _RELEASE_CORRECT)
        _write(work, "changelog.py", _CHANGELOG_HARDCODED)
        self.assertFalse(tasks.verify(work, "reuse_existing"))

    def test_removed_changelog_is_rejected(self):
        work, _ = _fixture(self, "reuse_existing")
        _write(work, "release.py", _RELEASE_CORRECT)
        (work / "changelog.py").unlink()
        self.assertFalse(tasks.verify(work, "reuse_existing"))


class CompleteChangeTests(unittest.TestCase):
    def test_initial_fixture_fails(self):
        work, _ = _fixture(self, "complete_change")
        self.assertFalse(tasks.verify(work, "complete_change"))

    def test_correct_solution_passes(self):
        work, _ = _fixture(self, "complete_change")
        _write(work, "durations.py", _DURATIONS_CORRECT)
        _write(work, "schedule.py", _SCHEDULE_CORRECT)
        self.assertTrue(tasks.verify(work, "complete_change"))

    def test_durations_only_is_incomplete(self):
        work, _ = _fixture(self, "complete_change")
        _write(work, "durations.py", _DURATIONS_CORRECT)
        self.assertFalse(tasks.verify(work, "complete_change"))

    def test_schedule_only_is_incomplete(self):
        work, _ = _fixture(self, "complete_change")
        _write(work, "schedule.py", _SCHEDULE_CORRECT)
        self.assertFalse(tasks.verify(work, "complete_change"))

    def test_empty_public_tests_do_not_pass(self):
        work, _ = _fixture(self, "complete_change")
        _empty_public_tests(work)
        self.assertFalse(tasks.verify(work, "complete_change"))

    def test_empty_public_tests_do_not_block_correct_solution(self):
        work, _ = _fixture(self, "complete_change")
        _write(work, "durations.py", _DURATIONS_CORRECT)
        _write(work, "schedule.py", _SCHEDULE_CORRECT)
        _empty_public_tests(work)
        self.assertTrue(tasks.verify(work, "complete_change"))

    def test_broken_public_test_does_not_block_correct_solution(self):
        work, _ = _fixture(self, "complete_change")
        _write(work, "durations.py", _DURATIONS_CORRECT)
        _write(work, "schedule.py", _SCHEDULE_CORRECT)
        _write(work, "test_schedule.py", "raise RuntimeError('public test rewritten')\n")
        self.assertTrue(tasks.verify(work, "complete_change"))

    def test_dropping_existing_units_is_rejected(self):
        work, _ = _fixture(self, "complete_change")
        _write(work, "durations.py", _DURATIONS_DROPS_HOURS)
        _write(work, "schedule.py", _SCHEDULE_CORRECT)
        self.assertFalse(tasks.verify(work, "complete_change"))

    def test_accepting_unknown_units_is_rejected(self):
        work, _ = _fixture(self, "complete_change")
        _write(work, "durations.py", _DURATIONS_ACCEPTS_ANY)
        _write(work, "schedule.py", _SCHEDULE_CORRECT)
        self.assertFalse(tasks.verify(work, "complete_change"))

    def test_removed_public_caller_is_rejected(self):
        work, _ = _fixture(self, "complete_change")
        _write(work, "durations.py", _DURATIONS_CORRECT)
        _write(work, "schedule.py", _SCHEDULE_CORRECT)
        (work / "notify.py").unlink()
        self.assertFalse(tasks.verify(work, "complete_change"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
