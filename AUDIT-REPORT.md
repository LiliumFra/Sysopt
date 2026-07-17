# Informe de auditoría — SysOpt 0.7.0-rc1

## Resultado

El roadmap funcional y de automatización se encuentra implementado en el árbol fuente. Se corrigieron durante el cierre:

- EcoQoS previamente ausente o confundido con prioridad baja.
- Persistencia de máscaras Power Throttling y temporizadores.
- Journal cgroup con una iteración duplicada que impediría resolver correctamente una actualización.
- Formato de escritura de `io.weight`, que ahora usa `default N`.
- Recuperación de controles eliminados de la configuración.
- Fuga de display X11 en una ruta temprana.
- ABI Objective-C para `BOOL` y `NSInteger`.
- Política macOS de I/O cambiada a `IOPOL_THROTTLE` para trabajo de utilidad.
- Benchmark con campo estructural duplicado.
- Dependencia de un job de hardware omitido en prereleases.
- Validador profundo obsoleto que todavía exigía cgroup membership-only.
- Higiene del validador, que no debe tratar `.git` del checkout CI como contenido del artefacto.
- Prereleases Windows ARM64 y macOS x86-64 sin JSON de calificación cuando la firma era opcional.
- Publicación incompleta que aplanaba solo los registros JSON y omitía sus archivos probatorios.
- Evidencias de recuperación emitidas después de un test agregado, sin filtros exactos por caso.
- Documentación y validador desalineados con la política pública `IOPOL_THROTTLE`.

## Cobertura incorporada

- EcoQoS/HighQoS/temporizadores con roundtrip nativo.
- cgroup `cpu.weight`, `io.weight`, `memory.high` con journal y pruebas transaccionales.
- PSI, energía, temperatura y foreground multiplataforma.
- Disyuntor, línea base adaptativa y presupuesto de overhead.
- OpenMetrics loopback-only.
- QoS macOS reversible.
- Benchmarks y calificación de hardware.
- Instaladores online/offline verificables.
- Authenticode, Developer ID/notarización, SBOM y attestations.

## Riesgo residual

No fue posible enlazar localmente por ausencia de dependencias y resolución DNS de crates.io. El código no se marca como binario validado localmente. El workflow bloquea publicación ante cualquier fallo de build, test, paquete, firma, hardware o evidencia. La evidencia se distribuye como un bundle autocontenido con manifiesto interno.

## Correcciones del workflow de release

- Los jobs de empaquetado generan siempre evidencia no vacía, aun cuando una prerelease no tenga firma.
- Python 3.13 se instala explícitamente en todos los jobs del workflow que ejecutan herramientas Python.
- La evidencia producida por CI no puede ser sobrescrita por archivos versionados dentro del repositorio.
- `verify_package_manifests.py` compara cada asset publicado con el manifiesto del runner nativo y bloquea omisiones, duplicados, symlinks y hashes distintos.
- El bundle de calificación es autocontenido, determinista y contiene un manifiesto interno.
- Los contratos dinámicos alteran archivos deliberadamente para confirmar que la validación falla de forma segura.
- Los archivos `ARTIFACTS.sha256` y `SHA256SUMS.txt` usan nombres relativos al directorio publicado; la verificación se ejecuta antes de crear la release.
- El archivo fuente se reconstruye desde `RELEASE-MANIFEST.sha256`, con metadatos normalizados y reproducción byte a byte.
- Las attestations reciben el manifiesto de checksums como sujeto y sus bundles Sigstore se incorporan a la release.
