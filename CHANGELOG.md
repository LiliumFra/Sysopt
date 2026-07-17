# Changelog

## 0.7.0-rc1

- Implementación nativa completa de Windows EcoQoS/HighQoS y política independiente de temporizadores, con journal durable y recuperación.
- Política automática: foreground sensible a latencia en HighQoS y categorías seguras de segundo plano en EcoQoS.
- Controles cgroup v2 `cpu.weight`, `io.weight` y `memory.high` con journal por grupo, verificación, rollback y respeto por cambios externos.
- Telemetría de batería, temperatura, Low Power Mode, PSI y aplicación activa.
- Disyuntor ampliado y línea base PSI adaptativa.
- Presupuesto persistente de overhead con cinco niveles de degradación.
- Exportador OpenMetrics limitado a loopback.
- QoS utility e I/O throttled reversibles para trabajo interno en macOS.
- Benchmark reproducible de 100/500/2.000 procesos y datasets SmartCache.
- Instaladores online/offline con timeout, SHA-256, firma y attestation exigibles.
- Release nativa en seis plataformas, smoke tests de instalación, hardware obligatorio para estable, Authenticode, Developer ID/notarización, SBOM CycloneDX y attestations.
- Matriz de 41 evidencias vinculadas a tag, commit y hash; la release estable se bloquea si falta una sola.
- Correcciones de `io.weight`, transacción cgroup, cierre X11, ABI Objective-C, benchmark y dependencia de jobs omitidos.

## 0.6.5-rc1

### Funciones nuevas

- Nuevo `--analyze`: observa el equipo durante un segundo, clasifica la carga, explica el perfil recomendado, lista acciones deterministas y evalúa SmartCache sin modificar el sistema.
- Nuevo `--analyze-json` para integración con herramientas externas y `--export-report <ruta>` para informes privados, atómicos y bloqueados contra escritores concurrentes; la salida JSON queda libre de mensajes operativos en stdout.
- Nuevo `--self-test`: valida configuración, firmas, clasificador, telemetría, política y construcción de SmartCache con una muestra funcional en vivo.
- Telemetría Linux PSI (`/proc/pressure/cpu`, `memory` e `io`) incorporada al snapshot y al estado runtime.
- SmartCache se detiene automáticamente ante presión PSI sostenida de memoria o E/S, además de sus límites existentes de CPU y RAM disponible.
- Disyuntor de enforcement configurable: abre después de fallos repetidos, bloquea cambios durante un enfriamiento y puede restaurar inmediatamente las modificaciones administradas.
- Guardarraíl de presión para no introducir nuevas prioridades, cgroups o precarga cuando el kernel informa contención severa.
- El estado JSON expone presión, circuito, ciclos fallidos, enfriamiento y motivo, facilitando observabilidad y soporte.

### Ingeniería y validación

- Toolchain y MSRV de desarrollo actualizados a Rust/Cargo 1.97.1.
- `.cargo/config.toml` usa `build.warnings = "deny"`; cualquier warning local bloquea la compilación, no solo Clippy.
- El perfil release conserva comprobaciones de overflow.
- CI y el gate de release añaden doctests; la validación profunda usa explícitamente Rust 1.97.1 y vuelve a ejecutarse antes de publicar.
- Plantillas de instalación y configuración incluyen los nuevos límites de presión y seguridad.
- Pruebas de regresión para PSI, circuito, recomendaciones y exclusividad de comandos de diagnóstico.
- Nuevos `DESIGN.md` y `AGENTS.md` documentan arquitectura, invariantes, extensión segura y reglas para agentes de mantenimiento.

### Estado

- Candidato de fuente `0.6.5-rc1`. La publicación de binarios sigue condicionada a build, Clippy, tests, RustSec y smoke tests nativos en la matriz de seis plataformas.

## 0.6.4-rc4

### Correcciones de la revisión final

- Se corrigió una expresión `Ok(true)` duplicada que impedía compilar la recuperación cgroup.
- El backend cgroup v2 pasa a modo conservador de **membresía solamente**: ya no modifica `cgroup.subtree_control`, `cpu.weight`, `io.weight` ni `memory.high` hasta disponer de journaling durable por grupo y referencias compartidas.
- La política emite `ResourceLimits::default()` para todas las asignaciones cgroup; el enforcer rechaza defensivamente cualquier control compartido no vacío.
- El heartbeat elimina de forma atómica entradas de journal correspondientes a procesos terminados o PIDs reciclados, evitando acumulación indefinida durante sesiones largas.
- El journal rechaza una segunda transacción pendiente para la misma identidad y backend, evitando entradas ambiguas tras fallos parciales.
- El cierre ejecuta una reconciliación durable final independiente del tracker para restaurar acciones aplicadas pero no confirmadas en memoria.
- SmartCache rechaza reparse points de Windows además de symlinks, incluidas raíces enlazadas, valida nuevamente el descriptor abierto y exige propietario/permisos privados para su archivo y directorio de estado en Unix.
- Se eliminó `tools/__pycache__` del artefacto fuente y se añadieron controles explícitos contra cachés o bytecode empaquetados.
- Se añadieron pruebas de regresión para límites cgroup deshabilitados, eliminación múltiple del journal, enlaces indirectos de SmartCache y permisos amplios del estado.

### Estado

- Continúan como bloqueadores la ausencia de `Cargo.lock` y la imposibilidad de ejecutar Cargo, rustfmt, Clippy, tests, RustSec y smoke tests nativos en este entorno.
- Sigue siendo un candidato de código fuente; no debe etiquetarse como `v0.6.4` estable.


## 0.6.4-rc2

### Correcciones de auditoría

- Los límites compartidos de cgroup (`cpu.weight`, `io.weight` y `memory.high`) se aplican ahora como una unidad reversible: se capturan los valores previos y se revierte cualquier escritura parcial o asignación abortada.
- La detección de una migración cgroup externa ya no puede omitir el rollback de límites aunque falle la limpieza durable del journal.
- Una restauración incompleta durante el cierre convierte la sesión en fallo, conserva el journal y evita marcar el estado de salud como cierre limpio.
- La recuperación cgroup valida backend e identidad de plataforma antes de interpretar rutas o recursos incompatibles.
- `ResourceLimits` implementa `Default`, corrigiendo un error real de compilación en los tests del tracker.
- Se añadieron pruebas de regresión para valores de recursos por defecto y aplicación/rollback de controles cgroup.
- El validador estático incorpora controles específicos para rollback cgroup, cierre recuperable y coherencia de `ResourceLimits::default()`.

### Estado

- Continúan como bloqueadores `Cargo.lock` y la ejecución real de formato, Clippy, build, tests, RustSec y smoke tests nativos.
- Este archivo sigue siendo un candidato de código fuente; no debe etiquetarse como `v0.6.4` estable.


## 0.6.4-rc1

### Fiabilidad y recuperación

- Journal transaccional de enforcement con lease, escritura atómica, esquema validado y recuperación al iniciar.
- Lock exclusivo dedicado al journal, independiente del runtime lock, para impedir dos escritores concurrentes.
- En Unix, journal, temporal y lock requieren propietario efectivo y permisos privados; el directorio no puede ser escribible por grupo/otros.
- Restauración segura de prioridades y cgroups después de cierres abruptos, únicamente cuando identidad y valor aplicado todavía coinciden.
- Transiciones de prioridad/cgroup conservan el valor aplicado anterior y el nuevo para cerrar la ventana de crash entre journal y syscall.
- Linux incorpora identidad exacta por `boot_id` + ticks de inicio y `pidfd_open()` con fallback conservador.
- La recuperación Linux descarta primero entradas de un arranque anterior antes de inicializar una jerarquía cgroup que pudo desaparecer.
- Windows conserva el `FILETIME` completo de creación y reemplaza por `windows-sys` las declaraciones Win32 manuales de enforcement, foreground y persistencia atómica.
- macOS usa `proc_pidinfo` con segundos y microsegundos de inicio.
- El modo seguro y dry-run ejecutan recuperación pendiente antes de abstenerse de aplicar cambios nuevos.
- Se evita normalizar procesos o reclamar cgroups que SysOpt no modificó.
- Doble lectura inmediatamente anterior a cada restauración; procesos que terminan durante recovery se descartan sin bloquear el arranque.
- Rollback de confirmaciones fallidas: las acciones nuevas limpian su entrada y las transiciones conservan el valor anterior recuperable.

### Estado

- Continúan como bloqueadores `Cargo.lock` y la ejecución de formato, Clippy, build, tests, RustSec y smoke tests de plataforma.


## 0.6.3-rc1

### Arquitectura multiplataforma

- Separación de configuración, datos, estado, caché y runtime según Windows, XDG y macOS.
- Persistencia atómica con reemplazo write-through en Windows y sincronización del directorio padre en Unix.
- LaunchAgent macOS unificado, reinicio solo tras fallo y prioridad de fondo.
- Task Scheduler con `IgnoreNew`, reintentos, prioridad de fondo y verificación de identidad por hora de creación.
- cgroup v2 confinado a la subjerarquía propia, sin modificar el padre, con validación inmediata de `subtree_control` y sin reactivar controladores ya habilitados.
- Ciclo de vida corregido: launchctl moderno en macOS, usuario de instalación persistido para prerm en Linux y actualización transaccional/autoinicio reconciliado en Windows.

### Rendimiento y control

- Telemetría en dos fases y deduplicación CPU/E/S para reducir llamadas al sistema.
- Tracker idempotente con residencia mínima entre grupos y eliminación de acciones repetidas.
- SmartCache evita árboles generados, raíces sensibles y formatos comprimidos de bajo beneficio; verifica descriptor, tamaño y lectura completa antes de registrar éxito.
- Se corrigió una inicialización duplicada de `background_io_weight` que era un error de compilación real.

### IA totalmente desatendida

- Descarga en subproceso con timeout configurable, terminación, manejo de la carrera de salida al vencer el límite y reintento automático.
- Captura acotada de stderr para impedir bloqueo por tubería llena.
- La instalación no espera a Hugging Face y el motor determinista permanece operativo.

### Entrega y cadena de suministro

- Matriz nativa x86_64/ARM64 para Linux y Windows, más Intel/Apple Silicon en macOS.
- Runners de GitHub fijados a versiones concretas de SO.
- `cargo-audit` 0.22.2 fijado y ejecutado con advertencias como error.
- Auxiliares de control/desinstalación verificados desde la misma release exacta.
- Instaladores Unix/Windows publicados como assets verificados con el repositorio de la release fijado; eliminadas recomendaciones de ejecutar ramas mutables.
- Validación de scripts y plists repetida dentro del workflow de release, no solo en CI.
- Dependabot semanal para Cargo y Actions.

### Estado

- `Cargo.lock` y la ejecución real del toolchain siguen siendo bloqueadores; este paquete no es una release estable.

## 0.6.2-rc1

### Automatización total de la IA

- Carga inmediata desde caché sin acceso a red.
- Descarga automática en un trabajador independiente para no bloquear el ciclo de optimización.
- Reintentos exponenciales configurables, de 30 segundos hasta 1 hora por defecto.
- Selección automática 2M/4M/8M y observabilidad de descarga/reintento en el estado.
- Descargas concurrentes protegidas con bloqueo global y caché atómica de `hf-hub`.
- Revisiones inmutables fijadas para los tres modelos POTION integrados.
- Validación de JSON, cabecera SafeTensors e inferencia real antes de marcar el modelo como listo.

### Auditoría y endurecimiento

- La descarga del modelo fue retirada por completo de la ruta síncrona de instalación.
- Se verifica que la instantánea de Hugging Face corresponda al commit solicitado.
- Las llamadas a Model2Vec se aíslan para convertir panics desenrollables en fallback seguro.
- El estado de enforcement solo se confirma después del éxito real del sistema operativo.
- Windows Job Objects quedan deshabilitados por no ser reversibles para procesos vivos.
- Escrituras de configuración, runtime y aprendizaje usan bloqueo, temporales exclusivos, sincronización y límites.
- Se rechazan enlaces simbólicos/reparse points en rutas sensibles de configuración.
- El instalador Unix calcula el hash del binario exacto y no confía en el nombre del archivo incluido en el checksum.
- Windows separa binario y datos, y el desinstalador rechaza prefijos raíz o de perfil.
- Se corrigió la generación/verificación de checksums del workflow de release.
- Las GitHub Actions externas están fijadas a SHA completos y los permisos son mínimos.
- `Cargo.lock` y la matriz ejecutable siguen siendo bloqueadores explícitos de publicación.

- Los instaladores descartan binarios prebuilt sin checksum válido y recurren al código fuente.
- Windows instala el arranque automático en contexto de usuario, sin elevación UAC ni tarea `Highest`.
- Servicios Linux/macOS y tarea Windows reciben la ruta explícita de configuración.
- `clippy` y `rustfmt` pasan a ser controles obligatorios en CI.
- La publicación de instaladores queda bloqueada por formato, Clippy y tests.
- El fallback de compilación clona el tag exacto de la release, nunca `main`.
- Informe de auditoría, alcance de validación y riesgos residuales documentados.

## 0.6.0

### IA híbrida ultra liviana

- Clasificador semántico Model2Vec/POTION integrado de forma nativa en Rust.
- Selección automática `potion-base-2M`, `4M` u `8M` según la RAM total.
- Descarga y prueba de inferencia con `sysopt --install-ai-model`.
- Instaladores Windows, Linux y macOS descargan el modelo sin bloquear la instalación si no hay red.
- La semántica solo complementa procesos desconocidos y tiene peso limitado; reglas, procesos protegidos y límites duros siguen siendo autoritativos.
- Fallback automático a firmas y aprendizaje online ante cualquier error del modelo.

### SmartCache y observabilidad

- Procesos reconocidos semánticamente pueden aportar raíces candidatas a SmartCache.
- Estado del servicio incluye modelo cargado, aciertos semánticos y error de fallback.
- Nuevo documento de selección de modelo y configuración avanzada.

## 0.5.0

### Inteligencia adaptativa

- Nuevo crate `intelligence` con aprendizaje online local y estado persistente.
- Puntuación por proceso basada en CPU, E/S, memoria, rango, categorías, contexto y persistencia.
- Unión de líderes por CPU y por disco, con deduplicación PID/tiempo de inicio.
- Detección nativa de la aplicación en primer plano en Windows; nunca se degrada y aumenta la precisión de la prioridad.
- Límite de una elevación por familia de ejecutable para evitar potenciar procesos auxiliares duplicados.
- Calibración automática del número máximo de boosts según los hilos lógicos del equipo.
- Decisiones explicables con score, confianza y motivo.
- Límites duros: prioridad High desactivada por defecto, máximo de procesos potenciados/reducidos y exclusión de procesos críticos.
- Persistencia atómica del aprendizaje y límites de tamaño del estado.

### SmartCache inteligente

- Puntuación aprendida por raíz activa.
- Preferencias de extensión por modo.
- Presupuesto e intervalo dinámicos enviados por la IA, reducidos automáticamente durante la fase inicial de aprendizaje.
- Precarga breve y conservadora al iniciar juegos; pausa automática cuando sube la carga, durante contenedores o cargas pesadas.
- Nuevas extensiones para creación, juegos y multimedia.

### Modos

- Perfil predeterminado `smart`.
- Perfiles nuevos: `gaming`, `development`, `creator`, `streaming` y `quiet`.
- Modos detectados nuevos: `gaming`, `creative` y `streaming`.
- Firmas ampliadas para juegos, launchers, creación, codificación, streaming, comunicación, indexadores y actualizadores.

### Experiencia de usuario

- Centros de control de Windows, Linux y macOS ampliados con todos los modos.
- Estado visible de mini‑IA, confianza, procesos priorizados y memoria aprendida.
- Instaladores y servicios usan el modo Inteligente de forma predeterminada.
- Nueva documentación de arquitectura, privacidad y seguridad.

## 0.4.0

- Instaladores nativos, Centro de control, comandos en caliente, instancia única y recuperación automática.

## 0.3.0

- SmartCache de lectura sobre la caché de páginas y automatización adaptativa.

## 0.2.0

- Configuración TOML, clasificador híbrido, aprendizaje etiquetado y grupos de recursos.
