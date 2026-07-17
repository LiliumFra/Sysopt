#!/usr/bin/env python3
"""Static contract tests for the release workflow's evidence and promotion gates."""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/release.yml"
MATRIX = ROOT / "qualification/release-matrix.json"
FULL_SHA = re.compile(r"^[0-9a-f]{40}$")


def fail(message: str) -> None:
    raise AssertionError(message)


def main() -> int:
    body = WORKFLOW.read_text(encoding="utf-8")
    matrix = json.loads(MATRIX.read_text(encoding="utf-8"))
    ids = {case["id"] for case in matrix["cases"]}

    package_cases = {
        "package-windows-x86_64",
        "package-windows-arm64",
        "package-linux-x86_64",
        "package-linux-arm64",
        "package-macos-arm64",
        "package-macos-x86_64",
    }
    if not package_cases <= ids:
        fail(f"faltan casos de paquete: {sorted(package_cases - ids)}")
    for marker in [
        'package-windows-${{ matrix.arch }}',
        'package-linux-$ARCH',
        'package-macos-$ARCH',
    ]:
        if marker not in body:
            fail(f"el workflow no registra paquetes mediante {marker}")

    required_exact_tests = [
        "tracker::tests::pid_reciclado_no_se_toca -- --exact",
        "tracker::tests::reset_final_ignora_pid_reciclado -- --exact",
        "tracker::tests::descarta_accion_nueva_para_pid_reciclado -- --exact",
        "cgroup_v2::tests::cgroup_membership_journal_reconciles_prepared_entry -- --exact",
        "cgroup_journal::tests::recupera_transaccion_preparada_y_confirma_aplicacion -- --exact",
        "cgroup_journal::tests::conserva_original_y_registra_valor_aplicado_anterior -- --exact",
        "ecoqos_roundtrip_current_process -- --ignored --exact",
        "macos_worker_qos_roundtrip -- --ignored --exact",
    ]
    missing = [test for test in required_exact_tests if test not in body]
    if missing:
        fail(f"faltan tests exactos: {missing}")

    if "cp qualification-evidence/*.json release-assets/" in body:
        fail("el workflow vuelve a publicar solo JSON superiores")
    if "qualification/evidence/$GITHUB_REF_NAME" in body:
        fail("el workflow permite que evidencia versionada sobrescriba evidencia de CI")
    if 'sha256sum "release-assets/$1"' in body:
        fail("los manifiestos SHA contienen rutas incompatibles con su verificación")
    if "if-no-files-found: ignore" in body:
        fail("un artefacto de calificación todavía tolera evidencia ausente")
    if re.search(r"path:\s*\|\s*\n\s+qualification-evidence/\*\*\s*\n\s+(?:benchmark|hardware-benchmark|native-cgroup-smoke)", body):
        fail("el workflow vuelve a mezclar evidencia copiada con archivos raíz colisionables")
    for marker in [
        "tools/build_qualification_bundle.py",
        "qualification-evidence.tar.gz",
        "qualification-evidence/**",
        "tools/write_asset_manifest.py",
        "tools/test_release_evidence.py",
        "tools/test_source_artifacts.py",
        "tools/generate_source_manifest.py --root . --check",
        "tools/build_source_archive.py",
        "SysOpt-${VERSION}-source.tar.gz",
        "tools/verify_package_manifests.py",
        "package-manifest-integrity",
        "SOURCE_DATE_EPOCH",
    ]:
        if marker not in body:
            fail(f"falta contrato de bundle/manifiesto: {marker}")

    uses = re.findall(r"^\s*-?\s*uses:\s*([^@\s]+)@([^\s#]+)", body, re.M)
    mutable = [f"{name}@{ref}" for name, ref in uses if not name.startswith("./") and not FULL_SHA.fullmatch(ref)]
    if mutable:
        fail(f"acciones no fijadas por SHA: {mutable}")

    # Prerelease package jobs must always produce one qualification record before
    # their qualification artifact upload. This avoids empty Windows ARM64 and
    # macOS x86_64 artifacts even when signing is intentionally unavailable.
    for job, case_prefix in [
        ("windows-packages", "package-windows-"),
        ("linux-packages", "package-linux-"),
        ("macos-packages", "package-macos-"),
    ]:
        start = body.find(f"  {job}:")
        if start < 0:
            fail(f"job ausente: {job}")
        match = re.search(r"^  [A-Za-z0-9_-]+:\s*$", body[start + 3 :], re.M)
        next_job = start + 3 + match.start() if match else -1
        section = body[start:] if next_job < 0 else body[start:next_job]
        if case_prefix not in section or "if-no-files-found: error" not in section:
            fail(f"{job} no garantiza evidencia no vacía")

    print("workflow contracts: PASS")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"workflow contracts: FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
