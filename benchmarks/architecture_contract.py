#!/usr/bin/env python3
"""Frozen P0 foreground wire contract, using the existing offline mock task.

Run:
    python3 benchmarks/architecture_contract.py --record  # P0 only
    python3 benchmarks/architecture_contract.py           # every later phase

The fixture contains no credential and masks the random temporary workspace.
This supplements, rather than replaces, fault/cancellation/WAL regressions.
"""
import json
from pathlib import Path
import re
import sys

import mock_profile_smoke

GOLDEN = Path(__file__).with_name("architecture_contract.json")


def normalize(value):
    # Only P0's isolated mock task is captured here, not the user's session.
    text = json.dumps(value, ensure_ascii=False, sort_keys=True)
    text = re.sub(r"/tmp/kroma-profile-smoke-[a-zA-Z0-9_-]+", "<WORKDIR>", text)
    return json.loads(text)


def main():
    mock_profile_smoke.run()
    requests = normalize(mock_profile_smoke.MockProvider.requests)
    if "--record" in sys.argv:
        assert not GOLDEN.exists(), "P0 golden already recorded; do not silently update"
        GOLDEN.write_text(json.dumps(requests, ensure_ascii=False, indent=2, sort_keys=True) + "\n")
        print("P0_ARCH golden saved:", GOLDEN)
        return
    expected = json.loads(GOLDEN.read_text())
    assert requests == expected, (
        "model request payload changed: inspect the change before editing the frozen golden"
    )
    print("P0_ARCH complete multi-turn model requests unchanged: PASS")


if __name__ == "__main__":
    main()
