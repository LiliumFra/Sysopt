# Motor de inteligencia híbrida — 0.6.5-rc1

SysOpt combina reglas deterministas, aprendizaje local y Model2Vec. La señal semántica clasifica procesos desconocidos; no aplica prioridades, no mueve procesos entre cgroups y no autoriza SmartCache directamente.

## Descarga automática supervisada

1. El servicio intenta cargar una instantánea local validada.
2. Si falta, continúa con reglas y aprendizaje.
3. Inicia el mismo ejecutable como subproceso interno de precarga.
4. Supervisa el proceso durante `semantic_download_timeout_secs`.
5. Si excede el límite, lo termina, drena stderr de forma acotada y programa un reintento.
6. Si finaliza, valida revisión, archivos e inferencia antes de activar el modelo.

Este diseño evita que una llamada síncrona de red de `hf-hub` bloquee indefinidamente el motor principal. La descarga no se ejecuta desde los instaladores.

## Modelos integrados

- `< 6 GiB`: `minishlab/potion-base-2M@389b9f64be5aa4ae7a6bc6fe95ef20ce485ae5da`
- `6–12 GiB`: `minishlab/potion-base-4M@9b3cff412d30be9ae8603fe10224c224f3401869`
- `>= 12 GiB`: `minishlab/potion-base-8M@ad78550e2b7d7f7a39d2abbc81b079aea6c20287`

Un modelo remoto personalizado debe usar `owner/modelo@<SHA de 40 caracteres>`. Una ruta local nunca activa red.

## Validaciones

- directorio y lock directos, sin symlink/reparse point;
- snapshot resuelto igual al commit solicitado;
- límites independientes para configuración, tokenizer y pesos;
- JSON válido;
- SafeTensors válido y con tensor utilizable;
- inferencia finita, no vacía y de dimensión coherente;
- frontera `catch_unwind` para panics desenrollables de la dependencia.

## Decisión

1. Se detectan contexto, presión y proceso en primer plano.
2. Las firmas reconocen procesos conocidos.
3. Model2Vec sugiere una categoría solo para desconocidos.
4. El aprendizaje pondera actividad y recurrencia.
5. Los guardarraíles limitan clase, cantidad y frecuencia de cambios.
6. El tracker elimina repeticiones.
7. El backend confirma la syscall antes de registrar el estado.

## Configuración

```toml
[intelligence]
semantic_enabled = true
semantic_auto_download = true
semantic_download_timeout_secs = 300
semantic_retry_initial_secs = 30
semantic_retry_max_secs = 3600
semantic_model_id = "auto"
```

El timeout debe estar entre 30 y 3600 segundos. El backoff inicial debe estar entre 5 y 3600 segundos y el máximo entre el inicial y 86400 segundos.
