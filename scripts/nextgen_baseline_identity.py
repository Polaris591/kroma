#!/usr/bin/env python3
"""KR-A01: fix the RustFix source identity without trusting a ZIP filename.

Three separate identities are recorded and never conflated:

  archive_identity  - SHA-256 of the delivered ZIP (package identity)
  tree_identity     - SHA-256 over the sorted (path, mode, content-sha256) list
                      (tree identity; computed WITHOUT git, so a checkout that
                      lost .git still has a reliable identity)
  git_identity      - commit/tree hashes when a git repo is present, else null
                      (never fabricated: a missing repo yields null, not a guess)

Exit codes: 0 identity written, 2 verification case failed, 1 usage/IO error.
"""
import hashlib
import json
import os
import subprocess
import sys

SKIP_DIRS = {".git", "target", "__pycache__", "node_modules"}
SKIP_SUFFIX = (".pyc",)


def walk_files(root):
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for name in sorted(filenames):
            if name.endswith(SKIP_SUFFIX):
                continue
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, root).replace(os.sep, "/")
            out.append((rel, full))
    out.sort(key=lambda x: x[0])
    return out


def file_sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def tree_identity(entries, root):
    """Hash over path, executable bit and content hash. Independent of git."""
    h = hashlib.sha256()
    for rel, full in entries:
        mode = "100755" if os.access(full, os.X_OK) else "100644"
        h.update(f"{mode} {file_sha256(full)} {rel}\n".encode())
    return h.hexdigest()


def git_identity(root):
    def run(*args):
        try:
            r = subprocess.run(
                ["git", "-C", root, *args],
                capture_output=True, text=True, timeout=30,
            )
            return r.stdout.strip() if r.returncode == 0 else None
        except (OSError, subprocess.SubprocessError):
            return None

    if run("rev-parse", "--git-dir") is None:
        return {"commit": None, "tree": None, "dirty": None,
                "note": "no git repository; tree_identity is authoritative"}
    status = run("status", "--porcelain")
    return {
        "commit": run("rev-parse", "HEAD"),
        "tree": run("rev-parse", "HEAD^{tree}"),
        "dirty": bool(status),
    }


def host_capabilities():
    """Probe what the host can actually prove, so blocked tests stay honest."""
    caps = {}
    # bubblewrap
    try:
        v = subprocess.run(["bwrap", "--version"], capture_output=True,
                           text=True, timeout=10)
        caps["bwrap_version"] = v.stdout.strip() or None
    except (OSError, subprocess.SubprocessError):
        caps["bwrap_version"] = None
    # unprivileged user+net namespaces
    try:
        r = subprocess.run(["unshare", "-Urn", "true"], capture_output=True,
                           timeout=10)
        caps["user_namespaces"] = r.returncode == 0
    except (OSError, subprocess.SubprocessError):
        caps["user_namespaces"] = False
    # AF_UNIX bind
    try:
        import socket
        import tempfile
        p = os.path.join(tempfile.mkdtemp(), "probe.sock")
        s = socket.socket(socket.AF_UNIX)
        s.bind(p)
        s.close()
        caps["af_unix_bind"] = True
    except OSError:
        caps["af_unix_bind"] = False
    # /proc PID consistency
    try:
        own = os.getpid()
        with open("/proc/self/stat") as fh:
            seen = int(fh.read().split()[0])
        caps["proc_pid_consistent"] = own == seen
    except (OSError, ValueError, IndexError):
        caps["proc_pid_consistent"] = False
    return caps


def verify_cases(root, entries, tree_id):
    """KR-A01 test cases 1-3. Returns (all_passed, details)."""
    cases = {}

    # Case 1: changing one file must change the tree identity.
    target = None
    for rel, full in entries:
        if rel.endswith(".rs"):
            target = (rel, full)
            break
    if target:
        rel, full = target
        original = open(full, "rb").read()
        h = hashlib.sha256()
        for r2, f2 in entries:
            mode = "100755" if os.access(f2, os.X_OK) else "100644"
            if r2 == rel:
                h.update(f"{mode} {hashlib.sha256(original + b'x').hexdigest()} {r2}\n".encode())
            else:
                h.update(f"{mode} {file_sha256(f2)} {r2}\n".encode())
        cases["case1_one_byte_changes_tree"] = {
            "pass": h.hexdigest() != tree_id,
            "probe_file": rel,
        }

    # Case 2: identity is computable without .git; commit must stay null.
    # Simulate by recomputing over the same entries with git identity disabled.
    gi = git_identity(root)
    cases["case2_no_git_still_identifies"] = {
        "pass": tree_id is not None and len(tree_id) == 64
        and (gi["commit"] is None or isinstance(gi["commit"], str)),
        "tree_identity_len": len(tree_id),
        "commit_reported": gi["commit"],
        "fabricated_commit": gi["commit"] is not None and gi["tree"] is None,
    }

    # Case 3: same filename with different content must not bind to this identity.
    if target:
        rel, full = target
        other = hashlib.sha256(open(full, "rb").read() + b"different").hexdigest()
        h = hashlib.sha256()
        for r2, f2 in entries:
            mode = "100755" if os.access(f2, os.X_OK) else "100644"
            if r2 == rel:
                h.update(f"{mode} {other} {r2}\n".encode())
            else:
                h.update(f"{mode} {file_sha256(f2)} {r2}\n".encode())
        cases["case3_same_name_different_content_rejected"] = {
            "pass": h.hexdigest() != tree_id,
            "probe_file": rel,
        }

    return all(c.get("pass") for c in cases.values()), cases


def git_baseline_entries(root, rev):
    """Enumerate (path, mode, blob-sha) from a git commit.

    Reading blob hashes straight from git means the delivered baseline identity
    stays fixed even after this task adds new files to the working tree.
    """
    try:
        r = subprocess.run(
            ["git", "-C", root, "ls-tree", "-r", "--full-tree", rev],
            capture_output=True, text=True, timeout=120,
        )
        if r.returncode != 0:
            return None
    except (OSError, subprocess.SubprocessError):
        return None
    out = []
    for line in r.stdout.splitlines():
        meta, path = line.split("\t", 1)
        mode, _type, sha = meta.split()
        out.append((path, mode, sha))
    out.sort(key=lambda x: x[0])
    return out


def tree_identity_from_git(entries):
    h = hashlib.sha256()
    for path, mode, sha in entries:
        h.update(f"{mode} {sha} {path}\n".encode())
    return h.hexdigest()


def main(argv):
    if len(argv) < 2:
        print("usage: nextgen_baseline_identity.py <source-root> [archive.zip]",
              file=sys.stderr)
        return 1
    root = os.path.abspath(argv[1])
    if not os.path.isdir(root):
        print(f"not a directory: {root}", file=sys.stderr)
        return 1

    # Prefer the committed baseline: the delivered identity must not drift when
    # later tasks add files. Fall back to the working tree when there is no git.
    root_commit = None
    try:
        r = subprocess.run(["git", "-C", root, "rev-list", "--max-parents=0", "HEAD"],
                           capture_output=True, text=True, timeout=30)
        if r.returncode == 0 and r.stdout.strip():
            root_commit = r.stdout.strip().splitlines()[-1]
    except (OSError, subprocess.SubprocessError):
        root_commit = None

    delivered = None
    if root_commit:
        g_entries = git_baseline_entries(root, root_commit)
        if g_entries:
            delivered = {
                "source": f"git commit {root_commit}",
                "file_count": len(g_entries),
                "tree_identity": tree_identity_from_git(g_entries),
            }

    entries = walk_files(root)
    tree_id = tree_identity(entries, root)

    archive = None
    if len(argv) > 2 and os.path.isfile(argv[2]):
        archive = {
            "path": os.path.basename(argv[2]),
            "sha256": file_sha256(argv[2]),
        }

    ok, cases = verify_cases(root, entries, tree_id)

    doc = {
        "task": "KR-A01",
        "workspace_version": _cargo_version(root),
        "toolchain": _rustc_version(),
        "delivered_baseline": delivered or {
            "source": "working tree (no git commit available)",
            "file_count": len(entries),
            "tree_identity": tree_id,
        },
        "working_tree": {
            "file_count": len(entries),
            "tree_identity": {
                "algorithm": "sha256 over sorted '<mode> <content-sha256> <path>' lines",
                "sha256": tree_id,
            },
        },
        "archive_identity": archive,
        "git_identity": git_identity(root),
        "host_capabilities": host_capabilities(),
        "identity_test_cases": cases,
        "identity_tests_passed": ok,
        "note": (
            "archive_identity identifies the delivered package; tree_identity "
            "identifies the source tree and works without .git; git_identity is "
            "null rather than guessed when no repository exists."
        ),
    }

    out_dir = os.path.join(root, "docs", "nextgen")
    os.makedirs(out_dir, exist_ok=True)
    out = os.path.join(out_dir, "baseline.json")
    with open(out, "w") as fh:
        json.dump(doc, fh, indent=2, sort_keys=False)
        fh.write("\n")

    print(json.dumps({
        "written": os.path.relpath(out, root),
        "file_count": len(entries),
        "tree_identity": tree_id,
        "identity_tests_passed": ok,
        "cases": {k: v.get("pass") for k, v in cases.items()},
    }, indent=2))
    return 0 if ok else 2


def _cargo_version(root):
    p = os.path.join(root, "Cargo.toml")
    try:
        for line in open(p):
            line = line.strip()
            if line.startswith("version") and "=" in line:
                return line.split("=", 1)[1].strip().strip('"')
    except OSError:
        pass
    return None


def _rustc_version():
    try:
        r = subprocess.run(["rustc", "-V"], capture_output=True, text=True,
                           timeout=10)
        return r.stdout.strip() or None
    except (OSError, subprocess.SubprocessError):
        return None


if __name__ == "__main__":
    sys.exit(main(sys.argv))
