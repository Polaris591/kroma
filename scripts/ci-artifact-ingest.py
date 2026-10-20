#!/usr/bin/env python3
"""CI artifact read path (M0-03-A3).

Downloading an artifact needs a token; *reading* one does not. This script turns a
downloaded GitHub Actions artifact — a `.zip` or an extracted directory — into a
manifest of (name, size, sha256) that anyone can recompute, and can verify a manifest
against the bytes on disk later.

The download step stays BLOCKED in `docs/goal/ENVELOPE.json` until a token exists;
this script never touches the network.

Usage:
    python3 ci-artifact-ingest.py build  --run-id <id> --artifact <path>... --out <manifest.json>
    python3 ci-artifact-ingest.py verify --manifest <manifest.json>

Exit codes:
    0  manifest written / verified
    1  verify found a mismatch, or an input path is missing
    2  bad usage (no artifacts given)
"""

import argparse
import hashlib
import json
import sys
import zipfile
from pathlib import Path

FORMAT = "kroma.ci_artifact.manifest/1"


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def entries_from_zip(path):
    with zipfile.ZipFile(path) as archive:
        return [
            {
                "path": info.filename,
                "size": info.file_size,
                "sha256": sha256_bytes(archive.read(info)),
            }
            for info in archive.infolist()
            if not info.is_dir()
        ]


def entries_from_dir(path):
    return [
        {
            "path": str(file.relative_to(path)),
            "size": file.stat().st_size,
            "sha256": sha256_bytes(file.read_bytes()),
        }
        for file in sorted(path.rglob("*"))
        if file.is_file()
    ]


def artifact_record(path):
    if path.is_dir():
        entries = entries_from_dir(path)
    elif path.suffix == ".zip":
        entries = entries_from_zip(path)
    else:
        entries = [{"path": path.name, "size": path.stat().st_size,
                    "sha256": sha256_bytes(path.read_bytes())}]
    return {
        "name": path.stem if path.is_file() else path.name,
        "source": str(path.resolve()),
        "file_count": len(entries),
        "total_bytes": sum(entry["size"] for entry in entries),
        "files": entries,
    }


def build(args):
    artifacts = [artifact_record(path) for path in args.artifact]
    manifest = {
        "format": FORMAT,
        "run_id": args.run_id,
        "artifact_count": len(artifacts),
        "total_bytes": sum(artifact["total_bytes"] for artifact in artifacts),
        "artifacts": artifacts,
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"manifest {args.out}: {len(artifacts)} artifact(s), {manifest['total_bytes']} bytes")
    return 0


def verify(args):
    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    mismatches = []
    for artifact in manifest["artifacts"]:
        source = Path(artifact["source"])
        if source.is_dir():
            actual = {entry["path"]: entry for entry in entries_from_dir(source)}
        elif source.suffix == ".zip":
            actual = {entry["path"]: entry for entry in entries_from_zip(source)}
        elif source.is_file():
            actual = {source.name: {"path": source.name, "size": source.stat().st_size,
                                    "sha256": sha256_bytes(source.read_bytes())}}
        else:
            mismatches.append(f"missing source {source}")
            continue
        for entry in artifact["files"]:
            got = actual.get(entry["path"])
            if got is None:
                mismatches.append(f"missing {artifact['name']}/{entry['path']}")
            elif got["sha256"] != entry["sha256"] or got["size"] != entry["size"]:
                mismatches.append(f"changed {artifact['name']}/{entry['path']}")

    if mismatches:
        for line in mismatches:
            print(f"MISMATCH {line}")
        return 1
    print(f"OK {args.manifest}: {manifest['artifact_count']} artifact(s) match")
    return 0


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    build_parser = sub.add_parser("build")
    build_parser.add_argument("--run-id", required=True)
    build_parser.add_argument("--artifact", type=Path, action="append", default=[])
    build_parser.add_argument("--out", type=Path, required=True)
    verify_parser = sub.add_parser("verify")
    verify_parser.add_argument("--manifest", type=Path, required=True)
    args = parser.parse_args()

    if args.command == "build":
        if not args.artifact:
            print("usage: build needs at least one --artifact", file=sys.stderr)
            return 2
        return build(args)
    return verify(args)


if __name__ == "__main__":
    raise SystemExit(main())
