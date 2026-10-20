#!/usr/bin/env python3
"""Focused tests for scripts/run-trusted-gate.py (M0-03 first slice).

Every test builds a throwaway git repo in a temp directory plus a contract that lives
outside that repo, so nothing here touches this repository, the network or cargo.
Only the standard library and `git` are used.

    python3 scripts/test-trusted-gate.py -v
"""

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

RUNNER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "run-trusted-gate.py")
GIT_IDENTITY = [
    "-c",
    "user.name=trusted-gate-test",
    "-c",
    "user.email=trusted-gate@example.invalid",
    "-c",
    "commit.gpgsign=false",
]


def sha256_of(path):
    with open(path, "rb") as handle:
        return "sha256:" + hashlib.sha256(handle.read()).hexdigest()


class GateFixture(unittest.TestCase):
    """A clean candidate repo at <tmp>/candidate with the trusted area at <tmp>/trusted."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="trusted-gate-")
        self.root = os.path.realpath(self._tmp.name)
        self.repo = os.path.join(self.root, "candidate")
        self.trusted = os.path.join(self.root, "trusted")
        self.out = os.path.join(self.root, "evidence")
        os.makedirs(self.repo)
        os.makedirs(self.trusted)
        self.git("init", "-q", "-b", "main")
        self.write("tracked.txt", "baseline\n")
        self.commit("baseline")

    def tearDown(self):
        self._tmp.cleanup()

    def git(self, *args):
        proc = subprocess.run(
            ["git", "-C", self.repo, *GIT_IDENTITY, *args],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        return proc.stdout

    def write(self, relative, text):
        path = os.path.join(self.repo, relative)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        return path

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)

    def write_contract(self, name, data, where=None):
        path = os.path.join(where or self.trusted, name)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(data, handle, ensure_ascii=False, indent=2)
            handle.write("\n")
        return path

    def contract(self, filename, commands):
        return self.write_contract(
            filename, {"gate_baseline": "kroma.gate.local.v1", "commands": commands}
        )

    def run_gate(self, contract, out=None, runner=RUNNER):
        return subprocess.run(
            [
                sys.executable,
                runner,
                "--repo",
                self.repo,
                "--contract",
                contract,
                "--out",
                out or self.out,
            ],
            capture_output=True,
            text=True,
        )

    def read_evidence(self, out=None):
        path = os.path.join(out or self.out, "gate-evidence.json")
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)

    def assert_no_evidence(self, out):
        self.assertFalse(
            os.path.exists(os.path.join(out, "gate-evidence.json")),
            "a refused run must not write evidence",
        )


class TrustedGateTest(GateFixture):
    def test_pass_binds_contract_and_recomputable_log_hashes(self):
        contract = self.write_contract(
            "gate-contract.json",
            {
                "gate_baseline": "kroma.gate.local.v1",
                "notes": "two trivially passing commands",
                "commands": [
                    {"name": "identity", "argv": [sys.executable, "-c", "print('tree-ok')"]},
                    {
                        # 256 KiB of output: it must land in the log file, not in the runner.
                        "name": "second",
                        "argv": [
                            sys.executable,
                            "-c",
                            "import sys; sys.stdout.write('x' * (256 * 1024))",
                        ],
                    },
                ],
            },
        )
        proc = self.run_gate(contract)
        self.assertEqual(proc.returncode, 0, proc.stderr)

        evidence = self.read_evidence()
        self.assertEqual(evidence["format"], "kroma.trusted_gate.evidence/1")
        self.assertEqual(evidence["gate_baseline"], "kroma.gate.local.v1")
        self.assertEqual(evidence["contract"]["sha256"], sha256_of(contract))
        self.assertEqual(evidence["contract"]["notes"], "two trivially passing commands")

        # Identity of the tree on disk, not a HEAD label copied from the contract.
        self.assertEqual(evidence["commit"], self.git("rev-parse", "HEAD").strip())
        self.assertEqual(evidence["subject_tree"], self.git("rev-parse", "HEAD^{tree}").strip())
        self.assertEqual(evidence["subject"]["branch"], "main")
        self.assertTrue(evidence["subject"]["worktree_clean"])

        # No artifact was built, so the digest stays null instead of being invented.
        self.assertIsNone(evidence["binary_digest"])

        self.assertEqual(evidence["overall"]["status"], "passed")
        self.assertEqual(evidence["overall"]["exit_code"], 0)
        self.assertIsNone(evidence["overall"]["failure_reason"])
        self.assertEqual(evidence["not_run"], [])
        self.assertTrue(evidence["post_run"]["unchanged"])
        self.assertTrue(evidence["post_run"]["worktree_clean"])
        self.assertEqual(evidence["post_run"]["reasons"], [])
        self.assertEqual(evidence["post_run"]["commit"], evidence["commit"])
        self.assertEqual(evidence["redaction_status"], "not_redacted")
        self.assertTrue(evidence["coverage_limits"])
        self.assertEqual(evidence["producer"]["runner_sha256"], sha256_of(RUNNER))

        self.assertEqual([receipt["name"] for receipt in evidence["commands"]], ["identity", "second"])
        for receipt in evidence["commands"]:
            log_path = os.path.join(self.out, receipt["raw_log_ref"])
            self.assertTrue(os.path.isfile(log_path))
            self.assertEqual(receipt["log_hash"], sha256_of(log_path))
            self.assertEqual(receipt["exit_code"], 0)
            self.assertFalse(receipt["timed_out"])
            self.assertEqual(receipt["timeout_seconds"], 900)  # finite default, recorded
            self.assertIsNone(receipt["signal"])
            self.assertTrue(receipt["started"] <= receipt["ended"])

        # Child output is the log file itself: size and hash both come from disk.
        second_log = os.path.join(self.out, evidence["commands"][1]["raw_log_ref"])
        self.assertEqual(os.path.getsize(second_log), 256 * 1024)
        self.assertEqual(evidence["commands"][1]["log_hash"], sha256_of(second_log))
        with open(os.path.join(self.out, evidence["commands"][0]["raw_log_ref"]), encoding="utf-8") as handle:
            self.assertIn("tree-ok", handle.read())

    def test_failures_keep_receipts_and_are_never_reported_as_passed(self):
        # 1. A failing command: its own exit code propagates and later commands do not run.
        marker = os.path.join(self.root, "after-ran.txt")
        contract = self.contract(
            "gate-contract.json",
            [
                {
                    "name": "failing-gate",
                    "argv": [
                        sys.executable,
                        "-c",
                        "import sys; print('boom', file=sys.stderr); sys.exit(3)",
                    ],
                },
                {
                    "name": "after",
                    "argv": [
                        sys.executable,
                        "-c",
                        "import pathlib, sys; pathlib.Path(sys.argv[1]).write_text('x')",
                        marker,
                    ],
                },
            ],
        )
        proc = self.run_gate(contract)

        # The gate's own exit code is the failing command's, not a generic 1.
        self.assertEqual(proc.returncode, 3, proc.stderr)
        evidence = self.read_evidence()
        self.assertEqual(evidence["overall"]["status"], "failed")
        self.assertEqual(evidence["overall"]["exit_code"], 3)
        self.assertEqual(evidence["overall"]["failure_reason"], "command_failed")
        self.assertEqual(evidence["overall"]["failed_command"], "failing-gate")
        self.assertEqual(len(evidence["commands"]), 1)
        self.assertEqual(evidence["commands"][0]["exit_code"], 3)
        self.assertEqual(evidence["not_run"], ["after"])
        self.assertFalse(os.path.exists(marker), "commands after the failure must not run")

        # The failure receipt and its raw log survive the non-zero exit.
        receipt = evidence["commands"][0]
        log_path = os.path.join(self.out, receipt["raw_log_ref"])
        self.assertTrue(os.path.isfile(log_path))
        self.assertEqual(receipt["log_hash"], sha256_of(log_path))
        with open(log_path, encoding="utf-8") as handle:
            self.assertIn("boom", handle.read())

        # 2. A command that outlives its contract timeout is killed, and the receipt says so.
        timeout_out = os.path.join(self.root, "evidence-timeout")
        timeout_contract = self.contract(
            "timeout-contract.json",
            [
                {
                    "name": "slow",
                    "argv": [
                        sys.executable,
                        "-c",
                        "import time; print('started', flush=True); time.sleep(30)",
                    ],
                    "timeout_seconds": 1,
                }
            ],
        )
        began = time.monotonic()
        proc = self.run_gate(timeout_contract, out=timeout_out)
        self.assertLess(time.monotonic() - began, 20, "the timeout must actually bound the run")
        self.assertEqual(proc.returncode, 124, proc.stderr)
        evidence = self.read_evidence(timeout_out)
        receipt = evidence["commands"][0]
        self.assertTrue(receipt["timed_out"])
        self.assertEqual(receipt["exit_code"], 124)
        self.assertEqual(receipt["timeout_seconds"], 1)
        self.assertEqual(evidence["overall"]["failure_reason"], "command_timeout")
        self.assertEqual(evidence["overall"]["failed_command"], "slow")
        timeout_log = os.path.join(timeout_out, receipt["raw_log_ref"])
        self.assertTrue(os.path.isfile(timeout_log))
        self.assertEqual(receipt["log_hash"], sha256_of(timeout_log))
        with open(timeout_log, encoding="utf-8") as handle:
            self.assertIn("started", handle.read())  # output written before the kill survives

        # 3. Commands pass but a tracked file was edited: the tested tree is no longer there.
        mutate_out = os.path.join(self.root, "evidence-mutate")
        mutate_contract = self.contract(
            "mutate-contract.json",
            [
                {
                    "name": "sneaky-edit",
                    "argv": [
                        sys.executable,
                        "-c",
                        "import pathlib; pathlib.Path('tracked.txt').write_text('tampered')",
                    ],
                }
            ],
        )
        proc = self.run_gate(mutate_contract, out=mutate_out)
        self.assertEqual(proc.returncode, 90, proc.stderr)
        evidence = self.read_evidence(mutate_out)
        self.assertEqual(evidence["commands"][0]["exit_code"], 0)
        self.assertEqual(evidence["overall"]["status"], "failed")
        self.assertEqual(evidence["overall"]["exit_code"], 90)
        self.assertEqual(evidence["overall"]["failure_reason"], "subject_mutated")
        self.assertFalse(evidence["post_run"]["unchanged"])
        self.assertFalse(evidence["post_run"]["worktree_clean"])
        self.assertTrue(any("dirty" in reason for reason in evidence["post_run"]["reasons"]))

        # 4. Commands pass but a new commit was made: the recorded tree is stale.
        self.write("tracked.txt", "baseline\n")
        commit_out = os.path.join(self.root, "evidence-commit")
        commit_contract = self.contract(
            "commit-contract.json",
            [
                {
                    "name": "sneaky-commit",
                    "argv": [
                        sys.executable,
                        "-c",
                        "import pathlib, subprocess; pathlib.Path('tracked.txt').write_text('committed'); "
                        "subprocess.run(['git', 'add', '-A'], check=True); "
                        "subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@example.invalid', "
                        "'-c', 'commit.gpgsign=false', 'commit', '-q', '-m', 'sneaky'], check=True)",
                    ],
                }
            ],
        )
        proc = self.run_gate(commit_contract, out=commit_out)
        self.assertEqual(proc.returncode, 90, proc.stderr)
        evidence = self.read_evidence(commit_out)
        self.assertEqual(evidence["commands"][0]["exit_code"], 0)
        self.assertEqual(evidence["overall"]["failure_reason"], "subject_mutated")
        self.assertNotEqual(evidence["post_run"]["commit"], evidence["commit"])
        self.assertTrue(any("commit changed" in reason for reason in evidence["post_run"]["reasons"]))

    def test_refusals_for_dirty_tree_in_repo_contract_runner_and_shell(self):
        valid = self.write_contract(
            "gate-contract.json",
            {"gate_baseline": "kroma.gate.local.v1", "commands": [{"name": "ok", "argv": [sys.executable, "-c", "print('ok')"]}]},
        )

        # 1. A dirty tree would let HEAD stand in for the real candidate tree.
        self.write("scratch.txt", "uncommitted\n")
        dirty_out = os.path.join(self.root, "evidence-dirty")
        proc = self.run_gate(valid, out=dirty_out)
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("dirty", proc.stderr)
        self.assertIn("scratch.txt", proc.stderr)
        self.assert_no_evidence(dirty_out)
        os.remove(os.path.join(self.repo, "scratch.txt"))

        # 2. A contract inside the candidate repo is candidate-controlled configuration.
        in_repo = self.write(
            "gate-contract.json",
            json.dumps(
                {
                    "gate_baseline": "kroma.gate.local.v1",
                    "commands": [{"name": "ok", "argv": [sys.executable, "-c", "print('ok')"]}],
                }
            ),
        )
        in_repo_out = os.path.join(self.root, "evidence-in-repo")
        proc = self.run_gate(in_repo, out=in_repo_out)
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("inside the candidate repo", proc.stderr)
        self.assert_no_evidence(in_repo_out)

        # 3. A runner copied into the candidate tree is not a trusted runner.
        frozen = self.write("gate/run-trusted-gate.py", "")
        shutil.copyfile(RUNNER, frozen)
        runner_out = os.path.join(self.root, "evidence-runner")
        proc = self.run_gate(valid, out=runner_out, runner=frozen)
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("cannot be trusted", proc.stderr)
        self.assert_no_evidence(runner_out)

        # 4. Commands are argv arrays; a shell string has no execution path.
        shell_contract = self.write_contract(
            "shell-contract.json",
            {
                "gate_baseline": "kroma.gate.local.v1",
                "commands": [{"name": "sneaky", "shell": "echo pwned"}],
            },
        )
        shell_out = os.path.join(self.root, "evidence-shell")
        proc = self.run_gate(shell_contract, out=shell_out)
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("argv arrays", proc.stderr)
        self.assert_no_evidence(shell_out)

    def test_candidate_forged_contract_cannot_flip_failure_to_pass(self):
        # The candidate commits its own passing gate configuration under the same filename.
        self.write(
            "gate-contract.json",
            json.dumps(
                {
                    "gate_baseline": "candidate.v0",
                    "commands": [
                        {
                            "name": "always-pass",
                            "argv": [sys.executable, "-c", "print('candidate says pass')"],
                        }
                    ],
                },
                indent=2,
            ),
        )
        self.commit("candidate adds its own gate contract")
        self.assertEqual(self.git("status", "--porcelain"), "")

        trusted = self.write_contract(
            "gate-contract.json",
            {
                "gate_baseline": "kroma.gate.local.v1",
                "commands": [
                    {
                        "name": "baseline-gate",
                        "argv": [
                            sys.executable,
                            "-c",
                            "print('baseline gate runs'); raise SystemExit(5)",
                        ],
                    }
                ],
            },
            where=self.trusted,
        )
        proc = self.run_gate(trusted)

        # The external contract still fails, so the candidate's edit bought nothing.
        self.assertEqual(proc.returncode, 5, proc.stderr)
        evidence = self.read_evidence()
        self.assertEqual(evidence["overall"]["status"], "failed")
        self.assertEqual(evidence["overall"]["exit_code"], 5)
        self.assertEqual(evidence["contract"]["sha256"], sha256_of(trusted))
        self.assertEqual(evidence["gate_baseline"], "kroma.gate.local.v1")
        self.assertEqual([receipt["name"] for receipt in evidence["commands"]], ["baseline-gate"])
        self.assertNotIn("always-pass", [receipt["name"] for receipt in evidence["commands"]])
        with open(os.path.join(self.out, evidence["commands"][0]["raw_log_ref"]), encoding="utf-8") as handle:
            self.assertIn("baseline gate runs", handle.read())


if __name__ == "__main__":
    unittest.main(verbosity=2)
