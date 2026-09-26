<div align="center">

<img src="./docs/readme-banner.svg" alt="SysOpt — reversible and verifiable system optimization" width="100%" />

**Windows · Linux · macOS · Rust**

[Capabilities](#capacidades-principales) · [Install](#instalación) · [Safety model](#recuperación-y-propiedad-de-cambios) · [Release integrity](#release-y-cadena-de-suministro) · [Limitations](#límites-deliberados)

</div>

# SysOpt 0.7.0-rc1 — optimización automática, reversible y verificable

> **Estado:** candidato de release `0.7.0-rc1`. El código y la automatización del roadmap están integrados. Una publicación estable queda bloqueada hasta que GitHub Actions complete builds, tests, Clippy, RustSec, instalación real, benchmarks de hardware, firma Authenticode, firma/notarización de macOS, SBOM y attestations.

SysOpt es un optimizador local para Windows, Linux y macOS. Clasifica la carga del equipo, ajusta prioridades y políticas energéticas, administra una precarga segura de archivos mediante la caché del sistema operativo y, opcionalmente, utiliza cgroup v2 dentro de una jerarquía delegada. Todas las acciones reales están rodeadas por identidad de proceso, journal durable, verificación posterior y rollback conservador.

La automatización combina reglas auditables, aprendizaje local y un modelo Model2Vec liviano descargado automáticamente. La señal semántica complementa la clasificación, pero no controla directamente el sistema: el motor determinista conserva la decisión final y los guardarraíles pueden bloquearla.

## Capacidades principales

### Windows

- Prioridad de proceso reversible.
- EcoQoS y HighQoS reales mediante `GetProcessInformation` y `SetProcessInformation` con `ProcessPowerThrottling`.
- Política independiente de resolución de temporizadores: control del sistema, ignorar solicitudes o respetarlas explícitamente.
- HighQoS automático para la aplicación interactiva y EcoQoS solo para categorías seguras de segundo plano.
- Captura del estado original, journal durable, verificación de identidad y restauración tras fallo o reinicio.
- Detección de ahorro de batería mediante `GetSystemPowerStatus`.
- Instalador Inno Setup, actualización silenciosa, desinstalación y firma Authenticode automatizadas en CI.

### Linux

- Prioridades `nice` reversibles con helper de privilegios confinado.
- cgroup v2 opt-in dentro de una raíz delegada explícita.
- Membresía reversible y controles `cpu.weight`, `io.weight` y `memory.high`.
- Journal independiente por grupo, transacciones `prepared`/`applied`, verificación posterior y rollback.
- Respeto por cambios externos: SysOpt no sobrescribe una decisión tomada por otra herramienta.
- PSI de CPU, memoria y E/S; energía desde `power_supply`; temperatura desde zonas térmicas.
- Detección X11 de ventana activa mediante `_NET_ACTIVE_WINDOW`/`_NET_WM_PID` cuando X11 está disponible.
- Paquetes `.deb`, instalación/actualización/desinstalación real y calificación cgroup en runners delegados.

### macOS

- Prioridad reversible del trabajo administrado.
- Detección de Low Power Mode, estado térmico y aplicación en primer plano.
- Trabajo interno de SmartCache con QoS `UTILITY` e I/O policy pública `IOPOL_THROTTLE`, restaurados automáticamente al salir del scope.
- LaunchAgent de usuario endurecido.
- Paquetes `.pkg`, Developer ID, notarización con `notarytool`, stapling y validación automatizados.

### SmartCache

SmartCache no instala un driver ni reemplaza al sistema de archivos. Lee archivos seleccionados para que el sistema operativo los mantenga en su page cache cuando existe memoria y capacidad disponibles.

- Raíces explícitas o aprendidas desde procesos activos.
- Restricción opcional al perfil del usuario.
- Presupuesto adaptativo de bytes.
- Protección por CPU, memoria disponible, PSI, batería y temperatura.
- Rechazo de symlinks/reparse points y rutas sensibles.
- Verificación de descriptor, tamaño y lectura completa.
- Persistencia privada del historial de archivos calientes.
- Suspensión automática cuando el presupuesto de overhead o la presión indican que competiría con la carga principal.

### Seguridad adaptativa

- Disyuntor por fallos consecutivos o ratio de fallos.
- Cooldown con restauración opcional de cambios administrados.
- Línea base PSI aprendida localmente con histéresis.
- Degradación automática en Battery Saver, batería baja o estrés térmico.
- Cinco niveles de presupuesto de overhead: normal, muestreo reducido, SmartCache aplazado, ciclo lento y operación mínima.
- Recuperación gradual para evitar oscilaciones.

### Observabilidad

- Estado humano: `sysopt --status`.
- Estado estructurado: `sysopt --status-json`.
- Análisis sin cambios: `sysopt --analyze`.
- JSON limpio para integraciones: `sysopt --analyze-json`.
- Exportación privada y atómica: `sysopt --export-report informe.json`.
- Autoprueba: `sysopt --self-test`.
- OpenMetrics opcional limitado obligatoriamente a loopback, por defecto `127.0.0.1:9898`.
- Benchmark reproducible: `sysopt-bench` mide clasificación con 100, 500 y 2.000 procesos y un dataset SmartCache.

## Instalación

Los instaladores publicados admiten dos modos:

1. **Online verificado:** resuelven una release exacta, descargan assets con timeout, verifican SHA-256 y pueden exigir firma/attestation.
2. **Offline verificado:** consumen un directorio local con assets, checksums, bundles Sigstore y raíz de confianza; rechazan enlaces indirectos y no degradan silenciosamente la política solicitada.

Ejemplos una vez publicados los assets:

```bash
bash sysopt-install-unix.sh --require-attestation --require-code-signature
```

```powershell
.\sysopt-install-windows.ps1 -RequireAttestation -RequireCodeSignature
```

Instalación offline:

```bash
bash sysopt-install-unix.sh --offline-asset-dir ./release-assets --require-attestation
```

```powershell
.\sysopt-install-windows.ps1 -OfflineAssetDir .\release-assets -RequireAttestation -RequireCodeSignature
```

Los scripts de la fuente también soportan fallback reproducible a compilación únicamente cuando existe `Cargo.lock`; siempre usan `--locked`.

## Primer uso

SysOpt permanece en dry-run por defecto:

```bash
sysopt --doctor
sysopt --analyze
sysopt --self-test
sysopt --once
```

Para aplicar cambios reales:

```bash
sysopt --apply
```

Perfiles automáticos:

```bash
sysopt --apply --profile smart
sysopt --apply --profile gaming
sysopt --apply --profile development
sysopt --apply --profile quiet
```

Control de la instancia activa:

```bash
sysopt --status
sysopt --pause
sysopt --set-profile performance
sysopt --resume
sysopt --shutdown
```

## Configuración

Genera una plantilla completa:

```bash
sysopt --init-config ./config.toml
```

Las opciones CLI prevalecen sobre TOML. Las secciones principales son:

- `classifier`: reglas, modelo o modo híbrido.
- `automation`: perfiles, histéresis e intervalos.
- `intelligence`: aprendizaje local y modelo semántico automático.
- `cache`: SmartCache y límites de presión.
- `safety`: disyuntor, PSI, batería y temperatura.
- `overhead`: presupuesto p95 y degradación progresiva.
- `metrics`: OpenMetrics local.
- `resources`: cgroup v2 y pesos/límites por grupo.
- `runtime`: estado, comandos, locks y journals.

Para cgroup v2, `resources.enabled = true` solo debe utilizarse con una subjerarquía realmente delegada. SysOpt valida controladores, archivos regulares y rutas antes de escribir. No intenta tomar control de la jerarquía global del host.

## Recuperación y propiedad de cambios

Antes de modificar un recurso, SysOpt:

1. abre el proceso con los permisos mínimos necesarios;
2. obtiene una identidad nativa resistente a reutilización de PID;
3. captura el valor original;
4. persiste una transacción `prepared` y sincroniza el journal;
5. relee identidad y recurso;
6. aplica el valor;
7. verifica el valor observado;
8. confirma `applied` de forma durable.

Al restaurar, solo actúa si la identidad todavía coincide y el recurso conserva el valor que SysOpt aplicó o el valor anterior de una transición pendiente. Si otra herramienta lo cambió, elimina su reclamación y no sobrescribe el cambio externo.

## IA automática

La IA semántica:

- selecciona automáticamente POTION 2M, 4M u 8M según RAM;
- usa revisiones inmutables fijadas por hash;
- descarga en un worker supervisado;
- aplica timeout, kill y stderr acotado;
- valida el artefacto antes de activarlo;
- reintenta con backoff;
- cae a reglas y aprendizaje local si no existe red o modelo;
- se suspende automáticamente cuando el presupuesto de overhead lo requiere.

No se necesita intervención para instalar el modelo. `sysopt --install-ai-model` solo permite precargarlo manualmente cuando se desea preparar un equipo offline.

## Release y cadena de suministro

El tag esperado es `v0.7.0-rc1`. El workflow usa Rust 1.97.1 y seis runners nativos:

- Ubuntu x86-64 y ARM64.
- Windows x86-64 y ARM64.
- macOS Apple Silicon e Intel.

Cada release ejecuta:

- metadata/tag gate;
- formato;
- Clippy con warnings denegados;
- tests de workspace, targets y doctests;
- pruebas nativas EcoQoS y QoS macOS;
- tests de recuperación y contrato de instaladores;
- RustSec fijado;
- benchmarks 100/500/2.000;
- build e instalación real de paquetes;
- SBOM CycloneDX 1.6 determinista desde `Cargo.lock`;
- attestations OIDC/Sigstore y checksums completos.

Una release **estable** además requiere:

- runners físicos etiquetados SATA 4 GiB, SATA 8 GiB y NVMe 16 GiB;
- cgroup v2 delegado real;
- Authenticode SHA-256 con timestamp RFC3161 para Windows;
- Developer ID, notarización y stapling para macOS;
- evidencia ligada al tag, commit y SHA-256 exactos.

Si falta una credencial, runner o evidencia, el job de publicación no puede promover el tag estable. No existe fallback inseguro.

## Validación local

```bash
cargo fmt --all -- --check
cargo metadata --locked --no-deps --format-version 1
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
cargo test --locked --workspace --doc
python3 tools/test_installers.py
python3 tools/deep_validate.py --root .
python3 tools/generate_cyclonedx.py --lock Cargo.lock --version 0.7.0-rc1 --output sysopt.cdx.json
```

El árbol incluye `RELEASE-MANIFEST.sha256` para verificar la integridad de la fuente empaquetada.

## Límites deliberados

- SysOpt no es un antivirus ni un sandbox.
- No modifica procesos protegidos cuando el sistema operativo deniega acceso.
- En Wayland no afirma soporte universal de ventana activa: la integración depende del compositor y se degrada de forma segura.
- macOS no ofrece una API pública equivalente para imponer QoS arbitrario a procesos de terceros; SysOpt aplica QoS reversible a su propio trabajo auxiliar.
- SmartCache depende de la política de caché del sistema operativo y no garantiza que una página permanezca residente.
- Las firmas y notarización requieren credenciales del propietario del proyecto; el código las automatiza, pero no las incorpora en la fuente.

Consulta `DESIGN.md`, `SECURITY.md`, `docs/RECOVERY.md`, `docs/PRESSURE-SAFETY.md`, `docs/PLATFORM-NOTES.md` y `VALIDATION.md` para los contratos completos.
