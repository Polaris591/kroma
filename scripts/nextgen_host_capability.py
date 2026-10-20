#!/usr/bin/env python3
"""KR-A02: host capability probe and test-tier declaration.

Every probe records the exact command, exit code and raw output, so a missing
capability is reported as BLOCKED with evidence instead of being folded into a
PASS. Nothing here may claim cross-platform support from a single host.

Output: docs/nextgen/host_capabilities.json
Exit:   0 probes written, 1 usage/IO error. Missing capabilities never fail
        this script -- they are data, not an error -- but `gate_verdict`
        reports which tiers cannot be proven on this host.
"""
import json
import os
import platform
import socket
import subprocess
import sys
import tempfile

# Test tiers. A tier is PROVEN only if its probes actually ran and passed.
TIERS = {
    "pure_logic": "no process, no kernel feature; runs anywhere",
    "process": "spawns child processes; needs a working fork/exec and /proc",
    "kernel_isolation": "requires bwrap plus unprivileged user+net namespaces",
    "ipc_unix_socket": "requires AF_UNIX bind in the test sandbox",
    "cross_platform": "requires an actual Windows/macOS runner; never inferable here",
    "paid_network": "requires a real provider credential and spend authorisation",
}


def run(argv, timeout=20, shell=False):
    """Run a probe and keep everything needed to re-check it later."""
    try:
        r = subprocess.run(argv, capture_output=True, text=True, timeout=timeout,
                           shell=shell)
        return {
            "command": argv if not shell else argv,
            "exit_code": r.returncode,
            "stdout": r.stdout.strip()[:2000],
            "stderr": r.stderr.strip()[:2000],
        }
    except subprocess.TimeoutExpired:
        return {"command": argv, "exit_code": None, "stdout": "",
                "stderr": "timeout"}
    except (OSError, subprocess.SubprocessError) as e:
        return {"command": argv, "exit_code": None, "stdout": "",
                "stderr": f"{type(e).__name__}: {e}"}


def probe_toolchain():
    return {
        "rustc": run(["rustc", "-V"]),
        "cargo": run(["cargo", "-V"]),
        "rustfmt": run(["rustfmt", "--version"]),
        "clippy": run(["cargo", "clippy", "--version"]),
    }


def probe_bwrap():
    out = {"version": run(["bwrap", "--version"])}
    # The real isolation probe: bind root read-only and unshare everything.
    out["namespace_probe"] = run([
        "bwrap", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
        "--unshare-all", "--die-with-parent", "true",
    ])
    out["available"] = (
        out["version"]["exit_code"] == 0
        and out["namespace_probe"]["exit_code"] == 0
    )
    return out


def probe_namespaces():
    return {
        "unshare_user_net": run(["unshare", "-Urn", "true"]),
        "unshare_user": run(["unshare", "-Ur", "true"]),
        "max_user_namespaces": _read("/proc/sys/user/max_user_namespaces"),
        "max_net_namespaces": _read("/proc/sys/user/max_net_namespaces"),
    }


def probe_af_unix():
    result = {"bind_attempt": None}
    try:
        d = tempfile.mkdtemp()
        p = os.path.join(d, "probe.sock")
        s = socket.socket(socket.AF_UNIX)
        try:
            s.bind(p)
            s.listen(1)
            result["bind_attempt"] = {
                "path": p, "ok": True, "errno": None, "error": None,
            }
        finally:
            s.close()
    except OSError as e:
        result["bind_attempt"] = {
            "path": locals().get("p"), "ok": False,
            "errno": e.errno, "error": f"{type(e).__name__}: {e}",
        }
    return result


def probe_proc():
    own = os.getpid()
    seen = None
    err = None
    try:
        with open("/proc/self/stat") as fh:
            seen = int(fh.read().split()[0])
    except (OSError, ValueError, IndexError) as e:
        err = f"{type(e).__name__}: {e}"
    return {
        "getpid": own,
        "proc_self_stat_pid": seen,
        "consistent": own == seen and seen is not None,
        "error": err,
        "proc_version": _read("/proc/version"),
        "in_container": os.path.exists("/.dockerenv"),
    }


def probe_platform():
    return {
        "system": platform.system(),
        "release": platform.release(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "cgroup": _read("/proc/1/cgroup")[:500],
    }


def _read(path):
    try:
        with open(path) as fh:
            return fh.read().strip()
    except OSError:
        return None


def main(argv):
    if len(argv) < 2:
        print("usage: nextgen_host_capability.py <source-root>", file=sys.stderr)
        return 1
    root = os.path.abspath(argv[1])
    if not os.path.isdir(root):
        print(f"not a directory: {root}", file=sys.stderr)
        return 1

    bwrap = probe_bwrap()
    ns = probe_namespaces()
    unix = probe_af_unix()
    proc = probe_proc()

    facts = {
        "bwrap_usable": bwrap["available"],
        "user_namespaces": ns["unshare_user_net"]["exit_code"] == 0,
        "af_unix_bind": unix["bind_attempt"]["ok"],
        "proc_pid_consistent": proc["consistent"],
    }

    # A tier is proven only when its prerequisites are actually present.
    tier_status = {
        "pure_logic": "PROVEN",
        "process": "PROVEN" if facts["proc_pid_consistent"] else "BLOCKED",
        "kernel_isolation": "PROVEN" if (facts["bwrap_usable"] and facts["user_namespaces"]) else "BLOCKED",
        "ipc_unix_socket": "PROVEN" if facts["af_unix_bind"] else "BLOCKED",
        "cross_platform": "BLOCKED",
        "paid_network": "BLOCKED",
    }

    doc = {
        "task": "KR-A02",
        "tier_definitions": TIERS,
        "tier_status": tier_status,
        "facts": facts,
        "probes": {
            "toolchain": probe_toolchain(),
            "bwrap": bwrap,
            "namespaces": ns,
            "af_unix": unix,
            "proc": proc,
            "platform": probe_platform(),
        },
        "blocked_tiers": [k for k, v in tier_status.items() if v != "PROVEN"],
        "gate_verdict": (
            "A full acceptance run cannot be claimed green while any required "
            "tier is BLOCKED. cross_platform and paid_network are BLOCKED by "
            "design on this host: a single Linux box can never prove Windows or "
            "macOS behaviour, and no provider spend is authorised by default."
        ),
        "note": (
            "Missing capability is reported as BLOCKED with its raw probe "
            "output; it is never converted into PASS."
        ),
    }

    out_dir = os.path.join(root, "docs", "nextgen")
    os.makedirs(out_dir, exist_ok=True)
    out = os.path.join(out_dir, "host_capabilities.json")
    with open(out, "w") as fh:
        json.dump(doc, fh, indent=2)
        fh.write("\n")

    print(json.dumps({
        "written": os.path.relpath(out, root),
        "facts": facts,
        "tier_status": tier_status,
        "blocked_tiers": doc["blocked_tiers"],
    }, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
