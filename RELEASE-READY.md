# SysOpt 0.7.0-rc1 — preparación de release

## Estado del árbol

- Roadmap de código: completo.
- Automatización de release: completa.
- Versión: `0.7.0-rc1` en workspace, lockfile e instaladores.
- Artefacto fuente: incluye manifiesto SHA-256.
- Build local enlazado: no ejecutable en el host de preparación porque DNS no resuelve `index.crates.io` y no existe caché de dependencias.

Esa limitación local no se transforma en una aprobación ficticia. El workflow exige en cada tag:

- Rust 1.97.1;
- seis builds nativos;
- formato, Clippy, tests, doctests y RustSec;
- pruebas EcoQoS/QoS/cgroup;
- benchmarks;
- instalación, actualización y desinstalación reales;
- SBOM y attestations.

## Promoción a estable

Un tag sin guion se trata como estable y además necesita:

- SATA 4 GiB;
- SATA 8 GiB;
- NVMe 16 GiB;
- cgroup v2 delegado real;
- certificado Authenticode y timestamp RFC3161;
- credenciales Developer ID/Apple notarization;
- todos los 41 casos con evidencia válida.

La ausencia de cualquiera de esos recursos bloquea `publish-release`.

## Tag previsto

`v0.7.0-rc1`

No reutilizar ni renombrar binarios de versiones anteriores. Los binarios deben provenir del workflow del mismo tag y commit.
