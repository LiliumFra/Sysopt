#!/usr/bin/env python3
"""Build a deterministic source archive from RELEASE-MANIFEST.sha256."""
from __future__ import annotations

import argparse
import gzip
import hashlib
import os
import re
import stat
import sys
import tarfile
from pathlib import Path

SHA_LINE = re.compile(r"^([0-9a-f]{64})  (.+)$")
MAX_FILES = 100_000
MAX_TOTAL_BYTES = 8 * 1024 * 1024 * 1024


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def secure_file(root: Path, relative: Path) -> Path:
    if relative.is_absolute() or not relative.parts or ".." in relative.parts:
        raise ValueError(f"ruta insegura en manifiesto: {relative}")
    current = root
    for part in relative.parts:
        current = current / part
        mode = current.lstat().st_mode
        if stat.S_ISLNK(mode):
            raise ValueError(f"ruta indirecta en manifiesto: {relative}")
    resolved_root = root.resolve(strict=True)
    resolved = current.resolve(strict=True)
    resolved.relative_to(resolved_root)
    if not stat.S_ISREG(resolved.stat().st_mode):
        raise ValueError(f"entrada no regular en manifiesto: {relative}")
    return resolved


def parse_manifest(root: Path, manifest: Path) -> list[tuple[Path, Path]]:
    manifest_mode = manifest.lstat().st_mode
    if stat.S_ISLNK(manifest_mode) or not stat.S_ISREG(manifest_mode):
        raise ValueError(f"manifiesto inválido o indirecto: {manifest}")
    entries: list[tuple[Path, Path]] = []
    seen: set[str] = set()
    total = 0
    for number, line in enumerate(manifest.read_text(encoding="utf-8").splitlines(), start=1):
        match = SHA_LINE.fullmatch(line)
        if not match:
            raise ValueError(f"línea de manifiesto inválida: {number}")
        expected, relative_text = match.groups()
        relative = Path(relative_text)
        normalized = relative.as_posix()
        if normalized in seen:
            raise ValueError(f"ruta duplicada en manifiesto: {normalized}")
        seen.add(normalized)
        source = secure_file(root, relative)
        if sha256(source) != expected:
            raise ValueError(f"hash fuente no coincide: {normalized}")
        total += source.stat().st_size
        if total > MAX_TOTAL_BYTES:
            raise ValueError(f"fuente excede {MAX_TOTAL_BYTES} bytes")
        entries.append((relative, source))
        if len(entries) > MAX_FILES:
            raise ValueError(f"manifiesto excede {MAX_FILES} archivos")
    if not entries:
        raise ValueError("manifiesto fuente vacío")
    return entries


def add_file(
    archive: tarfile.TarFile,
    source: Path,
    arcname: str,
    epoch: int,
) -> None:
    info = archive.gettarinfo(str(source), arcname=arcname)
    info.uid = info.gid = 0
    info.uname = info.gname = "root"
    info.mtime = epoch
    info.mode = 0o755 if source.stat().st_mode & 0o111 else 0o644
    with source.open("rb") as handle:
        archive.addfile(info, handle)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--prefix", required=True)
    parser.add_argument("--source-date-epoch", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        if not args.prefix or Path(args.prefix).name != args.prefix or args.prefix in {".", ".."}:
            raise ValueError("prefijo de archivo inválido")
        root_mode = args.root.lstat().st_mode
        if stat.S_ISLNK(root_mode) or not stat.S_ISDIR(root_mode):
            raise ValueError("raíz fuente inválida o indirecta")
        root = args.root.resolve(strict=True)
        manifest = args.manifest
        if not manifest.is_absolute():
            manifest = root / manifest
        entries = parse_manifest(root, manifest)

        files = entries + [(Path("RELEASE-MANIFEST.sha256"), manifest.resolve(strict=True))]
        directories = {Path(args.prefix)}
        for relative, _ in files:
            parent = Path(args.prefix) / relative.parent
            while parent != Path(".") and parent not in directories:
                directories.add(parent)
                if parent == Path(args.prefix):
                    break
                parent = parent.parent

        args.output.parent.mkdir(parents=True, exist_ok=True)
        temporary = args.output.with_suffix(args.output.suffix + f".tmp-{os.getpid()}")
        try:
            with temporary.open("wb") as raw:
                with gzip.GzipFile(
                    filename="",
                    mode="wb",
                    fileobj=raw,
                    mtime=args.source_date_epoch,
                ) as compressed:
                    with tarfile.open(
                        fileobj=compressed,
                        mode="w",
                        format=tarfile.PAX_FORMAT,
                    ) as archive:
                        for directory in sorted(
                            directories, key=lambda path: (len(path.parts), path.as_posix())
                        ):
                            info = tarfile.TarInfo(directory.as_posix())
                            info.type = tarfile.DIRTYPE
                            info.mode = 0o755
                            info.uid = info.gid = 0
                            info.uname = info.gname = "root"
                            info.mtime = args.source_date_epoch
                            archive.addfile(info)
                        for relative, source in sorted(files, key=lambda item: item[0].as_posix()):
                            add_file(
                                archive,
                                source,
                                (Path(args.prefix) / relative).as_posix(),
                                args.source_date_epoch,
                            )
            os.replace(temporary, args.output)
        finally:
            temporary.unlink(missing_ok=True)
        print(args.output)
        return 0
    except (OSError, UnicodeError, ValueError, tarfile.TarError) as error:
        print(f"ERROR source archive: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
