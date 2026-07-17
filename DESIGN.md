# Diseño técnico — SysOpt 0.7.0-rc1

## Principios

1. **Automático, pero no opaco:** la clasificación puede aprender, mientras las acciones finales son reglas explícitas y exportables.
2. **Reversible:** todo cambio real conserva identidad, estado original y evidencia durable.
3. **Conservador ante ambigüedad:** si un proceso cambia de identidad o un tercero modifica el recurso, SysOpt no sobrescribe ese estado.
4. **Degradación segura:** ausencia de red, modelo, PSI, privilegios, cgroup, X11 o métricas no derriba el daemon.
5. **Coste acotado:** el propio optimizador se degrada si excede su presupuesto p95.
6. **Release verificable:** código, paquete, firma, SBOM, attestation y evidencia corresponden al mismo tag y commit.

## Flujo de decisión

`Telemetry -> Classifier -> Intelligence -> Policy -> Safety -> Tracker -> Enforcer`

- **Telemetry** construye una instantánea de procesos, presión, energía, temperatura y foreground.
- **Classifier** identifica el modo del equipo con reglas/modelo/híbrido.
- **Intelligence** aporta historial local y señal semántica acotada.
- **Policy** genera acciones abstractas por proceso y grupo.
- **Safety** puede limitar o cancelar acciones.
- **Tracker** elimina no-ops, impone residencia mínima y conserva propiedad lógica.
- **Enforcer** verifica identidad, journaliza, aplica, confirma y restaura.

SmartCache recibe la misma instantánea y guardarraíles, pero su efecto es lectura de archivos, no control de procesos.

## Identidad de proceso

El PID no es suficiente. Cada backend usa una identidad nativa:

- Windows: tiempo de creación completo obtenido del handle.
- Linux: start ticks de `/proc`, PIDFD cuando está disponible y UID.
- macOS: `proc_bsdinfo` con timestamp de inicio.

Las acciones incluyen PID y start time; el enforcer vuelve a leer la identidad antes y después de preparar la transacción.

## Transacciones

El contrato general es:

1. capturar valor actual;
2. persistir `prepared` y sincronizar archivo/directorio;
3. releer identidad y valor;
4. aplicar;
5. verificar lectura posterior;
6. persistir `applied`;
7. registrar propiedad en memoria.

Una actualización conserva `previous_applied` para cerrar la ventana entre el nuevo journal y la syscall. La recuperación acepta el valor aplicado actual o el aplicado anterior de una transición preparada.

## EcoQoS Windows

`SetProcessPowerPolicy` modela dos ejes independientes:

- `ProcessPowerPolicy`: `system_managed`, `eco`, `high`.
- `TimerResolutionPolicy`: `system_managed`, `ignore`, `respect`.

El backend conserva bits no administrados de `PROCESS_POWER_THROTTLING_STATE`. Eco activa `EXECUTION_SPEED`; High controla ese bit y lo deja desactivado; la resolución de temporizador administra `IGNORE_TIMER_RESOLUTION` por separado. El journal almacena ambas máscaras completas.

## cgroup v2

SysOpt opera únicamente dentro de `resources.linux_cgroup_root`, que debe estar delegada. Valida:

- ruta absoluta sin traversal;
- directorios reales, no symlinks;
- presencia de cgroup v2;
- controladores delegados;
- archivos de control regulares;
- pertenencia del grupo aplicado a la raíz administrada.

Los controles por grupo son una unidad lógica:

- `cpu.weight`: 1..10000.
- `io.weight`: representación canónica `default N`.
- `memory.high`: mínimo 16 MiB.

El journal de grupo es independiente del journal de procesos. Cuando el último proceso sale del grupo, los valores originales se restauran. Si el valor observado no coincide con el aplicado por SysOpt, se abandona la restauración.

## Seguridad adaptativa y overhead

El disyuntor observa fallos y presión. La línea base PSI evita depender exclusivamente de umbrales globales. Batería y temperatura reducen trabajo secundario.

El controlador de overhead calcula p50/p95 sobre una ventana persistente:

- nivel 0: operación completa;
- nivel 1: reduce procesos evaluados;
- nivel 2: aplaza SmartCache y alarga intervalo;
- nivel 3: desactiva semántica temporalmente;
- nivel 4: operación mínima.

La recuperación baja un nivel después de suficientes ciclos sanos.

## OpenMetrics

El servidor usa un listener no bloqueante en loopback. Rechaza cualquier bind público durante la validación de configuración. Mantiene una copia compartida del estado y se detiene/join al destruirse.

## macOS QoS

El guard `WorkerQosGuard` captura QoS e I/O policy del hilo actual, aplica `QOS_CLASS_UTILITY` e `IOPOL_THROTTLE`, y restaura ambos en `Drop`. Solo se usa para trabajo interno de SysOpt; no depende de APIs privadas para alterar QoS de terceros.

## Supply chain

La release se construye por arquitectura nativa. `Cargo.lock` es obligatorio. El SBOM se deriva de ese lockfile y la evidencia registra:

- ID de caso;
- estado;
- tag;
- commit;
- timestamp con zona;
- plataforma;
- SHA-256 de evidencia.

El job de publicación valida la matriz antes de crear la release. Para estable, hardware y firmas son requisitos, no advertencias.
