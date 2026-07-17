use crate::config::RuntimeConfig;
use crate::safety::SafetyStatus;
use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum RuntimeCommand {
    Pause,
    Resume,
    SetProfile { profile: String },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeCommandEnvelope {
    schema_version: u32,
    target_pid: u32,
    issued_at_unix_secs: u64,
    command: RuntimeCommand,
}

const COMMAND_MAX_AGE_SECS: u64 = 120;
const COMMAND_MAX_FUTURE_SKEW_SECS: u64 = 30;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheRuntimeStatus {
    pub planned_only: bool,
    pub files_warmed: usize,
    pub bytes_warmed: u64,
    pub errors: usize,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntelligenceRuntimeStatus {
    pub enabled: bool,
    pub confidence: f32,
    pub boosted_processes: usize,
    pub demoted_processes: usize,
    pub learned_processes: usize,
    pub learned_roots: usize,
    #[serde(default)]
    pub semantic_model_loaded: bool,
    #[serde(default)]
    pub semantic_hits: usize,
    #[serde(default)]
    pub semantic_downloading: bool,
    #[serde(default)]
    pub semantic_retry_in_secs: Option<u64>,
    #[serde(default)]
    pub semantic_model: Option<String>,
    #[serde(default)]
    pub semantic_error: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub schema_version: u32,
    pub version: String,
    pub pid: u32,
    pub running: bool,
    pub paused: bool,
    pub safe_mode: bool,
    pub apply: bool,
    pub profile: String,
    pub observed_mode: String,
    pub effective_mode: String,
    pub classifier_source: String,
    pub confidence: f32,
    pub global_cpu_percent: f32,
    #[serde(default)]
    pub pressure_supported: bool,
    #[serde(default)]
    pub cpu_some_pressure_avg10: f32,
    #[serde(default)]
    pub memory_some_pressure_avg10: f32,
    #[serde(default)]
    pub memory_full_pressure_avg10: f32,
    #[serde(default)]
    pub io_some_pressure_avg10: f32,
    #[serde(default)]
    pub io_full_pressure_avg10: f32,
    pub available_memory_bytes: u64,
    pub total_memory_bytes: u64,
    pub process_count: usize,
    pub managed_changes: usize,
    #[serde(default)]
    pub actions_planned: usize,
    #[serde(default)]
    pub actions_applied: usize,
    #[serde(default)]
    pub action_failures: usize,
    #[serde(default)]
    pub cycle_duration_ms: u64,
    #[serde(default)]
    pub overhead_level: u8,
    #[serde(default)]
    pub overhead_p50_ms: u64,
    #[serde(default)]
    pub overhead_p95_ms: u64,
    #[serde(default)]
    pub power_supported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_ac_power: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_percent: Option<u8>,
    #[serde(default)]
    pub battery_saver: bool,
    #[serde(default)]
    pub thermal_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f32>,
    #[serde(default)]
    pub intelligence: IntelligenceRuntimeStatus,
    #[serde(default)]
    pub safety: SafetyStatus,
    pub cache: CacheRuntimeStatus,
    pub message: Option<String>,
    pub updated_at_unix_secs: u64,
}

impl Default for RuntimeStatus {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            running: true,
            paused: false,
            safe_mode: false,
            apply: false,
            profile: "smart".into(),
            observed_mode: "unknown".into(),
            effective_mode: "unknown".into(),
            classifier_source: "unknown".into(),
            confidence: 0.0,
            global_cpu_percent: 0.0,
            pressure_supported: false,
            cpu_some_pressure_avg10: 0.0,
            memory_some_pressure_avg10: 0.0,
            memory_full_pressure_avg10: 0.0,
            io_some_pressure_avg10: 0.0,
            io_full_pressure_avg10: 0.0,
            available_memory_bytes: 0,
            total_memory_bytes: 0,
            process_count: 0,
            managed_changes: 0,
            actions_planned: 0,
            actions_applied: 0,
            action_failures: 0,
            cycle_duration_ms: 0,
            overhead_level: 0,
            overhead_p50_ms: 0,
            overhead_p95_ms: 0,
            power_supported: false,
            on_ac_power: None,
            battery_percent: None,
            battery_saver: false,
            thermal_state: "unknown".into(),
            temperature_c: None,
            intelligence: IntelligenceRuntimeStatus::default(),
            safety: SafetyStatus::default(),
            cache: CacheRuntimeStatus::default(),
            message: None,
            updated_at_unix_secs: unix_now_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HealthState {
    schema_version: u32,
    consecutive_unclean_starts: u8,
    last_start_unix_secs: u64,
    clean_shutdown: bool,
}

impl Default for HealthState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            consecutive_unclean_starts: 0,
            last_start_unix_secs: 0,
            clean_shutdown: true,
        }
    }
}

pub struct RuntimeSession {
    config: RuntimeConfig,
    lock_file: Option<File>,
    last_status_write: Option<Instant>,
    safe_mode: bool,
    finished: bool,
}

impl RuntimeSession {
    pub fn start(config: RuntimeConfig) -> Result<Self> {
        config.validate()?;
        if !config.enabled {
            return Ok(Self {
                config,
                lock_file: None,
                last_status_write: None,
                safe_mode: false,
                finished: false,
            });
        }

        ensure_parent(&config.lock_path)?;
        ensure_parent(&config.status_path)?;
        ensure_parent(&config.control_path)?;
        ensure_parent(&config.health_path)?;

        let mut lock_file = open_private_file(&config.lock_path, true, false)
            .with_context(|| format!("no se pudo abrir {}", config.lock_path.display()))?;
        FileExt::try_lock_exclusive(&lock_file).with_context(|| {
            format!(
                "ya hay otra instancia de sysopt ejecutándose (lock: {})",
                config.lock_path.display()
            )
        })?;
        lock_file.set_len(0)?;
        lock_file.seek(SeekFrom::Start(0))?;
        writeln!(lock_file, "{}", std::process::id())?;
        lock_file.flush()?;

        let now = unix_now_secs();
        let health_exists = match fs::symlink_metadata(&config.health_path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            // PermissionDenied/errores de E/S deben activar la ruta no
            // confiable, no simular un primer arranque limpio.
            Err(_) => true,
        };
        let (mut health, health_untrusted) = if !health_exists {
            (HealthState::default(), false)
        } else {
            match read_json::<HealthState>(&config.health_path) {
                Ok(health) if health.schema_version == SCHEMA_VERSION => (health, false),
                Ok(_) => {
                    eprintln!(
                        "advertencia: estado de salud con esquema incompatible en {}; se activa modo seguro",
                        config.health_path.display()
                    );
                    (HealthState::default(), true)
                }
                Err(error) => {
                    eprintln!(
                        "advertencia: estado de salud no confiable en {}: {error:#}; se activa modo seguro",
                        config.health_path.display()
                    );
                    (HealthState::default(), true)
                }
            }
        };
        let previous_was_recent =
            now.saturating_sub(health.last_start_unix_secs) <= config.crash_window_secs;
        if !health.clean_shutdown && previous_was_recent {
            health.consecutive_unclean_starts = health.consecutive_unclean_starts.saturating_add(1);
        } else if health.clean_shutdown || !previous_was_recent {
            health.consecutive_unclean_starts = 0;
        }
        health.last_start_unix_secs = now;
        health.clean_shutdown = false;
        write_json_atomic(&config.health_path, &health)?;
        let safe_mode = health_untrusted
            || health.consecutive_unclean_starts >= config.safe_mode_crash_threshold;

        Ok(Self {
            config,
            lock_file: Some(lock_file),
            last_status_write: None,
            safe_mode,
            finished: false,
        })
    }

    pub fn safe_mode(&self) -> bool {
        self.safe_mode
    }

    pub fn read_command(&self) -> Result<Option<RuntimeCommand>> {
        if !self.config.enabled {
            return Ok(None);
        }
        let lock = open_sidecar_lock(&self.config.control_path)?;
        FileExt::lock_exclusive(&lock).with_context(|| {
            format!("no se pudo bloquear {}", self.config.control_path.display())
        })?;
        let result = (|| -> Result<Option<RuntimeCommand>> {
            if !self.config.control_path.exists() {
                return Ok(None);
            }
            let envelope_result =
                read_json_unlocked::<RuntimeCommandEnvelope>(&self.config.control_path);
            fs::remove_file(&self.config.control_path).with_context(|| {
                format!("no se pudo consumir {}", self.config.control_path.display())
            })?;
            let envelope = match envelope_result {
                Ok(envelope) => envelope,
                Err(error) => {
                    eprintln!(
                        "advertencia: comando de control inválido descartado en {}: {error:#}",
                        self.config.control_path.display()
                    );
                    return Ok(None);
                }
            };
            let now = unix_now_secs();
            let stale = envelope.issued_at_unix_secs
                > now.saturating_add(COMMAND_MAX_FUTURE_SKEW_SECS)
                || now.saturating_sub(envelope.issued_at_unix_secs) > COMMAND_MAX_AGE_SECS;
            if envelope.schema_version != SCHEMA_VERSION
                || envelope.target_pid != std::process::id()
                || stale
            {
                return Ok(None);
            }
            Ok(Some(envelope.command))
        })();
        FileExt::unlock(&lock).with_context(|| {
            format!("no se pudo liberar {}", self.config.control_path.display())
        })?;
        result
    }

    pub fn write_status(&mut self, status: &RuntimeStatus, force: bool) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let now = Instant::now();
        if !force
            && self.last_status_write.is_some_and(|last| {
                now.duration_since(last) < Duration::from_secs(self.config.status_interval_secs)
            })
        {
            return Ok(());
        }
        let mut status = status.clone();
        status.updated_at_unix_secs = unix_now_secs();
        write_json_atomic(&self.config.status_path, &status)?;
        self.last_status_write = Some(now);
        Ok(())
    }

    pub fn finish(mut self, mut status: RuntimeStatus) -> Result<()> {
        status.running = false;
        status.message = Some("cierre limpio".into());
        self.write_status(&status, true)?;
        self.mark_clean_shutdown()?;
        self.finished = true;
        if let Some(file) = self.lock_file.take() {
            let _ = FileExt::unlock(&file);
        }
        Ok(())
    }

    fn mark_clean_shutdown(&self) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let health = HealthState {
            schema_version: SCHEMA_VERSION,
            consecutive_unclean_starts: 0,
            last_start_unix_secs: unix_now_secs(),
            clean_shutdown: true,
        };
        write_json_atomic(&self.config.health_path, &health)
    }
}

impl Drop for RuntimeSession {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(file) = self.lock_file.take() {
            let _ = FileExt::unlock(&file);
        }
    }
}

pub fn send_command(config: &RuntimeConfig, command: &RuntimeCommand) -> Result<()> {
    config.validate()?;
    anyhow::ensure!(
        is_instance_running(config)?,
        "sysopt no está ejecutándose; inicia el servicio antes de enviar comandos"
    );
    let target_pid = read_lock_pid(config)?;
    let envelope = RuntimeCommandEnvelope {
        schema_version: SCHEMA_VERSION,
        target_pid,
        issued_at_unix_secs: unix_now_secs(),
        command: command.clone(),
    };
    write_json_atomic(&config.control_path, &envelope)
}

fn read_lock_pid(config: &RuntimeConfig) -> Result<u32> {
    let mut file = open_private_file(&config.lock_path, false, false)
        .with_context(|| format!("no se pudo abrir {}", config.lock_path.display()))?;
    file.seek(SeekFrom::Start(0))?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let pid = content
        .trim()
        .parse::<u32>()
        .with_context(|| format!("PID inválido en {}", config.lock_path.display()))?;
    anyhow::ensure!(pid > 0, "PID inválido en {}", config.lock_path.display());
    Ok(pid)
}

pub fn read_status(config: &RuntimeConfig) -> Result<Option<RuntimeStatus>> {
    config.validate()?;
    if !config.status_path.exists() {
        return Ok(None);
    }
    read_json(&config.status_path).map(Some)
}

pub fn is_instance_running(config: &RuntimeConfig) -> Result<bool> {
    ensure_parent(&config.lock_path)?;
    let file = open_private_file(&config.lock_path, true, false)?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let lock = open_sidecar_lock(path)?;
    FileExt::lock_shared(&lock)
        .with_context(|| format!("no se pudo bloquear {}", path.display()))?;
    let result = read_json_unlocked(path);
    FileExt::unlock(&lock).with_context(|| format!("no se pudo liberar {}", path.display()))?;
    result
}

fn read_json_unlocked<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "se rechazó una ruta JSON que no es un archivo regular: {}",
        path.display()
    );
    anyhow::ensure!(metadata.len() <= 1_048_576, "archivo JSON demasiado grande");
    let mut file = open_private_file(path, false, false)?;
    let mut content = String::with_capacity(metadata.len() as usize);
    file.read_to_string(&mut content)?;
    Ok(serde_json::from_str(&content)?)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    ensure_parent(path)?;
    let lock = open_sidecar_lock(path)?;
    FileExt::lock_exclusive(&lock)
        .with_context(|| format!("no se pudo bloquear {}", path.display()))?;
    let result = write_json_atomic_unlocked(path, value);
    FileExt::unlock(&lock).with_context(|| format!("no se pudo liberar {}", path.display()))?;
    result
}

fn write_json_atomic_unlocked<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink() && metadata.is_file(),
            "se rechazó reemplazar una ruta que no es un archivo regular: {}",
            path.display()
        );
    }
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let suffix = format!("tmp-{}-{unique}", std::process::id());
    let temporary = path.with_extension(suffix);
    let content = serde_json::to_vec_pretty(value)?;
    let result = (|| -> Result<()> {
        let mut file = open_private_file(&temporary, true, true)?;
        file.write_all(&content)?;
        file.flush()?;
        file.sync_all()?;
        replace_runtime_file(&temporary, path)?;
        set_private_permissions(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn replace_runtime_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source_wide = source
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
            source_wide.as_ptr(),
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
fn replace_runtime_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)
        .with_context(|| format!("no se pudo reemplazar {}", destination.display()))?;
    if let Some(parent) = destination.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("no se pudo sincronizar {}", parent.display()))?;
    }
    Ok(())
}

fn sidecar_lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "runtime".into(), |value| value.to_os_string());
    name.push(".lock");
    path.with_file_name(name)
}

fn open_sidecar_lock(path: &Path) -> Result<File> {
    ensure_parent(path)?;
    let lock_path = sidecar_lock_path(path);
    open_private_file(&lock_path, true, false)
        .with_context(|| format!("no se pudo abrir {}", lock_path.display()))
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        ensure_private_directory_tree(parent)?;
    }
    Ok(())
}

fn ensure_private_directory_tree(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata_is_safe_directory(&metadata),
                "el directorio no es un directorio regular seguro: {}",
                directory.display()
            );
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("no se pudo inspeccionar {}", directory.display()));
        }
    }

    if let Some(parent) = directory
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty() && *parent != directory)
    {
        ensure_private_directory_tree(parent)?;
    }

    match fs::create_dir(directory) {
        Ok(()) => {
            #[cfg(unix)]
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| format!("no se pudo crear {}", directory.display()));
        }
    }
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("no se pudo verificar {}", directory.display()))?;
    anyhow::ensure!(
        metadata_is_safe_directory(&metadata),
        "el directorio creado no es seguro: {}",
        directory.display()
    );
    Ok(())
}

fn metadata_is_safe_directory(metadata: &fs::Metadata) -> bool {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

fn open_private_file(path: &Path, create: bool, create_new: bool) -> Result<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.file_type().is_symlink() && metadata.is_file(),
                "se rechazó una ruta que no es un archivo regular: {}",
                path.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && (create || create_new) => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("no se pudo inspeccionar {}", path.display()));
        }
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .create_new(create_new);
    #[cfg(unix)]
    {
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    set_private_permissions(path)?;
    Ok(file)
}

fn set_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let _ = path;
    Ok(())
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(root: &Path) -> RuntimeConfig {
        RuntimeConfig {
            enabled: true,
            status_path: root.join("status.json"),
            control_path: root.join("control.json"),
            lock_path: root.join("sysopt.lock"),
            health_path: root.join("health.json"),
            enforcement_journal_path: root.join("enforcement-journal.json"),
            cgroup_control_journal_path: root.join("cgroup-control-journal.json"),
            status_interval_secs: 1,
            safe_mode_crash_threshold: 3,
            crash_window_secs: 600,
        }
    }

    #[test]
    fn comando_hace_roundtrip() {
        let root = std::env::temp_dir().join(format!("sysopt-runtime-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let config = test_config(&root);
        let session = RuntimeSession::start(config.clone()).unwrap();
        write_json_atomic(
            &config.control_path,
            &RuntimeCommandEnvelope {
                schema_version: SCHEMA_VERSION,
                target_pid: std::process::id(),
                issued_at_unix_secs: unix_now_secs(),
                command: RuntimeCommand::SetProfile {
                    profile: "eco".into(),
                },
            },
        )
        .unwrap();
        assert!(matches!(
            session.read_command().unwrap(),
            Some(RuntimeCommand::SetProfile { profile }) if profile == "eco"
        ));
        drop(session);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn comando_viejo_o_de_otro_proceso_se_descarta() {
        let root =
            std::env::temp_dir().join(format!("sysopt-runtime-stale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let config = test_config(&root);
        let session = RuntimeSession::start(config.clone()).unwrap();
        write_json_atomic(
            &config.control_path,
            &RuntimeCommandEnvelope {
                schema_version: SCHEMA_VERSION,
                target_pid: std::process::id().saturating_add(1),
                issued_at_unix_secs: unix_now_secs().saturating_sub(COMMAND_MAX_AGE_SECS + 1),
                command: RuntimeCommand::Shutdown,
            },
        )
        .unwrap();
        assert!(session.read_command().unwrap().is_none());
        drop(session);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn comando_malformado_se_consume_sin_interrumpir_el_servicio() {
        let root =
            std::env::temp_dir().join(format!("sysopt-runtime-malformed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let config = test_config(&root);
        let session = RuntimeSession::start(config.clone()).unwrap();
        ensure_parent(&config.control_path).unwrap();
        fs::write(&config.control_path, b"{esto-no-es-json").unwrap();
        assert!(session.read_command().unwrap().is_none());
        assert!(!config.control_path.exists());
        drop(session);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn status_default_es_serializable() {
        let encoded = serde_json::to_string(&RuntimeStatus::default()).unwrap();
        let decoded: RuntimeStatus = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn estado_de_salud_corrupto_fuerza_modo_seguro() {
        let root = std::env::temp_dir().join(format!(
            "sysopt-runtime-health-corrupt-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let config = test_config(&root);
        ensure_parent(&config.health_path).unwrap();
        fs::write(&config.health_path, b"{estado-invalido").unwrap();
        let session = RuntimeSession::start(config).unwrap();
        assert!(session.safe_mode());
        drop(session);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn directorio_intermedio_symlink_es_rechazado() {
        use std::os::unix::fs::symlink;

        let root =
            std::env::temp_dir().join(format!("sysopt-runtime-symlink-{}", std::process::id()));
        let outside = root.with_extension("outside");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("indirecto")).unwrap();
        let target = root.join("indirecto/estado/status.json");
        assert!(ensure_parent(&target).is_err());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }
}
