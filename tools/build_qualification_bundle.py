#!/usr/bin/env python3
"""Build a deterministic, self-contained qualification evidence bundle."""
from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import shutil
import stat
import sys
import tarfile
import tempfile
from pathlib import Path

MAX_FILES = 4096
MAX_TOTAL_BYTES = 2 * 1024 * 1024 * 1024


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_tree(root: Path) -> list[Path]:
    try:
        root_mode = root.lstat().st_mode
    except OSError as error:
        raise ValueError(f"directorio inaccesible: {root}: {error}") from error
    if stat.S_ISLNK(root_mode) or not stat.S_ISDIR(root_mode):
        raise ValueError(f"directorio inválido o indirecto: {root}")
    root = root.resolve(strict=True)
    files: list[Path] = []
    total = 0
    for current, dirs, names in os.walk(root, followlinks=False):
        current_path = Path(current)
        for name in dirs:
            path = current_path / name
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
                raise ValueError(f"directorio indirecto o inválido: {path}")
        for name in names:
            path = current_path / name
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
                raise ValueError(f"archivo indirecto o inválido: {path}")
            files.append(path)
            total += path.stat().st_size
            if len(files) > MAX_FILES:
                raise ValueError(f"bundle excede {MAX_FILES} archivos")
            if total > MAX_TOTAL_BYTES:
                raise ValueError(f"bundle excede {MAX_TOTAL_BYTES} bytes")
    return sorted(files)


def add_file(archive: tarfile.TarFile, source: Path, arcname: str, epoch: int) -> None:
    info = archive.gettarinfo(str(source), arcname=arcname)
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.mtime = epoch
    if info.isfile():
        info.mode = 0o644
        with source.open("rb") as handle:
            archive.addfile(info, handle)
    else:
        info.mode = 0o755
        archive.addfile(info)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--matrix", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source-date-epoch", type=int, default=0)
    args = parser.parse_args()
    try:
        validate_tree(args.evidence_dir)
        evidence = args.evidence_dir.resolve(strict=True)
        for label, candidate in (("matrix", args.matrix), ("report", args.report)):
            mode = candidate.lstat().st_mode
            if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
                raise ValueError(f"{label} inválido o indirecto: {candidate}")
        matrix = args.matrix.resolve(strict=True)
        report = args.report.resolve(strict=True)
        json.loads(matrix.read_text(encoding="utf-8"))
        json.loads(report.read_text(encoding="utf-8"))

        with tempfile.TemporaryDirectory(prefix="sysopt-qualification-") as temp_text:
            stage = Path(temp_text) / "qualification"
            stage.mkdir()
            shutil.copytree(evidence, stage / "records", symlinks=False)
            shutil.copy2(matrix, stage / "release-matrix.json")
            shutil.copy2(report, stage / "qualification-report.json")
            files = validate_tree(stage)
            manifest_lines = [
                f"{sha256(path)}  {path.relative_to(stage).as_posix()}"
                for path in files
                if path.name != "MANIFEST.sha256"
            ]
            (stage / "MANIFEST.sha256").write_text("\n".join(manifest_lines) + "\n", encoding="utf-8")
            files = validate_tree(stage)

            args.output.parent.mkdir(parents=True, exist_ok=True)
            temporary = args.output.with_suffix(args.output.suffix + f".tmp-{os.getpid()}")
            # tarfile's ``w:gz`` mode writes a gzip header with wall-clock
            # metadata. Stream through GzipFile with an explicit mtime and an
            # empty embedded filename so identical inputs produce identical
            # bundle bytes on every runner.
            with temporary.open("wb") as raw:
                with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=args.source_date_epoch) as compressed:
                    with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                        directories = sorted(
                            {Path("qualification")} | {
                                Path("qualification") / path.relative_to(stage).parent
                                for path in files
                                if path.relative_to(stage).parent != Path(".")
                            },
                            key=lambda path: (len(path.parts), path.as_posix()),
                        )
                        for directory in directories:
                            info = tarfile.TarInfo(directory.as_posix())
                            info.type = tarfile.DIRTYPE
                            info.mode = 0o755
                            info.uid = info.gid = 0
                            info.uname = info.gname = "root"
                            info.mtime = args.source_date_epoch
                            archive.addfile(info)
                        for path in files:
                            add_file(
                                archive,
                                path,
                                (Path("qualification") / path.relative_to(stage)).as_posix(),
                                args.source_date_epoch,
                            )
            os.replace(temporary, args.output)
        print(args.output)
        return 0
    except (OSError, ValueError, json.JSONDecodeError, tarfile.TarError) as error:
        print(f"ERROR qualification bundle: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
