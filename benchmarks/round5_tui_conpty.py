#!/usr/bin/env python3
"""Windows ConPTY TUI smoke tests without a network/model request.

Launches the actual native kroma.exe in a private console, verifies the initial
frame and Unicode draft display, checks resizing and Ctrl+C cleanup. It does
not test IME composition, physical mouse hardware or graphical clipboard.
Uses pywinpty and pyte only in uv's ephemeral dependency environment.
"""
from __future__ import annotations
import argparse, json, os, pathlib, queue, sys, tempfile, threading, time
from winpty import PtyProcess
import pyte

BIN = pathlib.Path("D:/Kroma/target-kroma-check/release/kroma.exe")
def main():
    p=argparse.ArgumentParser()
    p.add_argument("--out",type=pathlib.Path,default=pathlib.Path("D:/Kroma/experiments/kroma-tui-conpty"))
    p.add_argument("--binary",type=pathlib.Path,default=BIN)
    a=p.parse_args()
    a.out.mkdir(parents=True,exist_ok=True)
    env={k:v for k,v in os.environ.items() if not k.startswith("KROMA_")}
    stages=[]
    with tempfile.TemporaryDirectory(prefix="kroma-tui-pty-") as td:
        project=pathlib.Path(td)/"project"
        project.mkdir()
        env.update({
            "KROMA_WORKDIR":str(project),
            "KROMA_SESSION_FILE":str(pathlib.Path(td)/"pty-session.json"),
            "KROMA_PROVIDER_PROFILE":"deepseek",
            "KROMA_PLUGIN_DIR":str(pathlib.Path(td)/"disabled-plugins"),
            "KROMA_NO_MCP":"1",
            "KROMA_NO_CHECKPOINTS":"1",
        })
        proc=PtyProcess.spawn([str(a.binary),"--no-mcp"],cwd=str(project),env=env,dimensions=(30,100))
        screen=pyte.Screen(100,30)
        parser=pyte.Stream(screen)
        capture=[]
        lock=threading.Lock()
        faults=[]
        def reader():
            while True:
                try:
                    chunk=proc.read(8192)
                except EOFError:
                    break
                except Exception as e:
                    faults.append(type(e).__name__)
                    break
                if not chunk:
                    if not proc.isalive():break
                    time.sleep(.1)
                    continue
                with lock:
                    capture.append(chunk)
                    parser.feed(chunk)
        thread=threading.Thread(target=reader,daemon=True)
        thread.start()
        def dump():
            with lock:
                return "\n".join(screen.display)
        def wait_text(needles,seconds):
            end=time.monotonic()+seconds
            while time.monotonic()<end:
                view=dump().lower()
                if any(s.lower() in view for s in needles):
                    return True
                if not proc.isalive():break
                time.sleep(.15)
            return False
        try:
            ok=wait_text(["kroma","model","context","session"],15)
            stages.append(dict(step="initial_tui",ok=ok,alive=proc.isalive(),screen_excerpt=dump()[:360]))
            proc.write("中文测试 Unicode-Draft")
            time.sleep(1.0)
            with lock:
                draft_visible="Unicode-Draft" in "\n".join(screen.display)
                chinese_visible="中文测试" in "\n".join(screen.display)
            stages.append(dict(step="unicode_editor",ok=draft_visible,unicode_visible=chinese_visible))
            proc.setwinsize(24,80)
            time.sleep(.7)
            stages.append(dict(step="resize",ok=proc.isalive(),size=proc.getwinsize()))
            proc.write("\x03")
            time.sleep(.5)
            stages.append(dict(step="control_c_with_draft",ok=proc.isalive(),
                               screen_tail=dump()[-400:]))
            proc.write("\x03")
            thread.join(timeout=8)
            stages.append(dict(step="control_c_empty_exit",ok=not proc.isalive(),
                               alive=proc.isalive(),exitstatus=proc.exitstatus))
        finally:
            if proc.isalive():
                proc.terminate()
                time.sleep(.4)
                if proc.isalive():proc.kill()
            thread.join(timeout=3)
        result=dict(platform=sys.platform,backend="winpty/ConPTY",binary=str(a.binary),stages=stages,
                    read_faults=faults,capture_chars=sum(len(x) for x in capture))
        (a.out/"results.json").write_text(json.dumps(result,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
        print(json.dumps(result,ensure_ascii=False,indent=2))
        if not all(s["ok"] for s in stages):
            sys.exit(2)
if __name__=="__main__":main()
