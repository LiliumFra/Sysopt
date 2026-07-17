use crate::automation::PerformanceProfile;
use crate::config::AppConfig;
use anyhow::{Context, Result};
use fs2::FileExt;
use policy::{Action, Classifier, PolicyEngine, SignatureDb, SystemMode};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use telemetry::{PressureSnapshot, ProcessSample, Telemetry};

#[derive(Debug, Clone, Serialize)]
pub struct AnalysisReport {
    pub schema_version: u32,
    pub sysopt_version: String,
    pub generated_at_unix_secs: u64,
    pub apply_configured: bool,
    pub observed_mode: String,
    pub classifier_source: String,
    pub confidence: f32,
    pub recommended_profile: String,
    pub recommendation_reason: String,
    pub system: AnalysisSystem,
    pub top_processes: Vec<AnalysisProcess>,
    pub planned_actions: Vec<Action>,
    pub cache: CacheAnalysis,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnalysisSystem {
    pub global_cpu_percent: f32,
    pub available_memory_bytes: u64,
    pub total_memory_bytes: u64,
    pub process_count: usize,
    pub pressure: PressureSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnalysisProcess {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheAnalysis {
    pub enabled: bool,
    pub eligible_now: bool,
    pub reason: String,
    pub explicit_roots: usize,
    pub automatic_roots: bool,
    pub configured_budget_bytes: u64,
}

pub fn collect_analysis(
    config: &AppConfig,
    signatures: &SignatureDb,
    classifier: &Classifier,
) -> Result<AnalysisReport> {
    let mut telemetry = Telemetry::new();
    let _ = telemetry.snapshot(config.top_n);
    std::thread::sleep(Duration::from_millis(1_000));
    let snapshot = telemetry.snapshot(config.top_n);
    let prediction = classifier.predict(&snapshot, signatures)?;
    let (recommended_profile, recommendation_reason) = recommend_profile(
        prediction.mode,
        snapshot.pressure,
        snapshot.total_memory_bytes,
    );
    let planned_actions = PolicyEngine::decide_with_config(
        prediction.mode,
        &snapshot,
        signatures,
        &config.resource_policy(),
    );
    let cache = analyze_cache(
        config,
        snapshot.pressure,
        snapshot.global_cpu_percent,
        snapshot.available_memory_bytes,
        snapshot.total_memory_bytes,
    );
    let mut warnings = Vec::new();
    if !snapshot.pressure.supported {
        warnings.push(
            "PSI no está disponible en esta plataforma; se usarán CPU y memoria como señales de respaldo"
                .into(),
        );
    }
    if config.apply {
        warnings.push(
            "apply=true está configurado; este análisis no aplica cambios, pero el servicio normal sí"
                .into(),
        );
    }
    if config.resources.enabled && !cfg!(target_os = "linux") {
        warnings.push("resources.enabled solo puede aplicarse en Linux cgroup v2".into());
    }
    let top_processes = snapshot
        .top_processes
        .iter()
        .take(config.top_n.min(32))
        .map(sanitized_process)
        .collect();

    Ok(AnalysisReport {
        schema_version: 1,
        sysopt_version: env!("CARGO_PKG_VERSION").to_owned(),
        generated_at_unix_secs: unix_now_secs(),
        apply_configured: config.apply,
        observed_mode: prediction.mode.to_string(),
        classifier_source: format!("{:?}", prediction.source),
        confidence: prediction.confidence,
        recommended_profile: recommended_profile.as_str().to_owned(),
        recommendation_reason,
        system: AnalysisSystem {
            global_cpu_percent: snapshot.global_cpu_percent,
            available_memory_bytes: snapshot.available_memory_bytes,
            total_memory_bytes: snapshot.total_memory_bytes,
            process_count: snapshot.process_count,
            pressure: snapshot.pressure,
        },
        top_processes,
        planned_actions,
        cache,
        warnings,
    })
}

pub fn print_analysis(report: &AnalysisReport) {
    println!("sysopt {} — análisis no invasivo", report.sysopt_version);
    println!(
        "  modo detectado: {} ({}, confianza {:.0}%)",
        report.observed_mode,
        report.classifier_source,
        report.confidence * 100.0
    );
    println!(
        "  perfil recomendado: {} — {}",
        report.recommended_profile, report.recommendation_reason
    );
    println!(
        "  CPU {:.1}% | RAM libre {:.0} MiB de {:.0} MiB | procesos {}",
        report.system.global_cpu_percent,
        report.system.available_memory_bytes as f64 / 1_048_576.0,
        report.system.total_memory_bytes as f64 / 1_048_576.0,
        report.system.process_count
    );
    if report.system.pressure.supported {
        println!(
            "  PSI avg10: CPU some {:.2}% | memoria some/full {:.2}/{:.2}% | E/S some/full {:.2}/{:.2}%",
            report.system.pressure.cpu_some_avg10,
            report.system.pressure.memory_some_avg10,
            report.system.pressure.memory_full_avg10,
            report.system.pressure.io_some_avg10,
            report.system.pressure.io_full_avg10
        );
    } else {
        println!("  PSI: no disponible; se mantienen señales de respaldo");
    }
    println!(
        "  acciones deterministas planificadas: {} | SmartCache: {} ({})",
        report.planned_actions.len(),
        if report.cache.eligible_now {
            "elegible"
        } else {
            "en espera"
        },
        report.cache.reason
    );
    for process in report.top_processes.iter().take(8) {
        println!(
            "    PID {} | {:>5.1}% CPU | {:>7.0} MiB | {}",
            process.pid,
            process.cpu_percent,
            process.memory_bytes as f64 / 1_048_576.0,
            process.name
        );
    }
    for warning in &report.warnings {
        println!("  aviso: {warning}");
    }
}

pub fn write_analysis_report(path: &Path, report: &AnalysisReport) -> Result<()> {
    prepare_private_parent(path)?;
    validate_optional_regular_file(path)?;
    let lock_path = sidecar_lock_path(path);
    let lock = open_private_lock(&lock_path)
        .with_context(|| format!("no se pudo abrir {}", lock_path.display()))?;
    FileExt::lock_exclusive(&lock)
        .with_context(|| format!("no se pudo bloquear {}", path.display()))?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temporary = path.with_extension(format!("tmp-{}-{unique}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("no se pudo crear {}", temporary.display()))?;
        serde_json::to_writer_pretty(&mut file, report)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        replace_file(&temporary, path)?;
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    FileExt::unlock(&lock)
        .with_context(|| format!("no se pudo liberar {}", lock_path.display()))?;
    result
}

fn sanitized_process(process: &ProcessSample) -> AnalysisProcess {
    AnalysisProcess {
        pid: process.pid,
        name: process.name.clone(),
        cpu_percent: process.cpu_percent,
        memory_bytes: process.memory_bytes,
        read_bytes: process.read_bytes,
        written_bytes: process.written_bytes,
    }
}

fn recommend_profile(
    mode: SystemMode,
    pressure: PressureSnapshot,
    total_memory_bytes: u64,
) -> (PerformanceProfile, String) {
    if pressure.supported && (pressure.memory_full_avg10 >= 1.0 || pressure.io_full_avg10 >= 8.0) {
        return (
            PerformanceProfile::Eco,
            "la presión sostenida aconseja reducir precarga y frecuencia de intervención".into(),
        );
    }
    match mode {
        SystemMode::Gaming => (
            PerformanceProfile::Gaming,
            "se detectó una carga de juego reconocida".into(),
        ),
        SystemMode::Developing | SystemMode::MobileDevelopment => (
            PerformanceProfile::Development,
            "se detectaron herramientas de desarrollo o compilación".into(),
        ),
        SystemMode::Creative => (
            PerformanceProfile::Creator,
            "se detectó una carga creativa o de codificación multimedia".into(),
        ),
        SystemMode::Streaming => (
            PerformanceProfile::Streaming,
            "se detectó una carga de transmisión en vivo".into(),
        ),
        SystemMode::HeavyForeground => (
            PerformanceProfile::Performance,
            "hay una aplicación de primer plano con carga elevada".into(),
        ),
        SystemMode::Idle if total_memory_bytes < 6 * 1024 * 1024 * 1024 => (
            PerformanceProfile::Eco,
            "el equipo tiene memoria limitada y está ocioso".into(),
        ),
        _ => (
            PerformanceProfile::Smart,
            "el modo adaptativo ofrece el mejor equilibrio para la carga observada".into(),
        ),
    }
}

fn analyze_cache(
    config: &AppConfig,
    pressure: PressureSnapshot,
    cpu_percent: f32,
    available_memory: u64,
    total_memory: u64,
) -> CacheAnalysis {
    let available_percent = if total_memory == 0 {
        0.0
    } else {
        available_memory as f32 * 100.0 / total_memory as f32
    };
    let reason = if !config.cache.enabled {
        "desactivado por configuración".into()
    } else if cpu_percent > config.cache.max_cpu_percent {
        format!(
            "CPU {:.1}% supera {:.1}%",
            cpu_percent, config.cache.max_cpu_percent
        )
    } else if available_percent < f32::from(config.cache.min_available_memory_percent) {
        format!(
            "RAM disponible {:.1}% está por debajo de {}%",
            available_percent, config.cache.min_available_memory_percent
        )
    } else if config.cache.pressure_guard_enabled
        && pressure.supported
        && pressure.memory_some_avg10 > config.cache.max_memory_some_pressure_avg10
    {
        format!(
            "PSI memoria some {:.2}% supera {:.2}%",
            pressure.memory_some_avg10, config.cache.max_memory_some_pressure_avg10
        )
    } else if config.cache.pressure_guard_enabled
        && pressure.supported
        && pressure.io_some_avg10 > config.cache.max_io_some_pressure_avg10
    {
        format!(
            "PSI E/S some {:.2}% supera {:.2}%",
            pressure.io_some_avg10, config.cache.max_io_some_pressure_avg10
        )
    } else {
        "condiciones de CPU, memoria y presión dentro de límites".into()
    };
    let eligible_now = config.cache.enabled
        && cpu_percent <= config.cache.max_cpu_percent
        && available_percent >= f32::from(config.cache.min_available_memory_percent)
        && (!config.cache.pressure_guard_enabled
            || !pressure.supported
            || (pressure.memory_some_avg10 <= config.cache.max_memory_some_pressure_avg10
                && pressure.io_some_avg10 <= config.cache.max_io_some_pressure_avg10));
    CacheAnalysis {
        enabled: config.cache.enabled,
        eligible_now,
        reason,
        explicit_roots: config.cache.roots.len(),
        automatic_roots: config.cache.auto_process_roots,
        configured_budget_bytes: config.cache.bytes_per_cycle,
    }
}

fn sidecar_lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "analysis".into(), |value| value.to_os_string());
    name.push(".lock");
    path.with_file_name(name)
}

fn validate_optional_regular_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink() && metadata.is_file(),
            "se rechazó un destino no regular o enlace simbólico: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("no se pudo inspeccionar {}", path.display()));
        }
    }
    Ok(())
}

fn prepare_private_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        match fs::symlink_metadata(parent) {
            Ok(metadata) => anyhow::ensure!(
                !metadata.file_type().is_symlink() && metadata.is_dir(),
                "el directorio padre no es seguro: {}",
                parent.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(parent)
                    .with_context(|| format!("no se pudo crear {}", parent.display()))?;
                #[cfg(unix)]
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("no se pudo inspeccionar {}", parent.display()));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_private_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_private_lock(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_private_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("no se pudo reemplazar {}", destination.display()));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)
        .with_context(|| format!("no se pudo reemplazar {}", destination.display()))?;
    if let Some(parent) = destination.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("no se pudo sincronizar {}", parent.display()))?;
    }
    Ok(())
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recomienda_eco_ante_presion_sostenida() {
        let (profile, reason) = recommend_profile(
            SystemMode::Gaming,
            PressureSnapshot {
                supported: true,
                memory_full_avg10: 2.0,
                ..PressureSnapshot::default()
            },
            16 * 1024 * 1024 * 1024,
        );
        assert_eq!(profile, PerformanceProfile::Eco);
        assert!(reason.contains("presión"));
    }

    #[test]
    fn cache_no_es_elegible_con_psi_alto() {
        let config = AppConfig::default();
        let report = analyze_cache(
            &config,
            PressureSnapshot {
                supported: true,
                io_some_avg10: config.cache.max_io_some_pressure_avg10 + 1.0,
                ..PressureSnapshot::default()
            },
            5.0,
            8 * 1024 * 1024 * 1024,
            16 * 1024 * 1024 * 1024,
        );
        assert!(!report.eligible_now);
        assert!(report.reason.contains("PSI E/S"));
    }
}
