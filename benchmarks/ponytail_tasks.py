"""Offline synthetic tasks for the Ponytail principle ablation contest.

Each task is a tiny, dependency-free project. ``create_fixture`` writes it and
returns the English task prompt; ``verify`` grades the result from a separate
isolated interpreter and never trusts anything inside the work tree. The
acceptance script lives here, not in the fixture, so editing or deleting the
public tests cannot change the verdict.

Only the Python standard library is used and no network access is required.
"""

import subprocess
import sys
import textwrap
from pathlib import Path


TASKS = (
    {
        "id": "reuse_existing",
        "prompt": (
            "Implement release_tag(version) in release.py. It must return the "
            "string 'rel-' followed by the 7-character digest produced by "
            "helpers.short_hash(version), so release_tag('v1.2.3') is "
            "'rel-' plus that digest. Reuse the existing helpers.short_hash "
            "helper instead of reimplementing the hashing or hard-coding "
            "digests, and keep the behavior of helpers.py and changelog.py "
            "unchanged. Run the tests when you are done."
        ),
        "files": {
            "helpers.py": (
                '"""Small shared helpers for the release tooling."""\n'
                "\n"
                "import hashlib\n"
                "\n"
                "\n"
                "def short_hash(text, length=7):\n"
                '    """Return a short stable digest of text."""\n'
                '    return hashlib.sha1(text.encode("utf-8")).hexdigest()[:length]\n'
            ),
            "changelog.py": (
                "from helpers import short_hash\n"
                "\n"
                "\n"
                "def changelog_id(title):\n"
                '    """Return the changelog entry id for a title."""\n'
                '    return "chg-" + short_hash(title)\n'
            ),
            "release.py": (
                '"""Release publishing helpers."""\n'
                "\n"
                "\n"
                "def release_tag(version):\n"
                '    """Return the tag used to publish a release."""\n'
                '    raise NotImplementedError("release_tag is not implemented")\n'
            ),
            "test_release.py": (
                "import unittest\n"
                "\n"
                "from changelog import changelog_id\n"
                "\n"
                "\n"
                "class ChangelogTest(unittest.TestCase):\n"
                "    def test_id_prefix(self):\n"
                '        self.assertTrue(changelog_id("Fix crash").startswith("chg-"))\n'
                "\n"
                "\n"
                'if __name__ == "__main__":\n'
                "    unittest.main()\n"
            ),
        },
    },
    {
        "id": "complete_change",
        "prompt": (
            "Extend the scheduling helpers so durations accept the day unit "
            "'d' (1 d = 86400 s). durations.seconds(text) must parse it and "
            "schedule.schedule_delay(text) must accept it for the same values, "
            "for example schedule_delay('3d') == 259200 and seconds('0d') == 0. "
            "Keep all existing behavior: the units s, m and h and their values, "
            "the public function names and parameter names (keyword calls such "
            "as schedule_delay(text='10m') must keep working), "
            "notify.reminder_delay, and ValueError for unknown or non-lowercase "
            "units and for an empty schedule string. Run the tests when you are "
            "done."
        ),
        "files": {
            "durations.py": (
                '"""Duration parsing helpers."""\n'
                "\n"
                '_MULTIPLIERS = {"s": 1, "m": 60, "h": 60 * 60}\n'
                "\n"
                "\n"
                "def seconds(text):\n"
                '    """Parse a short duration such as \'30s\' or \'2h\' into seconds."""\n'
                "    unit = text[-1]\n"
                "    try:\n"
                "        multiplier = _MULTIPLIERS[unit]\n"
                "    except KeyError:\n"
                "        raise ValueError(f\"unsupported unit: {unit}\") from None\n"
                "    return int(text[:-1]) * multiplier\n"
            ),
            "schedule.py": (
                '"""Public scheduling helpers."""\n'
                "\n"
                "from durations import seconds\n"
                "\n"
                'SUPPORTED_UNITS = ("s", "m", "h")\n'
                "\n"
                "\n"
                "def schedule_delay(text):\n"
                '    """Return the delay in seconds for a public duration string."""\n'
                "    if not text or text[-1] not in SUPPORTED_UNITS:\n"
                "        raise ValueError(f\"unsupported duration: {text!r}\")\n"
                "    return seconds(text)\n"
            ),
            "notify.py": (
                "from schedule import schedule_delay\n"
                "\n"
                "\n"
                "def reminder_delay(spec):\n"
                '    """Return a reminder delay string for a duration spec."""\n'
                '    return f"{schedule_delay(spec)}s"\n'
            ),
            "test_schedule.py": (
                "import unittest\n"
                "\n"
                "from notify import reminder_delay\n"
                "from schedule import schedule_delay\n"
                "\n"
                "\n"
                "class ScheduleTest(unittest.TestCase):\n"
                "    def test_hours(self):\n"
                '        self.assertEqual(schedule_delay("2h"), 7200)\n'
                "\n"
                "    def test_reminder(self):\n"
                '        self.assertEqual(reminder_delay("90m"), "5400s")\n'
                "\n"
                "\n"
                'if __name__ == "__main__":\n'
                "    unittest.main()\n"
            ),
        },
    },
)


_REUSE_EXISTING_CHECK = textwrap.dedent(
    """
    import hashlib
    import sys

    sys.path.insert(0, sys.argv[1])

    import helpers

    original = helpers.short_hash
    calls = []

    def spy(text, *args, **kwargs):
        calls.append(text)
        return original(text, *args, **kwargs)

    helpers.short_hash = spy

    import changelog
    import release

    probe = hashlib.sha1(b"probe").hexdigest()[:7]
    assert original("probe") == probe, "helpers.short_hash no longer matches its contract"

    for version in ("v1.2.3", "2.0.0-rc1"):
        expected = "rel-" + original(version)
        actual = release.release_tag(version)
        assert actual == expected, (version, actual, expected)
        assert version in calls, ("helpers.short_hash was not reused", version, calls)

    expected_id = "chg-" + original("Fix crash")
    assert changelog.changelog_id("Fix crash") == expected_id, "changelog behavior changed"
    assert "Fix crash" in calls, "changelog no longer reuses helpers.short_hash"
    """
)


_COMPLETE_CHANGE_CHECK = textwrap.dedent(
    """
    import sys

    sys.path.insert(0, sys.argv[1])

    import durations
    import notify
    import schedule

    assert durations.seconds("3d") == 259200
    assert durations.seconds("1d") == 86400
    assert durations.seconds("0d") == 0
    assert durations.seconds("2h") == 7200
    assert durations.seconds("90m") == 5400
    assert durations.seconds("45s") == 45

    assert schedule.schedule_delay("3d") == 259200
    assert schedule.schedule_delay("0d") == 0
    assert schedule.schedule_delay("2h") == 7200
    assert schedule.schedule_delay("90m") == 5400
    assert schedule.schedule_delay(text="10m") == 600

    assert notify.reminder_delay("2d") == "172800s"
    assert notify.reminder_delay("90m") == "5400s"

    for value in ("5y", "1D", "d"):
        for func in (durations.seconds, schedule.schedule_delay):
            try:
                func(value)
            except ValueError:
                pass
            else:
                raise AssertionError("accepted unsupported duration %r" % (value,))

    try:
        schedule.schedule_delay("")
    except ValueError:
        pass
    else:
        raise AssertionError("accepted empty schedule string")
    """
)


_CHECKS = {
    "reuse_existing": _REUSE_EXISTING_CHECK,
    "complete_change": _COMPLETE_CHANGE_CHECK,
}


def _task(task_id):
    for task in TASKS:
        if task["id"] == task_id:
            return task
    raise ValueError(f"unknown task {task_id}")


def create_fixture(work, task_id):
    """Write the task project into ``work`` and return its English prompt."""
    task = _task(task_id)
    root = Path(work)
    for name, text in task["files"].items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return task["prompt"]


def verify(work, task_id):
    """Return True only when the hidden acceptance checks pass in ``work``."""
    script = _CHECKS.get(task_id)
    if script is None:
        raise ValueError(f"unknown task {task_id}")
    try:
        result = subprocess.run(
            [sys.executable, "-I", "-B", "-c", script, str(work)],
            cwd=str(work), capture_output=True, text=True, timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return result.returncode == 0
