#!/usr/bin/env python3
"""Validación estática profunda y reproducible para SysOpt.

No reemplaza build, Clippy, tests ni pruebas nativas. Esos controles forman
parte del workflow de release y de la matriz de calificación obligatoria.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import plistlib
import re
import shutil
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

try:
    import yaml  # type: ignore
except Exception:
    yaml = None

EXPECTED_VERSION = "0.7.0-rc1"
BASE_VERSION = "0.7.0"
FULL_SHA = re.compile(r"^[0-9a-f]{40}$")


@dataclass
class Finding:
    level: str
    name: str
    detail: str


class Validator:
    def __init__(self, root: Path) -> None:
        self.root = root.resolve()
        self.findings: list[Finding] = []

    def add(self, level: str, name: str, detail: str) -> None:
        self.findings.append(Finding(level, name, detail))

    def check(self, condition: bool, name: str, ok: str, error: str) -> None:
        self.add("PASS" if condition else "FAIL", name, ok if condition else error)

    def text(self, relative: str) -> str:
        return (self.root / relative).read_text(encoding="utf-8")

    def files(self, pattern: str) -> list[Path]:
        return sorted(p for p in self.root.glob(pattern) if p.is_file())

    def rel(self, path: Path) -> str:
        return str(path.relative_to(self.root))

    def run(self, command: list[str], *, cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            command,
            cwd=cwd or self.root,
            capture_output=True,
            text=True,
            check=False,
        )

    def structured_files(self) -> None:
        errors: list[str] = []
        tomls = sorted(set(self.files("**/*.toml")))
        for path in tomls:
            try:
                with path.open("rb") as handle:
                    tomllib.load(handle)
            except Exception as exc:
                errors.append(f"{self.rel(path)}: {exc}")
        self.check(not errors, "TOML", f"{len(tomls)} archivo(s) parseados", "; ".join(errors))

        jsons = self.files("qualification/**/*.json")
        errors = []
        for path in jsons:
            try:
                json.loads(path.read_text(encoding="utf-8"))
            except Exception as exc:
                errors.append(f"{self.rel(path)}: {exc}")
        self.check(not errors, "JSON", f"{len(jsons)} archivo(s) parseados", "; ".join(errors))

        yamls = self.files(".github/**/*.yml") + self.files(".github/**/*.yaml")
        if yaml is None:
            self.add("WARN", "YAML", "PyYAML no está disponible; CI vuelve a parsear los workflows")
        else:
            errors = []
            for path in yamls:
                try:
                    yaml.safe_load(path.read_text(encoding="utf-8"))
                except Exception as exc:
                    errors.append(f"{self.rel(path)}: {exc}")
            self.check(not errors, "YAML", f"{len(yamls)} archivo(s) parseados", "; ".join(errors))

        plists = self.files("**/*.plist")
        errors = []
        for path in plists:
            try:
                plistlib.loads(path.read_bytes())
            except Exception as exc:
                errors.append(f"{self.rel(path)}: {exc}")
        self.check(not errors, "PLIST", f"{len(plists)} archivo(s) parseados", "; ".join(errors))

    def source_syntax(self) -> None:
        scripts = [
            path
            for path in self.files("**/*")
            if path.suffix in {".sh", ".command"}
            or path.name in {"postinst", "prerm", "postrm", "preinstall", "postinstall", "sysopt-control"}
        ]
        errors: list[str] = []
        for path in scripts:
            head = path.read_bytes()[:80]
            shell = "bash" if path.suffix in {".sh", ".command"} or b"bash" in head else "sh"
            result = self.run([shell, "-n", str(path)])
            if result.returncode:
                errors.append(f"{self.rel(path)}: {(result.stderr or result.stdout).strip()}")
        self.check(not errors, "Shell", f"{len(scripts)} script(s) validados", "; ".join(errors))

        cargo = shutil.which("cargo")
        if cargo:
            result = self.run([cargo, "fmt", "--all", "--", "--check"])
            self.check(
                result.returncode == 0,
                "Rustfmt",
                "workspace formateado",
                (result.stderr or result.stdout).strip(),
            )
        else:
            self.add("BLOCKER", "Rustfmt", "cargo no está en PATH")

        ps_files = self.files("**/*.ps1")
        pwsh = shutil.which("pwsh") or shutil.which("powershell")
        if pwsh:
            errors = []
            for path in ps_files:
                quoted = str(path).replace("'", "''")
                command = (
                    "$errors=$null; [System.Management.Automation.PSParser]::Tokenize"
                    f"((Get-Content -Raw '{quoted}'), [ref]$errors) | Out-Null; "
                    "if($errors.Count){$errors | Out-String | Write-Error; exit 1}"
                )
                result = self.run([pwsh, "-NoProfile", "-Command", command])
                if result.returncode:
                    errors.append(f"{self.rel(path)}: {(result.stderr or result.stdout).strip()}")
            self.check(not errors, "PowerShell", f"{len(ps_files)} script(s) parseados", "; ".join(errors))
        else:
            self.add("WARN", "PowerShell", "pwsh no está disponible; los runners Windows lo validan")

    def version_consistency(self) -> None:
        cargo = tomllib.loads(self.text("Cargo.toml"))
        version = cargo["workspace"]["package"]["version"]
        iss = self.text("packaging/windows/sysopt.iss")
        plist = self.text("packaging/macos/control-app/Contents/Info.plist")
        lock = tomllib.loads(self.text("Cargo.lock"))
        workspace_names = {"telemetry", "enforcement", "policy", "smart-cache", "intelligence", "sysopt"}
        local_versions = {
            package["name"]: package["version"]
            for package in lock.get("package", [])
            if package.get("name") in workspace_names
        }
        docs = [
            "README.md",
            "ROADMAP.md",
            "DESIGN.md",
            "SECURITY.md",
            "RELEASE-READY.md",
            "VALIDATION.md",
            "AUDIT-REPORT.md",
            "docs/PLATFORM-NOTES.md",
        ]
        missing_docs = [path for path in docs if EXPECTED_VERSION not in self.text(path)]
        ok = (
            version == EXPECTED_VERSION
            and all(value == EXPECTED_VERSION for value in local_versions.values())
            and f'#define AppVersion "{EXPECTED_VERSION}"' in iss
            and plist.count(f"<string>{BASE_VERSION}</string>") >= 2
            and not missing_docs
            and f"## {EXPECTED_VERSION}" in self.text("CHANGELOG.md")
        )
        self.check(
            ok,
            "Versión coherente",
            f"workspace, lock, instaladores y documentos usan {EXPECTED_VERSION}",
            f"workspace={version}; lock={local_versions}; docs sin versión={missing_docs}",
        )

    def workflow(self) -> None:
        body = self.text(".github/workflows/release.yml")
        uses = re.findall(r"^\s*-?\s*uses:\s*([^@\s]+)@([^\s#]+)", body, re.M)
        mutable = [f"{action}@{ref}" for action, ref in uses if not action.startswith("./") and not FULL_SHA.fullmatch(ref)]
        self.check(not mutable, "Actions fijadas", f"{len(uses)} uso(s) externos fijados por SHA", ", ".join(mutable))

        required_runners = {
            "ubuntu-24.04",
            "ubuntu-24.04-arm",
            "windows-2025",
            "windows-11-arm",
            "macos-15",
            "macos-15-intel",
        }
        missing = sorted(value for value in required_runners if value not in body)
        self.check(not missing and "-latest" not in body, "Matriz nativa", "seis runners fijos x86-64/ARM64", f"faltan={missing}")

        cargo_lines = [line.strip() for line in body.splitlines() if re.search(r"\bcargo (?:metadata|build|test|clippy|run)\b", line)]
        unlocked = [line for line in cargo_lines if "--locked" not in line and "cargo fmt" not in line]
        self.check(not unlocked, "Cargo locked", "build/test/clippy/run usan Cargo.lock", "; ".join(unlocked))

        markers = [
            "cargo clippy --locked --workspace --all-targets -- -D warnings",
            "cargo test --locked --workspace --all-targets --verbose",
            "cargo test --locked --workspace --doc --verbose",
            "cargo audit -D warnings",
            "ecoqos_roundtrip_current_process",
            "native_cgroup_smoke.sh",
            "tools/test_installers.py",
            "tools/test_workflow_contracts.py",
            "tools/test_release_evidence.py",
            "tools/test_source_artifacts.py",
            "tools/generate_source_manifest.py --root . --check",
            "tools/build_source_archive.py",
            "tools/write_asset_manifest.py",
            "tools/build_qualification_bundle.py",
            "tools/verify_package_manifests.py",
            "package-manifest-integrity",
            "qualification-evidence.tar.gz",
            "sysopt-bench",
            "WINDOWS_CERTIFICATE_PFX_BASE64",
            "signtool",
            "xcrun notarytool submit",
            "xcrun stapler validate",
            "generate_cyclonedx.py",
            "gh attestation trusted-root",
            "actions/attest@",
            "qualify_release.py validate",
            "needs.hardware-qualification.result == 'skipped'",
            "needs.hardware-qualification.result == 'success'",
        ]
        missing = [marker for marker in markers if marker not in body]
        self.check(
            not missing,
            "Release automática",
            "tests, benchmarks, firma, notarización, SBOM, attestations y gate estable conectados",
            f"faltan={missing}",
        )

        stable_gate = all(
            marker in body
            for marker in [
                "Una release estable requiere certificado Authenticode",
                "Una release estable requiere Developer ID y credenciales de notarización",
                "hardware-qualification:",
                "required_for_stable",
            ]
        )
        # required_for_stable está en la matriz, no necesariamente en el workflow.
        stable_gate = (
            "Una release estable requiere certificado Authenticode" in body
            and "Una release estable requiere Developer ID y credenciales de notarización" in body
            and "hardware-qualification:" in body
        )
        self.check(stable_gate, "Bloqueo de estable", "credenciales y hardware son obligatorios para tags estables", "gate estable incompleto")

        contract = self.run([sys.executable, "tools/test_workflow_contracts.py"])
        self.check(
            contract.returncode == 0,
            "Contrato del workflow",
            "evidencia no vacía, tests exactos y bundle autocontenido",
            (contract.stderr or contract.stdout).strip(),
        )

        evidence_tests = self.run([sys.executable, "tools/test_release_evidence.py"])
        self.check(
            evidence_tests.returncode == 0,
            "Evidencia de release",
            "manifiestos, rechazo de enlaces, detección de alteraciones y bundle determinista",
            (evidence_tests.stderr or evidence_tests.stdout).strip(),
        )

        source_tests = self.run([sys.executable, "tools/test_source_artifacts.py"])
        manifest_check = self.run(
            [sys.executable, "tools/generate_source_manifest.py", "--root", ".", "--check"]
        )
        self.check(
            source_tests.returncode == 0 and manifest_check.returncode == 0,
            "Artefacto fuente",
            "manifiesto completo y archivo fuente reproducible verificados",
            " | ".join(
                value
                for value in [
                    (source_tests.stderr or source_tests.stdout).strip(),
                    (manifest_check.stderr or manifest_check.stdout).strip(),
                ]
                if value
            ),
        )

    def ecoqos(self) -> None:
        windows = self.text("crates/enforcement/src/windows_impl.rs")
        journal = self.text("crates/enforcement/src/journal.rs")
        policy = self.text("crates/policy/src/lib.rs")
        markers = [
            "GetProcessInformation",
            "SetProcessInformation",
            "ProcessPowerThrottling",
            "PROCESS_POWER_THROTTLING_EXECUTION_SPEED",
            "PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION",
            "ProcessPowerPolicy::Eco",
            "ProcessPowerPolicy::High",
            "TimerResolutionPolicy::SystemManaged",
            "TimerResolutionPolicy::Ignore",
            "TimerResolutionPolicy::Respect",
            "windows_power_throttling",
            "get_power_throttling",
            "set_power_throttling",
            "ecoqos_roundtrip_current_process",
        ]
        combined = windows + journal + policy
        missing = [marker for marker in markers if marker not in combined]
        self.check(not missing, "Windows EcoQoS", "Power Throttling nativo, temporizadores y roundtrip", f"faltan={missing}")
        durable = all(marker in combined for marker in ["PowerThrottlingValue", "previous_applied", "identity_from_handle", "mark_applied", "recover_power"])
        self.check(durable, "Rollback EcoQoS", "identidad, journal y restauración conservadora", "rollback durable incompleto")

    def cgroup(self) -> None:
        backend = self.text("crates/enforcement/src/cgroup_v2.rs")
        journal = self.text("crates/enforcement/src/cgroup_journal.rs")
        markers = [
            "GroupControlJournal",
            '"cpu.weight"',
            '"io.weight"',
            '"memory.high"',
            'format!("default {value}")',
            "cgroup.subtree_control",
            "prepare(group_path",
            "mark_applied(group_path)",
            "restore_control_record",
            "ensure_regular_control_file",
            "previous_applied",
            "try_lock_exclusive",
            "recupera_transaccion_preparada_y_confirma_aplicacion",
            "conserva_original_y_registra_valor_aplicado_anterior",
        ]
        combined = backend + journal
        missing = [marker for marker in markers if marker not in combined]
        self.check(not missing, "cgroup v2 transaccional", "membresía y controles compartidos con rollback durable", f"faltan={missing}")
        unsafe = ["Command::new(\"sh\")", "follow_links(true)"]
        self.check(not any(marker in combined for marker in unsafe), "cgroup confinado", "sin shell general ni seguimiento de enlaces", "atajos inseguros detectados")

    def telemetry_safety(self) -> None:
        telemetry = self.text("crates/telemetry/src/lib.rs")
        safety = self.text("crates/app/src/safety.rs")
        overhead = self.text("crates/app/src/overhead.rs")
        metrics = self.text("crates/app/src/metrics.rs")
        main = self.text("crates/app/src/main.rs")
        macos_qos = self.text("crates/app/src/macos_qos.rs")
        markers = [
            '"/proc/pressure/cpu"',
            '"/proc/pressure/memory"',
            '"/proc/pressure/io"',
            "GetSystemPowerStatus",
            "power_supply",
            "thermal_zone",
            "isLowPowerModeEnabled",
            "thermalState",
            "_NET_ACTIVE_WINDOW",
            "frontmostApplication",
        ]
        missing = [marker for marker in markers if marker not in telemetry]
        self.check(not missing, "Telemetría nativa", "PSI, energía, térmica y foreground multiplataforma", f"faltan={missing}")

        safety_markers = [
            "max_consecutive_failure_cycles",
            "max_failure_ratio",
            "cooldown_secs",
            "restore_on_trip",
            "adaptive_psi_enabled",
            "battery_saver",
            "thermal_state",
        ]
        missing = [marker for marker in safety_markers if marker not in safety + main]
        self.check(not missing, "Seguridad adaptativa", "disyuntor, PSI adaptativo, batería y térmica", f"faltan={missing}")

        overhead_markers = ["p95_budget_ms", "recovery_cycles", "top_n_divisor", "interval_multiplier", "allow_cache", "allow_semantic"]
        self.check(all(marker in overhead + main for marker in overhead_markers), "Presupuesto de overhead", "cinco niveles con recuperación gradual", "control adaptativo incompleto")

        metrics_markers = ["application/openmetrics-text", "is_loopback", "sysopt_overhead_level", "sysopt_safety_circuit_open", "impl Drop for MetricsServer"]
        self.check(all(marker in metrics for marker in metrics_markers), "OpenMetrics local", "loopback obligatorio y cierre limpio", "exportador incompleto")

        qos_markers = ["QOS_CLASS_UTILITY", "IOPOL_THROTTLE", "pthread_get_qos_class_np", "getiopolicy_np", "impl Drop for WorkerQosGuard"]
        self.check(all(marker in macos_qos for marker in qos_markers), "QoS macOS", "QoS utility e I/O throttled reversibles", "guard macOS incompleto")

    def benchmark_and_supply_chain(self) -> None:
        bench = self.text("crates/app/src/bin/sysopt-bench.rs")
        check = self.text("tools/check_benchmarks.py")
        self.check(
            all(marker in bench + check for marker in ["100usize, 500, 2_000", "p95_microseconds", "benchmark_cache", "--dataset", "process_count"]),
            "Benchmarks reproducibles",
            "100/500/2000 procesos y dataset SmartCache",
            "arnés de benchmark incompleto",
        )

        generator = self.text("tools/generate_cyclonedx.py")
        qualifier = self.text("tools/qualify_release.py")
        matrix = json.loads(self.text("qualification/release-matrix.json"))
        cases = matrix.get("cases", [])
        ids = {case.get("id") for case in cases}
        required = {
            "ecoqos-roundtrip-x86_64",
            "ecoqos-roundtrip-arm64",
            "cgroup-v2-native-controls",
            "benchmark-sata-4gib",
            "benchmark-sata-8gib",
            "benchmark-nvme-16gib",
            "authenticode-x86_64",
            "authenticode-arm64",
            "developer-id-notarization-arm64",
            "developer-id-notarization-x86_64",
            "cyclonedx-sbom",
            "github-build-provenance",
            "package-windows-x86_64",
            "package-windows-arm64",
            "package-linux-x86_64",
            "package-linux-arm64",
            "package-macos-arm64",
            "package-macos-x86_64",
        }
        self.check(required <= ids and len(cases) >= 41, "Matriz de calificación", f"{len(cases)} casos; estable exige hardware, firmas y supply chain", f"faltan={sorted(required - ids)}")
        self.check(
            all(case.get("required_for_stable") is True for case in cases),
            "Cobertura estable",
            "todos los casos definidos bloquean la promoción estable",
            "hay casos no obligatorios para estable",
        )
        supply_markers = ["CycloneDX", '"specVersion": "1.6"', "purl", "hashlib.sha256", '"sha256"', '"commit"', '"tag"']
        self.check(all(marker in generator + qualifier for marker in supply_markers), "Supply chain", "SBOM determinista y evidencia ligada a tag/commit/hash", "metadatos de supply chain incompletos")

    def installers(self) -> None:
        sh = self.text("scripts/install.sh")
        ps = self.text("scripts/install.ps1")
        tester = self.text("tools/test_installers.py")
        markers_sh = [
            "--offline-asset-dir",
            "--require-attestation",
            "--require-code-signature",
            "DOWNLOAD_TIMEOUT",
            "sha256_file",
            "install_file_atomically",
            "gh attestation verify",
            "trusted-root",
            "Cargo.lock",
            "--locked",
        ]
        markers_ps = [
            "OfflineAssetDir",
            "RequireAttestation",
            "RequireCodeSignature",
            "TimeoutSec",
            "Get-FileHash",
            "Install-FileAtomically",
            "attestation verify",
            "Get-AuthenticodeSignature",
            "Cargo.lock",
            "--locked",
        ]
        missing = [f"sh:{m}" for m in markers_sh if m not in sh] + [f"ps:{m}" for m in markers_ps if m not in ps]
        self.check(not missing, "Instaladores verificables", "online/offline, timeout, hash, firma, attestation y actualización atómica", f"faltan={missing}")
        self.check("symlink" in tester.lower() and "attestation" in tester.lower(), "Contrato de instaladores", "casos negativos offline y confianza probados", "tests de instalador insuficientes")

    def persistence_and_diagnostics(self) -> None:
        combined = "\n".join(
            self.text(path)
            for path in [
                "crates/enforcement/src/journal.rs",
                "crates/enforcement/src/cgroup_journal.rs",
                "crates/app/src/runtime.rs",
                "crates/app/src/overhead.rs",
                "crates/smart-cache/src/lib.rs",
            ]
        )
        markers = ["sync_all()", "try_lock_exclusive", "previous_applied", "schema_version", "symlink_metadata"]
        self.check(all(marker in combined for marker in markers), "Persistencia durable", "fsync, locks, esquema y anti-symlink", "persistencia incompleta")

        main = self.text("crates/app/src/main.rs")
        analysis = self.text("crates/app/src/analysis.rs")
        markers = ["--analyze", "--analyze-json", "--export-report", "--self-test", "write_analysis_report", "sanitized_process"]
        self.check(all(marker in main + analysis for marker in markers), "Diagnóstico explicable", "análisis humano/JSON, exportación y self-test", "diagnóstico incompleto")

    def docs(self) -> None:
        required = [
            "README.md",
            "ROADMAP.md",
            "DESIGN.md",
            "SECURITY.md",
            "RELEASE-READY.md",
            "VALIDATION.md",
            "AUDIT-REPORT.md",
            "docs/PLATFORM-NOTES.md",
            "docs/RECOVERY.md",
            "docs/PRESSURE-SAFETY.md",
            "docs/DIAGNOSTICS.md",
        ]
        missing_files = [path for path in required if not (self.root / path).is_file()]
        stale = []
        forbidden_current_claims = [
            "EcoQoS permanece como trabajo futuro",
            "controles compartidos de cgroup permanecen deliberadamente desactivados",
            "modo conservador de membresía solamente",
        ]
        for path in required:
            if not (self.root / path).is_file():
                continue
            body = self.text(path)
            if EXPECTED_VERSION not in body:
                stale.append(f"{path}: versión")
            if path != "CHANGELOG.md":
                stale.extend(f"{path}: {claim}" for claim in forbidden_current_claims if claim in body)
        self.check(not missing_files and not stale, "Documentación", "arquitectura, seguridad, recuperación, diagnóstico y release actualizados", f"faltan={missing_files}; obsoleto={stale}")

    def source_hygiene(self) -> None:
        forbidden: list[str] = []
        ignored_dirs = {".git"}
        forbidden_dirs = {"target", "__pycache__", ".pytest_cache", ".mypy_cache"}
        for path in self.root.rglob("*"):
            relative_parts = path.relative_to(self.root).parts
            if any(part in ignored_dirs for part in relative_parts):
                continue
            if any(part in forbidden_dirs for part in relative_parts):
                forbidden.append(self.rel(path))
            elif path.is_file() and (path.suffix in {".pyc", ".pyo"} or path.name.endswith("~")):
                forbidden.append(self.rel(path))
        self.check(not forbidden, "Higiene del artefacto", "sin builds, cachés ni bytecode", "; ".join(forbidden[:20]))

        rust = self.files("crates/**/*.rs")
        bad = []
        duplicate_param = re.compile(r"fn\s+\w+\s*\((.*?)\)\s*(?:->|\{)", re.S)
        for path in rust:
            body = path.read_text(encoding="utf-8")
            production = body.split("#[cfg(test)]", 1)[0]
            if re.search(r"\b(?:todo|unimplemented)!\s*\(", production):
                bad.append(f"{self.rel(path)}: macro pendiente")
            for match in duplicate_param.finditer(body):
                params = []
                depth = 0
                part = ""
                pieces = []
                for char in match.group(1):
                    if char in "([{<": depth += 1
                    elif char in ")]}>" and depth: depth -= 1
                    if char == "," and depth == 0:
                        pieces.append(part); part = ""
                    else:
                        part += char
                pieces.append(part)
                for piece in pieces:
                    name = piece.strip().split(":", 1)[0].strip().lstrip("&mut ")
                    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name): params.append(name)
                duplicates = sorted({name for name in params if params.count(name) > 1})
                if duplicates:
                    bad.append(f"{self.rel(path)}: parámetros duplicados {duplicates}")
        self.check(not bad, "Anomalías Rust", "sin TODOs ni parámetros duplicados detectables", "; ".join(bad))

        lock = self.root / "Cargo.lock"
        if lock.is_file():
            self.add("PASS", "Cargo.lock", hashlib.sha256(lock.read_bytes()).hexdigest())
        else:
            self.add("BLOCKER", "Cargo.lock", "ausente")

    def execute(self) -> int:
        self.structured_files()
        self.source_syntax()
        self.version_consistency()
        self.workflow()
        self.ecoqos()
        self.cgroup()
        self.telemetry_safety()
        self.benchmark_and_supply_chain()
        self.installers()
        self.persistence_and_diagnostics()
        self.docs()
        self.source_hygiene()

        order = {"FAIL": 0, "BLOCKER": 1, "WARN": 2, "PASS": 3}
        for finding in sorted(self.findings, key=lambda item: (order.get(item.level, 9), item.name)):
            print(f"[{finding.level}] {finding.name}: {finding.detail}")
        counts = {level: sum(item.level == level for item in self.findings) for level in ("PASS", "WARN", "FAIL", "BLOCKER")}
        print("\nSUMMARY " + " ".join(f"{key}={value}" for key, value in counts.items()))
        return 1 if counts["FAIL"] or counts["BLOCKER"] else 0


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args(list(argv) if argv is not None else None)
    return Validator(args.root).execute()


if __name__ == "__main__":
    raise SystemExit(main())
