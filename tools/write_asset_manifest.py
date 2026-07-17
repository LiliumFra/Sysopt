#!/usr/bin/env python3
"""Generate a deterministic manifest for release assets from one native job."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import sys
from pathlib import Path

MAX_FILES = 256
MAX_TOTAL_BYTES = 4 * 1024 * 1024 * 1024


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def regular_file_without_symlink(path: Path, root: Path) -> bool:
    try:
        relative = path.relative_to(root)
    except ValueError:
        return False
    current = root
    try:
        if stat.S_ISLNK(root.lstat().st_mode):
            return False
        for part in relative.parts:
            current = current / part
            mode = current.lstat().st_mode
            if stat.S_ISLNK(mode):
                return False
        return stat.S_ISREG(path.stat().st_mode)
    except OSError:
        return False


def build_manifest(directory: Path, platform: str, tag: str, commit: str) -> dict[str, object]:
    mode = directory.lstat().st_mode
    if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
        raise ValueError(f"directorio de assets inválido o indirecto: {directory}")
    directory = directory.resolve(strict=True)
    files = sorted(path for path in directory.iterdir() if path.is_file())
    if not files:
        raise ValueError("no hay assets para manifestar")
    if len(files) > MAX_FILES:
        raise ValueError(f"demasiados assets: {len(files)} > {MAX_FILES}")

    assets: list[dict[str, object]] = []
    total = 0
    for path in files:
        if path.name.endswith(".sha256") or path.name in {"ARTIFACTS.sha256", "SHA256SUMS.txt"}:
            continue
        if not regular_file_without_symlink(path, directory):
            raise ValueError(f"asset indirecto o no regular: {path.name}")
        size = path.stat().st_size
        total += size
        if total > MAX_TOTAL_BYTES:
            raise ValueError(f"assets exceden {MAX_TOTAL_BYTES} bytes")
        assets.append({"name": path.name, "sha256": sha256(path), "size_bytes": size})
    if not assets:
        raise ValueError("no hay assets primarios para manifestar")
    return {
        "schema_version": 1,
        "platform": platform,
        "tag": tag,
        "commit": commit,
        "runner": os.environ.get("RUNNER_NAME", "local"),
        "assets": assets,
        "total_size_bytes": total,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        manifest = build_manifest(args.directory, args.platform, args.tag, args.commit)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        temporary = args.output.with_suffix(args.output.suffix + f".tmp-{os.getpid()}")
        temporary.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        os.replace(temporary, args.output)
        print(args.output)
        return 0
    except (OSError, ValueError) as error:
        print(f"ERROR asset manifest: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
