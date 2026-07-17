# Roadmap — SysOpt 0.7.0-rc1

## Estado

El roadmap de implementación está cerrado para `0.7.0-rc1`. Las actividades que dependen de hardware o credenciales externas no se marcan como ejecutadas localmente: están convertidas en gates automáticos obligatorios de release.

## Núcleo y recuperación

- [x] Identidad nativa resistente a reutilización de PID en Windows, Linux y macOS.
- [x] Journals privados con lock exclusivo, esquema, tamaño máximo y reemplazo atómico.
- [x] Transacciones `prepared`/`applied` con valor original, aplicado y aplicado anterior.
- [x] Rollback después de error, interrupción o reinicio.
- [x] Detección y respeto por cambios externos.
- [x] Cierre limpio que no declara éxito mientras queden recursos pendientes.
- [x] Disyuntor por fallos repetidos y modo seguro tras crashes.

## Windows

- [x] Prioridades reversibles.
- [x] EcoQoS/HighQoS nativo mediante Process Power Throttling.
- [x] Política independiente de temporizadores.
- [x] Aplicación automática HighQoS foreground/EcoQoS background seguro.
- [x] Journal y recuperación de Power Throttling.
- [x] Roundtrip nativo x86-64 y ARM64 en CI.
- [x] Batería y Battery Saver.
- [x] Authenticode y timestamp RFC3161 automatizados.
- [x] Instalación, actualización y desinstalación silenciosas en CI.

## Linux

- [x] Prioridad y helper CAP_SYS_NICE confinado.
- [x] cgroup v2 dentro de raíz delegada.
- [x] Membresía reversible.
- [x] `cpu.weight`, `io.weight` y `memory.high` transaccionales.
- [x] Journal durable por grupo y restauración conservadora.
- [x] PSI CPU/memoria/E/S y línea base adaptativa.
- [x] Energía y zonas térmicas.
- [x] Foreground X11 sin dependencia de enlace estático.
- [x] Smoke test cgroup nativo obligatorio para estable.
- [x] Paquetes `.deb` y ciclo completo de instalación en CI.

## macOS

- [x] Prioridad reversible.
- [x] Low Power Mode, estado térmico y frontmost application.
- [x] QoS `UTILITY` e I/O `IOPOL_THROTTLE` reversibles para trabajo interno.
- [x] LaunchAgent endurecido.
- [x] Paquetes `.pkg` nativos Intel/Apple Silicon.
- [x] Developer ID, notarización, stapling y validación automatizados.
- [x] Instalación, actualización y desinstalación real en CI.

## Inteligencia y automatización

- [x] Reglas auditables, modelo entrenable e híbrido.
- [x] Aprendizaje local persistente.
- [x] POTION Model2Vec 2M/4M/8M seleccionado por RAM.
- [x] Revisiones de modelo inmutables.
- [x] Descarga automática supervisada, timeout, kill, backoff y fallback.
- [x] Perfiles smart, eco, balanced, performance, gaming, development, creator, streaming y quiet.
- [x] Histéresis de modos e intervalos adaptativos.
- [x] Presupuesto de overhead con cinco niveles y recuperación gradual.

## SmartCache

- [x] Precarga page-cache sin drivers.
- [x] Raíces explícitas y aprendidas.
- [x] Presupuesto adaptativo y filtros de extensiones.
- [x] Rechazo de symlinks/reparse points y rutas sensibles.
- [x] Verificación de descriptor/tamaño/lectura.
- [x] Guardas PSI, CPU, RAM, batería y térmica.
- [x] QoS de utilidad en macOS.
- [x] Benchmark reproducible con dataset sintético o físico.

## Observabilidad

- [x] Estado humano y JSON.
- [x] Control en caliente.
- [x] `--analyze`, `--analyze-json`, `--export-report` y `--self-test`.
- [x] OpenMetrics local, loopback-only.
- [x] Métricas de decisiones, fallos, presión, batería, overhead y circuito.

## Instalación y supply chain

- [x] Instaladores online/offline con timeout y SHA-256.
- [x] Política opcional obligatoria de firma y attestation.
- [x] Instalación atómica y rechazo de assets indirectos.
- [x] Fallback de fuente únicamente con `Cargo.lock` y `--locked`.
- [x] SBOM CycloneDX 1.6 determinista.
- [x] Attestations GitHub OIDC/Sigstore.
- [x] Matriz de 41 casos de calificación, incluidos seis casos explícitos de empaquetado nativo.
- [x] Evidencia ligada a tag, commit y hash.
- [x] Bloqueo automático de una release estable incompleta.

## Trabajo posterior a 0.7

Las siguientes ideas no son deuda del roadmap cerrado; son líneas futuras opcionales:

- Integraciones Wayland específicas para KDE, GNOME y wlroots cuando existan APIs estables adecuadas.
- Dashboard gráfico nativo sobre el estado JSON/OpenMetrics.
- Perfiles de cgroup administrados mediante scopes systemd cuando el entorno los prefiera.
- Modelos locales adicionales evaluados con un protocolo de privacidad, latencia y consumo.
- Suite de endurance de varios días para detectar deriva y regresiones térmicas.
