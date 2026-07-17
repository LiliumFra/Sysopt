# AGENTS.md — contexto de ingeniería para SysOpt

## Propósito

SysOpt es un optimizador local y reversible de procesos, recursos y caché de páginas para Windows, Linux y macOS. Toda modificación debe conservar cuatro invariantes: seguridad de identidad de proceso, recuperación transaccional, decisiones explicables y funcionamiento útil sin IA ni red.

## Arquitectura del workspace

- `crates/telemetry`: snapshots del sistema, identidad observable, CPU/RAM/E/S y Linux PSI.
- `crates/policy`: firmas, clasificación determinista/modelo, acciones y tracker idempotente.
- `crates/intelligence`: aprendizaje local y semántica Model2Vec; solo propone, nunca ejecuta.
- `crates/enforcement`: prioridades, cgroup v2, identidad nativa y journal durable.
- `crates/smart-cache`: precarga acotada en page cache; sin driver ni write-back.
- `crates/app`: CLI, configuración, automatización, runtime, diagnóstico y guardarraíles.

## Reglas no negociables

1. Ninguna acción puede basarse solo en PID; validar también identidad nativa/start time inmediatamente antes de la syscall.
2. Todo cambio reversible debe prepararse en el journal antes de aplicarse y confirmarse después.
3. No sobrescribir cambios externos detectados después de una acción de SysOpt.
4. No seguir symlinks/reparse points en configuración, estado, journals, reportes ni raíces de caché.
5. La IA únicamente aporta señales acotadas. La política determinista y los límites de seguridad tienen la última palabra.
6. La ausencia de modelo, Internet, PSI, privilegios o cgroup debe degradar la función, no derribar el proceso principal.
7. No añadir drivers, write-back cache, inyección, hooks globales, lectura de argumentos, pantalla, teclado, red o contenido de archivos.
8. Toda función nueva debe tener modo dry-run o explicación observable cuando sea aplicable.
9. No afirmar soporte de plataforma sin prueba en el runner nativo correspondiente.
10. Mantener `Cargo.lock`, acciones fijadas por SHA y herramientas de seguridad versionadas.

## Flujo de validación obligatorio

```bash
cargo metadata --locked --no-deps --format-version 1
cargo fmt --all -- --check
cargo build --locked --workspace --all-targets
cargo test --locked --workspace --all-targets
cargo test --locked --workspace --doc
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo audit -D warnings
python3 tools/deep_validate.py --root .
```

Además, validar scripts shell con `bash -n`/ShellCheck, PowerShell con el parser nativo y plists con `plutil`. Una validación estática nunca reemplaza build/tests.

## Criterios para cambios de configuración

- Usar `#[serde(default, deny_unknown_fields)]`.
- Proveer defaults conservadores y validar rangos/NaN/infinito.
- Actualizar `sysopt.example.toml` y las tres plantillas `packaging/*/config.install.toml`.
- Mantener compatibilidad de lectura mediante defaults cuando se añaden campos.
- Documentar impacto, fallback y plataforma.

## Criterios para telemetría y decisiones

- Acotar rankings y materializar rutas solo para candidatos necesarios.
- Preferir señales del kernel soportadas oficialmente; ante error marcar no disponible.
- Aplicar histéresis o ciclos consecutivos para evitar oscilación.
- Exponer motivo de cada bloqueo o recomendación en runtime/análisis.
- No persistir más información de proceso que la necesaria.

## Criterios para SmartCache

- Es page-cache warming, no una caché de bloque ni sustituto de PrimoCache.
- Respetar presupuesto, CPU, RAM, PSI, cooldown, profundidad, extensiones y exclusiones.
- Verificar identidad del archivo abierto, tamaño estable y lectura acotada.
- La presión o una duda de seguridad siempre deben posponer la precarga.

## Entrega

Una RC debe incluir changelog, documentación, checksum y reporte honesto de qué se ejecutó. No reutilizar binarios de una versión anterior bajo un número nuevo. Si no fue posible compilar por falta de dependencias o red, entregar fuente y patch, marcar la limitación y dejar CI como gate obligatorio.
