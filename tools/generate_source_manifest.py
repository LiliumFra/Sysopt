#!/usr/bin/env python3
"""Generate or verify the deterministic SHA-256 manifest for the source tree."""
from __future__ import annotations

import argparse
import hashlib
import os
import stat
import sys
from pathlib import Path

EXCLUDED_DIRS = {".git", "target", "__pycache__", ".pytest_cache", ".mypy_cache"}
EXCLUDED_FILES = {"RELEASE-MANIFEST.sha256"}
MAX_FILES = 100_000
MAX_TOTAL_BYTES = 8 * 1024 * 1024 * 1024


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def collect(root: Path) -> list[Path]:
    mode = root.lstat().st_mode
    if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
        raise ValueError(f"raíz inválida o indirecta: {root}")
    root = root.resolve(strict=True)
    files: list[Path] = []
    total = 0
    for current, dirs, names in os.walk(root, topdown=True, followlinks=False):
        current_path = Path(current)
        kept_dirs: list[str] = []
        for name in sorted(dirs):
            path = current_path / name
            path_mode = path.lstat().st_mode
            if stat.S_ISLNK(path_mode) or not stat.S_ISDIR(path_mode):
                raise ValueError(f"directorio indirecto o inválido: {path.relative_to(root)}")
            if name not in EXCLUDED_DIRS:
                kept_dirs.append(name)
        dirs[:] = kept_dirs
        for name in sorted(names):
            path = current_path / name
            relative = path.relative_to(root)
            if relative.as_posix() in EXCLUDED_FILES or name.endswith((".pyc", ".pyo")):
                continue
            path_mode = path.lstat().st_mode
            if stat.S_ISLNK(path_mode) or not stat.S_ISREG(path_mode):
                raise ValueError(f"archivo indirecto o inválido: {relative}")
            size = path.stat().st_size
            total += size
            files.append(path)
            if len(files) > MAX_FILES:
                raise ValueError(f"demasiados archivos: {len(files)} > {MAX_FILES}")
            if total > MAX_TOTAL_BYTES:
                raise ValueError(f"árbol fuente excede {MAX_TOTAL_BYTES} bytes")
    return sorted(files, key=lambda path: path.relative_to(root).as_posix())


def render(root: Path) -> str:
    resolved = root.resolve(strict=True)
    return "".join(
        f"{sha256(path)}  {path.relative_to(resolved).as_posix()}\n" for path in collect(root)
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path("."))
    parser.add_argument("--output", type=Path, default=Path("RELEASE-MANIFEST.sha256"))
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    try:
        expected = render(args.root)
        output = args.output
        if not output.is_absolute():
            output = args.root / output
        if args.check:
            if output.is_symlink() or not output.is_file():
                raise ValueError(f"manifiesto ausente o indirecto: {output}")
            actual = output.read_text(encoding="utf-8")
            if actual != expected:
                raise ValueError("RELEASE-MANIFEST.sha256 está desactualizado")
            print(f"source manifest: PASS ({expected.count(chr(10))} files)")
            return 0
        output.parent.mkdir(parents=True, exist_ok=True)
        temporary = output.with_suffix(output.suffix + f".tmp-{os.getpid()}")
        try:
            temporary.write_text(expected, encoding="utf-8")
            os.replace(temporary, output)
        finally:
            temporary.unlink(missing_ok=True)
        print(f"{output} ({expected.count(chr(10))} files)")
        return 0
    except (OSError, UnicodeError, ValueError) as error:
        print(f"ERROR source manifest: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
