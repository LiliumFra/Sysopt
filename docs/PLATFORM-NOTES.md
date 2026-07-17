# Matriz de capacidades — SysOpt 0.7.0-rc1

| Capacidad | Windows | Linux | macOS |
|---|---|---|---|
| Prioridad reversible | Sí | Sí | Sí |
| EcoQoS/HighQoS | Sí, Power Throttling nativo | No aplica | No existe API pública equivalente para terceros |
| Temporizadores Power Throttling | Sí | No aplica | No aplica |
| cgroup v2 | No | Sí, opt-in y raíz delegada | No |
| `cpu.weight`/`io.weight`/`memory.high` | No | Sí, journalizados | No |
| PSI | No | Sí | No |
| Energía/batería | Sí | Sí cuando sysfs la expone | Sí |
| Estado térmico | No genérico | Sí cuando thermal zones lo exponen | Sí |
| Foreground | Win32 | X11; Wayland degrada | NSWorkspace |
| QoS del trabajo interno | Scheduler normal | nice/I/O del servicio | QoS UTILITY + IOPOL_THROTTLE reversible |
| SmartCache | Sí | Sí | Sí |
| Instalador nativo | Inno Setup | `.deb` | `.pkg` |
| Firma estable | Authenticode | Attestation/checksum | Developer ID + notarización |

## Windows

EcoQoS requiere soporte de Process Power Throttling. Un fallo de API se reporta y alimenta el disyuntor; no se reemplaza silenciosamente por una clase de prioridad. Los procesos protegidos pueden denegar acceso.

## Linux

La raíz configurada debe ser una subjerarquía cgroup v2 realmente delegada. Las reglas del ancestro común pueden impedir mover procesos de otros servicios. El smoke test estable utiliza un runner con `SYSOPT_TEST_CGROUP_ROOT` preparado por el operador.

X11 se carga dinámicamente. En Wayland no se inventa una API universal; si no existe una integración disponible, el foreground se infiere por las demás señales.

## macOS

La API pública permite clasificar el trabajo propio por QoS. SysOpt no usa APIs privadas para imponer QoS a procesos de terceros. SmartCache usa un guard de utilidad y restaura el estado del hilo.
