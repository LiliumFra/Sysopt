#!/usr/bin/env python3
"""Offline/static contract tests for the cross-platform installers."""
from __future__ import annotations

import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def require(body: str, markers: list[str], label: str) -> None:
    missing = [marker for marker in markers if marker not in body]
    if missing:
        raise SystemExit(f"{label}: faltan {missing}")


def run(command: list[str], expected: int = 0) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(command, cwd=ROOT, text=True, capture_output=True)
    if proc.returncode != expected:
        raise SystemExit(
            f"comando inesperado ({proc.returncode}, esperado {expected}): {' '.join(command)}\n"
            f"stdout={proc.stdout}\nstderr={proc.stderr}"
        )
    return proc


def main() -> int:
    unix_path = ROOT / "scripts/install.sh"
    windows_path = ROOT / "scripts/install.ps1"
    unix = unix_path.read_text(encoding="utf-8")
    windows = windows_path.read_text(encoding="utf-8")

    run(["bash", "-n", str(unix_path)])
    require(
        unix,
        [
            "--offline-asset-dir",
            "--timeout",
            "--require-attestation",
            "--require-code-signature",
            "trusted_root.jsonl",
            "--custom-trusted-root",
            "--source-ref",
            "install_file_atomically",
            "Cargo.lock",
            "--locked",
        ],
        "instalador Unix",
    )
    require(
        windows,
        [
            "OfflineAssetDir",
            "TimeoutSec",
            "RequireAttestation",
            "RequireCodeSignature",
            "trusted_root.jsonl",
            "--custom-trusted-root",
            "--source-ref",
            "Install-FileAtomically",
            "Cargo.lock",
            "--locked",
        ],
        "instalador Windows",
    )

    # El contrato de seguridad debe rechazar explícitamente una compilación desde
    # fuente cuando se exige verificar el artefacto publicado.
    proc = subprocess.run(
        ["bash", str(unix_path), "--from-source", "--require-attestation"],
        cwd=ROOT,
        text=True,
        capture_output=True,
    )
    if proc.returncode == 0 or "no puede combinarse" not in (proc.stdout + proc.stderr):
        raise SystemExit("el fallback seguro Unix no rechazó --from-source + attestation")

    # Un directorio offline enlazado no puede convertirse en una raíz de confianza.
    with tempfile.TemporaryDirectory(prefix="sysopt-installer-contract-") as temp:
        base = Path(temp)
        real = base / "real"
        real.mkdir()
        link = base / "link"
        link.symlink_to(real, target_is_directory=True)
        proc = subprocess.run(
            ["bash", str(unix_path), "--offline-asset-dir", str(link), "--repo", "owner/repo"],
            cwd=ROOT,
            text=True,
            capture_output=True,
        )
        if proc.returncode == 0 or "enlace" not in (proc.stdout + proc.stderr).lower():
            raise SystemExit("el instalador Unix aceptó una raíz offline enlazada")

    print("installer contract: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
