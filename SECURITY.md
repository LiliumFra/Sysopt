# Seguridad — SysOpt 0.7.0-rc1

## Modelo de seguridad

SysOpt ejecuta optimizaciones locales con los privilegios del usuario y, en Linux, puede utilizar un helper CAP_SYS_NICE restringido. No es un antivirus, sandbox ni mecanismo de aislamiento frente a un administrador/root comprometido.

El objetivo de seguridad es impedir que una clasificación errónea, un cierre abrupto, la reutilización de PID, un enlace indirecto o una release incompleta produzcan cambios persistentes o no atribuibles.

## Propiedad y restauración

- PID más identidad nativa; nunca PID solo.
- Journal privado con lock exclusivo.
- Estados `prepared` y `applied`.
- Valor original, aplicado y aplicado anterior.
- Relectura antes de modificar y verificación después.
- Restauración solo si identidad y valor todavía coinciden.
- Cambios externos tienen precedencia.
- Una recuperación incompleta conserva el journal y devuelve error.

## Archivos y rutas

- Directorios de estado privados.
- Rechazo de symlinks/reparse points para journals, configuración sensible, assets offline y rutas cgroup.
- Reemplazo atómico y `fsync`/write-through donde la plataforma lo permite.
- Límites de tamaño y versión de esquema.
- SmartCache no sigue enlaces y rechaza rutas del sistema sensibles.

## EcoQoS

EcoQoS no se emula mediante una clase de prioridad. El backend usa Power Throttling nativo y conserva las máscaras completas. La política de temporizador es independiente y el valor original se recupera mediante journal.

## cgroup v2

Los controles solo se habilitan dentro de una raíz delegada explícita. SysOpt no toma posesión de la jerarquía global. Valida controladores y archivos antes de escribir. `cpu.weight`, `io.weight` y `memory.high` se journalizan como transacción de grupo.

## IA y red

- Revisiones Hugging Face fijadas por hash.
- Descarga automática supervisada con timeout y backoff.
- Carga aislada frente a panic.
- Sin modelo o sin red, se mantiene el motor determinista.
- La IA no ejecuta comandos ni escribe prioridades/cgroups directamente.
- El exportador OpenMetrics solo acepta loopback.

## Instaladores y releases

- SHA-256 por asset.
- Modo offline sin enlaces indirectos.
- Política `require-attestation` y `require-code-signature` sin fallback silencioso.
- Fallback a fuente únicamente con `Cargo.lock` y `--locked`.
- Authenticode para Windows estable.
- Developer ID, notarización y stapling para macOS estable.
- SBOM CycloneDX y attestations OIDC/Sigstore.
- Evidencia obligatoria por tag/commit/hash.

## Reporte de vulnerabilidades

No publiques datos sensibles, certificados ni claves en un issue público. Incluye versión, plataforma, configuración mínima, impacto, pasos reproducibles y si la prueba requiere privilegios.

## Limitaciones

Un proceso con los mismos privilegios puede modificar archivos que ese usuario controla si consigue eludir las protecciones del sistema operativo. Los checksums alojados junto al artefacto ayudan a detectar corrupción, pero la autenticidad fuerte depende de firma y attestation verificadas contra una raíz de confianza.
