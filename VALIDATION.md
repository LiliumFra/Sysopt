# Validación — SysOpt 0.7.0-rc1

## Controles locales reproducibles

```bash
cargo fmt --all -- --check
cargo metadata --locked --no-deps --format-version 1
python3 tools/test_installers.py
python3 tools/test_workflow_contracts.py
python3 tools/test_release_evidence.py
python3 tools/test_source_artifacts.py
python3 tools/generate_source_manifest.py --root . --check
python3 tools/deep_validate.py --root .
python3 tools/generate_cyclonedx.py --lock Cargo.lock --version 0.7.0-rc1 --output sysopt.cdx.json
bash tools/native_cgroup_smoke.sh   # requiere SYSOPT_TEST_CGROUP_ROOT delegado
```

## Controles enlazados obligatorios en CI

```bash
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets --verbose
cargo test --locked --workspace --doc --verbose
cargo audit -D warnings
```

Se ejecutan con Rust 1.97.1 en Ubuntu x86-64/ARM64, Windows x86-64/ARM64 y macOS Intel/Apple Silicon.

## Matriz de calificación

`qualification/release-matrix.json` define 41 casos, incluidos builds, paquetes, pruebas, instaladores, hardware, firmas y supply chain. Cada evidencia contiene tag, commit, timestamp, estado y, cuando corresponde, SHA-256 del archivo probatorio. `tools/qualify_release.py validate` rechaza IDs desconocidos, duplicados, evidencia de otro tag/commit, hash incorrecto o caso faltante.

## Límites del host de preparación

El host utilizado para construir el paquete fuente no resuelve `index.crates.io`; por ello no puede descargar las dependencias de `Cargo.lock`. El error se conserva en los logs. No se declara build/test local aprobado. La matriz CI es el gate de ejecución real.

## Contratos del workflow de release

- Cada job de paquete genera siempre un manifiesto SHA-256 y un registro de calificación, incluso en prereleases sin firma.
- La publicación verifica que todos los assets descargados coincidan exactamente con los seis manifiestos nativos antes de generar nuevos archivos.
- Los artefactos de calificación transfieren el árbol completo `qualification-evidence/**`, incluidos los archivos probatorios.
- La publicación crea `qualification-evidence.tar.gz` con matriz, informe, registros, evidencia y manifiesto interno.
- PID reuse y recuperación de journals se ejecutan mediante filtros de test exactos antes de emitir sus registros.
- `tools/test_workflow_contracts.py` impide regresar a artefactos vacíos, evidencia aplanada, evidencia versionada que sobrescriba CI o actions no fijadas por SHA.
- `tools/test_release_evidence.py` comprueba rechazo de symlinks, detección de alteraciones y reproducibilidad byte a byte del bundle.
- `tools/test_source_artifacts.py` valida el manifiesto completo y el archivo fuente reproducible que se publica con cada tag.
