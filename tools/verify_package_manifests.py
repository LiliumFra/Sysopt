#!/usr/bin/env python3
"""Verify native package manifests against the exact assets selected for release."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import sys
from pathlib import Path
from typing import Any

CASE_RE = re.compile(r"^package-[A-Za-z0-9._-]+$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
MAX_ASSETS = 1024
MAX_TOTAL_BYTES = 16 * 1024 * 1024 * 1024
IGNORED_ASSET_NAMES = {"ARTIFACTS.sha256", "SHA256SUMS.txt"}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def secure_regular(path: Path, root: Path) -> bool:
    try:
        root_mode = root.lstat().st_mode
        if stat.S_ISLNK(root_mode) or not stat.S_ISDIR(root_mode):
            return False
        root_resolved = root.resolve(strict=True)
        relative = path.relative_to(root)
        current = root
        for part in relative.parts:
            current = current / part
            mode = current.lstat().st_mode
            if stat.S_ISLNK(mode):
                return False
        resolved = path.resolve(strict=True)
        resolved.relative_to(root_resolved)
        return stat.S_ISREG(resolved.stat().st_mode)
    except (OSError, RuntimeError, ValueError):
        return False


def load_json(path: Path, *, max_bytes: int = 1024 * 1024) -> Any:
    if path.stat().st_size > max_bytes:
        raise ValueError(f"JSON demasiado grande: {path}")
    return json.loads(path.read_text(encoding="utf-8"))


def expected_package_cases(matrix: Path) -> set[str]:
    data = load_json(matrix)
    if data.get("schema_version") != 1 or not isinstance(data.get("cases"), list):
        raise ValueError("matriz de calificación inválida")
    cases = {
        case.get("id")
        for case in data["cases"]
        if isinstance(case, dict) and case.get("category") == "package"
    }
    if not cases or any(not isinstance(case, str) or not CASE_RE.fullmatch(case) for case in cases):
        raise ValueError("casos package inválidos o ausentes")
    return set(cases)


def verify(
    assets_dir: Path,
    evidence_dir: Path,
    matrix: Path,
    tag: str,
    commit: str,
) -> dict[str, Any]:
    for label, directory in (("assets", assets_dir), ("evidence", evidence_dir)):
        try:
            mode = directory.lstat().st_mode
        except OSError as error:
            raise ValueError(f"directorio {label} inaccesible: {error}") from error
        if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
            raise ValueError(f"directorio {label} inválido o indirecto: {directory}")

    expected = expected_package_cases(matrix)
    manifests: dict[str, dict[str, Any]] = {}
    covered: dict[str, tuple[str, int, str]] = {}
    total = 0

    for case_id in sorted(expected):
        record_path = evidence_dir / f"{case_id}.json"
        if not secure_regular(record_path, evidence_dir):
            raise ValueError(f"registro de paquete ausente o indirecto: {case_id}")
        record = load_json(record_path)
        if (
            record.get("schema_version") != 1
            or record.get("case_id") != case_id
            or record.get("status") != "pass"
            or record.get("tag") != tag
            or record.get("commit") != commit
        ):
            raise ValueError(f"registro de paquete no coincide con la release: {case_id}")
        artifact = record.get("artifact")
        if not isinstance(artifact, dict):
            raise ValueError(f"registro sin manifiesto adjunto: {case_id}")
        relative_text = artifact.get("path")
        if not isinstance(relative_text, str):
            raise ValueError(f"ruta de manifiesto inválida: {case_id}")
        relative = Path(relative_text)
        if relative.is_absolute() or ".." in relative.parts:
            raise ValueError(f"ruta de manifiesto insegura: {case_id}")
        manifest_path = evidence_dir / relative
        if not secure_regular(manifest_path, evidence_dir):
            raise ValueError(f"manifiesto de paquete ausente o indirecto: {case_id}")
        expected_manifest_hash = artifact.get("sha256")
        expected_manifest_size = artifact.get("size_bytes")
        if (
            not isinstance(expected_manifest_hash, str)
            or not SHA256_RE.fullmatch(expected_manifest_hash.lower())
            or sha256(manifest_path) != expected_manifest_hash.lower()
            or expected_manifest_size != manifest_path.stat().st_size
        ):
            raise ValueError(f"manifiesto de paquete alterado: {case_id}")

        manifest = load_json(manifest_path)
        assets = manifest.get("assets")
        expected_platform = case_id.removeprefix("package-")
        if (
            manifest.get("schema_version") != 1
            or manifest.get("tag") != tag
            or manifest.get("commit") != commit
            or manifest.get("platform") != expected_platform
            or not isinstance(assets, list)
            or not assets
        ):
            raise ValueError(f"contenido de manifiesto inválido: {case_id}")
        if len(assets) > MAX_ASSETS:
            raise ValueError(f"demasiados assets en {case_id}")
        manifests[case_id] = manifest

        manifest_total = 0
        for entry in assets:
            if not isinstance(entry, dict):
                raise ValueError(f"entrada de asset inválida: {case_id}")
            name = entry.get("name")
            expected_hash = entry.get("sha256")
            expected_size = entry.get("size_bytes")
            if (
                not isinstance(name, str)
                or not name
                or name != Path(name).name
                or name in {".", ".."}
                or not isinstance(expected_hash, str)
                or not SHA256_RE.fullmatch(expected_hash.lower())
                or not isinstance(expected_size, int)
                or expected_size < 0
            ):
                raise ValueError(f"metadatos de asset inválidos en {case_id}")
            candidate = assets_dir / name
            if not secure_regular(candidate, assets_dir):
                raise ValueError(f"asset ausente o indirecto: {name}")
            actual_size = candidate.stat().st_size
            actual_hash = sha256(candidate)
            if actual_size != expected_size or actual_hash != expected_hash.lower():
                raise ValueError(f"asset no coincide con su manifiesto: {name}")
            previous = covered.get(name)
            current = (actual_hash, actual_size, case_id)
            if previous is not None:
                raise ValueError(
                    f"asset duplicado entre manifiestos: {name} ({previous[2]} y {case_id})"
                )
            covered[name] = current
            manifest_total += actual_size
            total += actual_size
            if total > MAX_TOTAL_BYTES:
                raise ValueError(f"assets exceden {MAX_TOTAL_BYTES} bytes")
        if manifest.get("total_size_bytes") != manifest_total:
            raise ValueError(f"total_size_bytes no coincide: {case_id}")

    actual_primary: set[str] = set()
    for path in assets_dir.iterdir():
        if path.name.endswith(".sha256") or path.name in IGNORED_ASSET_NAMES:
            continue
        if not secure_regular(path, assets_dir):
            raise ValueError(f"asset publicado indirecto o no regular: {path.name}")
        actual_primary.add(path.name)
    covered_names = set(covered)
    if actual_primary != covered_names:
        missing = sorted(actual_primary - covered_names)
        absent = sorted(covered_names - actual_primary)
        raise ValueError(f"cobertura de assets incompleta: sin_manifiesto={missing}; ausentes={absent}")

    return {
        "schema_version": 1,
        "tag": tag,
        "commit": commit,
        "package_manifests": len(manifests),
        "assets_verified": len(covered),
        "total_size_bytes": total,
        "cases": sorted(manifests),
        "ready": True,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--assets-dir", type=Path, required=True)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--matrix", type=Path, required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        report = verify(
            args.assets_dir,
            args.evidence_dir,
            args.matrix,
            args.tag,
            args.commit,
        )
        args.output.parent.mkdir(parents=True, exist_ok=True)
        temporary = args.output.with_suffix(args.output.suffix + f".tmp-{os.getpid()}")
        try:
            temporary.write_text(
                json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
            os.replace(temporary, args.output)
        finally:
            temporary.unlink(missing_ok=True)
        print(args.output)
        return 0
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"ERROR package manifest verification: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
