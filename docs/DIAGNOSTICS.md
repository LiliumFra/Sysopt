# Diagnóstico — SysOpt 0.7.0-rc1

## Análisis sin aplicar

```bash
sysopt --analyze
sysopt --analyze-json
sysopt --analyze-json --export-report ./sysopt-report.json
```

El JSON se emite sin mensajes informativos en stdout; avisos y rutas se escriben en stderr. Los procesos se sanitizan para evitar exponer rutas innecesarias.

## Autoprueba

```bash
sysopt --self-test
```

Valida configuración efectiva, telemetría, PSI si está disponible, clasificación, política, seguridad y planificación SmartCache sin convertir una limitación opcional en fallo fatal.

## Estado y métricas

```bash
sysopt --status
sysopt --status-json
```

Con `[metrics] enabled = true`, consulta `http://127.0.0.1:9898/metrics`. La configuración rechaza direcciones no loopback.

## Benchmark

```bash
sysopt-bench --output benchmark.json
sysopt-bench --dataset /ruta/dataset-fisico --output benchmark.json
python3 tools/check_benchmarks.py benchmark.json
```

El resultado contiene tiempos de clasificación 100/500/2.000, p95 y planificación SmartCache.
