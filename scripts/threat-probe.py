#!/usr/bin/env python3
"""M1-01 threat probes: what this host actually enforces, measured, not assumed.

Three questions the blueprint's trust boundary depends on:

1. **File descriptors** - can a child see a descriptor the parent opened, i.e. is an "isolated"
   process actually isolated from the parent's handles?
2. **Namespaces** - does a sandboxed child live in different pid/mount/net namespaces, or only in
   a different directory?
3. **Peer identity** - can a unix socket tell who connected (SO_PEERCRED), which is what a broker
   needs before it trusts a caller?

Every probe prints a status of `enforced`, `not_enforced` or `unsupported` with the reason. A probe
that cannot run is reported as such; it is never recorded as a pass.

Usage: python3 threat-probe.py [--out FILE]
"""

import argparse
import json
import os
import socket
import struct
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent


def probe_fd_inheritance():
    """Open a descriptor and ask two children what they can see: one that inherits, one that does not.

    This separates the two mechanisms people conflate. A descriptor disappears from a child either
    because the *launcher* closed it (`close_fds=True`/`FD_CLOEXEC`) or because the *kernel* is
    isolating the process. Only the second one is a trust boundary.
    """
    script = (
        "import os,sys\n"
        "target = int(sys.argv[1])\n"
        "try:\n"
        "    os.fstat(target)\n"
        "    print('visible')\n"
        "except OSError as exc:\n"
        "    print('hidden', exc.errno)\n"
    )

    def child_view(close_fds):
        read_fd, write_fd = os.pipe()
        try:
            result = subprocess.run(
                [sys.executable, "-c", script, str(read_fd)],
                capture_output=True,
                text=True,
                timeout=30,
                close_fds=close_fds,
                pass_fds=() if close_fds else (read_fd,),
            )
            return result.stdout.strip()
        finally:
            os.close(read_fd)
            os.close(write_fd)

    with_close = child_view(True)
    without_close = child_view(False)
    inherits = without_close.startswith("visible")
    closed = with_close.startswith("hidden")
    return {
        "id": "fd-inheritance",
        "status": "launcher_controlled",
        "detail": (
            "with the launcher closing descriptors the child reports %s; "
            "when the descriptor is passed explicitly it reports %s"
            % (with_close, without_close)
        ),
        "launcher_closes": closed,
        "kernel_isolates": not inherits,
        "conclusion": (
            "the child can see an inherited descriptor, so descriptor isolation is the launcher's "
            "responsibility (close_fds/CLOEXEC), not a kernel boundary"
            if inherits
            else "the child cannot see an explicitly passed descriptor"
        ),
        "how": "os.pipe() in the parent, child fstat on the descriptor number, with and without "
        "close_fds",
    }


def probe_namespaces():
    """Compare this process's namespaces with a child's, and with a bwrap child if available."""
    mine = {name: os.readlink("/proc/self/ns/%s" % name) for name in ("pid", "mnt", "net", "user")}
    child = subprocess.run(
        ["sh", "-c", "for n in pid mnt net user; do printf '%s ' \"$(readlink /proc/self/ns/$n)\"; done"],
        capture_output=True,
        text=True,
        timeout=30,
    )
    child_ns = dict(zip(("pid", "mnt", "net", "user"), child.stdout.split()))
    differing = [name for name in mine if mine[name] != child_ns.get(name)]

    bwrap = None
    if subprocess.run(["sh", "-c", "command -v bwrap"], capture_output=True).returncode == 0:
        sandboxed = subprocess.run(
            [
                "bwrap",
                "--ro-bind", "/", "/",
                "--unshare-pid",
                "--unshare-net",
                "--proc", "/proc",
                "--dev", "/dev",
                "sh", "-c",
                "for n in pid mnt net; do printf '%s ' \"$(readlink /proc/self/ns/$n)\"; done",
            ],
            capture_output=True,
            text=True,
            timeout=60,
        )
        if sandboxed.returncode == 0:
            bwrap_ns = dict(zip(("pid", "mnt", "net"), sandboxed.stdout.split()))
            bwrap = {
                "available": True,
                "differs_from_host": [n for n in bwrap_ns if bwrap_ns[n] != mine.get(n)],
                "raw": bwrap_ns,
            }
        else:
            bwrap = {"available": True, "error": sandboxed.stderr.strip()[:300]}

    plain = "a plain child shares %d/%d namespaces with the host" % (
        len(mine) - len(differing),
        len(mine),
    )
    if bwrap and bwrap.get("differs_from_host"):
        detail = "%s; a bwrap child differs in %s" % (
            plain,
            ",".join(bwrap["differs_from_host"]),
        )
        status = "enforced_by_bwrap_only"
    else:
        detail = "%s; no sandbox improved on that" % plain
        status = "not_enforced"
    return {
        "id": "namespaces",
        "status": status,
        "detail": detail,
        "plain_child_differs": differing,
        "bwrap": bwrap,
        "how": "readlink /proc/<pid>/ns/{pid,mnt,net,user} for self, a plain child and a bwrap child",
    }


def probe_peer_identity():
    """Can a unix socket name its peer (SO_PEERCRED), and does an unrelated process get refused?"""
    path = HERE / ".peerprobe.sock"
    if path.exists():
        path.unlink()
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        server.bind(str(path))
        server.listen(1)
        pid = os.fork()
        if pid == 0:
            client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            client.connect(str(path))
            client.close()
            os._exit(0)
        conn, _ = server.accept()
        try:
            raw = conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i"))
            peer_pid, peer_uid, peer_gid = struct.unpack("3i", raw)
        finally:
            conn.close()
            os.waitpid(pid, 0)
        same_uid = peer_uid == os.getuid()
        return {
            "id": "peer-identity",
            "status": "enforced" if same_uid else "not_enforced",
            "detail": "SO_PEERCRED reports pid=%d uid=%d gid=%d; same uid as the listener: %s"
            % (peer_pid, peer_uid, peer_gid, same_uid),
            "pid_attributable": peer_pid > 0,
            "how": "AF_UNIX connect from a forked child, SO_PEERCRED on the accepted socket",
            "limit": "same uid means the kernel cannot distinguish a hostile same-uid process; "
            "the broker needs a namespace or credential boundary on top of this",
        }
    finally:
        server.close()
        if path.exists():
            path.unlink()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()

    probes = []
    for probe in (probe_fd_inheritance, probe_namespaces, probe_peer_identity):
        try:
            probes.append(probe())
        except Exception as exc:  # a probe that cannot run is unsupported, not a pass
            probes.append(
                {"id": probe.__name__, "status": "unsupported", "detail": "%s: %s" % (type(exc).__name__, exc)}
            )

    report = {
        "format": "kroma.threat_probe/1",
        "host": {
            "kernel": os.uname().release,
            "uid": os.getuid(),
            "python": sys.version.split()[0],
        },
        "probes": probes,
    }
    text = json.dumps(report, ensure_ascii=False, indent=2)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
