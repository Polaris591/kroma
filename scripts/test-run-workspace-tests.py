#!/usr/bin/env python3
"""Regression for scripts/run-workspace-tests.sh and scripts/select-test-binaries.py.

The defect this pins: `cargo test --no-run` also builds the workspace's ordinary binaries and
examples, and the old selector ran every artifact that had an `executable`. `examples/paired_smoke`
then failed for lack of required arguments and `kroma` started a real product run (which exits when no
owner budget is declared, and could spend money where one is), so the gate was red on a clean tree.

Every test below is behavioural. A fake `cargo` emits a Cargo JSON message stream that includes a
product binary, an example and the two sandbox suites; each fake artifact records that it ran. A test
that passed by reading the script's source could not tell whether the product entry point executed.

    python3 scripts/test-run-workspace-tests.py -v
"""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RUNNER = ROOT / "scripts/run-workspace-tests.sh"
SELECTOR = ROOT / "scripts/select-test-binaries.py"
BASH = shutil.which("bash")
PYTHON = shutil.which("python3")


def artifact(name, kind, test_profile, executable):
    """One `compiler-artifact` message shaped like Cargo's (`--message-format=json`)."""
    return {
        "reason": "compiler-artifact",
        "package_id": "path+file:///fixture/crates/kroma-core#1.5.0",
        "manifest_path": "/fixture/crates/kroma-core/Cargo.toml",
        "target": {
            "kind": [kind],
            "crate_types": ["bin"],
            "name": name,
            "src_path": "/fixture/crates/kroma-core/src/%s.rs" % name,
            "edition": "2021",
            "doc": False,
            "doctest": False,
            "test": test_profile,
        },
        "profile": {
            "opt_level": "0",
            "debuginfo": 0,
            "debug_assertions": True,
            "overflow_checks": True,
            "test": test_profile,
        },
        "features": [],
        "filenames": [executable],
        "executable": executable,
        "fresh": False,
    }


@unittest.skipUnless(BASH and PYTHON, "bash and python3 are required")
class WorkspaceTestSelection(unittest.TestCase):
    """A throwaway tree with a fake `cargo` and fake artifacts that leave markers."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="workspace-tests-")
        self.root = Path(self._tmp.name)
        self.fakebin = self.root / "bin"
        self.artifacts = self.root / "artifacts"
        self.markers = self.root / "markers"
        for path in (self.fakebin, self.artifacts, self.markers):
            path.mkdir()
        self.stream = self.root / "cargo-stream.jsonl"
        self.write_executable(
            self.fakebin / "cargo",
            '#!/bin/sh\n'
            'if [ "$1" != "test" ]; then\n'
            '  echo "unexpected cargo invocation: $*" >&2\n'
            '  exit 64\n'
            "fi\n"
            'cat "$FAKE_CARGO_STREAM"\n'
            'exit "${FAKE_CARGO_STATUS:-0}"\n',
        )

    def tearDown(self):
        self._tmp.cleanup()

    def write_executable(self, path, body):
        path.write_text(body, encoding="utf-8")
        path.chmod(0o755)
        return path

    def marker_script(self, name, status=0):
        """An artifact that records its own execution in the marker directory.

        The file name is the target name, so a path-based filter sees the same shape it would in a
        real `target/debug/deps` tree.
        """
        return self.write_executable(
            self.artifacts / name,
            '#!/bin/sh\nprintf "%s\\n" "%s" >> "$MARKER_DIR/ran"\nexit %d\n'
            % (name, name, status),
        )

    def write_stream(self, messages):
        # Compact separators: that is the shape Cargo actually prints, and a selector that only works
        # on pretty-printed JSON is not a selector for Cargo's output.
        self.stream.write_text(
            "".join(json.dumps(message, separators=(",", ":")) + "\n" for message in messages),
            encoding="utf-8",
        )

    def ran(self):
        log = self.markers / "ran"
        if not log.exists():
            return set()
        return {line for line in log.read_text(encoding="utf-8").splitlines() if line}

    def run_script(self, cargo_status=0):
        env = {
            "PATH": "%s:%s" % (self.fakebin, os.environ.get("PATH", "")),
            "HOME": os.environ.get("HOME", ""),
            "FAKE_CARGO_STREAM": str(self.stream),
            "FAKE_CARGO_STATUS": str(cargo_status),
            "MARKER_DIR": str(self.markers),
        }
        return subprocess.run(
            [BASH, str(RUNNER)],
            cwd=str(ROOT),
            env=env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=120,
        )

    def test_product_binary_and_example_are_never_run(self):
        self.write_stream(
            [
                # What `cargo test --no-run` really emits for this workspace: the product binary and
                # an example, both with an executable and neither a test profile.
                artifact("kroma", "bin", False, str(self.marker_script("kroma"))),
                artifact(
                    "paired_smoke", "example", False, str(self.marker_script("paired_smoke"))
                ),
                # Harnesses that must run.
                artifact("t01_alpha", "test", True, str(self.marker_script("t01_alpha"))),
                artifact("kroma_core", "lib", True, str(self.marker_script("kroma_core"))),
                # The two sandbox-dependent suites are excluded by exact target name...
                artifact(
                    "b13_sandbox_isolation",
                    "test",
                    True,
                    str(self.marker_script("b13_sandbox_isolation")),
                ),
                artifact(
                    "t09_execution_boundary",
                    "test",
                    True,
                    str(self.marker_script("t09_execution_boundary")),
                ),
                # ...and a differently named target is not caught by a substring match.
                artifact(
                    "b13_sandbox_isolation_helper",
                    "test",
                    True,
                    str(self.marker_script("b13_sandbox_isolation_helper")),
                ),
            ]
        )
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.ran(), {"t01_alpha", "kroma_core", "b13_sandbox_isolation_helper"}, "wrong artifacts executed"
        )
        self.assertIn("running 3 test binaries", result.stdout)
        self.assertIn("paired_smoke", result.stderr)
        self.assertIn("kroma", result.stderr)

    def test_no_test_profile_artifact_is_a_failure(self):
        self.write_stream(
            [
                artifact("kroma", "bin", False, str(self.marker_script("kroma"))),
                artifact(
                    "paired_smoke", "example", False, str(self.marker_script("paired_smoke"))
                ),
            ]
        )
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0, "a stream with no harnesses must not pass")
        self.assertEqual(self.ran(), set(), "a non-test artifact was executed")
        self.assertIn("no test-profile artifacts", result.stderr)

    def test_failing_harness_fails_the_script_without_hiding_later_ones(self):
        self.write_stream(
            [
                artifact("t01_alpha", "test", True, str(self.marker_script("t01_alpha", status=1))),
                artifact("t02_beta", "test", True, str(self.marker_script("t02_beta"))),
            ]
        )
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0, "a failing harness must fail the gate")
        self.assertEqual(self.ran(), {"t01_alpha", "t02_beta"})
        self.assertIn("at least one test binary failed", result.stderr)

    def test_malformed_cargo_message_is_refused(self):
        self.stream.write_text(
            "Compiling kroma-core v1.5.0\n"
            + json.dumps(
                artifact("t01_alpha", "test", True, str(self.marker_script("t01_alpha")))
            )
            + "\n",
            encoding="utf-8",
        )
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0, "an unparsable message stream must not pass")
        self.assertEqual(self.ran(), set())
        self.assertIn("not a Cargo JSON message", result.stderr)

    def test_cargo_failure_is_not_reported_as_success(self):
        self.write_stream(
            [artifact("t01_alpha", "test", True, str(self.marker_script("t01_alpha")))]
        )
        result = self.run_script(cargo_status=101)
        self.assertNotEqual(result.returncode, 0, "a failed build must fail the gate")

    def test_selector_deduplicates_and_ignores_other_reasons(self):
        harness = str(self.marker_script("t01_alpha"))
        messages = [
            {"reason": "build-script-executed", "package_id": "x", "out_dir": "/fixture/out"},
            artifact("t01_alpha", "test", True, harness),
            artifact("t01_alpha", "test", True, harness),
            {"reason": "compiler-artifact", "executable": None, "target": {"name": "kroma_core"}},
        ]
        result = subprocess.run(
            [PYTHON, str(SELECTOR)],
            input="".join(json.dumps(message) + "\n" for message in messages),
            capture_output=True,
            text=True,
            timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [harness])


if __name__ == "__main__":
    unittest.main()
