use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const OVERHEAD_STATE_SCHEMA_VERSION: u32 = 1;
const MAX_OVERHEAD_STATE_BYTES: u64 = 1024 * 1024;
const MAX_SAMPLE_MS: u64 = 24 * 60 * 60 * 1000;
const PERSIST_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OverheadConfig {
    pub enabled: bool,
    pub state_path: PathBuf,
    pub window_size: usize,
    pub p95_budget_ms: u64,
    pub recovery_cycles: u16,
    pub max_level: u8,
}

impl Default for OverheadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            state_path: PathBuf::from("./overhead-state.json"),
            window_size: 120,
            p95_budget_ms: 120,
            recovery_cycles: 20,
            max_level: 4,
        }
    }
}

impl OverheadConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (20..=10_000).contains(&self.window_size),
            "overhead.window_size debe estar entre 20 y 10000"
        );
        anyhow::ensure!(
            (5..=60_000).contains(&self.p95_budget_ms),
            "overhead.p95_budget_ms debe estar entre 5 y 60000"
        );
        anyhow::ensure!(
            (1..=10_000).contains(&self.recovery_cycles),
            "overhead.recovery_cycles debe estar entre 1 y 10000"
        );
        anyhow::ensure!(
            self.max_level <= 4,
            "overhead.max_level debe estar entre 0 y 4"
        );
        anyhow::ensure!(
            !self.state_path.as_os_str().is_empty(),
            "overhead.state_path no puede estar vacío"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct OverheadPlan {
    pub level: u8,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub top_n_divisor: usize,
    pub interval_multiplier: u64,
    pub allow_cache: bool,
    pub allow_semantic: bool,
}

impl OverheadPlan {
    fn for_level(level: u8, p50_ms: u64, p95_ms: u64) -> Self {
        let (top_n_divisor, interval_multiplier, allow_cache, allow_semantic) = match level {
            0 => (1, 1, true, true),
            1 => (2, 1, true, true),
            2 => (2, 2, false, true),
            3 => (4, 3, false, false),
            _ => (8, 5, false, false),
        };
        Self {
            level,
            p50_ms,
            p95_ms,
            top_n_divisor,
            interval_multiplier,
            allow_cache,
            allow_semantic,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistentState {
    schema_version: u32,
    samples_ms: VecDeque<u64>,
    level: u8,
    healthy_cycles: u16,
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            schema_version: OVERHEAD_STATE_SCHEMA_VERSION,
            samples_ms: VecDeque::new(),
            level: 0,
            healthy_cycles: 0,
        }
    }
}

pub struct OverheadController {
    config: OverheadConfig,
    state: PersistentState,
    last_persist: Option<Instant>,
}

impl OverheadController {
    pub fn new(config: OverheadConfig) -> Result<Self> {
        config.validate()?;
        let state = match load_state(&config.state_path, config.window_size, config.max_level) {
            Ok(state) => state,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                PersistentState::default()
            }
            Err(error) => {
                eprintln!(
                    "aviso: se ignoró un estado de overhead inválido en {}: {error:#}",
                    config.state_path.display()
                );
                PersistentState::default()
            }
        };
        Ok(Self {
            config,
            state,
            last_persist: None,
        })
    }

    pub fn current_plan(&self) -> OverheadPlan {
        let (p50, p95) = percentiles(&self.state.samples_ms);
        OverheadPlan::for_level(self.state.level, p50, p95)
    }

    pub fn observe(&mut self, duration_ms: u64) -> OverheadPlan {
        if !self.config.enabled {
            return OverheadPlan::for_level(0, duration_ms, duration_ms);
        }
        self.state.samples_ms.push_back(duration_ms);
        while self.state.samples_ms.len() > self.config.window_size {
            self.state.samples_ms.pop_front();
        }
        let (p50, p95) = percentiles(&self.state.samples_ms);
        let budget = self.config.p95_budget_ms;
        let requested_level = if p95 >= budget.saturating_mul(4) {
            4
        } else if p95 >= budget.saturating_mul(3) {
            3
        } else if p95 >= budget.saturating_mul(2) {
            2
        } else if p95 > budget {
            1
        } else {
            0
        };
        let requested_level = requested_level.min(self.config.max_level);
        let previous_level = self.state.level;
        if requested_level > self.state.level {
            self.state.level = requested_level;
            self.state.healthy_cycles = 0;
        } else if requested_level < self.state.level {
            self.state.healthy_cycles = self.state.healthy_cycles.saturating_add(1);
            if self.state.healthy_cycles >= self.config.recovery_cycles {
                self.state.level = self.state.level.saturating_sub(1);
                self.state.healthy_cycles = 0;
            }
        } else {
            self.state.healthy_cycles = 0;
        }
        let should_persist = self.state.level != previous_level
            || self
                .last_persist
                .is_none_or(|instant| instant.elapsed() >= PERSIST_INTERVAL);
        if should_persist {
            if let Err(error) = save_state(&self.config.state_path, &self.state) {
                eprintln!("aviso: no se pudo persistir presupuesto de overhead: {error:#}");
            } else {
                self.last_persist = Some(Instant::now());
            }
        }
        OverheadPlan::for_level(self.state.level, p50, p95)
    }
}

fn percentiles(samples: &VecDeque<u64>) -> (u64, u64) {
    if samples.is_empty() {
        return (0, 0);
    }
    let mut sorted = samples.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    let at = |quantile: f64| -> u64 {
        let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
        sorted[index.min(sorted.len() - 1)]
    };
    (at(0.50), at(0.95))
}

fn load_state(path: &Path, window_size: usize, max_level: u8) -> Result<PersistentState> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "estado de overhead inseguro: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_OVERHEAD_STATE_BYTES,
        "estado de overhead excede {} bytes",
        MAX_OVERHEAD_STATE_BYTES
    );
    let mut state: PersistentState = serde_json::from_slice(&fs::read(path)?)?;
    anyhow::ensure!(
        state.schema_version == OVERHEAD_STATE_SCHEMA_VERSION,
        "versión de estado de overhead incompatible"
    );
    anyhow::ensure!(state.level <= max_level, "nivel de overhead fuera de rango");
    anyhow::ensure!(
        state
            .samples_ms
            .iter()
            .all(|sample| *sample <= MAX_SAMPLE_MS),
        "estado de overhead contiene una duración fuera de rango"
    );
    while state.samples_ms.len() > window_size {
        state.samples_ms.pop_front();
    }
    Ok(state)
}

fn save_state(path: &Path, state: &PersistentState) -> Result<()> {
    prepare_private_parent(path)?;
    validate_optional_regular_file(path)?;
    let bytes = serde_json::to_vec_pretty(state)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_OVERHEAD_STATE_BYTES,
        "estado de overhead excede el límite"
    );
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
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
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        replace_state_file(&temp, path)?;
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn prepare_private_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent().filter(|value| !value.as_os_str().is_empty()) else {
        return Ok(());
    };
    ensure_private_directory_tree(parent)
}

fn ensure_private_directory_tree(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata_is_safe_directory(&metadata),
                "directorio de overhead inseguro: {}",
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
        "directorio de overhead creado de forma insegura: {}",
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

fn validate_optional_regular_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "se rechazó reemplazar un estado indirecto: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(windows)]
fn replace_state_file(source: &Path, destination: &Path) -> Result<()> {
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
fn replace_state_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)
        .with_context(|| format!("no se pudo reemplazar {}", destination.display()))?;
    if let Some(parent) = destination.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("no se pudo sincronizar {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn degrada_y_recupera_gradualmente() {
        let path =
            std::env::temp_dir().join(format!("sysopt-overhead-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut controller = OverheadController::new(OverheadConfig {
            state_path: path.clone(),
            window_size: 20,
            p95_budget_ms: 10,
            recovery_cycles: 2,
            ..OverheadConfig::default()
        })
        .unwrap();
        for _ in 0..20 {
            controller.observe(50);
        }
        assert!(controller.current_plan().level >= 3);
        for _ in 0..80 {
            controller.observe(1);
        }
        assert!(controller.current_plan().level < 3);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn descarta_estado_sobredimensionado_o_con_muestras_invalidas() {
        let path = std::env::temp_dir().join(format!(
            "sysopt-overhead-invalid-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            format!(
                r#"{{"schema_version":1,"samples_ms":[{}],"level":0,"healthy_cycles":0}}"#,
                MAX_SAMPLE_MS + 1
            ),
        )
        .unwrap();
        assert!(load_state(&path, 20, 4).is_err());
        let _ = fs::remove_file(path);
    }
}
