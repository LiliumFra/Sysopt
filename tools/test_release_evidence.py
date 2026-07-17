#!/usr/bin/env python3
"""Behavioral tests for release evidence, manifests and deterministic bundles."""
from __future__ import annotations

import hashlib
import json
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


def digest(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def assert_manifest(extracted: Path) -> None:
    root = extracted / "qualification"
    manifest = root / "MANIFEST.sha256"
    if not manifest.is_file():
        raise AssertionError("bundle has no internal MANIFEST.sha256")
    for line in manifest.read_text(encoding="utf-8").splitlines():
        expected, relative = line.split("  ", 1)
        candidate = root / relative
        if not candidate.is_file() or candidate.is_symlink():
            raise AssertionError(f"manifest entry is absent or indirect: {relative}")
        if digest(candidate) != expected:
            raise AssertionError(f"manifest mismatch: {relative}")


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="sysopt-release-evidence-test-") as temp_text:
        temp = Path(temp_text)
        assets = temp / "assets"
        assets.mkdir()
        (assets / "sysopt.bin").write_bytes(b"sysopt-test-asset\n")
        (assets / "sysopt.bin.sha256").write_text("ignored sidecar\n", encoding="utf-8")
        asset_manifest = temp / "package-assets.json"
        run(
            PYTHON,
            ROOT / "tools/write_asset_manifest.py",
            "--directory",
            assets,
            "--platform",
            "linux-x86_64",
            "--tag",
            "v0.0.0-test",
            "--commit",
            "0123456789abcdef",
            "--output",
            asset_manifest,
        )
        manifest_data = json.loads(asset_manifest.read_text(encoding="utf-8"))
        if [asset["name"] for asset in manifest_data["assets"]] != ["sysopt.bin"]:
            raise AssertionError("asset manifest did not exclude checksum sidecars")

        linked_assets = temp / "assets-link"
        linked_assets.symlink_to(assets, target_is_directory=True)
        run(
            PYTHON,
            ROOT / "tools/write_asset_manifest.py",
            "--directory",
            linked_assets,
            "--platform",
            "linux-x86_64",
            "--tag",
            "v0.0.0-test",
            "--commit",
            "0123456789abcdef",
            "--output",
            temp / "must-not-exist.json",
            expect=2,
        )

        evidence = temp / "qualification-evidence"
        record = evidence / "package-linux-x86_64.json"
        run(
            PYTHON,
            ROOT / "tools/qualify_release.py",
            "record",
            "--case",
            "package-linux-x86_64",
            "--status",
            "pass",
            "--tag",
            "v0.0.0-test",
            "--commit",
            "0123456789abcdef",
            "--output",
            record,
            "--evidence",
            asset_manifest,
            "--details",
            "behavioral test",
        )
        copied = evidence / "evidence" / "package-linux-x86_64--package-assets.json"
        if not copied.is_file():
            raise AssertionError("qualification record did not copy its evidence")

        linked_evidence = temp / "linked-evidence.json"
        linked_evidence.symlink_to(asset_manifest)
        run(
            PYTHON,
            ROOT / "tools/qualify_release.py",
            "record",
            "--case",
            "must-reject-link",
            "--status",
            "pass",
            "--tag",
            "v0.0.0-test",
            "--commit",
            "0123456789abcdef",
            "--output",
            evidence / "must-reject-link.json",
            "--evidence",
            linked_evidence,
            expect=2,
        )

        matrix = temp / "matrix.json"
        matrix.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "cases": [
                        {
                            "id": "package-linux-x86_64",
                            "category": "package",
                            "required_for_stable": True,
                        }
                    ],
                }
            )
            + "\n",
            encoding="utf-8",
        )
        report = temp / "report.json"
        report.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "ready": True,
                    "tag": "v0.0.0-test",
                    "commit": "0123456789abcdef",
                }
            )
            + "\n",
            encoding="utf-8",
        )
        first = temp / "qualification-1.tar.gz"
        second = temp / "qualification-2.tar.gz"
        common = (
            PYTHON,
            ROOT / "tools/build_qualification_bundle.py",
            "--evidence-dir",
            evidence,
            "--matrix",
            matrix,
            "--report",
            report,
            "--source-date-epoch",
            "1700000000",
        )
        verification = temp / "package-manifest-verification.json"
        run(
            PYTHON,
            ROOT / "tools/verify_package_manifests.py",
            "--assets-dir",
            assets,
            "--evidence-dir",
            evidence,
            "--matrix",
            matrix,
            "--tag",
            "v0.0.0-test",
            "--commit",
            "0123456789abcdef",
            "--output",
            verification,
        )
        verification_data = json.loads(verification.read_text(encoding="utf-8"))
        if verification_data.get("assets_verified") != 1 or not verification_data.get("ready"):
            raise AssertionError("package manifest verifier did not validate the expected asset")
        original_asset = (assets / "sysopt.bin").read_bytes()
        (assets / "sysopt.bin").write_bytes(b"tampered asset\n")
        run(
            PYTHON,
            ROOT / "tools/verify_package_manifests.py",
            "--assets-dir",
            assets,
            "--evidence-dir",
            evidence,
            "--matrix",
            matrix,
            "--tag",
            "v0.0.0-test",
            "--commit",
            "0123456789abcdef",
            "--output",
            temp / "must-reject-assets.json",
            expect=2,
        )
        (assets / "sysopt.bin").write_bytes(original_asset)

        run(*common, "--output", first)
        run(*common, "--output", second)
        if first.read_bytes() != second.read_bytes():
            raise AssertionError("qualification bundle is not byte-for-byte deterministic")

        extract = temp / "extract"
        extract.mkdir()
        with tarfile.open(first, "r:gz") as archive:
            archive.extractall(extract, filter="data")
        assert_manifest(extract)

        linked_tree = temp / "qualification-link"
        linked_tree.symlink_to(evidence, target_is_directory=True)
        run(*(
            PYTHON,
            ROOT / "tools/build_qualification_bundle.py",
            "--evidence-dir",
            linked_tree,
            "--matrix",
            matrix,
            "--report",
            report,
            "--source-date-epoch",
            "1700000000",
            "--output",
            temp / "must-reject-tree.tar.gz",
        ), expect=2)

        unknown = evidence / "unknown-case.json"
        unknown.write_text("{}\n", encoding="utf-8")
        run(
            PYTHON,
            ROOT / "tools/qualify_release.py",
            "validate",
            "--matrix",
            matrix,
            "--evidence-dir",
            evidence,
            "--channel",
            "stable",
            "--tag",
            "v0.0.0-test",
            "--commit",
            "0123456789abcdef",
            expect=1,
        )
        unknown.unlink()

        # A record whose copied evidence changes after recording must fail.
        copied.write_text("tampered\n", encoding="utf-8")
        run(
            PYTHON,
            ROOT / "tools/qualify_release.py",
            "validate",
            "--matrix",
            matrix,
            "--evidence-dir",
            evidence,
            "--channel",
            "stable",
            "--tag",
            "v0.0.0-test",
            "--commit",
            "0123456789abcdef",
            expect=1,
        )

    print("release evidence tests: PASS")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, OSError, ValueError, KeyError, json.JSONDecodeError, tarfile.TarError) as error:
        print(f"release evidence tests: FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
