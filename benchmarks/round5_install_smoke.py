#!/usr/bin/env python3
"""Package and smoke-test the merged Windows Kroma Release binary.

Produces a portable zip and SHA-256 in a separate experiment directory. It
never modifies PATH, replaces the user's installed binary, or accesses keys.
"""
from __future__ import annotations
import hashlib, json, os, pathlib, shutil, subprocess, zipfile

SOURCE=pathlib.Path("D:/Kroma/target-kroma-check/release/kroma.exe")
OUT=pathlib.Path("D:/Kroma/experiments/kroma-release-ce0b386")
EXPECTED="ce0b38641eb1"
def run(command,env=None):
    p=subprocess.run(command,capture_output=True,text=True,encoding="utf-8",errors="replace",env=env,timeout=25)
    if p.returncode:
        raise RuntimeError(f"command failed {command[0]} exit={p.returncode} stderr={p.stderr[-300:]}")
    return p.stdout.strip()
def main():
    if not SOURCE.is_file():
        raise RuntimeError("run the verified Release build first")
    OUT.mkdir(parents=True,exist_ok=True)
    copy=OUT/"kroma.exe"
    shutil.copy2(SOURCE,copy)
    info=run([str(copy),"--build-info"])
    if EXPECTED not in info or "dirty" in info.lower():
        raise AssertionError("build info must match clean merged main: "+info[:130])
    help_text=run([str(copy),"--help"])
    if "kroma" not in help_text.lower():
        raise AssertionError("help output not recognized")
    env={k:v for k,v in os.environ.items() if not k.startswith("KROMA_")}
    env["KROMA_PLUGIN_DIR"]=str(OUT/"isolated-state"/"plugins")
    listed=run([str(copy),"plugins","list"],env=env)
    if "on" in listed.lower():
        raise AssertionError("isolated install unexpectedly has enabled plugins")
    digest=hashlib.sha256(copy.read_bytes()).hexdigest()
    archive=OUT/"kroma-windows-x64-ce0b386.zip"
    with zipfile.ZipFile(archive,"w",zipfile.ZIP_DEFLATED,compresslevel=6) as z:
        z.write(copy,arcname="kroma.exe")
        z.writestr("BUILD-INFO.txt",info+"\nsha256="+digest+"\n")
    report={
        "version":info,
        "sha256":digest,
        "portable_executable":str(copy),
        "archive":str(archive),
        "archive_bytes":archive.stat().st_size,
        "help_ok":True,
        "isolated_plugins_empty":True,
        "user_install_unchanged":True,
    }
    (OUT/"install-smoke.json").write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
    print(json.dumps(report,ensure_ascii=False,indent=2))
if __name__=="__main__": main()
