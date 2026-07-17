# Recuperación durable — SysOpt 0.7.0-rc1

## Recursos cubiertos

- Prioridad Windows/Linux/macOS.
- Power Throttling Windows.
- Membresía cgroup v2.
- Controles cgroup `cpu.weight`, `io.weight`, `memory.high`.

## Estados

- `prepared`: el estado original está sincronizado; la syscall puede no haberse ejecutado o no haberse confirmado.
- `applied`: la lectura posterior confirmó el valor y el journal lo registró.

Una actualización conserva `previous_applied`. Esto permite recuperar tanto si el crash ocurrió antes como después de la segunda syscall.

## Reglas de recuperación

1. Validar esquema, permisos, archivo regular y lock.
2. Validar plataforma/backend.
3. Abrir proceso y comprobar identidad nativa.
4. Leer el recurso dos veces alrededor de la decisión.
5. Restaurar solo si coincide con `applied` o `previous_applied`.
6. Si el proceso desapareció, descartar la entrada.
7. Si un tercero cambió el valor, abandonar propiedad y no sobrescribir.
8. Sincronizar la eliminación del journal.

## cgroup por grupo

El journal de controles es independiente del journal de membresía. Conserva el valor original de cada archivo. Un grupo se restaura cuando queda sin procesos administrados o durante recuperación. Un cgroup eliminado se trata como recurso desaparecido; un valor cambiado externamente se respeta.

## Cierre

`--once`, pausa, shutdown y salida por señal intentan restaurar. Si queda una reconciliación incompleta, el runtime no escribe estado de cierre limpio y el siguiente inicio recupera antes de aplicar acciones nuevas.
