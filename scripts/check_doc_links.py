#!/usr/bin/env python3
"""Check tracked, maintained Markdown for missing local link targets.

Uses only Python's standard library. Archived reviews and benchmarks are
excluded because they are historical research, not current user documentation.
This checks paths, not remote URLs or fragment/heading anchors.
"""
from __future__ import annotations

import posixpath
import re
import subprocess
import sys
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parent.parent
ROOT_DOCS = {
    "README.md",
    "README.zh-CN.md",
    "CONTRIBUTING.md",
    "CHANGELOG.md",
    "SECURITY.md",
    "PLUGIN_ROADMAP.md",
    "benchmarks/README.md",
    "reviews/README.md",
}
MAINTAINED_PREFIXES = ("docs/", "desktop/", "examples/plugins/", "npm/")
EXCLUDED_PREFIXES = ("reviews/", "benchmarks/")
INLINE = re.compile(r"!?\[[^\]\n]+\]\((<[^>\n]+>|[^)\n]+)\)")
REFERENCE = re.compile(r"^\s{0,3}\[[^\]\n]+\]:\s*(<[^>\n]+>|\S+)")
FENCE = re.compile(r"^\s*(\x60{3,}|~{3,})")


def tracked_files() -> set[str]:
    result = subprocess.check_output(
        ["git", "-C", str(ROOT), "ls-files", "-z"], stderr=subprocess.STDOUT
    )
    return {p.decode("utf-8") for p in result.split(b"\0") if p}


def is_maintained(path: str) -> bool:
    return path.endswith(".md") and (
        path in ROOT_DOCS
        or (not path.startswith(EXCLUDED_PREFIXES) and path.startswith(MAINTAINED_PREFIXES))
    )


def extract_destinations(markdown: str):
    fence_char = ""
    fence_len = 0
    for lineno, line in enumerate(markdown.splitlines(), start=1):
        marker = FENCE.match(line)
        if marker:
            opening = marker.group(1)
            if not fence_char:
                fence_char, fence_len = opening[0], len(opening)
            elif opening[0] == fence_char and len(opening) >= fence_len:
                fence_char, fence_len = "", 0
            continue
        if fence_char:
            continue
        for match in INLINE.finditer(line):
            yield lineno, match.group(1)
        reference = REFERENCE.match(line)
        if reference:
            yield lineno, reference.group(1)


def resolve_local(doc: str, raw: str) -> str | None:
    target = raw.strip()
    if target.startswith("<"):
        target = target[1:].split(">", 1)[0]
    else:
        target = target.split(None, 1)[0]
    if not target or target.startswith("#"):
        return None
    url = urlsplit(target)
    if url.scheme or url.netloc or target.startswith("//"):
        return None
    local = unquote(url.path)
    if not local:
        return None
    result = posixpath.normpath(posixpath.join(posixpath.dirname(doc), local))
    return result


def main() -> int:
    tracked = tracked_files()
    docs = sorted(path for path in tracked if is_maintained(path))
    failures = []
    scanned = 0
    for doc in docs:
        content = (ROOT / doc).read_text(encoding="utf-8")
        for lineno, raw in extract_destinations(content):
            target = resolve_local(doc, raw)
            if target is None:
                continue
            scanned += 1
            if target not in tracked and not any(
                path.startswith(target.rstrip("/") + "/") for path in tracked
            ):
                failures.append((doc, lineno, raw, target))
    for doc, lineno, raw, target in failures:
        print(f"{doc}:{lineno}: missing {raw} (resolved to {target})", file=sys.stderr)
    print(f"Checked {scanned} local links across {len(docs)} maintained Markdown files.")
    if failures:
        print(f"Found {len(failures)} broken local link(s).", file=sys.stderr)
        return 1
    print("All local link targets exist.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
