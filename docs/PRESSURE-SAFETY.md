# Presión, energía y overhead — SysOpt 0.7.0-rc1

## Señales

- Linux PSI: `some`/`full` avg10 para CPU, memoria y E/S.
- Memoria disponible y CPU global en todas las plataformas.
- Battery Saver y porcentaje de batería cuando existen.
- Estado térmico o temperatura cuando la plataforma los expone.
- Duración p50/p95 del propio ciclo de SysOpt.

## Línea base adaptativa

El controlador persiste una línea base PSI local. Los límites efectivos combinan umbrales absolutos y desviación respecto del comportamiento normal del equipo. La histéresis evita alternar por picos aislados.

## Disyuntor

Se abre por fallos consecutivos, ratio de fallos o presión severa. Puede restaurar recursos administrados, entra en cooldown y no acepta cambios nuevos hasta recuperarse.

## Overhead

- Nivel 0: funciones completas.
- Nivel 1: menos procesos por ciclo.
- Nivel 2: SmartCache aplazado e intervalo mayor.
- Nivel 3: señal semántica suspendida.
- Nivel 4: operación mínima.

La reducción es inmediata; la recuperación requiere ciclos sanos consecutivos.
