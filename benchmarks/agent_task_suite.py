#!/usr/bin/env python3
"""Reproducible task-quality / latency / usage baseline for a real Kroma provider.

No network calls are made unless --run is explicitly specified. Model settings
come only from the KROMA_* environment variables. Synthetic tasks run in a
fresh temporary directory and do not modify the Kroma checkout.

  python3 benchmarks/agent_task_suite.py --list
  python3 benchmarks/agent_task_suite.py --self-test
  KROMA_BASE_URL=... KROMA_MODEL=... KROMA_API_KEY=... \
      python3 benchmarks/agent_task_suite.py --run --output /tmp/kroma-baseline.json
"""

import argparse
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import tempfile
import time


TASKS = (
    {
        "id": "exact_lookup",
        "prompt": (
            "Read settings.ini using shell and reply with the exact SESSION_MARKER "
            "value. Do not guess and do not edit files."
        ),
        "files": {"settings.ini": "[app]\nSESSION_MARKER=KROMA_BMARK_29K\n"},
    },
    {
        "id": "independent_inspections",
        "prompt": (
            "Inspect alpha.txt, beta.txt and gamma.txt independently with shell. "
            "Report their three CODE values in alphabetical order. Do not edit files."
        ),
        "files": {
            "alpha.txt": "CODE=AX91\n",
            "beta.txt": "CODE=BR28\n",
            "gamma.txt": "CODE=CY37\n",
        },
    },
    {
        "id": "fix_arithmetic",
        "prompt": (
            "Fix average(values) in math_ops.py so the included unit tests pass, "
            "including fractional and negative averages. Run the tests and report."
        ),
        "files": {
            "math_ops.py": (
                "def average(values):\n"
                "    if not values:\n"
                "        return 0\n"
                "    return sum(values) // len(values)\n"
            ),
            "test_math_ops.py": (
                "import unittest\n"
                "from math_ops import average\n"
                "class TestAverage(unittest.TestCase):\n"
                "    def test_fraction(self): self.assertEqual(average([1,2,4]), 7/3)\n"
                "    def test_negative(self): self.assertEqual(average([-4,-1]), -2.5)\n"
                "    def test_empty(self): self.assertEqual(average([]), 0)\n"
            ),
        },
    },
    {
        "id": "implement_records",
        "prompt": (
            "Implement parse_records(text) in records.py. It should parse key=value "
            "lines into a dict, trim whitespace, skip empty lines and lines whose "
            "first non-whitespace character is #, and preserve values after "
            "the first '='. Run the supplied unit tests."
        ),
        "files": {
            "records.py": "def parse_records(text):\n    return {}\n",
            "test_records.py": (
                "import unittest\n"
                "from records import parse_records\n"
                "class TestRecords(unittest.TestCase):\n"
                "    def test_parser(self):\n"
                "        self.assertEqual(parse_records(' # skip\\n a = 1=2\\n\\n b=two \\n'),"
                " {'a':'1=2','b':'two'})\n"
                "    def test_empty(self): self.assertEqual(parse_records(''), {})\n"
                "    def test_unicode(self): self.assertEqual(parse_records('城市 = 北京'), {'城市':'北京'})\n"
            ),
        },
    },
    {
        "id": "cross_file_fix",
        "prompt": (
            "Fix the string normalization mismatch between core.py and app.py. "
            "normalize_tag should strip surrounding whitespace, lowercase using "
            "Unicode casefold, and return the normalized string. Leave the public "
            "normalize_tag import working; run the unit tests."
        ),
        "files": {
            "core.py": "def normalize_tag(value):\n    return value.lower()\n",
            "app.py": "from core import normalize_tag\n\ndef tag_key(text):\n    return normalize_tag(text)\n",
            "test_app.py": (
                "import unittest\nfrom app import tag_key\n"
                "class TestTag(unittest.TestCase):\n"
                "    def test_whitespace(self): self.assertEqual(tag_key(' Foo '), 'foo')\n"
                "    def test_casefold(self): self.assertEqual(tag_key(' Straße '), 'strasse')\n"
            ),
        },
    },
    {
        "id": "diagnose_large_log",
        "prompt": (
            "Inspect build.log and create diagnosis.txt containing the failing "
            "module and source file in precisely this one-line format: "
            "module=<name>;file=<path>. Identify the actual failure, not the "
            "routine successful compilation messages. Do not edit build.log."
        ),
        "files": {
            "build.log": (
                ("INFO compiling normal-dependency v1.0.0\n" * 3_400)
                + "error: unresolved import in module=transport_bridge file=src/bridge.rs\n"
                + ("INFO compiling normal-dependency v1.0.0\n" * 3_400)
            )
        },
    },
)


def parse_usage(log):
    # Missing provider usage is genuinely unknown, not zero.
    for line in reversed(log.splitlines()):
        if line.startswith("[usage] task:"):
            fields = re.findall(r"\b(input|output|cached):\s*(\d+|n/a)", line)
            result = {key: int(value) if value != "n/a" else None for key, value in fields}
            result["requests"] = int(re.search(r"task:\s*(\d+) request", line).group(1))
            return result
    return {"requests": None, "input": None, "output": None, "cached": None}


def parse_profile(log):
    for line in log.splitlines():
        if line.startswith("[profile] "):
            pairs = re.findall(r"\b([a-z_]+)=([0-9.]+)", line)
            return {
                key: float(value) if key.endswith("_ms") else int(value)
                for key, value in pairs
            }
    return {}


def create_fixture(root, task):
    for name, data in task["files"].items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(data, encoding="utf-8")


def verify(root, task_id, answer):
    if task_id == "exact_lookup":
        return "KROMA_BMARK_29K" in answer
    if task_id == "independent_inspections":
        positions = [answer.find(value) for value in ("AX91", "BR28", "CY37")]
        return all(n >= 0 for n in positions) and positions == sorted(positions)
    if task_id in {"fix_arithmetic", "implement_records", "cross_file_fix"}:
        test = subprocess.run(
            ["python3", "-m", "unittest", "-q"],
            cwd=root, capture_output=True, text=True, timeout=15,
        )
        return test.returncode == 0
    if task_id == "diagnose_large_log":
        result = root / "diagnosis.txt"
        return result.is_file() and result.read_text().strip() == (
            "module=transport_bridge;file=src/bridge.rs"
        )
    raise ValueError(f"unknown task {task_id}")


def run_case(binary, task, repeat, timeout):
    with tempfile.TemporaryDirectory(prefix="kroma-agent-suite-") as directory:
        root = Path(directory)
        fixture = root / "project"
        fixture.mkdir()
        create_fixture(fixture, task)
        env = os.environ.copy()
        env.update({
            "HOME": str(root),
            "KROMA_WORKDIR": str(fixture),
            "KROMA_SESSION_FILE": str(root / "session.json"),
            "KROMA_PROFILE": "1",
        })
        start = time.monotonic()
        try:
            child = subprocess.run(
                [str(binary), task["prompt"]], env=env, cwd=fixture,
                capture_output=True, text=True, timeout=timeout,
            )
            elapsed = round(time.monotonic() - start, 3)
            passed = child.returncode == 0 and verify(fixture, task["id"], child.stdout)
            return {
                "task": task["id"], "repeat": repeat, "passed": passed,
                "exit_code": child.returncode, "wall_s": elapsed,
                "usage": parse_usage(child.stderr),
                "profile": parse_profile(child.stderr),
            }
        except subprocess.TimeoutExpired:
            return {
                "task": task["id"], "repeat": repeat, "passed": False,
                "exit_code": None, "wall_s": round(time.monotonic() - start, 3),
                "timeout": True, "usage": {}, "profile": {},
            }


def self_test():
    expected = {"input": 220, "output": 22, "cached": 50, "requests": 2}
    assert parse_usage("[usage] task: 2 request(s), 0.07s model time | input: 220"
                       " | output: 22 | cached: 50") == expected
    assert parse_usage("[usage] task: 1 request(s) | input: n/a"
                       " | output: 4 | cached: n/a")["input"] is None
    assert parse_profile("[profile] wall_ms=12.34 | tool_calls=3") == {
        "wall_ms": 12.34, "tool_calls": 3,
    }
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        for task in TASKS:
            create_fixture(root, task)
            if task["id"] == "diagnose_large_log":
                assert (root / "build.log").stat().st_size > 200_000
                assert not verify(root, task["id"], "")
            elif task["id"] in {"fix_arithmetic", "implement_records", "cross_file_fix"}:
                assert not verify(root, task["id"], "")
            for name in task["files"]:
                (root / name).unlink()
    print(f"self-test OK: {len(TASKS)} task fixtures, parsers and baseline failure checks")


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    choice = cli.add_mutually_exclusive_group(required=True)
    choice.add_argument("--list", action="store_true", help="show tasks without API requests")
    choice.add_argument("--self-test", action="store_true")
    choice.add_argument("--run", action="store_true", help="perform real API calls; may incur cost")
    cli.add_argument("--binary", default="target/release/kroma")
    cli.add_argument("--output", default="benchmarks/live-results.json")
    cli.add_argument("--repeat", type=int, default=1)
    cli.add_argument("--timeout", type=int, default=240)
    cli.add_argument("--task", action="append", choices=[t["id"] for t in TASKS])
    args = cli.parse_args()
    if args.list:
        for task in TASKS:
            print(task["id"], "-", task["prompt"].split(".")[0])
        return
    if args.self_test:
        self_test()
        return
    if args.repeat < 1 or args.repeat > 5:
        cli.error("--repeat must be between 1 and 5")
    if not os.environ.get("KROMA_BASE_URL") or not os.environ.get("KROMA_MODEL"):
        cli.error("--run requires explicit KROMA_BASE_URL and KROMA_MODEL environment variables")
    binary = Path(args.binary).resolve()
    if not binary.is_file():
        cli.error(f"binary not found: {binary}")
    cases = [task for task in TASKS if not args.task or task["id"] in args.task]
    results = []
    for task in cases:
        for trial in range(1, args.repeat + 1):
            row = run_case(binary, task, trial, args.timeout)
            results.append(row)
            usage = row["usage"]
            print(
                f"{row['task']}: pass={row['passed']} wall={row['wall_s']}s "
                f"requests={usage.get('requests')} input={usage.get('input')} "
                f"output={usage.get('output')} cached={usage.get('cached')}",
                flush=True,
            )
    passed = sum(row["passed"] for row in results)
    summary = {
        "task_count": len(results), "passed": passed,
        "pass_rate": passed / len(results),
        "median_wall_s": statistics.median(row["wall_s"] for row in results),
    }
    report = {
        "schema": 1, "model_alias": os.environ["KROMA_MODEL"],
        "note": "Actual upstream model identity and prices require provider verification",
        "summary": summary, "cases": results,
    }
    target = Path(args.output)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    print("summary:", json.dumps(summary), "| saved:", str(target))


if __name__ == "__main__":
    main()
