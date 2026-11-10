#!/usr/bin/env python3
"""Revalidate the corrected Round 12 Unicode preview fixture in isolation.

Original fixture lacked the system message required by Kroma's real archive
reader. This checker ensures the negative control now fails for the intended
Unicode snippet condition, not for invalid archive structure.
"""
from pathlib import Path
import subprocess
import round12_prompt_tournament as t
import round9_complete_function_read as work

T=Path("D:/Kroma/experiments/kroma-round14-unicode-corrected")
t.LAB=T
t.setup()
work.LAB=T
old_environment=t.environment
def isolated(project,trial,arm):
    env=old_environment(project,trial,arm)
    env["CARGO_TARGET_DIR"]=str(T/"target-preflight")
    return env
t.environment=isolated
work.environment=isolated

def main():
    # t.preflight builds under the corrected root and uses the isolated target.
    project=T/"preflight"/"preview-broken"
    if not project.exists():
        t.preflight()
    source=(project/"src/sessions.rs").read_text(encoding="utf-8")
    assert 'crate::protocol::Message::new("system", "Round12 fixture identity")' in source
    tests=source[source.index("mod round12_session_unicode_preview"): ]
    assert "snippet omitted actual match" in tests
    result=subprocess.run(
        ["cargo", "test", "--locked", "--offline", "--lib", "round12_session_unicode_preview", "--", "--nocapture"],
        cwd=project,env=isolated(project,project.parent,"baseline"),
        capture_output=True,text=True,timeout=90,
    )
    transcript=result.stdout+result.stderr
    print("UNICODE_NEGATIVE_CONTROL_EXIT",result.returncode,flush=True)
    print(transcript[-1450:],flush=True)
    if result.returncode == 0 or "snippet omitted actual match" not in transcript:
        raise RuntimeError("fixture fails for wrong reason, not Unicode snippet")
    print("CORRECTED_FIXTURE_FAILS_FOR_INTENDED_UNICODE_SNIPPET",flush=True)

if __name__=="__main__":main()
