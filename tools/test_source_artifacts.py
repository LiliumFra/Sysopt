#!/usr/bin/env python3
"""Behavioral tests for source manifests and deterministic source archives."""
from __future__ import annotations

import hashlib
import os
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PYTHON = sys.executable


def run(*args: object, expect: int = 0) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        [str(arg) for arg in args],
        cwd=ROOT,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode != expect:
        raise AssertionError(
            f"command returned {result.returncode}, expected {expect}: {' '.join(map(str, args))}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_manifest(root: Path) -> None:
    manifest = root / "RELEASE-MANIFEST.sha256"
    for line in manifest.read_text(encoding="utf-8").splitlines():
        expected, relative = line.split("  ", 1)
        candidate = root / relative
        if not candidate.is_file() or candidate.is_symlink():
            raise AssertionError(f"missing or indirect source entry: {relative}")
        if sha256(candidate) != expected:
            raise AssertionError(f"source manifest mismatch: {relative}")


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="sysopt-source-artifact-test-") as temp_text:
        temp = Path(temp_text)
        source = temp / "source"
        (source / "src").mkdir(parents=True)
        (source / "src/main.rs").write_text("fn main() {}\n", encoding="utf-8")
        script = source / "install.sh"
        script.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        script.chmod(0o755)
        (source / ".git").mkdir()
        (source / ".git/ignored").write_text("ignored\n", encoding="utf-8")
        (source / "target").mkdir()
        (source / "target/ignored").write_text("ignored\n", encoding="utf-8")

        run(
            PYTHON,
            ROOT / "tools/generate_source_manifest.py",
            "--root",
            source,
            "--output",
            "RELEASE-MANIFEST.sha256",
        )
        run(
            PYTHON,
            ROOT / "tools/generate_source_manifest.py",
            "--root",
            source,
            "--output",
            "RELEASE-MANIFEST.sha256",
            "--check",
        )
        manifest_text = (source / "RELEASE-MANIFEST.sha256").read_text(encoding="utf-8")
        if ".git/" in manifest_text or "target/" in manifest_text:
            raise AssertionError("source manifest included excluded build metadata")

        first = temp / "source-1.tar.gz"
        second = temp / "source-2.tar.gz"
        common = (
            PYTHON,
            ROOT / "tools/build_source_archive.py",
            "--root",
            source,
            "--manifest",
            "RELEASE-MANIFEST.sha256",
            "--prefix",
            "SysOpt-test-source",
            "--source-date-epoch",
            "1700000000",
        )
        run(*common, "--output", first)
        run(*common, "--output", second)
        if first.read_bytes() != second.read_bytes():
            raise AssertionError("source archive is not byte-for-byte deterministic")

        extracted = temp / "extracted"
        extracted.mkdir()
        with tarfile.open(first, "r:gz") as archive:
            archive.extractall(extracted, filter="data")
        extracted_root = extracted / "SysOpt-test-source"
        verify_manifest(extracted_root)
        if extracted_root.joinpath("install.sh").stat().st_mode & 0o111 == 0:
            raise AssertionError("source archive lost executable mode")

        (source / "src/main.rs").write_text("fn main() { panic!(); }\n", encoding="utf-8")
        run(
            PYTHON,
            ROOT / "tools/generate_source_manifest.py",
            "--root",
            source,
            "--output",
            "RELEASE-MANIFEST.sha256",
            "--check",
            expect=2,
        )
        run(*common, "--output", temp / "must-reject-stale.tar.gz", expect=2)

        (source / "src/main.rs").write_text("fn main() {}\n", encoding="utf-8")
        linked = source / "linked.txt"
        linked.symlink_to(source / "src/main.rs")
        run(
            PYTHON,
            ROOT / "tools/generate_source_manifest.py",
            "--root",
            source,
            "--output",
            "RELEASE-MANIFEST.sha256",
            expect=2,
        )

    print("source artifact tests: PASS")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, OSError, ValueError, tarfile.TarError) as error:
        print(f"source artifact tests: FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
