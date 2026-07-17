mod analysis;
mod automation;
mod config;
mod macos_qos;
mod metrics;
mod overhead;
mod runtime;
mod safety;

use analysis::{collect_analysis, print_analysis, write_analysis_report};
use anyhow::{Context, Result};
use automation::{AutomationController, PerformanceProfile};
use config::{default_config_path, AppConfig, CliOptions};
use enforcement::{DryRunEnforcer, EnforcementConfig, PlatformEnforcer, SystemEnforcer};
use intelligence::{
    prefetch_semantic_model, resolve_semantic_model_spec, AdaptiveIntelligence, IntelligenceReport,
};
use metrics::MetricsServer;
use overhead::OverheadController;
use policy::{
    append_learning_record, load_learning_records, Classifier, LearningRecord, PriorityTracker,
    SignatureDb, SystemMode, TrainedModel,
};
use runtime::{
    CacheRuntimeStatus, IntelligenceRuntimeStatus, RuntimeCommand, RuntimeSession, RuntimeStatus,
};
use safety::SafetyController;
use smart_cache::{CacheConditions, CacheTuning, SmartCache, WarmReport};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use telemetry::{SystemSnapshot, Telemetry};

fn print_help() {
    println!(
        r#"sysopt — optimización automática de procesos, recursos y caché de archivos

USO
  sysopt [opciones]

EJECUCIÓN
  --apply                    Aplica cambios reales (default: dry-run)
  --dry-run                  Fuerza simulación aunque config.toml use apply=true
  --interval <seg>           Intervalo base cuando la automatización está desactivada
  --top <n>                  Procesos incluidos en la evaluación
  --once                     Ejecuta un solo ciclo y restaura antes de salir
  --signatures <ruta>        Firmas de procesos TOML personalizadas

CONTROL DEL SERVICIO
  --status                   Estado resumido de la instancia activa
  --status-json              Estado completo en JSON
  --pause                    Pausa y restaura los cambios administrados
  --resume                   Reanuda la optimización
  --set-profile <perfil>     Cambia cualquier perfil en caliente
  --shutdown                 Cierra limpiamente la instancia activa

AUTOMATIZACIÓN
  --auto / --no-auto         Activa o desactiva histéresis e intervalos adaptativos
  --profile <perfil>         smart, eco, balanced, performance, gaming,
                             development, creator, streaming o quiet
  --ai / --no-ai             Activa o desactiva la IA híbrida local adaptativa
  --install-ai-model         Descarga, valida y deja listo el modelo de Hugging Face
  --doctor                   Valida y muestra la configuración efectiva

DIAGNÓSTICO Y EXPLICABILIDAD
  --analyze                  Analiza durante 1 s y explica el plan sin aplicar cambios
  --analyze-json             Emite el análisis estructurado en JSON
  --export-report <ruta>     Guarda el análisis JSON con reemplazo atómico y permisos privados
  --self-test                Ejecuta configuración, telemetría, PSI, política y SmartCache

SMARTCACHE (PRECARGA SEGURA EN LA CACHÉ DEL SO)
  --cache / --no-cache       Activa o desactiva SmartCache
  --cache-root <ruta>        Añade una raíz explícita; puede repetirse
  --warm-cache <ruta>        Planifica o precarga una ruta y termina; puede repetirse
                             Usa --apply para efectuar la lectura

CONFIGURACIÓN
  --config <ruta>            Usa un archivo TOML específico
  --init-config <ruta>       Escribe una configuración completa de ejemplo

CLASIFICACIÓN Y APRENDIZAJE
  --classifier <tipo>        rules, model o hybrid
  --model <ruta>             Modelo entrenado TOML
  --min-confidence <0..1>    Umbral del modelo en modo hybrid
  --learn / --no-learn       Registra cada ciclo en el dataset JSONL
  --dataset <ruta>           Ruta del dataset
  --label <modo>             Captura y etiqueta una muestra; luego sale
  --train-model <dataset>    Entrena desde muestras etiquetadas; luego sale
  --model-output <ruta>      Destino de --train-model

RECURSOS AVANZADOS (OPT-IN)
  --resources                Activa cgroups v2 en Linux (no disponible en Windows/macOS)
  --no-resources             Los desactiva
  --cgroup-root <ruta>       Raíz cgroup v2 delegada a sysopt en Linux

OTROS
  -h, --help                 Ayuda
  -V, --version              Versión

MODOS PARA --label
  idle, interactive, developing, mobile_development, gaming, creative,
  streaming, containerized, heavy_foreground, unknown
"#
    );
}

fn main() -> Result<()> {
    let cli = CliOptions::parse().context("argumentos inválidos; usa --help")?;
    if cli.internal_ai_prefetch {
        return run_internal_ai_prefetch(&cli);
    }
    if cli.show_help {
        print_help();
        return Ok(());
    }
    if cli.show_version {
        println!("sysopt {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(path) = &cli.init_config {
        AppConfig::save_example(path)?;
        println!("configuración de ejemplo creada en {}", path.display());
        return Ok(());
    }

    let selected_config_path = cli.config_path.clone().unwrap_or_else(default_config_path);
    let base_config = if selected_config_path.exists() {
        let config = AppConfig::load(&selected_config_path)?;
        if !cli.status && !cli.status_json && !cli.analyze_json {
            println!(
                "configuración cargada desde {}",
                selected_config_path.display()
            );
        }
        config
    } else if cli.config_path.is_some() {
        anyhow::bail!("no existe el archivo {}", selected_config_path.display());
    } else {
        AppConfig::default()
    };
    let config = cli.clone().merge(base_config)?;

    if cli.install_ai_model {
        return install_ai_model(&config);
    }

    if cli.status {
        return print_runtime_status(&config, cli.status_json);
    }
    if cli.pause {
        return send_runtime_command(&config, RuntimeCommand::Pause, "pausa solicitada");
    }
    if cli.resume {
        return send_runtime_command(&config, RuntimeCommand::Resume, "reanudación solicitada");
    }
    if let Some(profile) = &cli.runtime_profile {
        return send_runtime_command(
            &config,
            RuntimeCommand::SetProfile {
                profile: profile.clone(),
            },
            &format!("perfil {profile} solicitado"),
        );
    }
    if cli.shutdown {
        return send_runtime_command(&config, RuntimeCommand::Shutdown, "cierre solicitado");
    }

    if cli.doctor {
        return doctor(&config, &selected_config_path);
    }

    if let Some(dataset) = &cli.train_model_dataset {
        return train_model(
            dataset,
            cli.model_output
                .clone()
                .or_else(|| config.classifier.model_path.clone())
                .unwrap_or_else(|| PathBuf::from("sysopt-model.toml")),
        );
    }

    if !cli.warm_cache_paths.is_empty() {
        let mut cache = SmartCache::new(config.cache.clone())?;
        let report = cache.warm_paths_now(&cli.warm_cache_paths, config.apply)?;
        print_cache_report(&report, true);
        if !config.apply {
            println!("  simulación: agrega --apply para efectuar la precarga");
        }
        return Ok(());
    }

    let signatures = load_signatures(&config)?;
    let classifier = Classifier::new(
        config.strategy()?,
        config.classifier.model_path.as_deref(),
        config.classifier.min_confidence,
    )?;

    if cli.analyze {
        let report = collect_analysis(&config, &signatures, &classifier)?;
        if cli.analyze_json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_analysis(&report);
        }
        if let Some(path) = &cli.export_report {
            write_analysis_report(path, &report)?;
            eprintln!("informe privado guardado en {}", path.display());
        }
        return Ok(());
    }

    if cli.self_test {
        return self_test(&config, &selected_config_path, &signatures, &classifier);
    }

    if let Some(label) = cli.label {
        return capture_labeled_sample(&config, &signatures, &classifier, label);
    }

    run(config, signatures, classifier)
}

fn run_internal_ai_prefetch(cli: &CliOptions) -> Result<()> {
    let model_id = cli
        .internal_ai_model_id
        .as_deref()
        .context("falta el identificador interno del modelo")?;
    let cache_dir = cli
        .internal_ai_cache_dir
        .as_deref()
        .context("falta el directorio interno de caché")?;
    let total_memory = cli
        .internal_ai_total_memory
        .context("falta la memoria total para seleccionar el modelo")?;
    let min_similarity = cli
        .internal_ai_min_similarity
        .context("falta el umbral semántico interno")?;
    let max_cache_entries = cli
        .internal_ai_max_cache_entries
        .context("falta el límite de caché semántica interno")?;
    prefetch_semantic_model(
        model_id,
        cache_dir,
        total_memory,
        min_similarity,
        max_cache_entries,
    )?;
    Ok(())
}

fn install_ai_model(config: &AppConfig) -> Result<()> {
    anyhow::ensure!(
        config.intelligence.semantic_enabled,
        "la IA semántica está desactivada en la configuración"
    );
    let cache_dir = config
        .intelligence
        .semantic_cache_dir
        .as_deref()
        .context("intelligence.semantic_cache_dir no está configurado")?;
    let mut telemetry = Telemetry::new();
    let snapshot = telemetry.snapshot(1);
    let selected = resolve_semantic_model_spec(
        &config.intelligence.semantic_model_id,
        snapshot.total_memory_bytes,
    );
    println!(
        "preparando modelo semántico {}{}...",
        selected.model_id,
        selected
            .revision
            .as_deref()
            .map_or_else(String::new, |revision| format!("@{revision}"))
    );
    let info = prefetch_semantic_model(
        &config.intelligence.semantic_model_id,
        cache_dir,
        snapshot.total_memory_bytes,
        config.intelligence.semantic_min_similarity,
        config.intelligence.semantic_max_cache_entries,
    )?;
    println!(
        "modelo listo: {}{} | dimensiones {} | prototipos {} | caché {}",
        info.model_id,
        info.revision
            .as_deref()
            .map_or_else(String::new, |revision| format!("@{revision}")),
        info.embedding_dimensions,
        info.prototype_count,
        info.cache_dir.display()
    );
    Ok(())
}

fn print_runtime_status(config: &AppConfig, as_json: bool) -> Result<()> {
    let running = runtime::is_instance_running(&config.runtime)?;
    let Some(mut status) = runtime::read_status(&config.runtime)? else {
        println!(
            "sysopt: sin estado registrado; servicio {}",
            if running { "activo" } else { "detenido" }
        );
        return Ok(());
    };
    status.running = running;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    println!(
        "sysopt {} | {}{} | perfil {} | modo {} | CPU {:.1}% | RAM libre {:.0} MiB",
        status.version,
        if running { "activo" } else { "detenido" },
        if status.paused { " (pausado)" } else { "" },
        status.profile,
        status.effective_mode,
        status.global_cpu_percent,
        status.available_memory_bytes as f64 / 1_048_576.0
    );
    println!(
        "  apply: {} | modo seguro: {} | cambios administrados: {} | SmartCache: {:.1} MiB",
        on_off(status.apply),
        on_off(status.safe_mode),
        status.managed_changes,
        status.cache.bytes_warmed as f64 / 1_048_576.0
    );
    println!(
        "  último ciclo: {} ms | acciones: {} planificadas / {} aplicadas / {} fallidas",
        status.cycle_duration_ms,
        status.actions_planned,
        status.actions_applied,
        status.action_failures
    );
    println!(
        "  overhead: nivel {} | p50/p95 {} / {} ms",
        status.overhead_level, status.overhead_p50_ms, status.overhead_p95_ms
    );
    if status.power_supported {
        println!(
            "  energía: CA {:?} | batería {} | ahorro {} | térmico {}{}",
            status.on_ac_power,
            status
                .battery_percent
                .map_or_else(|| "n/d".into(), |value| format!("{value}%")),
            on_off(status.battery_saver),
            status.thermal_state,
            status
                .temperature_c
                .map_or_else(String::new, |value| format!(" | {value:.1} °C"))
        );
    }
    if status.pressure_supported {
        println!(
            "  PSI avg10: CPU {:.2}% | memoria some/full {:.2}/{:.2}% | E/S some/full {:.2}/{:.2}%",
            status.cpu_some_pressure_avg10,
            status.memory_some_pressure_avg10,
            status.memory_full_pressure_avg10,
            status.io_some_pressure_avg10,
            status.io_full_pressure_avg10
        );
    }
    println!(
        "  seguridad adaptativa: {} | disyuntor: {} | presión: {} | fallos consecutivos: {} | cooldown: {}s{}",
        on_off(status.safety.enabled),
        if status.safety.circuit_open {
            "abierto"
        } else {
            "cerrado"
        },
        if status.safety.pressure_limited {
            "limitada"
        } else {
            "normal"
        },
        status.safety.consecutive_failure_cycles,
        status.safety.cooldown_remaining_secs,
        status
            .safety
            .reason
            .as_deref()
            .map_or_else(String::new, |reason| format!(" | {reason}"))
    );
    println!(
        "  IA híbrida: {} | confianza {:.0}% | priorizados {} | reducidos {} | memoria: {} procesos / {} raíces",
        on_off(status.intelligence.enabled),
        status.intelligence.confidence * 100.0,
        status.intelligence.boosted_processes,
        status.intelligence.demoted_processes,
        status.intelligence.learned_processes,
        status.intelligence.learned_roots
    );
    let semantic_state = if status.intelligence.semantic_model_loaded {
        status
            .intelligence
            .semantic_model
            .as_deref()
            .unwrap_or("cargado")
            .to_owned()
    } else if status.intelligence.semantic_downloading {
        "descargando automáticamente".into()
    } else if let Some(seconds) = status.intelligence.semantic_retry_in_secs {
        format!("fallback; reintento en {seconds}s")
    } else {
        "fallback".into()
    };
    println!(
        "  modelo semántico: {} | aciertos ciclo: {}{}",
        semantic_state,
        status.intelligence.semantic_hits,
        status
            .intelligence
            .semantic_error
            .as_deref()
            .map_or_else(String::new, |error| format!(" | aviso: {error}"))
    );
    if let Some(message) = status.message {
        println!("  estado: {message}");
    }
    Ok(())
}

fn send_runtime_command(config: &AppConfig, command: RuntimeCommand, message: &str) -> Result<()> {
    runtime::send_command(&config.runtime, &command)?;
    println!("{message}");
    Ok(())
}

fn doctor(config: &AppConfig, config_path: &Path) -> Result<()> {
    config.validate()?;
    println!("sysopt {} — diagnóstico", env!("CARGO_PKG_VERSION"));
    println!(
        "  configuración: {} ({})",
        config_path.display(),
        if config_path.exists() {
            "existente"
        } else {
            "valores predeterminados"
        }
    );
    println!(
        "  ejecución: {} | automatización: {} | perfil: {}",
        if config.apply { "apply" } else { "dry-run" },
        on_off(config.automation.enabled),
        config.automation.profile
    );
    println!(
        "  clasificador: {} | modelo: {}",
        config.classifier.strategy,
        config.classifier.model_path.as_ref().map_or_else(
            || "no configurado".into(),
            |path| path.display().to_string()
        )
    );
    println!(
        "  IA híbrida local: {} | persistencia: {} | estado: {} | confianza mínima: {:.0}%",
        on_off(config.intelligence.enabled),
        on_off(config.intelligence.persist_state),
        config
            .intelligence
            .state_path
            .as_ref()
            .map_or_else(|| "en memoria".into(), |path| path.display().to_string()),
        config.intelligence.min_confidence * 100.0
    );
    println!(
        "  semántica: {} | modelo: {} | caché: {} | timeout descarga: {}s",
        on_off(config.intelligence.semantic_enabled),
        config.intelligence.semantic_model_id,
        config.intelligence.semantic_cache_dir.as_ref().map_or_else(
            || "no configurada".into(),
            |path| path.display().to_string()
        ),
        config.intelligence.semantic_download_timeout_secs
    );
    println!(
        "  SmartCache: {} | auto-roots: {} | presupuesto máximo/ciclo: {:.0} MiB | adaptativo: {}",
        on_off(config.cache.enabled),
        on_off(config.cache.auto_process_roots),
        config.cache.bytes_per_cycle as f64 / 1_048_576.0,
        on_off(config.cache.adaptive_budget)
    );
    println!(
        "  límites SmartCache: CPU <= {:.0}% | RAM libre >= {}% | archivo <= {:.0} MiB",
        config.cache.max_cpu_percent,
        config.cache.min_available_memory_percent,
        config.cache.max_file_size_bytes as f64 / 1_048_576.0
    );
    println!(
        "  presión SmartCache: {} | memoria some <= {:.2}% | E/S some <= {:.2}%",
        on_off(config.cache.pressure_guard_enabled),
        config.cache.max_memory_some_pressure_avg10,
        config.cache.max_io_some_pressure_avg10
    );
    println!(
        "  seguridad: {} | disyuntor tras {} ciclos >= {:.0}% fallos | cooldown {}s | restauración {}",
        on_off(config.safety.enabled),
        config.safety.max_consecutive_failure_cycles,
        config.safety.max_failure_ratio * 100.0,
        config.safety.cooldown_secs,
        on_off(config.safety.restore_on_trip)
    );
    println!(
        "  guardia PSI: {} | CPU some <= {:.2}% | memoria full <= {:.2}% | E/S full <= {:.2}%",
        on_off(config.safety.pressure_guard_enabled),
        config.safety.max_cpu_some_pressure_avg10,
        config.safety.max_memory_full_pressure_avg10,
        config.safety.max_io_full_pressure_avg10
    );
    if config.cache.roots.is_empty() {
        println!("  raíces explícitas: ninguna; se aprenderán desde procesos activos");
    } else {
        for root in &config.cache.roots {
            println!(
                "  raíz: {} ({})",
                root.display(),
                if root.exists() {
                    "accesible"
                } else {
                    "no existe"
                }
            );
        }
    }
    println!("  grupos de recursos: {}", on_off(config.resources.enabled));
    println!(
        "  control local: {} | instancia activa: {}",
        on_off(config.runtime.enabled),
        on_off(runtime::is_instance_running(&config.runtime)?)
    );
    println!(
        "    estado efímero: {}",
        config.runtime.status_path.display()
    );
    println!("    comandos: {}", config.runtime.control_path.display());
    println!("    lock: {}", config.runtime.lock_path.display());
    println!(
        "    salud persistente: {}",
        config.runtime.health_path.display()
    );
    println!(
        "    journal de recuperación: {}",
        config.runtime.enforcement_journal_path.display()
    );

    #[cfg(all(unix, not(target_os = "macos")))]
    if std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .is_none_or(|path| !path.is_absolute())
    {
        println!(
            "  aviso: XDG_RUNTIME_DIR no está disponible; control y lock usan el fallback persistente {}",
            config
                .runtime
                .lock_path
                .parent()
                .map_or_else(|| Path::new(".").display().to_string(), |path| path.display().to_string())
        );
    }

    #[cfg(target_os = "linux")]
    println!(
        "  PSI Linux: {}",
        if [
            "/proc/pressure/cpu",
            "/proc/pressure/memory",
            "/proc/pressure/io"
        ]
        .iter()
        .all(|path| Path::new(path).is_file())
        {
            "disponible"
        } else {
            "no disponible; se usarán señales de respaldo"
        }
    );

    #[cfg(target_os = "linux")]
    if config.resources.enabled {
        let root = &config.resources.linux_cgroup_root;
        let parent = root.parent();
        let looks_delegated = parent.is_some_and(|path| {
            path.join("cgroup.controllers").is_file()
                && path.join("cgroup.subtree_control").is_file()
        });
        println!(
            "  cgroup v2: {} | raíz: {}",
            if looks_delegated {
                "jerarquía detectada"
            } else {
                "no verificada"
            },
            root.display()
        );
        println!(
            "  aviso: los grupos de recursos requieren una subjerarquía realmente delegada; mover procesos de otros servicios puede fallar por las reglas del ancestro común de cgroup v2"
        );
    }
    #[cfg(not(target_os = "linux"))]
    if config.resources.enabled {
        println!("  aviso: resources.enabled solo puede aplicarse en Linux cgroup v2");
    }

    println!("diagnóstico completado: configuración válida");
    Ok(())
}

fn self_test(
    config: &AppConfig,
    config_path: &Path,
    signatures: &SignatureDb,
    classifier: &Classifier,
) -> Result<()> {
    doctor(config, config_path)?;
    let _cache = SmartCache::new(config.cache.clone())?;
    let report = collect_analysis(config, signatures, classifier)?;
    println!("\nprueba funcional en vivo:");
    print_analysis(&report);
    println!(
        "autoprueba completada: configuración, firmas, clasificador, telemetría, política y SmartCache operativos"
    );
    Ok(())
}

fn on_off(value: bool) -> &'static str {
    if value {
        "activo"
    } else {
        "inactivo"
    }
}

fn load_signatures(config: &AppConfig) -> Result<SignatureDb> {
    match &config.signatures_path {
        Some(path) => {
            let database = SignatureDb::load_from_file(path.to_string_lossy().as_ref())?;
            eprintln!(
                "firmas cargadas desde {} ({} categorías)",
                path.display(),
                database.category_count()
            );
            Ok(database)
        }
        None => SignatureDb::try_embedded_default(),
    }
}

fn train_model(dataset: &Path, output: PathBuf) -> Result<()> {
    let records = load_learning_records(dataset)?;
    let labeled = records
        .iter()
        .filter(|record| record.user_label.is_some())
        .count();
    let model = TrainedModel::train(&records)?;
    model.save(&output)?;
    println!(
        "modelo entrenado con {labeled} muestras etiquetadas y {} clases; guardado en {}",
        model.classes.len(),
        output.display()
    );
    Ok(())
}

fn capture_labeled_sample(
    config: &AppConfig,
    signatures: &SignatureDb,
    classifier: &Classifier,
    label: SystemMode,
) -> Result<()> {
    let mut telemetry = Telemetry::new();
    let _ = telemetry.snapshot(config.top_n);
    thread::sleep(Duration::from_secs(1));
    let snapshot = telemetry.snapshot(config.top_n);
    let prediction = classifier.predict(&snapshot, signatures)?;
    let record = LearningRecord::new(
        &snapshot,
        signatures,
        prediction.mode,
        prediction.confidence,
        prediction.source,
        Some(label),
    );
    append_learning_record(&config.learning.dataset_path, &record)?;
    println!(
        "muestra etiquetada como {label} guardada en {} (predicción previa: {}, confianza {:.1}%)",
        config.learning.dataset_path.display(),
        prediction.mode,
        prediction.confidence * 100.0
    );
    Ok(())
}

fn run(mut config: AppConfig, signatures: SignatureDb, classifier: Classifier) -> Result<()> {
    let mut runtime_session = RuntimeSession::start(config.runtime.clone())?;
    let safe_mode = runtime_session.safe_mode();
    if safe_mode {
        config.apply = false;
        config.resources.enabled = false;
        eprintln!(
            "sysopt — MODO SEGURO: se detectaron cierres anómalos repetidos; esta sesión funcionará en dry-run"
        );
    }

    let maximum_cycle_interval = config
        .interval_secs
        .max(config.automation.idle_interval_secs)
        .max(config.automation.active_interval_secs)
        .max(config.automation.busy_interval_secs);
    let enforcement_config = EnforcementConfig {
        resource_groups_enabled: config.resources.enabled,
        linux_cgroup_root: Some(config.resources.linux_cgroup_root.clone()),
        journal_path: Some(config.runtime.enforcement_journal_path.clone()),
        cgroup_control_journal_path: Some(config.runtime.cgroup_control_journal_path.clone()),
        journal_lease_secs: maximum_cycle_interval.saturating_mul(4).clamp(120, 3_600),
    };
    let recovery_required = [
        &config.runtime.enforcement_journal_path,
        &config.runtime.cgroup_control_journal_path,
    ]
    .into_iter()
    .try_fold(false, |required, path| {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(required),
            Err(error) => Err(error)
                .with_context(|| format!("no se pudo inspeccionar el journal {}", path.display())),
        }
    })?;
    let mut platform_enforcer = if config.apply || recovery_required {
        Some(PlatformEnforcer::new(enforcement_config)?)
    } else {
        None
    };
    if let Some(recovery_enforcer) = platform_enforcer.as_mut() {
        let recovery = recovery_enforcer.recover();
        if recovery.restored > 0 || recovery.discarded > 0 || recovery.has_failures() {
            println!("sysopt — {}", recovery.summary());
        }
        if recovery.has_failures() {
            anyhow::bail!(
                "recuperación incompleta; no se aplicarán cambios nuevos:\n  {}",
                recovery.failures.join("\n  ")
            );
        }
    }
    let mut enforcer: Box<dyn SystemEnforcer> = if config.apply {
        println!("sysopt — APPLY: se modificarán prioridades y se habilitará la precarga");
        if config.resources.enabled {
            println!("resource groups activados (función avanzada opt-in)");
        }
        Box::new(platform_enforcer.context("backend de enforcement no inicializado")?)
    } else {
        println!("sysopt — DRY-RUN: no se modifican prioridades ni se leen archivos para precarga");
        Box::new(DryRunEnforcer)
    };
    let priority_enforcement_available = enforcer.supports_priority_changes();
    let power_policy_enforcement_available = enforcer.supports_power_policy_changes();
    if config.apply && !priority_enforcement_available {
        eprintln!(
            "sysopt — aviso seguro: este usuario no puede restaurar prioridades Unix; las acciones de prioridad se omitirán y SmartCache seguirá activo"
        );
    }
    println!(
        "automatización {} | perfil {} | IA híbrida {} | SmartCache {}",
        on_off(config.automation.enabled),
        config.automation.profile,
        on_off(config.intelligence.enabled),
        on_off(config.cache.enabled)
    );
    println!("Ctrl+C para salir y restaurar los cambios confirmados que sean recuperables\n");

    let running = Arc::new(AtomicBool::new(true));
    install_shutdown_handlers(&running)?;

    let mut telemetry = Telemetry::new();
    let mut tracker = PriorityTracker::new();
    let resource_policy = config.resource_policy();
    let mut automation = AutomationController::new(config.automation.clone())?;
    let mut intelligence = AdaptiveIntelligence::new(config.intelligence.clone())?;
    let mut cache = SmartCache::new(config.cache.clone())?;
    let mut safety = SafetyController::new(config.safety.clone())?;
    let mut overhead = OverheadController::new(config.overhead.clone())?;
    let mut overhead_plan = overhead.current_plan();
    let mut paused = false;
    let mut pending_command = None;
    let mut status = RuntimeStatus {
        apply: config.apply,
        profile: automation.profile_name().to_owned(),
        safe_mode,
        ..RuntimeStatus::default()
    };
    runtime_session.write_status(&status, true)?;
    let metrics = MetricsServer::start(&config.metrics, status.clone())?;

    let _ = telemetry.snapshot(config.top_n);
    sleep_interruptible(Duration::from_millis(250), &running);

    let loop_result = (|| -> Result<()> {
        while running.load(Ordering::SeqCst) {
            let cycle_started = Instant::now();
            enforcer
                .heartbeat()
                .context("no se pudo renovar el journal de enforcement")?;
            let mut actions_planned = 0usize;
            let mut actions_applied = 0usize;
            let mut action_failures = 0usize;
            let command = pending_command.take().or(runtime_session.read_command()?);
            if let Some(command) = command {
                match command {
                    RuntimeCommand::Pause => {
                        if !paused {
                            telemetry.refresh_process_identities();
                            let cleanup = tracker.reset_all(|pid, start_time| {
                                telemetry.is_same_process(pid, start_time)
                            });
                            if !cleanup.is_empty() {
                                let report = enforcer.apply(&cleanup);
                                telemetry.refresh_process_identities();
                                tracker.commit(&report.succeeded, |pid, start_time| {
                                    telemetry.is_same_process(pid, start_time)
                                });
                                if let Some(error) = report.failure_summary() {
                                    status.message =
                                        Some(format!("pausa con restauración incompleta: {error}"));
                                    eprintln!(
                                        "control: no se pudo restaurar todo al pausar: {error}"
                                    );
                                }
                            }
                            paused = true;
                            if tracker.is_empty() {
                                status.message = Some("optimización pausada".into());
                            }
                            println!("control: optimización pausada");
                        }
                    }
                    RuntimeCommand::Resume => {
                        paused = false;
                        status.message = Some("optimización reanudada".into());
                        println!("control: optimización reanudada");
                    }
                    RuntimeCommand::SetProfile { profile } => {
                        let profile = PerformanceProfile::from_str(&profile)?;
                        automation.set_profile(profile);
                        apply_cache_runtime_profile(&mut cache, profile)?;
                        status.profile = automation.profile_name().to_owned();
                        status.message = Some(format!("perfil cambiado a {}", status.profile));
                        println!("control: perfil cambiado a {}", status.profile);
                    }
                    RuntimeCommand::Shutdown => {
                        status.message =
                            Some("cierre solicitado desde el centro de control".into());
                        running.store(false, Ordering::SeqCst);
                        continue;
                    }
                }
            }

            let effective_top_n = (config.top_n / overhead_plan.top_n_divisor).max(1);
            let snapshot = telemetry.snapshot(effective_top_n);
            let safety_decision = safety.preflight(snapshot.pressure, snapshot.power);
            let prediction = classifier.predict(&snapshot, &signatures)?;
            let automatic = automation.observe(prediction.mode, config.interval_secs);
            let effective_mode = automatic.effective_mode;
            if automatic.changed {
                println!(
                    "  automatización: modo estabilizado {:?}, próximo ciclo en {}s",
                    effective_mode, automatic.interval_secs
                );
            }

            let hot_roots = detect_hot_roots(&snapshot, &signatures, effective_mode);
            let foreground_pid = telemetry.foreground_pid();
            intelligence.set_semantic_runtime_allowed(overhead_plan.allow_semantic);
            cache.remember_process_roots(hot_roots.clone());
            let intelligent = intelligence.decide(
                effective_mode,
                &snapshot,
                &signatures,
                &resource_policy,
                &hot_roots,
                foreground_pid,
            )?;
            cache.remember_process_roots(intelligent.cache.root_scores.keys().cloned());

            let mode_text = if effective_mode == prediction.mode {
                effective_mode.to_string()
            } else {
                format!("{} -> {} (estabilizando)", prediction.mode, effective_mode)
            };
            println!(
                "CPU {:.1}% | RAM libre {:.0} MiB | procesos {} | modo {} ({:?}, {:.0}%) | próximo {}s{}",
                snapshot.global_cpu_percent,
                snapshot.available_memory_bytes as f64 / 1_048_576.0,
                snapshot.process_count,
                mode_text,
                prediction.source,
                prediction.confidence * 100.0,
                automatic.interval_secs,
                if paused { " | PAUSADO" } else { "" }
            );
            if snapshot.pressure.supported {
                println!(
                    "  PSI avg10: CPU {:.2}% | memoria some/full {:.2}/{:.2}% | E/S some/full {:.2}/{:.2}%",
                    snapshot.pressure.cpu_some_avg10,
                    snapshot.pressure.memory_some_avg10,
                    snapshot.pressure.memory_full_avg10,
                    snapshot.pressure.io_some_avg10,
                    snapshot.pressure.io_full_avg10
                );
            }

            let mut cache_status = status.cache.clone();
            if safety_decision.allow_actions
                && status
                    .message
                    .as_deref()
                    .is_some_and(|message| message.starts_with("seguridad:"))
            {
                status.message = None;
            }
            if paused {
                cache_status.reason = Some("optimización pausada".into());
            } else if !safety_decision.allow_actions {
                let reason = safety_decision
                    .reason
                    .clone()
                    .unwrap_or_else(|| "guardarraíl de seguridad activo".into());
                let guard = if safety_decision.pressure_limited {
                    "guardarraíl de presión"
                } else if safety_decision.power_limited {
                    "guardarraíl de energía/térmico"
                } else {
                    "disyuntor"
                };
                eprintln!("  seguridad ({guard}): {reason}");
                cache_status.reason = Some(reason.clone());
                status.message = Some(format!("seguridad: {reason}"));
            } else {
                println!("  {}", intelligent.report.summary);
                for decision in intelligent
                    .report
                    .process_decisions
                    .iter()
                    .filter(|decision| decision.priority != policy::Priority::Normal)
                    .take(6)
                {
                    println!(
                        "    IA: {} -> {:?} (score {:.0}%, confianza {:.0}%{})",
                        decision.name,
                        decision.priority,
                        decision.score * 100.0,
                        decision.confidence * 100.0,
                        decision.semantic_category.as_deref().map_or_else(
                            String::new,
                            |category| format!(
                                ", semántica {category} {:.0}%",
                                decision.semantic_similarity * 100.0
                            ),
                        )
                    );
                }
                let mut requested_actions = intelligent.actions.clone();
                if !priority_enforcement_available {
                    requested_actions.retain(|action| {
                        !matches!(action, policy::Action::SetProcessPriority { .. })
                    });
                }
                if !power_policy_enforcement_available {
                    requested_actions.retain(|action| {
                        !matches!(action, policy::Action::SetProcessPowerPolicy { .. })
                    });
                }
                telemetry.refresh_process_identities();
                let actions = tracker.plan(requested_actions, |pid, start_time| {
                    telemetry.is_same_process(pid, start_time)
                });
                actions_planned = actions.len();
                if actions.is_empty() {
                    println!("  (sin cambios de prioridad)");
                } else {
                    let report = enforcer.apply(&actions);
                    actions_applied = report.succeeded.len();
                    action_failures = report.failures.len();
                    telemetry.refresh_process_identities();
                    tracker.commit(&report.succeeded, |pid, start_time| {
                        telemetry.is_same_process(pid, start_time)
                    });
                    if let Some(error) = report.failure_summary() {
                        eprintln!("  error aplicando acciones: {error}");
                        status.message = Some(format!("error de enforcement: {error}"));
                    } else if status
                        .message
                        .as_deref()
                        .is_some_and(|message| message.starts_with("error de enforcement:"))
                    {
                        status.message = None;
                    }
                }

                let safety_transition = safety.record_outcome(actions_planned, action_failures);
                let block_cache_for_safety = safety_transition.tripped;
                if safety_transition.tripped {
                    let reason = safety_transition
                        .reason
                        .unwrap_or_else(|| "disyuntor de enforcement abierto".into());
                    eprintln!("  seguridad: {reason}");
                    status.message = Some(format!("seguridad: {reason}"));
                    cache_status.reason = Some(reason);
                    if safety.should_restore_on_trip() && !tracker.is_empty() {
                        telemetry.refresh_process_identities();
                        let cleanup = tracker.reset_all(|pid, start_time| {
                            telemetry.is_same_process(pid, start_time)
                        });
                        if !cleanup.is_empty() {
                            let restore_report = enforcer.apply(&cleanup);
                            telemetry.refresh_process_identities();
                            tracker.commit(&restore_report.succeeded, |pid, start_time| {
                                telemetry.is_same_process(pid, start_time)
                            });
                            if let Some(error) = restore_report.failure_summary() {
                                status.message =
                                    Some(format!("disyuntor con restauración incompleta: {error}"));
                                eprintln!(
                                    "  seguridad: restauración incompleta al abrir el disyuntor: {error}"
                                );
                            } else {
                                println!(
                                    "  seguridad: cambios administrados restaurados tras abrir el disyuntor"
                                );
                            }
                        }
                    }
                }

                let cache_conditions = CacheConditions {
                    global_cpu_percent: snapshot.global_cpu_percent,
                    available_memory_bytes: snapshot.available_memory_bytes,
                    total_memory_bytes: snapshot.total_memory_bytes,
                    allow_warmup: intelligent.cache.allow_warmup
                        && !block_cache_for_safety
                        && overhead_plan.allow_cache,
                    pressure_supported: snapshot.pressure.supported,
                    memory_some_pressure_avg10: snapshot.pressure.memory_some_avg10,
                    io_some_pressure_avg10: snapshot.pressure.io_some_avg10,
                };
                let cache_tuning = CacheTuning {
                    budget_multiplier: intelligent.cache.budget_multiplier,
                    scan_interval_multiplier: intelligent.cache.scan_interval_multiplier,
                    root_scores: intelligent.cache.root_scores.clone(),
                    extension_scores: intelligent.cache.extension_scores.clone(),
                    decision_reason: if block_cache_for_safety {
                        cache_status.reason.clone()
                    } else {
                        Some(intelligent.cache.reason.clone())
                    },
                };
                let _worker_qos = macos_qos::WorkerQosGuard::utility_io().ok();
                match cache.tick_with_tuning(cache_conditions, config.apply, &cache_tuning) {
                    Ok(Some(report)) => {
                        print_cache_report(&report, false);
                        cache_status = cache_status_from_report(&report);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("  SmartCache: {error}");
                        cache_status.errors = cache_status.errors.saturating_add(1);
                        cache_status.reason = Some(error.to_string());
                    }
                }
            }

            if config.learning.enabled {
                let record = LearningRecord::new(
                    &snapshot,
                    &signatures,
                    prediction.mode,
                    prediction.confidence,
                    prediction.source,
                    None,
                );
                if let Err(error) = append_learning_record(&config.learning.dataset_path, &record) {
                    eprintln!("  aviso: no se pudo registrar aprendizaje: {error}");
                }
            }

            status.running = true;
            status.paused = paused;
            status.profile = automation.profile_name().to_owned();
            status.observed_mode = prediction.mode.to_string();
            status.effective_mode = effective_mode.to_string();
            status.classifier_source = format!("{:?}", prediction.source);
            status.confidence = prediction.confidence;
            status.global_cpu_percent = snapshot.global_cpu_percent;
            status.pressure_supported = snapshot.pressure.supported;
            status.cpu_some_pressure_avg10 = snapshot.pressure.cpu_some_avg10;
            status.memory_some_pressure_avg10 = snapshot.pressure.memory_some_avg10;
            status.memory_full_pressure_avg10 = snapshot.pressure.memory_full_avg10;
            status.io_some_pressure_avg10 = snapshot.pressure.io_some_avg10;
            status.io_full_pressure_avg10 = snapshot.pressure.io_full_avg10;
            status.available_memory_bytes = snapshot.available_memory_bytes;
            status.total_memory_bytes = snapshot.total_memory_bytes;
            status.process_count = snapshot.process_count;
            status.managed_changes = tracker.len();
            status.actions_planned = actions_planned;
            status.actions_applied = actions_applied;
            status.action_failures = action_failures;
            status.cycle_duration_ms = cycle_started
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            overhead_plan = overhead.observe(status.cycle_duration_ms);
            status.overhead_level = overhead_plan.level;
            status.overhead_p50_ms = overhead_plan.p50_ms;
            status.overhead_p95_ms = overhead_plan.p95_ms;
            status.power_supported = snapshot.power.supported;
            status.on_ac_power = snapshot.power.on_ac_power;
            status.battery_percent = snapshot.power.battery_percent;
            status.battery_saver = snapshot.power.battery_saver;
            status.thermal_state =
                format!("{:?}", snapshot.power.thermal_state).to_ascii_lowercase();
            status.temperature_c = snapshot.power.temperature_c;
            status.intelligence = intelligence_runtime_status(&intelligent.report);
            status.safety = safety.status();
            status.cache = cache_status;
            runtime_session.write_status(&status, false)?;
            if let Some(metrics) = &metrics {
                metrics.update(&status);
            }

            if config.once {
                break;
            }
            pending_command = sleep_interruptible_with_control(
                Duration::from_secs(if paused {
                    1
                } else {
                    automatic
                        .interval_secs
                        .saturating_mul(overhead_plan.interval_multiplier)
                }),
                &running,
                &runtime_session,
            )?;
        }
        Ok(())
    })();

    let mut final_result = loop_result;
    telemetry.refresh_process_identities();
    let cleanup = tracker.reset_all(|pid, start_time| telemetry.is_same_process(pid, start_time));
    if !cleanup.is_empty() {
        println!(
            "\nrestaurando {} cambio(s) antes de salir...",
            cleanup.len()
        );
        let report = enforcer.apply(&cleanup);
        telemetry.refresh_process_identities();
        tracker.commit(&report.succeeded, |pid, start_time| {
            telemetry.is_same_process(pid, start_time)
        });
        if let Some(error) = report.failure_summary() {
            eprintln!("aviso: no se pudo restaurar todo: {error}");
            status.message = Some(format!("cierre con restauración incompleta: {error}"));
            if final_result.is_ok() {
                final_result = Err(anyhow::anyhow!(
                    "cierre con restauración incompleta; el journal se conservará para el próximo arranque: {error}"
                ));
            }
        }
    }
    // Reconciliación final independiente del tracker. Cubre una acción que
    // alcanzó el SO pero no pudo confirmarse en memoria o cuyo rollback falló.
    // Solo se marca cierre limpio cuando tampoco quedan entradas durables.
    let residual_recovery = enforcer.recover();
    if residual_recovery.restored > 0
        || residual_recovery.discarded > 0
        || residual_recovery.has_failures()
    {
        println!(
            "sysopt — reconciliación final: {}",
            residual_recovery.summary()
        );
    }
    if residual_recovery.has_failures() && final_result.is_ok() {
        final_result = Err(anyhow::anyhow!(
            "reconciliación final incompleta; el journal se conservará: {}",
            residual_recovery.failures.join("; ")
        ));
    }

    if !tracker.is_empty() && final_result.is_ok() {
        final_result = Err(anyhow::anyhow!(
            "quedaron {} cambio(s) administrados sin restaurar; el cierre no se marcará como limpio",
            tracker.len()
        ));
    }
    if let Err(error) = intelligence.flush() {
        eprintln!("aviso: no se pudo guardar el aprendizaje de la IA híbrida: {error}");
    }
    status.managed_changes = tracker.len();
    status.paused = paused;
    match &final_result {
        Ok(()) => runtime_session.finish(status)?,
        Err(error) => {
            status.running = false;
            status.message = Some(format!("cierre por error: {error}"));
            let _ = runtime_session.write_status(&status, true);
            // No se marca como cierre limpio: el supervisor podrá activar
            // modo seguro y el journal será recuperado en el próximo arranque.
        }
    }
    println!("listo, saliendo.");
    final_result
}

fn apply_cache_runtime_profile(cache: &mut SmartCache, profile: PerformanceProfile) -> Result<()> {
    match profile {
        PerformanceProfile::Smart | PerformanceProfile::Balanced => {
            cache.update_operating_limits(60, 256 * 1024 * 1024, 25, 55.0)
        }
        PerformanceProfile::Eco | PerformanceProfile::Quiet => {
            cache.update_operating_limits(120, 64 * 1024 * 1024, 35, 35.0)
        }
        PerformanceProfile::Performance => {
            cache.update_operating_limits(30, 512 * 1024 * 1024, 15, 75.0)
        }
        PerformanceProfile::Gaming => {
            cache.update_operating_limits(90, 192 * 1024 * 1024, 22, 45.0)
        }
        PerformanceProfile::Development => {
            cache.update_operating_limits(35, 384 * 1024 * 1024, 18, 70.0)
        }
        PerformanceProfile::Creator => {
            cache.update_operating_limits(45, 384 * 1024 * 1024, 20, 65.0)
        }
        PerformanceProfile::Streaming => {
            cache.update_operating_limits(120, 96 * 1024 * 1024, 30, 40.0)
        }
    }
}

fn intelligence_runtime_status(report: &IntelligenceReport) -> IntelligenceRuntimeStatus {
    IntelligenceRuntimeStatus {
        enabled: report.enabled,
        confidence: report.confidence,
        boosted_processes: report.boosted_processes,
        demoted_processes: report.demoted_processes,
        learned_processes: report.learned_processes,
        learned_roots: report.learned_roots,
        semantic_model_loaded: report.semantic_model_loaded,
        semantic_hits: report.semantic_hits,
        semantic_downloading: report.semantic_downloading,
        semantic_retry_in_secs: report.semantic_retry_in_secs,
        semantic_model: report.semantic_model.clone(),
        semantic_error: report.semantic_error.clone(),
        summary: Some(report.summary.clone()),
    }
}

fn cache_status_from_report(report: &WarmReport) -> CacheRuntimeStatus {
    CacheRuntimeStatus {
        planned_only: report.planned_only,
        files_warmed: report.files_warmed,
        bytes_warmed: report.bytes_warmed,
        errors: report.errors,
        reason: report.reason.clone(),
    }
}

fn detect_hot_roots(
    snapshot: &SystemSnapshot,
    signatures: &SignatureDb,
    mode: SystemMode,
) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    snapshot
        .top_processes
        .iter()
        .enumerate()
        .filter(|(index, process)| {
            let categories = signatures.categorize(&process.name);
            let tool = categories.contains(&"ide")
                || categories.contains(&"build_tool")
                || categories.contains(&"language_server")
                || categories.contains(&"mobile_tooling")
                || categories.contains(&"container_vm")
                || categories.contains(&"game")
                || categories.contains(&"creative")
                || categories.contains(&"media_encoder")
                || categories.contains(&"streaming");
            tool || (*index == 0 && mode == SystemMode::Interactive)
        })
        .map(|(_, process)| process)
        .chain(snapshot.io_processes.iter().filter(|process| {
            process.read_bytes >= 512 * 1024 || process.written_bytes >= 512 * 1024
        }))
        .filter_map(|process| {
            process.cwd.clone().or_else(|| {
                process
                    .executable
                    .as_ref()
                    .and_then(|path| path.parent())
                    .map(Path::to_path_buf)
            })
        })
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn print_cache_report(report: &WarmReport, explicit: bool) {
    let prefix = if explicit {
        "SmartCache"
    } else {
        "  SmartCache"
    };
    if let Some(reason) = &report.reason {
        println!("{prefix}: en espera — {reason}");
        return;
    }
    println!(
        "{prefix}: {} {} archivo(s), {:.1} MiB de {:.1} MiB; entradas {}, candidatos {}, omitidos {}, errores {}, {} ms",
        if report.planned_only {
            "planificaría"
        } else {
            "precargó"
        },
        report.files_warmed,
        report.bytes_warmed as f64 / 1_048_576.0,
        report.budget_bytes as f64 / 1_048_576.0,
        report.entries_scanned,
        report.candidates,
        report.files_skipped,
        report.errors,
        report.elapsed_ms
    );
}

fn install_shutdown_handlers(running: &Arc<AtomicBool>) -> Result<()> {
    {
        let running = Arc::clone(running);
        ctrlc::set_handler(move || running.store(false, Ordering::SeqCst))
            .context("no se pudo instalar el handler de Ctrl+C")?;
    }
    #[cfg(unix)]
    {
        use signal_hook::consts::signal::SIGTERM;
        use signal_hook::iterator::Signals;
        let mut signals =
            Signals::new([SIGTERM]).context("no se pudo instalar el handler de SIGTERM")?;
        let running = Arc::clone(running);
        thread::spawn(move || {
            if signals.forever().next().is_some() {
                running.store(false, Ordering::SeqCst);
            }
        });
    }
    Ok(())
}

fn sleep_interruptible(total: Duration, running: &AtomicBool) {
    const STEP: Duration = Duration::from_millis(200);
    let mut elapsed = Duration::ZERO;
    while elapsed < total && running.load(Ordering::SeqCst) {
        let remaining = total - elapsed;
        thread::sleep(STEP.min(remaining));
        elapsed += STEP;
    }
}

fn sleep_interruptible_with_control(
    total: Duration,
    running: &AtomicBool,
    runtime: &RuntimeSession,
) -> Result<Option<RuntimeCommand>> {
    const STEP: Duration = Duration::from_millis(200);
    let mut elapsed = Duration::ZERO;
    while elapsed < total && running.load(Ordering::SeqCst) {
        if let Some(command) = runtime.read_command()? {
            return Ok(Some(command));
        }
        let remaining = total - elapsed;
        thread::sleep(STEP.min(remaining));
        elapsed += STEP;
    }
    Ok(None)
}
