#!/usr/bin/env python3
"""Generate a deterministic CycloneDX 1.6 SBOM from Cargo.lock.

The generator deliberately uses only Python's standard library so release jobs do
not install a mutable SBOM tool. It records every locked package and the resolved
Cargo dependency graph available in Cargo.lock.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import tomllib
import urllib.parse
import uuid
from collections import defaultdict
from pathlib import Path
from typing import Any

SCHEMA = "https://cyclonedx.org/schema/bom-1.6.schema.json"
NAMESPACE = uuid.UUID("39f4bc28-b06f-4f44-97df-f5e972c82c35")
DEP_RE = re.compile(r"^(?P<name>[^ ]+)(?: (?P<version>[^ ]+))?(?: \(.+\))?$")


def purl(name: str, version: str, source: str | None) -> str:
    value = f"pkg:cargo/{urllib.parse.quote(name, safe='')}@{urllib.parse.quote(version, safe='')}"
    if source and not source.startswith("registry+"):
        value += "?vcs_url=" + urllib.parse.quote(source, safe="")
    return value


def make_ref(package: dict[str, Any]) -> str:
    base = purl(package["name"], package["version"], package.get("source"))
    source = package.get("source")
    if source and source.startswith("registry+"):
        return base
    if source:
        suffix = hashlib.sha256(source.encode()).hexdigest()[:12]
        return f"{base}#{suffix}"
    return base


def dependency_target(
    raw: str,
    exact: dict[tuple[str, str], list[str]],
    by_name: dict[str, list[str]],
) -> str | None:
    match = DEP_RE.match(raw)
    if not match:
        return None
    name = match.group("name")
    version = match.group("version")
    if version:
        candidates = exact.get((name, version), [])
        if len(candidates) == 1:
            return candidates[0]
    candidates = by_name.get(name, [])
    return candidates[0] if len(candidates) == 1 else None


def generate(lock_path: Path, output: Path, version: str) -> dict[str, Any]:
    lock_bytes = lock_path.read_bytes()
    lock = tomllib.loads(lock_bytes.decode("utf-8"))
    packages: list[dict[str, Any]] = lock.get("package", [])
    refs = [make_ref(package) for package in packages]
    exact: dict[tuple[str, str], list[str]] = defaultdict(list)
    by_name: dict[str, list[str]] = defaultdict(list)
    for package, ref in zip(packages, refs, strict=True):
        exact[(package["name"], package["version"])].append(ref)
        by_name[package["name"]].append(ref)

    components: list[dict[str, Any]] = []
    dependencies: list[dict[str, Any]] = []
    for package, ref in sorted(zip(packages, refs, strict=True), key=lambda item: item[1]):
        component: dict[str, Any] = {
            "type": "library",
            "bom-ref": ref,
            "name": package["name"],
            "version": package["version"],
            "purl": purl(package["name"], package["version"], package.get("source")),
            "properties": [
                {
                    "name": "cargo:source",
                    "value": package.get("source", "workspace"),
                }
            ],
        }
        checksum = package.get("checksum")
        if checksum:
            component["hashes"] = [{"alg": "SHA-256", "content": checksum}]
        components.append(component)
        targets = []
        for raw in package.get("dependencies", []):
            target = dependency_target(raw, exact, by_name)
            if target and target != ref:
                targets.append(target)
        dependencies.append({"ref": ref, "dependsOn": sorted(set(targets))})

    lock_digest = hashlib.sha256(lock_bytes).hexdigest()
    serial = uuid.uuid5(NAMESPACE, f"sysopt:{version}:{lock_digest}")
    root_ref = f"pkg:cargo/sysopt@{urllib.parse.quote(version, safe='')}"
    workspace_refs = sorted(
        ref for package, ref in zip(packages, refs, strict=True) if not package.get("source")
    )
    dependencies.append({"ref": root_ref, "dependsOn": workspace_refs})
    bom = {
        "$schema": SCHEMA,
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "serialNumber": f"urn:uuid:{serial}",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "bom-ref": root_ref,
                "name": "sysopt",
                "version": version,
                "purl": root_ref,
            },
            "properties": [
                {"name": "sysopt:cargo_lock_sha256", "value": lock_digest},
                {"name": "sysopt:generator", "value": "tools/generate_cyclonedx.py"},
            ],
        },
        "components": components,
        "dependencies": sorted(dependencies, key=lambda item: item["ref"]),
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(bom, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return bom


def validate(bom: dict[str, Any]) -> None:
    if bom.get("bomFormat") != "CycloneDX" or bom.get("specVersion") != "1.6":
        raise ValueError("SBOM no es CycloneDX 1.6")
    refs = [component.get("bom-ref") for component in bom.get("components", [])]
    if not refs or any(not ref for ref in refs) or len(refs) != len(set(refs)):
        raise ValueError("componentes SBOM vacíos o bom-ref duplicados")
    valid_refs = set(refs)
    valid_refs.add(bom["metadata"]["component"]["bom-ref"])
    for relation in bom.get("dependencies", []):
        if relation.get("ref") not in valid_refs:
            raise ValueError(f"dependencia con ref desconocido: {relation.get('ref')}")
        unknown = set(relation.get("dependsOn", [])) - valid_refs
        if unknown:
            raise ValueError(f"dependencias desconocidas para {relation['ref']}: {sorted(unknown)}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--lock", type=Path, default=Path("Cargo.lock"))
    parser.add_argument("--output", type=Path, default=Path("dist/sysopt.cdx.json"))
    parser.add_argument("--version", required=True)
    parser.add_argument("--validate-only", type=Path)
    args = parser.parse_args()
    try:
        if args.validate_only:
            bom = json.loads(args.validate_only.read_text(encoding="utf-8"))
        else:
            bom = generate(args.lock, args.output, args.version)
        validate(bom)
        print(f"CycloneDX 1.6 válido: {len(bom.get('components', []))} componentes")
        return 0
    except (OSError, ValueError, KeyError, tomllib.TOMLDecodeError, json.JSONDecodeError) as error:
        print(f"ERROR SBOM: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
