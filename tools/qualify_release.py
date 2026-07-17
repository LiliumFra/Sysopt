#!/usr/bin/env python3
"""Record and validate tamper-evident release qualification evidence."""
from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import shutil
import stat
import sys
from pathlib import Path
from typing import Any

CASE_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
MAX_RECORD_BYTES = 256 * 1024
MAX_CLOCK_SKEW = dt.timedelta(minutes=10)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load_json(path: Path) -> Any:
    if path.stat().st_size > MAX_RECORD_BYTES:
        raise ValueError(f"JSON excede {MAX_RECORD_BYTES} bytes: {path}")
    return json.loads(path.read_text(encoding="utf-8"))


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


def record(args: argparse.Namespace) -> int:
    if not CASE_ID_RE.fullmatch(args.case):
        raise ValueError("case_id contiene caracteres no permitidos")
    if len(args.tag) > 256 or not args.tag:
        raise ValueError("tag inválido")
    if len(args.commit) > 128 or not args.commit:
        raise ValueError("commit inválido")
    if args.details and len(args.details) > 4096:
        raise ValueError("details excede 4096 caracteres")

    evidence_file = None
    if args.evidence:
        evidence_mode = args.evidence.lstat().st_mode
        if stat.S_ISLNK(evidence_mode) or not stat.S_ISREG(evidence_mode):
            raise ValueError(f"evidencia inexistente o indirecta: {args.evidence}")
        evidence_file = args.evidence.resolve(strict=True)
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "schema_version": 1,
        "case_id": args.case,
        "status": args.status,
        "tag": args.tag,
        "commit": args.commit,
        "runner": args.runner or os.environ.get("RUNNER_NAME", "local"),
        "timestamp": dt.datetime.now(dt.timezone.utc).isoformat(),
        "details": args.details or "",
    }
    if evidence_file:
        evidence_dir = output.parent / "evidence"
        evidence_dir.mkdir(parents=True, exist_ok=True)
        destination = evidence_dir / f"{args.case}--{evidence_file.name}"
        temporary = destination.with_suffix(destination.suffix + f".tmp-{os.getpid()}")
        try:
            shutil.copyfile(evidence_file, temporary)
            os.replace(temporary, destination)
        finally:
            temporary.unlink(missing_ok=True)
        payload["artifact"] = {
            "path": destination.relative_to(output.parent).as_posix(),
            "name": evidence_file.name,
            "sha256": sha256(destination),
            "size_bytes": destination.stat().st_size,
        }
    temporary_output = output.with_suffix(output.suffix + f".tmp-{os.getpid()}")
    try:
        temporary_output.write_text(
            json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        os.replace(temporary_output, output)
    finally:
        temporary_output.unlink(missing_ok=True)
    print(output)
    return 0


def validate_record(
    record_data: dict[str, Any],
    case_id: str,
    tag: str,
    commit: str,
    evidence_root: Path,
) -> list[str]:
    errors: list[str] = []
    if record_data.get("schema_version") != 1:
        errors.append("schema_version inválida")
    if record_data.get("case_id") != case_id:
        errors.append("case_id no coincide")
    if record_data.get("status") != "pass":
        errors.append(f"status={record_data.get('status')!r}")
    if record_data.get("tag") != tag:
        errors.append("tag no coincide")
    if record_data.get("commit") != commit:
        errors.append("commit no coincide")
    if not isinstance(record_data.get("runner"), str) or not record_data.get("runner"):
        errors.append("runner ausente")
    if not isinstance(record_data.get("details"), str) or len(record_data.get("details", "")) > 4096:
        errors.append("details inválido")

    timestamp = record_data.get("timestamp", "")
    try:
        parsed = dt.datetime.fromisoformat(timestamp)
        if parsed.tzinfo is None:
            errors.append("timestamp sin zona horaria")
        elif parsed > dt.datetime.now(dt.timezone.utc) + MAX_CLOCK_SKEW:
            errors.append("timestamp demasiado adelantado")
    except (TypeError, ValueError):
        errors.append("timestamp inválido")

    artifact = record_data.get("artifact")
    if artifact is not None:
        if not isinstance(artifact, dict):
            errors.append("artifact inválido")
            return errors
        relative_text = artifact.get("path", "")
        try:
            relative = Path(relative_text)
            if not relative_text or relative.is_absolute() or ".." in relative.parts:
                raise ValueError
            evidence_root_resolved = evidence_root.resolve()
            path = (evidence_root_resolved / relative).resolve(strict=True)
            if not regular_file_without_symlink(path, evidence_root_resolved):
                errors.append("archivo de evidencia ausente, indirecto o fuera del directorio")
                return errors
        except (OSError, RuntimeError, ValueError):
            errors.append("ruta de evidencia inválida")
            return errors

        expected_hash = artifact.get("sha256", "")
        if (
            not isinstance(expected_hash, str)
            or len(expected_hash) != 64
            or any(char not in "0123456789abcdef" for char in expected_hash.lower())
        ):
            errors.append("sha256 de evidencia inválido")
        elif sha256(path) != expected_hash.lower():
            errors.append("sha256 de evidencia no coincide")
        expected_size = artifact.get("size_bytes")
        if not isinstance(expected_size, int) or expected_size < 0:
            errors.append("size_bytes de evidencia inválido")
        elif path.stat().st_size != expected_size:
            errors.append("tamaño de evidencia no coincide")
    return errors


def validate(args: argparse.Namespace) -> int:
    matrix = load_json(args.matrix)
    if matrix.get("schema_version") != 1:
        raise ValueError("schema de matriz incompatible")
    cases = matrix.get("cases", [])
    if not isinstance(cases, list):
        raise ValueError("cases debe ser una lista")
    ids = [case.get("id") for case in cases if isinstance(case, dict)]
    if (
        len(ids) != len(cases)
        or not ids
        or any(not isinstance(value, str) or not CASE_ID_RE.fullmatch(value) for value in ids)
        or len(ids) != len(set(ids))
    ):
        raise ValueError("matriz vacía o con IDs inválidos/duplicados")
    known = set(ids)
    required_cases = [
        case for case in cases if args.channel == "stable" and case.get("required_for_stable", False)
    ]
    if args.channel == "prerelease":
        required_cases = [
            case for case in cases if case.get("category") not in {"hardware", "signature"}
        ]
    required = {case["id"] for case in required_cases}

    try:
        root_mode = args.evidence_dir.lstat().st_mode
    except OSError as error:
        raise ValueError(f"directorio de evidencia inaccesible: {error}") from error
    if stat.S_ISLNK(root_mode) or not stat.S_ISDIR(root_mode):
        raise ValueError("directorio de evidencia inválido o indirecto")
    evidence_root = args.evidence_dir.resolve(strict=True)

    failures: list[str] = []
    valid: set[str] = set()
    present: set[str] = set()
    for path in sorted(evidence_root.iterdir()):
        if path.name == "evidence":
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
                failures.append("evidence: directorio adjunto inválido o indirecto")
            continue
        if path.suffix != ".json":
            failures.append(f"{path.name}: archivo raíz inesperado")
            continue
        case_id = path.stem
        if case_id not in known:
            failures.append(f"{case_id}: ID de evidencia desconocido")
            continue
        if case_id in present:
            failures.append(f"{case_id}: evidencia duplicada")
            continue
        present.add(case_id)
        if not regular_file_without_symlink(path, evidence_root):
            failures.append(f"{case_id}: evidencia ausente o indirecta")
            continue
        try:
            errors = validate_record(
                load_json(path), case_id, args.tag, args.commit, evidence_root
            )
        except (OSError, json.JSONDecodeError, ValueError) as error:
            failures.append(f"{case_id}: evidencia ilegible: {error}")
            continue
        if errors:
            failures.append(f"{case_id}: " + "; ".join(errors))
        else:
            valid.add(case_id)

    for case_id in sorted(required - present):
        failures.append(f"{case_id}: evidencia ausente")

    report = {
        "schema_version": 1,
        "channel": args.channel,
        "tag": args.tag,
        "commit": args.commit,
        "required": len(required),
        "passed": len(required & valid),
        "optional_validated": len(valid - required),
        "failures": failures,
        "ready": not failures and required <= valid,
    }
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["ready"] else 1


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    record_parser = sub.add_parser("record")
    record_parser.add_argument("--case", required=True)
    record_parser.add_argument("--status", choices=["pass", "fail", "skip"], required=True)
    record_parser.add_argument("--tag", required=True)
    record_parser.add_argument("--commit", required=True)
    record_parser.add_argument("--output", type=Path, required=True)
    record_parser.add_argument("--evidence", type=Path)
    record_parser.add_argument("--runner")
    record_parser.add_argument("--details")
    record_parser.set_defaults(func=record)

    validate_parser = sub.add_parser("validate")
    validate_parser.add_argument(
        "--matrix", type=Path, default=Path("qualification/release-matrix.json")
    )
    validate_parser.add_argument("--evidence-dir", type=Path, required=True)
    validate_parser.add_argument("--channel", choices=["stable", "prerelease"], required=True)
    validate_parser.add_argument("--tag", required=True)
    validate_parser.add_argument("--commit", required=True)
    validate_parser.add_argument("--report", type=Path)
    validate_parser.set_defaults(func=validate)

    args = parser.parse_args()
    try:
        return args.func(args)
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"ERROR qualification: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
