#!/usr/bin/env python3
"""Offline regression: tail can hide a successful named test even with exit 0.

This reproduces the evidence-loss mechanism found in Kroma's Rust traces.
It does not claim to predict subsequent LLM decisions.
"""
import subprocess


def inspect(command, text):
    result = subprocess.run(
        ["bash", "-o", "pipefail", "-c", command],
        input=text, text=True, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, check=False,
    )
    return result.returncode, result.stdout


def main():
    named = "test sessions::engineering_regression::wal_append_controls_recent_session_order ... ok"
    payload = named + "\n" + "\n".join("test result: ok. 0 passed; 0 failed" for _ in range(16)) + "\n"
    bad_status, bad_output = inspect("tail -12", payload)
    good_status, good_output = inspect("grep -E 'wal_append_controls_recent_session_order|test result:|error:'", payload)
    assert bad_status == good_status == 0
    assert named not in bad_output, "tail unexpectedly kept the relevant test status"
    assert named in good_output, "status-aware extraction lost test evidence"
    print("PASS: identical exit code 0; tail omitted named pass; evidence-preserving filter retained it")
    print("bad visible_lines", len(bad_output.splitlines()), "good visible_lines", len(good_output.splitlines()))


if __name__ == '__main__':
    main()
