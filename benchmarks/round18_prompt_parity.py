#!/usr/bin/env python3
"""Prevent an accidental change to the tested default system guidance.

The old release assembled FINISH_ON_EVIDENCE_GUIDANCE + V7_GUIDANCE as two
pieces. This refactor assembles their exact UTF-8 bytes as one production
segment; the internal environment-switchable variants stay archived only.
"""
import json
import re
import subprocess
from pathlib import Path

ROOT=Path(__file__).resolve().parents[1]
old=subprocess.check_output(["git","show","HEAD:src/prompt.rs"],cwd=ROOT).decode("utf-8")
new=(ROOT/"src/prompt.rs").read_text(encoding="utf-8")

def expand(code, name):
    match=re.search(r"pub const "+re.escape(name)+r": &str = concat!\((.*?)\n\);",code,re.S)
    if not match:
        raise ValueError("missing original concat: "+name)
    return "".join(json.loads(blob) for blob in re.findall(
        r'^\s*("(?:\\.|[^"\\])*")\s*,?\s*$',match.group(1),re.M))

expected=expand(old,"FINISH_ON_EVIDENCE_GUIDANCE")+expand(old,"V7_GUIDANCE")
actual=expand(new,"EXECUTION_GUIDANCE")
assert actual==expected, "production prompt altered"
assert "KROMA_EXPERIMENT_" not in new.split("#[cfg(test)]")[0]
print("DEFAULT_PROMPT_BYTE_IDENTICAL",len(actual.encode("utf-8")),"bytes",flush=True)
