#!/usr/bin/env python3
"""B15: a repeatable PTY probe for the Kroma interactive REPL/TUI.

Unit tests prove the REPL's *logic*; this probe drives the real binary through a
pseudo-terminal and checks the operator-facing behaviours that only a terminal can
show:

  1. startup banner and session status render on a real TTY
  2. no rendered line exceeds the terminal width, including under long CJK (wide)
     input — a wrong width calculation shows up as an over-wide line here
  3. a bracketed multi-line paste is accepted as input instead of being executed
     line by line
  4. resizing the window redraws and the prompt keeps responding, at the new width
  5. heavy output scrolls and the prompt still answers afterwards
  6. busy / cancel / error / resume states are visibly rendered
  7. the process exits cleanly on /quit

Usage:
    /kt/ptyvenv/bin/python scripts/pty-tui-probe.py <kroma-binary> [--workspace DIR] [--log DIR]

Requires `pyte` (a pure-Python terminal emulator) so the assertions are made against the
*rendered screen* — line wrapping, backspace erasure and redraws are interpreted exactly like a
real terminal would, instead of being guessed from raw bytes. A virtualenv with pyte is kept at
`/kt/ptyvenv`.

Exit status 0 when every check passes; 1 otherwise. Every run writes the raw PTY
transcript, a de-ANSI'd rendering, and the final screen dump into the log directory so a failure
can be replayed byte for byte.
"""

import argparse
import fcntl
import json
import os
import pty
import re
import select
import signal
import socket
import struct
import sys
import termios
import threading
import time
import unicodedata

try:
    import pyte
except ImportError:  # pragma: no cover
    print(
        "probe: pyte is required for screen assertions; run with /kt/ptyvenv/bin/python",
        file=sys.stderr,
    )
    sys.exit(2)

ANSI_RE = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07|\x1b[@-Z\\-_]")


def strip_ansi(data: bytes) -> str:
    return ANSI_RE.sub(b"", data).decode("utf-8", "replace").replace("\r\n", "\n").replace("\r", "\n")


def display_width(text: str) -> int:
    width = 0
    for ch in text:
        if unicodedata.combining(ch):
            continue
        width += 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
    return width


class Probe:
    def __init__(self, argv, env, cols=100, rows=30):
        self.argv = argv
        self.env = env
        self.cols = cols
        self.rows = rows
        self.pid = None
        self.fd = None
        self.raw = bytearray()
        self.checks = []
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.Stream(self.screen)

    # -- lifecycle ---------------------------------------------------------
    def start(self):
        pid, fd = pty.fork()
        if pid == 0:  # child
            os.environ.update(self.env)
            try:
                os.execv(self.argv[0], self.argv)
            except OSError as exc:  # pragma: no cover - exec failure is fatal
                print(f"probe: exec failed: {exc}", file=sys.stderr)
                os._exit(127)
        self.pid, self.fd = pid, fd
        self.set_winsize(self.cols, self.rows)

    def set_winsize(self, cols, rows):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.screen.resize(rows, cols)

    def send(self, data: str):
        os.write(self.fd, data.encode())

    def drain(self, quiet=0.4, hard=20.0):
        """Read until the child has been quiet for `quiet` seconds (or `hard`)."""
        deadline = time.time() + hard
        last = time.time()
        while time.time() < deadline:
            remaining = min(quiet, deadline - time.time())
            r, _, _ = select.select([self.fd], [], [], max(remaining, 0.0))
            if r:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                self.raw.extend(chunk)
                self.stream.feed(chunk.decode("utf-8", "replace"))
                last = time.time()
            elif time.time() - last >= quiet:
                break
        return strip_ansi(bytes(self.raw))

    def alive(self):
        try:
            pid, _ = os.waitpid(self.pid, os.WNOHANG)
            return pid == 0
        except ChildProcessError:
            return False

    def stop(self):
        if self.pid is None:
            return
        try:
            os.kill(self.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass

    # -- checks ------------------------------------------------------------
    def check(self, name, ok, detail=""):
        status = "ok" if ok else "FAIL"
        self.checks.append((name, bool(ok), detail))
        print(f"  {status}  {name}" + (f" — {detail}" if detail and not ok else ""))

    def rendered_lines(self, text):
        return [line for line in text.split("\n") if line.strip()]

    def widest_line(self, text):
        lines = self.rendered_lines(text)
        if not lines:
            return -1, ""
        best = max(lines, key=display_width)
        return display_width(best), best

    def screen_text(self):
        """The rendered screen as a terminal would show it (pyte applies wrapping/erasure)."""
        return "\n".join(self.screen.display)

    def screen_lines(self):
        return list(self.screen.display)


def start_hang_server():
    """A TCP server that accepts and then never answers, so the client stays busy."""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    srv.listen(4)
    port = srv.getsockname()[1]

    requests = []
    conns = []

    def capture(conn):
        conn.settimeout(8)
        try:
            data = bytearray()
            while b"\r\n\r\n" not in data and len(data) < 65536:
                chunk = conn.recv(4096)
                if not chunk:
                    return
                data.extend(chunk)
            header, body = bytes(data).split(b"\r\n\r\n", 1)
            length = next(int(line.split(b":", 1)[1]) for line in header.split(b"\r\n")
                          if line.lower().startswith(b"content-length:"))
            if not 0 <= length <= 2 * 1024 * 1024:
                return
            while len(body) < length:
                chunk = conn.recv(min(65536, length - len(body)))
                if not chunk:
                    return
                body += chunk
            requests.append(json.loads(body))
        except (OSError, ValueError, StopIteration):
            return
        # Intentionally no response: cancellation must interrupt the pending provider call.

    def serve():
        try:
            while True:
                conn, _ = srv.accept()
                conns.append(conn)
                threading.Thread(target=capture, args=(conn,), daemon=True).start()
        except OSError:
            pass

    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    return srv, port, requests


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("--workspace", default=None)
    parser.add_argument("--log", default="/tmp/kroma-pty-probe")
    parser.add_argument("--cols", type=int, default=100)
    parser.add_argument("--rows", type=int, default=30)
    args = parser.parse_args()

    binary = os.path.abspath(args.binary)
    workspace = args.workspace or "/tmp/kroma-pty-probe-ws"
    os.makedirs(workspace, exist_ok=True)
    os.makedirs(args.log, exist_ok=True)

    server, hang_port, requests = start_hang_server()
    # A second, closed port: connecting to it fails immediately, which drives the
    # error path without any external network.
    probe_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    probe_sock.bind(("127.0.0.1", 0))
    dead_port = probe_sock.getsockname()[1]
    probe_sock.close()

    env = {
        "TERM": "xterm-256color",
        "KROMA_API_KEY": "pty-probe-key",
        "KROMA_MODEL": "pty-probe-model",
        "KROMA_BASE_URL": f"http://127.0.0.1:{hang_port}/v1",
        "NO_COLOR": "",
    }

    probe = Probe(
        [binary, "repl", "--workspace", workspace, "--policy", "read-only"],
        env,
        cols=args.cols,
        rows=args.rows,
    )
    print("B15 PTY probe")
    probe.start()

    try:
        # 1. startup ------------------------------------------------------
        text = probe.drain(quiet=0.8, hard=20.0)
        probe.check("startup banner and session status render", "kroma" in text and "policy:" in text)

        # 2. CJK width: a long wide-character input must stay inside the terminal, and a
        #    redraw must not leave a stale copy of it behind (the screen is rendered by pyte).
        long_cjk = "这是一段用来检查中文宽度的很长的输入文本" * 3
        probe.send(long_cjk)
        probe.drain(quiet=0.6)
        screen = probe.screen_text()
        lines = [line for line in probe.screen_lines() if line.strip()]
        widest = max((display_width(line) for line in lines), default=0)
        # The whole input must appear exactly once on the rendered screen; wrapping may split it
        # across lines, so compare against the screen with padding/line breaks removed.
        flat = screen.replace(" ", "").replace("\n", "")
        occurrences = flat.count(long_cjk)
        probe.check(
            f"CJK input renders inside {probe.cols} columns with its tail visible",
            widest <= probe.cols and long_cjk[-6:] in screen,
            f"widest={widest} tail_visible={long_cjk[-6:] in screen}",
        )
        probe.check(
            "the rendered input is shown exactly once (no stale duplicate)",
            occurrences == 1,
            f"the exact input appears {occurrences}x on screen",
        )

        # clear the line, then 3. bracketed multi-line paste -----------------
        probe.send("\x15")  # Ctrl-U
        probe.drain(quiet=0.3)
        before = len(probe.raw)
        paste = "\x1b[200~line one\nline two\nline three\x1b[201~"
        probe.send(paste)
        text = probe.drain(quiet=0.6)
        pasted = strip_ansi(bytes(probe.raw[before:]))
        probe.check(
            "multi-line paste echoes all three lines",
            "line one" in pasted and "line two" in pasted and "line three" in pasted,
        )
        probe.check("paste does not dispatch before explicit submit", len(requests) == 0,
                    f"request count before submit={len(requests)}")

        # submit the pasted prompt: the hang server keeps the client busy -----
        before_busy = len(probe.raw)
        probe.send("\r")
        time.sleep(1.5)
        probe.drain(quiet=0.5)
        busy_text = strip_ansi(bytes(probe.raw[before_busy:]))
        captured = json.dumps(requests, ensure_ascii=False)
        probe.check("paste dispatches exactly one request containing all three lines",
                    len(requests) == 1 and "line one\\nline two\\nline three" in captured,
                    f"request count={len(requests)}; complete multiline={('line one' in captured and 'line three' in captured)}")
        probe.check(
            "busy state is visible while the provider hangs",
            any(word in busy_text.lower() for word in ("working", "thinking", "waiting", "busy", "…", "cancell")),
        )

        # 6a. cancel ------------------------------------------------------
        before_cancel = len(probe.raw)
        probe.send("\x03")  # Ctrl-C
        probe.drain(quiet=0.8, hard=20.0)
        cancel_text = strip_ansi(bytes(probe.raw[before_cancel:]))
        probe.check(
            "cancel is visible and returns to the prompt",
            any(word in cancel_text.lower() for word in ("cancel", "interrupt", "aborted"))
            or "\n>" in cancel_text
            or cancel_text.rstrip().endswith(">"),
        )

        # 4. resize: the typed text must survive a redraw, and the redraw must respect the new
        #    width. The input stays unsubmitted across the resize on purpose.
        typed = "中文字段保留检查"
        probe.send(typed)
        probe.drain(quiet=0.4)
        probe.set_winsize(60, 20)
        probe.drain(quiet=0.8)
        resized_screen = probe.screen_text()
        resized_lines = [line for line in probe.screen_lines() if line.strip()]
        resized_widest = max((display_width(line) for line in resized_lines), default=0)
        probe.check(
            "resize keeps the in-progress input visible (redraw does not swallow it)",
            typed in resized_screen,
            "typed text disappeared from the screen after resize",
        )
        probe.check(
            "post-resize lines fit the new 60-column width",
            resized_widest <= 60,
            f"widest={resized_widest}",
        )
        probe.send("\x15")  # clear the input again
        probe.set_winsize(100, 30)
        before_status = len(probe.raw)
        probe.send("/status\r")
        probe.drain(quiet=0.8)
        status_delta = strip_ansi(bytes(probe.raw[before_status:]))
        probe.check(
            "the prompt still answers after resize",
            "session" in status_delta or "policy" in status_delta,
        )

        # 5. scroll: heavy output then a normal command ---------------------
        before_help = len(probe.raw)
        probe.send("/help\r")
        probe.drain(quiet=0.8)
        scroll = strip_ansi(bytes(probe.raw[before_help:]))
        before_status = len(probe.raw)
        probe.send("/status\r")
        probe.drain(quiet=0.8)
        after_scroll = strip_ansi(bytes(probe.raw[before_status:]))
        probe.check(
            "heavy output scrolls and the prompt still answers",
            ("usage" in scroll.lower() or "/" in scroll) and ("session" in after_scroll or "policy" in after_scroll),
        )

        # 7a. resume -------------------------------------------------------
        before_resume = len(probe.raw)
        probe.send("/resume latest\r")
        probe.drain(quiet=1.0)
        resumed = strip_ansi(bytes(probe.raw[before_resume:]))
        probe.check(
            "resume reports a visible outcome",
            "resumed" in resumed.lower() or "error" in resumed.lower(),
        )

        # 7. clean exit ----------------------------------------------------
        probe.send("/quit\r")
        time.sleep(0.8)
        probe.drain(quiet=0.6)
        deadline = time.time() + 5
        while probe.alive() and time.time() < deadline:
            time.sleep(0.2)
        probe.check("/quit exits the process cleanly", not probe.alive())
    finally:
        probe.stop()

    # 6b. error path: a closed port fails immediately -------------------------
    env_error = dict(env)
    env_error["KROMA_BASE_URL"] = f"http://127.0.0.1:{dead_port}/v1"
    err_probe = Probe([binary, "repl", "--workspace", workspace, "--policy", "read-only"], env_error)
    err_probe.start()
    try:
        err_probe.drain(quiet=0.8, hard=20.0)
        before_error = len(err_probe.raw)
        err_probe.send("hello there\r")
        err_probe.drain(quiet=1.5, hard=30.0)
        err_text = strip_ansi(bytes(err_probe.raw[before_error:]))
        probe.check(
            "provider failure surfaces an error state",
            any(word in err_text.lower() for word in ("error", "failed", "refused", "unreachable", "connect")),
        )
        err_probe.send("/quit\r")
        err_probe.drain(quiet=0.5)
    finally:
        err_probe.stop()

    server.close()

    # -- logs ---------------------------------------------------------------
    raw_path = os.path.join(args.log, "pty-raw.bin")
    session_path = os.path.join(args.log, "pty-session.txt")
    error_path = os.path.join(args.log, "pty-error-raw.bin")
    screen_path = os.path.join(args.log, "pty-final-screen.txt")
    with open(raw_path, "wb") as fh:
        fh.write(bytes(probe.raw))
    with open(session_path, "w", encoding="utf-8") as fh:
        fh.write(strip_ansi(bytes(probe.raw)))
    with open(error_path, "wb") as fh:
        fh.write(bytes(err_probe.raw))
    with open(screen_path, "w", encoding="utf-8") as fh:
        fh.write(probe.screen_text())
    # The captured requests use only this probe's local fixture credentials; never log headers.
    with open(os.path.join(args.log, "provider-fixture-requests.json"), "w", encoding="utf-8") as fh:
        json.dump(requests, fh, ensure_ascii=False, indent=2)
    with open(os.path.join(args.log, "checks.json"), "w", encoding="utf-8") as fh:
        json.dump(probe.checks, fh, ensure_ascii=False, indent=2)

    passed = sum(1 for _, ok, _ in probe.checks if ok)
    total = len(probe.checks)
    print(f"\n{passed}/{total} PTY checks passed; transcripts in {args.log}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
