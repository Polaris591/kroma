#!/usr/bin/env python3
"""Minimal local trusted gate runner (M0-03, first slice).

The controller starts this script from a *frozen copy outside the candidate repo*,
passes the candidate worktree and a trusted contract JSON that also lives outside
that worktree, and gets back an evidence file plus raw per-command logs. Because the
gate configuration never comes from the candidate tree, a candidate cannot turn a
failing gate into a passing one by editing an in-repo copy of the contract.

What this does not do is stated in `coverage_limits` in every evidence file: it is not
a sandbox, not an attestation, and not the CI-side gate. See docs/goal/TRUSTED_GATE.md.

Usage:
    python3 run-trusted-gate.py --repo <candidate worktree> \
        --contract <trusted-contract.json> --out <evidence-dir>

Exit codes:
    0    every contract command exited 0 and the candidate worktree/HEAD was unchanged
    124  a command exceeded its contract timeout_seconds and was killed
    90   every command exited 0, but the candidate worktree/HEAD changed during the run
    2    refusal: bad input, dirty candidate tree, contract/runner inside the candidate
         tree (no evidence file is written, so "no evidence" means "refused")
    1    internal error while running or writing evidence
    N    otherwise the first failing command's exit code (128+N for a signalled child)
"""

import argparse
import getpass
import hashlib
import json
import os
import platform
import re
import signal
import subprocess
import sys
import time
from datetime import datetime, timezone

EVIDENCE_FORMAT = "kroma.trusted_gate.evidence/1"
RUNNER_NAME = "kroma-trusted-gate"
RUNNER_VERSION = "0.2.0"
REFUSAL_EXIT = 2
INTERNAL_EXIT = 1
# A command that ran out of its contract timeout is killed and reported as 124, the same
# code timeout(1) uses.
TIMEOUT_EXIT = 124
# All commands passed but the candidate worktree/HEAD no longer matches what was recorded
# before the run, so the recorded subject tree does not describe what was actually tested.
SUBJECT_MUTATED_EXIT = 90
# Finite wall-clock bound per command. A contract may shorten or lengthen it per command,
# but there is no way to ask for an unbounded run.
DEFAULT_TIMEOUT_SECONDS = 900

# A contract command is argv-only. Anything else (notably a shell string) is refused
# instead of being interpreted, so there is no code path that turns a contract string
# into shell syntax.
CONTRACT_KEYS = {"gate_baseline", "commands", "notes"}
COMMAND_KEYS = {"name", "argv", "timeout_seconds"}

COVERAGE_LIMITS = [
    "commands run as the same UID, environment and host as the controller: no sandbox, "
    "no privilege separation, no isolation from a hostile same-UID process",
    "the contract is trusted because the controller starts this runner from outside the "
    "candidate tree; candidate code executed by the gate commands is still untrusted",
    "no signature or attestation: contract_sha256/log_hash/runner_sha256 prove byte "
    "equality only, so producer and environment trust must be verified independently",
    "binary_digest is null: this gate neither builds nor receives a binary artifact",
    "raw logs hold the child's stdout and stderr interleaved; stream separation and "
    "line timings are not preserved",
    "timeout enforcement is POSIX-only in this slice: the child is started in its own "
    "session and its process group is signalled, but side effects a timed-out command "
    "already produced are not undone; on platforms without os.killpg only the direct "
    "child is killed",
    "the subject check compares worktree/HEAD before and after the run only: a change "
    "reverted inside the run window, or a write to an ignored path, is not detected and "
    "no complete tamper-proofing is claimed",
    "child stdin is /dev/null, so interactive gates cannot run here",
    "no CI artifact ingestion and no CI-side gate: M0-03-A3 stays open",
]


class Refusal(Exception):
    """Input or candidate state that makes the gate result meaningless."""


def _now():
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def _sha256_bytes(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def _sha256_path(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return "sha256:" + digest.hexdigest()


def _is_within(path, root):
    try:
        return os.path.commonpath([path, root]) == root
    except ValueError:  # different drives
        return False


def _slug(name):
    return re.sub(r"[^a-z0-9._-]+", "-", name.lower()).strip("-") or "cmd"


def _git(repo, args):
    proc = subprocess.run(
        ["git", "-C", repo, *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return (
        proc.returncode,
        proc.stdout.decode("utf-8", "replace"),
        proc.stderr.decode("utf-8", "replace"),
    )


def _worktree_entries(repo):
    code, out, err = _git(
        repo,
        ["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none"],
    )
    if code != 0:
        raise Refusal("git status failed in %s: %s" % (repo, err.strip()))
    return [entry for entry in out.split("\0") if entry]


def _read_identity(repo_real):
    """Identity of the tree that is actually on disk, not just the HEAD label."""
    code, out, err = _git(repo_real, ["rev-parse", "--is-inside-work-tree"])
    if code != 0 or out.strip() != "true":
        raise Refusal("%s is not a git worktree: %s" % (repo_real, err.strip()))

    code, out, err = _git(repo_real, ["rev-parse", "--show-toplevel"])
    if code != 0:
        raise Refusal("git rev-parse --show-toplevel failed in %s: %s" % (repo_real, err.strip()))
    if os.path.realpath(out.strip()) != repo_real:
        raise Refusal("--repo must be the root of the candidate worktree, got %s" % out.strip())

    code, commit, err = _git(repo_real, ["rev-parse", "HEAD"])
    if code != 0:
        raise Refusal("%s has no commit to test: %s" % (repo_real, err.strip()))
    code, tree, err = _git(repo_real, ["rev-parse", "HEAD^{tree}"])
    if code != 0:
        raise Refusal("cannot read the candidate tree: %s" % err.strip())
    code, branch, _ = _git(repo_real, ["symbolic-ref", "--short", "-q", "HEAD"])
    return {
        "commit": commit.strip(),
        "subject_tree": tree.strip(),
        "branch": branch.strip() if code == 0 else None,
        "worktree_entries": _worktree_entries(repo_real),
    }


def read_subject(repo_real):
    """The pre-run subject: a clean worktree, so HEAD does describe the tested tree."""
    identity = _read_identity(repo_real)
    entries = identity["worktree_entries"]
    if entries:
        shown = ", ".join(entry.strip() for entry in entries[:10])
        more = "" if len(entries) <= 10 else " (+%d more)" % (len(entries) - 10)
        raise Refusal(
            "candidate worktree is dirty, so HEAD would not describe the tested tree: %s%s"
            % (shown, more)
        )
    return {
        "repo": repo_real,
        "commit": identity["commit"],
        "subject_tree": identity["subject_tree"],
        "branch": identity["branch"],
        "worktree_clean": True,
    }


def _post_run_check(repo_real, subject):
    """Compare worktree/HEAD after the run against the identity recorded before it.

    This is a before/after comparison and nothing more: a change that was reverted inside
    the run window, or a write to a git-ignored path, is invisible here.
    """
    try:
        after = _read_identity(repo_real)
    except Refusal as exc:
        return {
            "readable": False,
            "commit": None,
            "subject_tree": None,
            "worktree_clean": False,
            "unchanged": False,
            "reasons": ["post-run identity unreadable: %s" % exc],
        }

    entries = after["worktree_entries"]
    reasons = []
    if entries:
        shown = ", ".join(entry.strip() for entry in entries[:10])
        more = "" if len(entries) <= 10 else " (+%d more)" % (len(entries) - 10)
        reasons.append("worktree is dirty after the run: %s%s" % (shown, more))
    if after["commit"] != subject["commit"]:
        reasons.append("commit changed from %s to %s" % (subject["commit"], after["commit"]))
    if after["subject_tree"] != subject["subject_tree"]:
        reasons.append(
            "subject tree changed from %s to %s" % (subject["subject_tree"], after["subject_tree"])
        )
    return {
        "readable": True,
        "commit": after["commit"],
        "subject_tree": after["subject_tree"],
        "worktree_clean": not entries,
        "unchanged": not reasons,
        "reasons": reasons,
    }


def load_contract(contract_path, repo_real):
    real = os.path.realpath(contract_path)
    if _is_within(real, repo_real):
        raise Refusal(
            "contract %s is inside the candidate repo %s; the gate configuration must come "
            "from a trusted location outside the candidate tree" % (real, repo_real)
        )
    if not os.path.isfile(real):
        raise Refusal("contract %s is not a readable file" % real)

    with open(real, "rb") as handle:
        raw = handle.read()
    try:
        data = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise Refusal("contract %s is not valid UTF-8 JSON: %s" % (real, exc))
    if not isinstance(data, dict):
        raise Refusal("contract %s must be a JSON object" % real)

    unknown = sorted(set(data) - CONTRACT_KEYS)
    if unknown:
        raise Refusal("contract %s has unsupported keys: %s" % (real, ", ".join(unknown)))

    gate_baseline = data.get("gate_baseline")
    if not isinstance(gate_baseline, str) or not gate_baseline.strip():
        raise Refusal("contract %s needs a non-empty string gate_baseline" % real)

    commands = data.get("commands")
    if not isinstance(commands, list) or not commands:
        raise Refusal("contract %s needs a non-empty commands array" % real)

    parsed = []
    for index, command in enumerate(commands):
        if not isinstance(command, dict):
            raise Refusal("contract command #%d must be an object" % index)
        extra = sorted(set(command) - COMMAND_KEYS)
        if extra:
            raise Refusal(
                "contract command #%d has unsupported keys: %s (commands are argv arrays, "
                "never shell strings)" % (index, ", ".join(extra))
            )
        argv = command.get("argv")
        if not isinstance(argv, list) or not argv or not all(
            isinstance(item, str) and item for item in argv
        ):
            raise Refusal("contract command #%d needs a non-empty argv array of strings" % index)
        name = command.get("name", "command-%d" % index)
        if not isinstance(name, str) or not name.strip():
            raise Refusal("contract command #%d has an invalid name" % index)
        timeout_seconds = command.get("timeout_seconds", DEFAULT_TIMEOUT_SECONDS)
        if (
            isinstance(timeout_seconds, bool)
            or not isinstance(timeout_seconds, (int, float))
            or not (0 < timeout_seconds < float("inf"))
        ):
            raise Refusal(
                "contract command #%d needs a finite positive timeout_seconds" % index
            )
        parsed.append({"name": name, "argv": list(argv), "timeout_seconds": timeout_seconds})

    notes = data.get("notes")
    if notes is not None and not isinstance(notes, str):
        raise Refusal("contract %s has a non-string notes field" % real)

    return {
        "path": real,
        "sha256": _sha256_bytes(raw),
        "gate_baseline": gate_baseline.strip(),
        "notes": notes,
        "commands": parsed,
    }


def _kill_process_group(proc, grace_seconds=2.0):
    """Kill the session the child was started in, so grandchildren cannot outlive the gate.

    POSIX only: on a platform without os.killpg the direct child is killed and its
    descendants may survive, which is recorded in coverage_limits.
    """
    if not hasattr(os, "killpg"):
        proc.kill()
        proc.wait()
        return
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + grace_seconds
    try:
        proc.wait(timeout=grace_seconds)
    except subprocess.TimeoutExpired:
        pass
    # The leader exiting does not mean its descendants have exited. Always sweep
    # the original group after the grace period, including TERM-ignoring children.
    remaining = deadline - time.monotonic()
    if remaining > 0:
        time.sleep(remaining)
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    proc.wait()


def _run_command(repo_real, command, index, logs_dir):
    """Run one command with stdout+stderr going straight to its raw log file.

    The log is the child's own bytes on disk, so a chatty command cannot grow the runner's
    memory; the hash is streamed from that file once the child is gone.
    """
    log_name = "%02d-%s.log" % (index, _slug(command["name"]))
    log_path = os.path.join(logs_dir, log_name)
    timeout_seconds = command["timeout_seconds"]

    started = _now()
    began = time.monotonic()
    timed_out = False
    with open(log_path, "wb") as log_handle:
        proc = subprocess.Popen(
            command["argv"],
            cwd=repo_real,
            stdin=subprocess.DEVNULL,
            stdout=log_handle,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            proc.wait(timeout=timeout_seconds)
        except subprocess.TimeoutExpired:
            timed_out = True
            _kill_process_group(proc)
    ended = _now()
    duration = round(time.monotonic() - began, 3)

    signal_number = None
    if timed_out:
        exit_code = TIMEOUT_EXIT
    else:
        exit_code = proc.returncode
        if exit_code < 0:
            signal_number = -exit_code
            exit_code = 128 + signal_number

    return {
        "index": index,
        "name": command["name"],
        "argv": command["argv"],
        "timeout_seconds": timeout_seconds,
        "started": started,
        "ended": ended,
        "duration_seconds": duration,
        "timed_out": timed_out,
        "exit_code": exit_code,
        "signal": signal_number,
        "raw_log_ref": os.path.join("logs", log_name),
        "log_hash": _sha256_path(log_path),
    }


def run_gate(repo_real, contract, out_dir):
    # Refuse before touching the evidence directory: a refused run leaves no partial output.
    subject = read_subject(repo_real)
    os.makedirs(os.path.join(out_dir, "logs"), exist_ok=True)

    started = _now()
    receipts = []
    failure = None
    for index, command in enumerate(contract["commands"]):
        receipt = _run_command(repo_real, command, index, os.path.join(out_dir, "logs"))
        receipts.append(receipt)
        if receipt["exit_code"] != 0:
            failure = receipt
            break
    ended = _now()

    not_run = [command["name"] for command in contract["commands"][len(receipts) :]]
    post_run = _post_run_check(repo_real, subject)

    if failure is not None:
        overall = {
            "status": "failed",
            "exit_code": failure["exit_code"],
            "failure_reason": "command_timeout" if failure["timed_out"] else "command_failed",
            "failed_command": failure["name"],
        }
    elif not post_run["unchanged"]:
        # Commands claimed success, but the tree that was tested is gone: the recorded
        # subject tree no longer describes what ran, so this cannot be a pass.
        overall = {
            "status": "failed",
            "exit_code": SUBJECT_MUTATED_EXIT,
            "failure_reason": "subject_mutated",
            "failed_command": None,
        }
    else:
        overall = {
            "status": "passed",
            "exit_code": 0,
            "failure_reason": None,
            "failed_command": None,
        }

    code, git_version, _ = _git(repo_real, ["--version"])
    evidence = {
        "format": EVIDENCE_FORMAT,
        "producer": {
            "name": RUNNER_NAME,
            "version": RUNNER_VERSION,
            "runner_path": os.path.realpath(__file__),
            "runner_sha256": _sha256_path(os.path.realpath(__file__)),
            "user": getpass.getuser(),
        },
        "subject": subject,
        "subject_tree": subject["subject_tree"],
        "commit": subject["commit"],
        "gate_baseline": contract["gate_baseline"],
        "contract": {
            "path": contract["path"],
            "sha256": contract["sha256"],
            "notes": contract["notes"],
        },
        "binary_digest": None,
        "environment": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "python_version": platform.python_version(),
            "python_executable": sys.executable,
            "git_version": git_version.strip() if code == 0 else None,
            "PATH": os.environ.get("PATH"),
        },
        "started": started,
        "ended": ended,
        "commands": receipts,
        "not_run": not_run,
        "overall": overall,
        "post_run": post_run,
        "redaction_status": "not_redacted",
        "coverage_limits": list(COVERAGE_LIMITS),
    }

    evidence_path = os.path.join(out_dir, "gate-evidence.json")
    with open(evidence_path, "w", encoding="utf-8") as handle:
        json.dump(evidence, handle, ensure_ascii=False, indent=2)
        handle.write("\n")
    return evidence, evidence_path


def main(argv):
    parser = argparse.ArgumentParser(
        prog="run-trusted-gate.py",
        description=(
            "Run a trusted gate contract against a clean candidate worktree and write "
            "evidence plus raw logs. Start this from a frozen copy outside the candidate repo."
        ),
        allow_abbrev=False,
    )
    parser.add_argument("--repo", required=True, help="candidate git worktree root")
    parser.add_argument("--contract", required=True, help="trusted contract JSON outside the repo")
    parser.add_argument("--out", required=True, help="evidence directory outside the repo")
    args = parser.parse_args(argv)

    try:
        if not os.path.isdir(args.repo):
            raise Refusal("--repo %s is not a directory" % args.repo)
        repo_real = os.path.realpath(args.repo)

        runner_path = os.path.realpath(__file__)
        if _is_within(runner_path, repo_real):
            raise Refusal(
                "runner %s lives inside the candidate repo %s; a runner loaded from the "
                "candidate tree cannot be trusted. The controller must freeze an audited copy "
                "outside the repo, record its sha256 and start that copy." % (runner_path, repo_real)
            )

        contract = load_contract(args.contract, repo_real)

        out_real = os.path.realpath(args.out)
        if _is_within(out_real, repo_real):
            raise Refusal("--out %s is inside the candidate repo; evidence must not touch it" % out_real)

        evidence, evidence_path = run_gate(repo_real, contract, out_real)
    except Refusal as exc:
        print("trusted-gate: refused: %s" % exc, file=sys.stderr)
        return REFUSAL_EXIT
    except OSError as exc:
        print("trusted-gate: internal error: %s" % exc, file=sys.stderr)
        return INTERNAL_EXIT

    overall = evidence["overall"]
    if overall["status"] == "passed":
        print(
            "trusted-gate: passed %d command(s) on tree %s"
            % (len(evidence["commands"]), evidence["subject_tree"])
        )
    elif overall["failure_reason"] == "subject_mutated":
        print(
            "trusted-gate: failed: candidate subject changed during the run: %s"
            % "; ".join(evidence["post_run"]["reasons"])
        )
    elif overall["failure_reason"] == "command_timeout":
        print(
            "trusted-gate: failed at command %r: timed out after %ss on tree %s"
            % (
                overall["failed_command"],
                evidence["commands"][-1]["timeout_seconds"],
                evidence["subject_tree"],
            )
        )
    else:
        print(
            "trusted-gate: failed at command %r with exit %d on tree %s"
            % (overall["failed_command"], overall["exit_code"], evidence["subject_tree"])
        )
    print("trusted-gate: evidence %s" % evidence_path)
    return overall["exit_code"]


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
