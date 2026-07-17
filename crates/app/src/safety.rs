use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SAFETY_STATE_SCHEMA_VERSION: u32 = 1;
const MAX_SAFETY_STATE_BYTES: u64 = 64 * 1024;
const MIN_ADAPTIVE_PSI_OBSERVATIONS: u64 = 5;
use telemetry::{PowerSnapshot, PressureSnapshot, ThermalState};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SafetyConfig {
    /// Activa guardarraíles dinámicos además del modo seguro por cierres anómalos.
    pub enabled: bool,
    /// Ciclos consecutivos con una proporción elevada de fallos antes de abrir
    /// el disyuntor de enforcement.
    pub max_consecutive_failure_cycles: u8,
    /// Proporción mínima de acciones fallidas en un ciclo para contabilizarlo
    /// como fallo del backend.
    pub max_failure_ratio: f32,
    /// Tiempo durante el cual se bloquean acciones nuevas tras abrir el disyuntor.
    pub cooldown_secs: u64,
    /// Restaura inmediatamente cambios administrados al abrir el disyuntor.
    pub restore_on_trip: bool,
    /// Impide acciones y precarga mientras PSI indique contención severa.
    pub pressure_guard_enabled: bool,
    pub max_cpu_some_pressure_avg10: f32,
    pub max_memory_full_pressure_avg10: f32,
    pub max_io_full_pressure_avg10: f32,
    /// Ciclos consecutivos necesarios para activar o liberar limitaciones.
    pub guard_hysteresis_cycles: u8,
    /// Limita trabajo secundario al entrar en ahorro de batería.
    pub battery_guard_enabled: bool,
    /// Porcentaje de batería bajo el cual se limita trabajo secundario sin CA.
    pub min_battery_percent: u8,
    /// Limita acciones cuando el sistema reporta estado térmico serio/crítico.
    pub thermal_guard_enabled: bool,
    /// Autoajusta umbrales PSI con una línea base exponencial local.
    pub adaptive_psi_enabled: bool,
    pub adaptive_psi_factor: f32,
    /// Estado local de la línea base PSI aprendida.
    pub state_path: PathBuf,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_consecutive_failure_cycles: 3,
            max_failure_ratio: 0.50,
            cooldown_secs: 120,
            restore_on_trip: true,
            pressure_guard_enabled: true,
            max_cpu_some_pressure_avg10: 35.0,
            max_memory_full_pressure_avg10: 2.0,
            max_io_full_pressure_avg10: 12.0,
            guard_hysteresis_cycles: 2,
            battery_guard_enabled: true,
            min_battery_percent: 20,
            thermal_guard_enabled: true,
            adaptive_psi_enabled: true,
            adaptive_psi_factor: 3.0,
            state_path: PathBuf::from("./safety-state.json"),
        }
    }
}

impl SafetyConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=20).contains(&self.max_consecutive_failure_cycles),
            "safety.max_consecutive_failure_cycles debe estar entre 1 y 20"
        );
        anyhow::ensure!(
            self.max_failure_ratio.is_finite() && (0.05..=1.0).contains(&self.max_failure_ratio),
            "safety.max_failure_ratio debe estar entre 0.05 y 1"
        );
        anyhow::ensure!(
            (5..=86_400).contains(&self.cooldown_secs),
            "safety.cooldown_secs debe estar entre 5 y 86400"
        );
        anyhow::ensure!(
            (1..=20).contains(&self.guard_hysteresis_cycles),
            "safety.guard_hysteresis_cycles debe estar entre 1 y 20"
        );
        anyhow::ensure!(
            self.min_battery_percent <= 100,
            "safety.min_battery_percent debe estar entre 0 y 100"
        );
        anyhow::ensure!(
            self.adaptive_psi_factor.is_finite()
                && (1.0..=20.0).contains(&self.adaptive_psi_factor),
            "safety.adaptive_psi_factor debe estar entre 1 y 20"
        );
        anyhow::ensure!(
            !self.state_path.as_os_str().is_empty(),
            "safety.state_path no puede estar vacío"
        );
        for (name, value, maximum) in [
            (
                "max_cpu_some_pressure_avg10",
                self.max_cpu_some_pressure_avg10,
                100.0,
            ),
            (
                "max_memory_full_pressure_avg10",
                self.max_memory_full_pressure_avg10,
                100.0,
            ),
            (
                "max_io_full_pressure_avg10",
                self.max_io_full_pressure_avg10,
                100.0,
            ),
        ] {
            anyhow::ensure!(
                value.is_finite() && (0.0..=maximum).contains(&value),
                "safety.{name} debe estar entre 0 y {maximum}"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SafetyStatus {
    pub enabled: bool,
    pub circuit_open: bool,
    pub pressure_limited: bool,
    #[serde(default)]
    pub power_limited: bool,
    pub consecutive_failure_cycles: u8,
    pub cooldown_remaining_secs: u64,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SafetyDecision {
    pub allow_actions: bool,
    pub pressure_limited: bool,
    pub power_limited: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SafetyTransition {
    pub tripped: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedSafetyState {
    schema_version: u32,
    psi_baseline_cpu: f32,
    psi_baseline_memory: f32,
    psi_baseline_io: f32,
    observations: u64,
    #[serde(default)]
    consecutive_failure_cycles: u8,
    #[serde(default)]
    circuit_open_until_unix_secs: u64,
    #[serde(default)]
    last_reason: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

pub struct SafetyController {
    config: SafetyConfig,
    consecutive_failure_cycles: u8,
    open_until: Option<Instant>,
    last_reason: Option<String>,
    pressure_limited: bool,
    power_limited: bool,
    limited_streak: u8,
    healthy_streak: u8,
    psi_baseline_cpu: f32,
    psi_baseline_memory: f32,
    psi_baseline_io: f32,
    baseline_observations: u64,
    last_persist: Option<Instant>,
}

impl SafetyController {
    pub fn new(config: SafetyConfig) -> Result<Self> {
        config.validate()?;
        let state_exists = match fs::symlink_metadata(&config.state_path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            // Un error de permisos o E/S no equivale a “archivo ausente”. Se
            // fuerza la ruta de estado no confiable para fallar de forma segura.
            Err(_) => true,
        };
        let (persisted, state_untrusted) = if !state_exists {
            (PersistedSafetyState::default(), false)
        } else {
            match load_safety_state(&config.state_path) {
                Ok(state) => (state, false),
                Err(error) => {
                    eprintln!(
                        "aviso: estado de seguridad no confiable en {}: {error:#}; se abre el disyuntor de forma preventiva",
                        config.state_path.display()
                    );
                    (PersistedSafetyState::default(), true)
                }
            }
        };
        let now_unix = unix_now_secs();
        let persisted_remaining = persisted
            .circuit_open_until_unix_secs
            .saturating_sub(now_unix);
        let (open_until, last_reason) = if state_untrusted {
            (
                Some(Instant::now() + Duration::from_secs(config.cooldown_secs)),
                Some("estado de seguridad corrupto o no confiable".into()),
            )
        } else if persisted_remaining > 0 {
            (
                Some(Instant::now() + Duration::from_secs(persisted_remaining)),
                persisted.last_reason.clone(),
            )
        } else {
            (None, None)
        };
        let controller = Self {
            config,
            consecutive_failure_cycles: persisted.consecutive_failure_cycles,
            open_until,
            last_reason,
            pressure_limited: false,
            power_limited: false,
            limited_streak: 0,
            healthy_streak: 0,
            psi_baseline_cpu: persisted.psi_baseline_cpu,
            psi_baseline_memory: persisted.psi_baseline_memory,
            psi_baseline_io: persisted.psi_baseline_io,
            baseline_observations: persisted.observations,
            last_persist: None,
        };
        // Si el estado era ilegible, materializa inmediatamente el disyuntor
        // preventivo. Así un segundo reinicio no pierde la protección.
        if state_untrusted {
            controller.persist_state("disyuntor preventivo por estado no confiable");
        }
        Ok(controller)
    }

    pub fn preflight(
        &mut self,
        pressure: PressureSnapshot,
        power: PowerSnapshot,
    ) -> SafetyDecision {
        self.expire_cooldown();
        if !self.config.enabled {
            return SafetyDecision {
                allow_actions: true,
                pressure_limited: false,
                power_limited: false,
                reason: None,
            };
        }
        if let Some(open_until) = self.open_until {
            let remaining = open_until
                .saturating_duration_since(Instant::now())
                .as_secs();
            let reason = self.last_reason.clone().or_else(|| {
                Some(format!(
                    "disyuntor de enforcement abierto; reintento en {remaining}s"
                ))
            });
            return SafetyDecision {
                allow_actions: false,
                pressure_limited: false,
                power_limited: false,
                reason,
            };
        }

        // La muestra actual se evalúa contra la línea base *anterior*. Actualizar
        // primero permitiría que un arranque bajo presión envenenara la EMA y
        // ocultara precisamente la anomalía que debe frenar el enforcement.
        let mut pressure_reason = None;
        if self.config.pressure_guard_enabled && pressure.supported {
            let cpu_limit = self.dynamic_limit(
                self.config.max_cpu_some_pressure_avg10,
                self.psi_baseline_cpu,
            );
            let memory_limit = self.dynamic_limit(
                self.config.max_memory_full_pressure_avg10,
                self.psi_baseline_memory,
            );
            let io_limit =
                self.dynamic_limit(self.config.max_io_full_pressure_avg10, self.psi_baseline_io);
            pressure_reason = if pressure.memory_full_avg10 > memory_limit {
                Some(format!(
                    "presión de memoria PSI full {:.2}% supera {:.2}%",
                    pressure.memory_full_avg10, memory_limit
                ))
            } else if pressure.io_full_avg10 > io_limit {
                Some(format!(
                    "presión de E/S PSI full {:.2}% supera {:.2}%",
                    pressure.io_full_avg10, io_limit
                ))
            } else if pressure.cpu_some_avg10 > cpu_limit {
                Some(format!(
                    "presión de CPU PSI some {:.2}% supera {:.2}%",
                    pressure.cpu_some_avg10, cpu_limit
                ))
            } else {
                None
            };
        }
        // Solo las muestras que no dispararon el guardarraíl pueden alimentar la
        // línea base. Así una anomalía o un pico sostenido no eleva el umbral.
        if pressure_reason.is_none() {
            self.update_psi_baseline(pressure);
        }

        let power_reason = if self.config.thermal_guard_enabled
            && matches!(
                power.thermal_state,
                ThermalState::Serious | ThermalState::Critical
            ) {
            Some(format!("estado térmico {:?}", power.thermal_state))
        } else if self.config.battery_guard_enabled
            && (power.battery_saver
                || (power.on_ac_power == Some(false)
                    && power
                        .battery_percent
                        .is_some_and(|value| value <= self.config.min_battery_percent)))
        {
            Some(format!(
                "ahorro/batería baja{}",
                power
                    .battery_percent
                    .map_or_else(String::new, |value| format!(" ({value}%)"))
            ))
        } else {
            None
        };

        if pressure_reason.is_some() || power_reason.is_some() {
            self.limited_streak = self.limited_streak.saturating_add(1);
            self.healthy_streak = 0;
        } else {
            self.healthy_streak = self.healthy_streak.saturating_add(1);
            self.limited_streak = 0;
        }
        if self.limited_streak >= self.config.guard_hysteresis_cycles {
            self.pressure_limited = pressure_reason.is_some();
            self.power_limited = power_reason.is_some();
            let reason = pressure_reason.or(power_reason);
            self.last_reason = reason.clone();
            return SafetyDecision {
                allow_actions: false,
                pressure_limited: self.pressure_limited,
                power_limited: self.power_limited,
                reason,
            };
        }
        if self.healthy_streak >= self.config.guard_hysteresis_cycles {
            self.pressure_limited = false;
            self.power_limited = false;
            self.last_reason = None;
        }
        SafetyDecision {
            allow_actions: !(self.pressure_limited || self.power_limited),
            pressure_limited: self.pressure_limited,
            power_limited: self.power_limited,
            reason: self.last_reason.clone(),
        }
    }

    fn update_psi_baseline(&mut self, pressure: PressureSnapshot) {
        if !self.config.adaptive_psi_enabled || !pressure.supported {
            return;
        }
        const ALPHA: f32 = 0.02;
        self.psi_baseline_cpu = ema(self.psi_baseline_cpu, pressure.cpu_some_avg10, ALPHA);
        self.psi_baseline_memory = ema(self.psi_baseline_memory, pressure.memory_full_avg10, ALPHA);
        self.psi_baseline_io = ema(self.psi_baseline_io, pressure.io_full_avg10, ALPHA);
        self.baseline_observations = self.baseline_observations.saturating_add(1);
        if self
            .last_persist
            .is_none_or(|instant| instant.elapsed() >= Duration::from_secs(60))
        {
            if self.persist_state("línea base de seguridad") {
                self.last_persist = Some(Instant::now());
            }
        }
    }

    fn dynamic_limit(&self, configured: f32, baseline: f32) -> f32 {
        if self.config.adaptive_psi_enabled
            && self.baseline_observations >= MIN_ADAPTIVE_PSI_OBSERVATIONS
            && baseline.is_finite()
            && baseline >= 0.0
        {
            configured.max(baseline * self.config.adaptive_psi_factor + 0.25)
        } else {
            configured
        }
    }

    pub fn record_outcome(&mut self, planned: usize, failures: usize) -> SafetyTransition {
        if !self.config.enabled {
            return SafetyTransition::default();
        }
        if planned == 0 {
            // La ausencia de acciones no demuestra que el backend se recuperó.
            // Conserva la racha hasta observar un ciclo aplicado correctamente.
            return SafetyTransition::default();
        }
        let previous_failures = self.consecutive_failure_cycles;
        let failure_ratio = failures as f32 / planned as f32;
        if failures > 0 && failure_ratio >= self.config.max_failure_ratio {
            self.consecutive_failure_cycles = self.consecutive_failure_cycles.saturating_add(1);
        } else {
            self.consecutive_failure_cycles = 0;
        }
        if self.consecutive_failure_cycles < self.config.max_consecutive_failure_cycles {
            if self.consecutive_failure_cycles != previous_failures {
                self.persist_state("estado del disyuntor");
            }
            return SafetyTransition::default();
        }

        let reason = format!(
            "disyuntor abierto tras {} ciclo(s) con {:.0}% de fallos; enfriamiento {}s",
            self.consecutive_failure_cycles,
            failure_ratio * 100.0,
            self.config.cooldown_secs
        );
        self.open_until = Some(Instant::now() + Duration::from_secs(self.config.cooldown_secs));
        self.last_reason = Some(reason.clone());
        self.persist_state("apertura del disyuntor");
        SafetyTransition {
            tripped: true,
            reason: Some(reason),
        }
    }

    pub fn should_restore_on_trip(&self) -> bool {
        self.config.restore_on_trip
    }

    pub fn status(&mut self) -> SafetyStatus {
        self.expire_cooldown();
        let cooldown_remaining_secs = self.open_until.map_or(0, |until| {
            until.saturating_duration_since(Instant::now()).as_secs()
        });
        SafetyStatus {
            enabled: self.config.enabled,
            circuit_open: self.open_until.is_some(),
            pressure_limited: self.pressure_limited,
            power_limited: self.power_limited,
            consecutive_failure_cycles: self.consecutive_failure_cycles,
            cooldown_remaining_secs,
            reason: self.last_reason.clone(),
        }
    }

    fn expire_cooldown(&mut self) {
        if self.open_until.is_some_and(|until| Instant::now() >= until) {
            self.open_until = None;
            self.consecutive_failure_cycles = 0;
            self.last_reason = Some("disyuntor restablecido automáticamente".into());
            self.pressure_limited = false;
            self.power_limited = false;
            self.persist_state("restablecimiento del disyuntor");
        }
    }

    fn persist_state(&self, context: &str) -> bool {
        let open_until_unix_secs = self.open_until.map_or(0, |until| {
            unix_now_secs()
                .saturating_add(until.saturating_duration_since(Instant::now()).as_secs())
        });
        let state = PersistedSafetyState {
            schema_version: SAFETY_STATE_SCHEMA_VERSION,
            psi_baseline_cpu: self.psi_baseline_cpu,
            psi_baseline_memory: self.psi_baseline_memory,
            psi_baseline_io: self.psi_baseline_io,
            observations: self.baseline_observations,
            consecutive_failure_cycles: self.consecutive_failure_cycles,
            circuit_open_until_unix_secs: open_until_unix_secs,
            last_reason: self.last_reason.clone(),
            metadata: BTreeMap::from([("source".into(), "local_ema".into())]),
        };
        match save_safety_state(&self.config.state_path, &state) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("aviso: no se pudo persistir {context}: {error:#}");
                false
            }
        }
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn ema(previous: f32, current: f32, alpha: f32) -> f32 {
    if previous <= 0.0 {
        current
    } else {
        previous + alpha * (current - previous)
    }
}

fn load_safety_state(path: &Path) -> Result<PersistedSafetyState> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "estado de seguridad inseguro: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_SAFETY_STATE_BYTES,
        "estado de seguridad excede {} bytes",
        MAX_SAFETY_STATE_BYTES
    );
    let state: PersistedSafetyState = serde_json::from_slice(&fs::read(path)?)?;
    anyhow::ensure!(
        state.schema_version == SAFETY_STATE_SCHEMA_VERSION,
        "estado de seguridad incompatible"
    );
    for (name, value) in [
        ("cpu", state.psi_baseline_cpu),
        ("memory", state.psi_baseline_memory),
        ("io", state.psi_baseline_io),
    ] {
        anyhow::ensure!(
            value.is_finite() && (0.0..=100.0).contains(&value),
            "línea base PSI {name} inválida: {value}"
        );
    }
    anyhow::ensure!(
        state.consecutive_failure_cycles <= 20,
        "racha de fallos persistida fuera de rango"
    );
    let now = unix_now_secs();
    anyhow::ensure!(
        state.circuit_open_until_unix_secs <= now.saturating_add(7 * 24 * 60 * 60),
        "cooldown persistido fuera de rango"
    );
    anyhow::ensure!(
        state
            .last_reason
            .as_ref()
            .is_none_or(|reason| reason.len() <= 1024 && !reason.contains('\0')),
        "razón persistida fuera de límites"
    );
    anyhow::ensure!(
        state.metadata.len() <= 16
            && state
                .metadata
                .iter()
                .all(|(key, value)| key.len() <= 64 && value.len() <= 256),
        "metadata del estado de seguridad fuera de límites"
    );
    Ok(state)
}

fn save_safety_state(path: &Path, state: &PersistedSafetyState) -> Result<()> {
    prepare_private_parent(path)?;
    validate_optional_regular_file(path)?;
    let bytes = serde_json::to_vec_pretty(state)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_SAFETY_STATE_BYTES,
        "estado de seguridad excede el límite"
    );
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
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
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        replace_safety_file(&temporary, path)?;
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
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
                "directorio de estado inseguro: {}",
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
        "directorio de estado creado de forma insegura: {}",
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
fn replace_safety_file(source: &Path, destination: &Path) -> Result<()> {
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
fn replace_safety_file(source: &Path, destination: &Path) -> Result<()> {
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
    fn abre_el_disyuntor_por_fallos_repetidos() {
        let mut controller = SafetyController::new(SafetyConfig {
            max_consecutive_failure_cycles: 2,
            ..SafetyConfig::default()
        })
        .expect("configuración válida");
        assert!(!controller.record_outcome(4, 3).tripped);
        assert!(controller.record_outcome(4, 3).tripped);
        assert!(
            !controller
                .preflight(PressureSnapshot::default(), PowerSnapshot::default())
                .allow_actions
        );
    }

    #[test]
    fn ciclo_sin_acciones_no_borra_racha_de_fallos() {
        let path = std::env::temp_dir().join(format!(
            "sysopt-safety-empty-cycle-{}-{}.json",
            std::process::id(),
            unix_now_secs()
        ));
        let mut controller = SafetyController::new(SafetyConfig {
            state_path: path.clone(),
            max_consecutive_failure_cycles: 2,
            ..SafetyConfig::default()
        })
        .expect("configuración válida");
        assert!(!controller.record_outcome(2, 2).tripped);
        assert!(!controller.record_outcome(0, 0).tripped);
        assert!(controller.record_outcome(2, 2).tripped);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn bloquea_por_presion_de_memoria() {
        let path = std::env::temp_dir().join(format!(
            "sysopt-safety-pressure-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut controller = SafetyController::new(SafetyConfig {
            state_path: path.clone(),
            ..SafetyConfig::default()
        })
        .expect("configuración válida");
        let pressure = PressureSnapshot {
            supported: true,
            memory_full_avg10: 10.0,
            ..PressureSnapshot::default()
        };
        assert!(
            controller
                .preflight(pressure, PowerSnapshot::default())
                .allow_actions
        );
        let decision = controller.preflight(pressure, PowerSnapshot::default());
        assert!(!decision.allow_actions);
        assert!(decision.pressure_limited);
        assert_eq!(controller.baseline_observations, 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn una_muestra_anomala_de_arranque_no_envenena_la_linea_base() {
        let path = std::env::temp_dir().join(format!(
            "sysopt-safety-cold-start-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut controller = SafetyController::new(SafetyConfig {
            guard_hysteresis_cycles: 1,
            state_path: path.clone(),
            ..SafetyConfig::default()
        })
        .expect("configuración válida");
        let pressure = PressureSnapshot {
            supported: true,
            memory_full_avg10: 50.0,
            ..PressureSnapshot::default()
        };
        let decision = controller.preflight(pressure, PowerSnapshot::default());
        assert!(!decision.allow_actions);
        assert_eq!(controller.psi_baseline_memory, 0.0);
        assert_eq!(controller.baseline_observations, 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn muestras_sanas_forman_linea_base_antes_de_elevar_umbral() {
        let path = std::env::temp_dir().join(format!(
            "sysopt-safety-baseline-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut controller = SafetyController::new(SafetyConfig {
            guard_hysteresis_cycles: 1,
            state_path: path.clone(),
            ..SafetyConfig::default()
        })
        .expect("configuración válida");
        let healthy = PressureSnapshot {
            supported: true,
            memory_full_avg10: 1.0,
            ..PressureSnapshot::default()
        };
        for _ in 0..MIN_ADAPTIVE_PSI_OBSERVATIONS {
            assert!(
                controller
                    .preflight(healthy, PowerSnapshot::default())
                    .allow_actions
            );
        }
        assert_eq!(
            controller.baseline_observations,
            MIN_ADAPTIVE_PSI_OBSERVATIONS
        );
        assert!(controller.dynamic_limit(2.0, controller.psi_baseline_memory) > 2.0);
        let _ = fs::remove_file(path);
    }
}
