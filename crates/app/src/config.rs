use crate::automation::{AutomationConfig, PerformanceProfile};
use crate::metrics::MetricsConfig;
use crate::overhead::OverheadConfig;
use crate::safety::SafetyConfig;
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use intelligence::IntelligenceConfig;
use policy::{ClassifierStrategy, ResourcePolicyConfig, SystemMode};
use serde::{Deserialize, Serialize};
use smart_cache::SmartCacheConfig;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub apply: bool,
    pub interval_secs: u64,
    pub top_n: usize,
    pub once: bool,
    pub signatures_path: Option<PathBuf>,
    pub classifier: ClassifierConfig,
    pub learning: LearningConfig,
    pub automation: AutomationConfig,
    pub intelligence: IntelligenceConfig,
    pub cache: SmartCacheConfig,
    pub safety: SafetyConfig,
    pub overhead: OverheadConfig,
    pub metrics: MetricsConfig,
    pub resources: ResourcesConfig,
    pub runtime: RuntimeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClassifierConfig {
    pub strategy: String,
    pub model_path: Option<PathBuf>,
    pub min_confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LearningConfig {
    pub enabled: bool,
    pub dataset_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub enabled: bool,
    pub status_path: PathBuf,
    pub control_path: PathBuf,
    pub lock_path: PathBuf,
    pub health_path: PathBuf,
    pub enforcement_journal_path: PathBuf,
    pub cgroup_control_journal_path: PathBuf,
    pub status_interval_secs: u64,
    pub safe_mode_crash_threshold: u8,
    pub crash_window_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResourcesConfig {
    pub enabled: bool,
    pub linux_cgroup_root: PathBuf,
    pub foreground_cpu_weight: u16,
    pub development_cpu_weight: u16,
    pub containers_cpu_weight: u16,
    pub background_cpu_weight: u16,
    pub foreground_io_weight: u16,
    pub development_io_weight: u16,
    pub containers_io_weight: u16,
    pub background_io_weight: u16,
    pub containers_memory_high_percent: u8,
    pub background_memory_high_percent: u8,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            apply: false,
            interval_secs: 3,
            top_n: 10,
            once: false,
            signatures_path: None,
            classifier: ClassifierConfig::default(),
            learning: LearningConfig::default(),
            automation: AutomationConfig::default(),
            intelligence: default_intelligence_config(),
            cache: default_cache_config(),
            safety: default_safety_config(),
            overhead: default_overhead_config(),
            metrics: MetricsConfig::default(),
            resources: ResourcesConfig::default(),
            runtime: RuntimeConfig::default(),
        }
    }
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            strategy: "hybrid".to_owned(),
            model_path: None,
            min_confidence: 0.72,
        }
    }
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dataset_path: default_data_dir().join("learning.jsonl"),
        }
    }
}

fn default_intelligence_config() -> IntelligenceConfig {
    IntelligenceConfig {
        state_path: Some(default_state_dir().join("intelligence-state.json")),
        semantic_cache_dir: Some(default_cache_dir().join("ai-models")),
        ..IntelligenceConfig::default()
    }
}

fn default_cache_config() -> SmartCacheConfig {
    SmartCacheConfig {
        state_path: Some(default_state_dir().join("cache-state.json")),
        ..SmartCacheConfig::default()
    }
}

fn default_safety_config() -> SafetyConfig {
    SafetyConfig {
        state_path: default_state_dir().join("safety-state.json"),
        ..SafetyConfig::default()
    }
}

fn default_overhead_config() -> OverheadConfig {
    OverheadConfig {
        state_path: default_state_dir().join("overhead-state.json"),
        ..OverheadConfig::default()
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let runtime_root = default_runtime_dir();
        Self {
            enabled: true,
            status_path: runtime_root.join("status.json"),
            control_path: runtime_root.join("control.json"),
            lock_path: runtime_root.join("sysopt.lock"),
            health_path: default_state_dir().join("health.json"),
            enforcement_journal_path: default_state_dir().join("enforcement-journal.json"),
            cgroup_control_journal_path: default_state_dir().join("cgroup-control-journal.json"),
            status_interval_secs: 3,
            safe_mode_crash_threshold: 3,
            crash_window_secs: 600,
        }
    }
}

impl RuntimeConfig {
    pub fn validate(&self) -> Result<()> {
        let paths = [
            ("status_path", &self.status_path),
            ("control_path", &self.control_path),
            ("lock_path", &self.lock_path),
            ("health_path", &self.health_path),
            ("enforcement_journal_path", &self.enforcement_journal_path),
            (
                "cgroup_control_journal_path",
                &self.cgroup_control_journal_path,
            ),
        ];
        for &(name, path) in &paths {
            anyhow::ensure!(
                !path.as_os_str().is_empty(),
                "runtime.{name} no puede estar vacío"
            );
        }
        let mut reserved_paths = paths
            .iter()
            .map(|&(name, path)| (name, path.clone()))
            .collect::<Vec<_>>();
        reserved_paths.extend([
            ("status_path.lock", sidecar_lock_path(&self.status_path)),
            ("control_path.lock", sidecar_lock_path(&self.control_path)),
            ("health_path.lock", sidecar_lock_path(&self.health_path)),
            (
                "enforcement_journal_path.lock",
                sidecar_lock_path(&self.enforcement_journal_path),
            ),
            (
                "cgroup_control_journal_path.lock",
                sidecar_lock_path(&self.cgroup_control_journal_path),
            ),
        ]);
        for left_index in 0..reserved_paths.len() {
            let (left_name, left_path) = &reserved_paths[left_index];
            for (right_name, right_path) in &reserved_paths[left_index + 1..] {
                anyhow::ensure!(
                    left_path != right_path,
                    "runtime.{left_name} y runtime.{right_name} no pueden usar la misma ruta"
                );
            }
        }
        anyhow::ensure!(
            (1..=60).contains(&self.status_interval_secs),
            "runtime.status_interval_secs debe estar entre 1 y 60"
        );
        anyhow::ensure!(
            (1..=10).contains(&self.safe_mode_crash_threshold),
            "runtime.safe_mode_crash_threshold debe estar entre 1 y 10"
        );
        anyhow::ensure!(
            (60..=86_400).contains(&self.crash_window_secs),
            "runtime.crash_window_secs debe estar entre 60 y 86400"
        );
        Ok(())
    }
}

impl Default for ResourcesConfig {
    fn default() -> Self {
        let policy = ResourcePolicyConfig::default();
        Self {
            enabled: false,
            linux_cgroup_root: PathBuf::from("/sys/fs/cgroup/sysopt"),
            foreground_cpu_weight: policy.foreground_cpu_weight,
            development_cpu_weight: policy.development_cpu_weight,
            containers_cpu_weight: policy.containers_cpu_weight,
            background_cpu_weight: policy.background_cpu_weight,
            foreground_io_weight: policy.foreground_io_weight,
            development_io_weight: policy.development_io_weight,
            containers_io_weight: policy.containers_io_weight,
            background_io_weight: policy.background_io_weight,
            containers_memory_high_percent: policy.containers_memory_high_percent,
            background_memory_high_percent: policy.background_memory_high_percent,
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "se rechazó un enlace simbólico como configuración: {}",
            path.display()
        );
        anyhow::ensure!(
            metadata.len() <= 1024 * 1024,
            "archivo de configuración demasiado grande"
        );
        let mut file = open_config_read(path)
            .with_context(|| format!("no se pudo abrir {}", path.display()))?;
        let mut content = String::with_capacity(metadata.len() as usize);
        file.read_to_string(&mut content)
            .with_context(|| format!("no se pudo leer {}", path.display()))?;
        let mut config: Self = toml::from_str(&content)
            .with_context(|| format!("TOML inválido en {}", path.display()))?;
        config.fill_runtime_paths();
        config.validate()?;
        Ok(config)
    }

    pub fn save_example(path: &Path) -> Result<()> {
        prepare_private_parent(path)?;
        let lock_path = sidecar_lock_path(path);
        let lock = open_config_lock(&lock_path)
            .with_context(|| format!("no se pudo abrir {}", lock_path.display()))?;
        FileExt::lock_exclusive(&lock)
            .with_context(|| format!("no se pudo bloquear {}", path.display()))?;
        validate_optional_regular_file(path)?;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let temporary = path.with_extension(format!("tmp-{}-{unique}", std::process::id()));
        let content = toml::to_string_pretty(&Self::default())?;
        let result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options
                .open(&temporary)
                .with_context(|| format!("no se pudo crear {}", temporary.display()))?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            replace_config_file(&temporary, path)?;
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        FileExt::unlock(&lock)
            .with_context(|| format!("no se pudo liberar {}", lock_path.display()))?;
        result
    }

    pub fn fill_runtime_paths(&mut self) {
        if self.intelligence.state_path.is_none() {
            self.intelligence.state_path =
                Some(default_state_dir().join("intelligence-state.json"));
        }
        let default_relative_ai_dir = Path::new("./ai-models");
        if self.intelligence.semantic_cache_dir.is_none()
            || self
                .intelligence
                .semantic_cache_dir
                .as_deref()
                .is_some_and(|path| path == default_relative_ai_dir)
        {
            self.intelligence.semantic_cache_dir = Some(default_cache_dir().join("ai-models"));
        }
        if self.cache.state_path.is_none() {
            self.cache.state_path = Some(default_state_dir().join("cache-state.json"));
        }
        if self.overhead.state_path == PathBuf::from("./overhead-state.json") {
            self.overhead.state_path = default_state_dir().join("overhead-state.json");
        }
        if self.safety.state_path == PathBuf::from("./safety-state.json") {
            self.safety.state_path = default_state_dir().join("safety-state.json");
        }
    }

    pub fn strategy(&self) -> Result<ClassifierStrategy> {
        ClassifierStrategy::from_str(&self.classifier.strategy)
    }

    pub fn resource_policy(&self) -> ResourcePolicyConfig {
        ResourcePolicyConfig {
            enabled: self.resources.enabled,
            foreground_cpu_weight: self.resources.foreground_cpu_weight,
            development_cpu_weight: self.resources.development_cpu_weight,
            containers_cpu_weight: self.resources.containers_cpu_weight,
            background_cpu_weight: self.resources.background_cpu_weight,
            foreground_io_weight: self.resources.foreground_io_weight,
            development_io_weight: self.resources.development_io_weight,
            containers_io_weight: self.resources.containers_io_weight,
            background_io_weight: self.resources.background_io_weight,
            containers_memory_high_percent: self.resources.containers_memory_high_percent,
            background_memory_high_percent: self.resources.background_memory_high_percent,
        }
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.interval_secs > 0, "interval_secs debe ser mayor que 0");
        anyhow::ensure!(
            (1..=512).contains(&self.top_n),
            "top_n debe estar entre 1 y 512"
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.classifier.min_confidence),
            "classifier.min_confidence debe estar entre 0 y 1"
        );
        let strategy = self.strategy()?;
        anyhow::ensure!(
            strategy != ClassifierStrategy::Model || self.classifier.model_path.is_some(),
            "classifier.strategy=model requiere classifier.model_path"
        );
        anyhow::ensure!(
            !self.learning.dataset_path.as_os_str().is_empty(),
            "learning.dataset_path no puede estar vacío"
        );
        self.automation.validate()?;
        self.intelligence.validate()?;
        self.cache.validate()?;
        self.safety.validate()?;
        self.overhead.validate()?;
        self.metrics.validate()?;
        self.runtime.validate()?;
        anyhow::ensure!(
            !self.resources.linux_cgroup_root.as_os_str().is_empty(),
            "resources.linux_cgroup_root no puede estar vacío"
        );
        if let Some(path) = &self.signatures_path {
            anyhow::ensure!(
                !path.as_os_str().is_empty(),
                "signatures_path no puede estar vacío"
            );
        }
        if let Some(path) = &self.classifier.model_path {
            anyhow::ensure!(
                !path.as_os_str().is_empty(),
                "classifier.model_path no puede estar vacío"
            );
        }
        for (name, value) in [
            (
                "foreground_cpu_weight",
                self.resources.foreground_cpu_weight,
            ),
            (
                "development_cpu_weight",
                self.resources.development_cpu_weight,
            ),
            (
                "containers_cpu_weight",
                self.resources.containers_cpu_weight,
            ),
            (
                "background_cpu_weight",
                self.resources.background_cpu_weight,
            ),
            ("foreground_io_weight", self.resources.foreground_io_weight),
            (
                "development_io_weight",
                self.resources.development_io_weight,
            ),
            ("containers_io_weight", self.resources.containers_io_weight),
            ("background_io_weight", self.resources.background_io_weight),
        ] {
            anyhow::ensure!(
                (1..=10_000).contains(&value),
                "resources.{name} fuera de rango"
            );
        }
        for (name, value) in [
            (
                "containers_memory_high_percent",
                self.resources.containers_memory_high_percent,
            ),
            (
                "background_memory_high_percent",
                self.resources.background_memory_high_percent,
            ),
        ] {
            anyhow::ensure!(
                (1..=100).contains(&value),
                "resources.{name} fuera de rango"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct CliOptions {
    pub config_path: Option<PathBuf>,
    pub init_config: Option<PathBuf>,
    pub apply: Option<bool>,
    pub interval_secs: Option<u64>,
    pub top_n: Option<usize>,
    pub once: Option<bool>,
    pub signatures_path: Option<PathBuf>,
    pub classifier_strategy: Option<String>,
    pub model_path: Option<PathBuf>,
    pub min_confidence: Option<f32>,
    pub learning_enabled: Option<bool>,
    pub dataset_path: Option<PathBuf>,
    pub label: Option<SystemMode>,
    pub train_model_dataset: Option<PathBuf>,
    pub model_output: Option<PathBuf>,
    pub resources_enabled: Option<bool>,
    pub linux_cgroup_root: Option<PathBuf>,
    pub automation_enabled: Option<bool>,
    pub profile: Option<String>,
    pub intelligence_enabled: Option<bool>,
    pub cache_enabled: Option<bool>,
    pub cache_roots: Vec<PathBuf>,
    pub warm_cache_paths: Vec<PathBuf>,
    pub status: bool,
    pub status_json: bool,
    pub pause: bool,
    pub resume: bool,
    pub runtime_profile: Option<String>,
    pub shutdown: bool,
    pub doctor: bool,
    pub self_test: bool,
    pub analyze: bool,
    pub analyze_json: bool,
    pub export_report: Option<PathBuf>,
    pub install_ai_model: bool,
    #[doc(hidden)]
    pub internal_ai_prefetch: bool,
    #[doc(hidden)]
    pub internal_ai_model_id: Option<String>,
    #[doc(hidden)]
    pub internal_ai_cache_dir: Option<PathBuf>,
    #[doc(hidden)]
    pub internal_ai_total_memory: Option<u64>,
    #[doc(hidden)]
    pub internal_ai_min_similarity: Option<f32>,
    #[doc(hidden)]
    pub internal_ai_max_cache_entries: Option<usize>,
    pub show_help: bool,
    pub show_version: bool,
}

fn sidecar_lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "config".into(), |value| value.to_os_string());
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

#[cfg(unix)]
fn open_config_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_config_lock(path: &Path) -> std::io::Result<File> {
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
fn open_config_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
}

fn prepare_private_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let existed = match fs::symlink_metadata(parent) {
            Ok(metadata) => {
                anyhow::ensure!(
                    !metadata.file_type().is_symlink() && metadata.is_dir(),
                    "el directorio padre no es un directorio regular seguro: {}",
                    parent.display()
                );
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("no se pudo inspeccionar {}", parent.display()));
            }
        };
        if !existed {
            fs::create_dir_all(parent)
                .with_context(|| format!("no se pudo crear {}", parent.display()))?;
            let metadata = fs::symlink_metadata(parent)
                .with_context(|| format!("no se pudo verificar {}", parent.display()))?;
            anyhow::ensure!(
                !metadata.file_type().is_symlink() && metadata.is_dir(),
                "el directorio creado no es seguro: {}",
                parent.display()
            );
            #[cfg(unix)]
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn replace_config_file(source: &Path, destination: &Path) -> Result<()> {
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
fn replace_config_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)
        .with_context(|| format!("no se pudo reemplazar {}", destination.display()))?;
    if let Some(parent) = destination.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("no se pudo sincronizar {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn open_config_read(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_config_read(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_config_read(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

impl CliOptions {
    pub fn parse() -> Result<Self> {
        let mut options = Self::default();
        let mut args = env::args().skip(1);
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--config" => options.config_path = Some(next_path(&mut args, "--config")?),
                "--init-config" => {
                    options.init_config = Some(next_path(&mut args, "--init-config")?)
                }
                "--apply" => options.apply = Some(true),
                "--dry-run" => options.apply = Some(false),
                "--interval" => {
                    options.interval_secs = Some(next_value(&mut args, "--interval")?.parse()?)
                }
                "--top" => options.top_n = Some(next_value(&mut args, "--top")?.parse()?),
                "--once" => options.once = Some(true),
                "--signatures" => {
                    options.signatures_path = Some(next_path(&mut args, "--signatures")?)
                }
                "--classifier" => {
                    options.classifier_strategy = Some(next_value(&mut args, "--classifier")?)
                }
                "--model" => options.model_path = Some(next_path(&mut args, "--model")?),
                "--min-confidence" => {
                    options.min_confidence =
                        Some(next_value(&mut args, "--min-confidence")?.parse()?)
                }
                "--learn" => options.learning_enabled = Some(true),
                "--no-learn" => options.learning_enabled = Some(false),
                "--dataset" => options.dataset_path = Some(next_path(&mut args, "--dataset")?),
                "--label" => {
                    options.label = Some(SystemMode::from_str(&next_value(&mut args, "--label")?)?)
                }
                "--train-model" => {
                    options.train_model_dataset = Some(next_path(&mut args, "--train-model")?)
                }
                "--model-output" => {
                    options.model_output = Some(next_path(&mut args, "--model-output")?)
                }
                "--resources" => options.resources_enabled = Some(true),
                "--no-resources" => options.resources_enabled = Some(false),
                "--cgroup-root" => {
                    options.linux_cgroup_root = Some(next_path(&mut args, "--cgroup-root")?)
                }
                "--auto" => options.automation_enabled = Some(true),
                "--no-auto" => options.automation_enabled = Some(false),
                "--profile" => options.profile = Some(next_value(&mut args, "--profile")?),
                "--ai" | "--intelligence" => options.intelligence_enabled = Some(true),
                "--no-ai" | "--no-intelligence" => options.intelligence_enabled = Some(false),
                "--cache" => options.cache_enabled = Some(true),
                "--no-cache" => options.cache_enabled = Some(false),
                "--cache-root" => options
                    .cache_roots
                    .push(next_path(&mut args, "--cache-root")?),
                "--warm-cache" => options
                    .warm_cache_paths
                    .push(next_path(&mut args, "--warm-cache")?),
                "--status" => options.status = true,
                "--status-json" => {
                    options.status = true;
                    options.status_json = true;
                }
                "--pause" => options.pause = true,
                "--resume" => options.resume = true,
                "--set-profile" => {
                    options.runtime_profile = Some(next_value(&mut args, "--set-profile")?)
                }
                "--shutdown" => options.shutdown = true,
                "--doctor" => options.doctor = true,
                "--self-test" => options.self_test = true,
                "--analyze" => options.analyze = true,
                "--analyze-json" => {
                    options.analyze = true;
                    options.analyze_json = true;
                }
                "--export-report" => {
                    options.analyze = true;
                    options.export_report = Some(next_path(&mut args, "--export-report")?);
                }
                "--install-ai-model" => options.install_ai_model = true,
                "--internal-ai-prefetch" => options.internal_ai_prefetch = true,
                "--internal-ai-model-id" => {
                    options.internal_ai_model_id =
                        Some(next_value(&mut args, "--internal-ai-model-id")?)
                }
                "--internal-ai-cache-dir" => {
                    options.internal_ai_cache_dir =
                        Some(next_path(&mut args, "--internal-ai-cache-dir")?)
                }
                "--internal-ai-total-memory" => {
                    options.internal_ai_total_memory =
                        Some(next_value(&mut args, "--internal-ai-total-memory")?.parse()?)
                }
                "--internal-ai-min-similarity" => {
                    options.internal_ai_min_similarity =
                        Some(next_value(&mut args, "--internal-ai-min-similarity")?.parse()?)
                }
                "--internal-ai-max-cache-entries" => {
                    options.internal_ai_max_cache_entries =
                        Some(next_value(&mut args, "--internal-ai-max-cache-entries")?.parse()?)
                }
                "-h" | "--help" => options.show_help = true,
                "-V" | "--version" => options.show_version = true,
                other => bail!("argumento desconocido: {other}"),
            }
        }
        options.validate_commands()?;
        Ok(options)
    }

    fn validate_commands(&self) -> Result<()> {
        if self.internal_ai_prefetch {
            anyhow::ensure!(
                self.internal_ai_model_id
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                    && self
                        .internal_ai_cache_dir
                        .as_ref()
                        .is_some_and(|path| !path.as_os_str().is_empty())
                    && self.internal_ai_total_memory.is_some()
                    && self
                        .internal_ai_min_similarity
                        .is_some_and(|value| value.is_finite() && (0.0..=1.0).contains(&value))
                    && self
                        .internal_ai_max_cache_entries
                        .is_some_and(|value| (64..=100_000).contains(&value)),
                "trabajador interno de IA incompleto"
            );
            return Ok(());
        }
        anyhow::ensure!(
            !(self.label.is_some() && self.train_model_dataset.is_some()),
            "--label y --train-model no pueden usarse juntos"
        );
        anyhow::ensure!(
            self.model_output.is_none() || self.train_model_dataset.is_some(),
            "--model-output requiere --train-model"
        );
        anyhow::ensure!(
            self.warm_cache_paths.is_empty()
                || (self.label.is_none() && self.train_model_dataset.is_none()),
            "--warm-cache no puede combinarse con --label o --train-model"
        );
        let runtime_commands = usize::from(self.status)
            + usize::from(self.pause)
            + usize::from(self.resume)
            + usize::from(self.runtime_profile.is_some())
            + usize::from(self.shutdown);
        anyhow::ensure!(
            runtime_commands <= 1,
            "usa solo uno de --status, --pause, --resume, --set-profile o --shutdown"
        );

        let primary_commands = usize::from(runtime_commands > 0)
            + usize::from(self.doctor)
            + usize::from(self.self_test)
            + usize::from(self.analyze)
            + usize::from(self.install_ai_model)
            + usize::from(self.label.is_some())
            + usize::from(self.train_model_dataset.is_some())
            + usize::from(!self.warm_cache_paths.is_empty())
            + usize::from(self.init_config.is_some());
        anyhow::ensure!(
            primary_commands <= 1,
            "los comandos --doctor, --self-test, --analyze, --install-ai-model, --label, --train-model, --warm-cache, --init-config y los comandos de runtime deben ejecutarse por separado"
        );
        if let Some(profile) = &self.runtime_profile {
            let _ = PerformanceProfile::from_str(profile)?;
        }
        Ok(())
    }

    pub fn merge(self, mut config: AppConfig) -> Result<AppConfig> {
        if let Some(value) = self.apply {
            config.apply = value;
        }
        if let Some(value) = self.interval_secs {
            config.interval_secs = value;
        }
        if let Some(value) = self.top_n {
            config.top_n = value;
        }
        if let Some(value) = self.once {
            config.once = value;
        }
        if let Some(value) = self.signatures_path {
            config.signatures_path = Some(value);
        }
        if let Some(value) = self.classifier_strategy {
            config.classifier.strategy = value;
        }
        if let Some(value) = self.model_path {
            config.classifier.model_path = Some(value);
        }
        if let Some(value) = self.min_confidence {
            config.classifier.min_confidence = value;
        }
        if let Some(value) = self.learning_enabled {
            config.learning.enabled = value;
        }
        if let Some(value) = self.dataset_path {
            config.learning.dataset_path = value;
        }
        if let Some(value) = self.resources_enabled {
            config.resources.enabled = value;
        }
        if let Some(value) = self.linux_cgroup_root {
            config.resources.linux_cgroup_root = value;
        }
        if let Some(value) = self.automation_enabled {
            config.automation.enabled = value;
        }
        if let Some(value) = self.profile {
            let profile = PerformanceProfile::from_str(&value)?;
            config.automation.apply_profile(profile);
            apply_cache_profile(&mut config.cache, profile);
        }
        if let Some(value) = self.intelligence_enabled {
            config.intelligence.enabled = value;
        }
        if let Some(value) = self.cache_enabled {
            config.cache.enabled = value;
        }
        if !self.cache_roots.is_empty() {
            config.cache.roots.extend(self.cache_roots);
        }
        config.fill_runtime_paths();
        config.validate()?;
        Ok(config)
    }
}

fn apply_cache_profile(cache: &mut SmartCacheConfig, profile: PerformanceProfile) {
    match profile {
        PerformanceProfile::Smart | PerformanceProfile::Balanced => {
            cache.scan_interval_secs = 60;
            cache.bytes_per_cycle = 256 * 1024 * 1024;
            cache.min_available_memory_percent = 25;
            cache.max_cpu_percent = 55.0;
        }
        PerformanceProfile::Eco | PerformanceProfile::Quiet => {
            cache.scan_interval_secs = 120;
            cache.bytes_per_cycle = 64 * 1024 * 1024;
            cache.min_available_memory_percent = 35;
            cache.max_cpu_percent = 35.0;
        }
        PerformanceProfile::Performance => {
            cache.scan_interval_secs = 30;
            cache.bytes_per_cycle = 512 * 1024 * 1024;
            cache.min_available_memory_percent = 15;
            cache.max_cpu_percent = 75.0;
        }
        PerformanceProfile::Gaming => {
            cache.scan_interval_secs = 90;
            cache.bytes_per_cycle = 192 * 1024 * 1024;
            cache.min_available_memory_percent = 22;
            cache.max_cpu_percent = 45.0;
        }
        PerformanceProfile::Development => {
            cache.scan_interval_secs = 35;
            cache.bytes_per_cycle = 384 * 1024 * 1024;
            cache.min_available_memory_percent = 18;
            cache.max_cpu_percent = 70.0;
        }
        PerformanceProfile::Creator => {
            cache.scan_interval_secs = 45;
            cache.bytes_per_cycle = 384 * 1024 * 1024;
            cache.min_available_memory_percent = 20;
            cache.max_cpu_percent = 65.0;
        }
        PerformanceProfile::Streaming => {
            cache.scan_interval_secs = 120;
            cache.bytes_per_cycle = 96 * 1024 * 1024;
            cache.min_available_memory_percent = 30;
            cache.max_cpu_percent = 40.0;
        }
    }
}

pub fn default_config_path() -> PathBuf {
    default_config_dir().join("config.toml")
}

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(target_os = "windows")]
fn default_config_dir() -> PathBuf {
    absolute_env_path("APPDATA")
        .unwrap_or_else(home_dir)
        .join("sysopt")
}

#[cfg(target_os = "macos")]
fn default_config_dir() -> PathBuf {
    home_dir()
        .join("Library")
        .join("Application Support")
        .join("SysOpt")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn default_config_dir() -> PathBuf {
    absolute_env_path("XDG_CONFIG_HOME")
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("sysopt")
}

#[cfg(target_os = "windows")]
pub fn default_data_dir() -> PathBuf {
    absolute_env_path("LOCALAPPDATA")
        .unwrap_or_else(home_dir)
        .join("sysopt")
}

#[cfg(target_os = "macos")]
pub fn default_data_dir() -> PathBuf {
    home_dir()
        .join("Library")
        .join("Application Support")
        .join("SysOpt")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn default_data_dir() -> PathBuf {
    absolute_env_path("XDG_DATA_HOME")
        .unwrap_or_else(|| home_dir().join(".local").join("share"))
        .join("sysopt")
}

#[cfg(target_os = "windows")]
pub fn default_state_dir() -> PathBuf {
    default_data_dir()
}

#[cfg(target_os = "macos")]
pub fn default_state_dir() -> PathBuf {
    default_data_dir().join("State")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn default_state_dir() -> PathBuf {
    absolute_env_path("XDG_STATE_HOME")
        .unwrap_or_else(|| home_dir().join(".local").join("state"))
        .join("sysopt")
}

#[cfg(target_os = "windows")]
pub fn default_cache_dir() -> PathBuf {
    default_data_dir().join("cache")
}

#[cfg(target_os = "macos")]
pub fn default_cache_dir() -> PathBuf {
    home_dir().join("Library").join("Caches").join("SysOpt")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn default_cache_dir() -> PathBuf {
    absolute_env_path("XDG_CACHE_HOME")
        .unwrap_or_else(|| home_dir().join(".cache"))
        .join("sysopt")
}

#[cfg(target_os = "windows")]
pub fn default_runtime_dir() -> PathBuf {
    default_data_dir().join("runtime")
}

#[cfg(target_os = "macos")]
pub fn default_runtime_dir() -> PathBuf {
    default_cache_dir().join("runtime")
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn default_runtime_dir() -> PathBuf {
    absolute_env_path("XDG_RUNTIME_DIR").map_or_else(
        || default_state_dir().join("runtime"),
        |path| path.join("sysopt"),
    )
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} requiere un valor"))
}

fn next_path(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(next_value(args, flag)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuracion_default_es_valida() {
        AppConfig::default().validate().unwrap();
    }

    #[test]
    fn configuracion_de_ejemplo_es_valida() {
        let example = include_str!("../../../sysopt.example.toml");
        let config: AppConfig = toml::from_str(example).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn configuracion_toml_hace_roundtrip() {
        let original = AppConfig::default();
        let encoded = toml::to_string_pretty(&original).unwrap();
        let decoded: AppConfig = toml::from_str(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.interval_secs, original.interval_secs);
        assert_eq!(decoded.classifier.strategy, original.classifier.strategy);
        assert_eq!(
            decoded.resources.background_cpu_weight,
            original.resources.background_cpu_weight
        );
    }

    #[test]
    fn rechaza_rutas_runtime_compartidas() {
        let mut config = AppConfig::default();
        config.runtime.enforcement_journal_path = config.runtime.health_path.clone();
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.runtime.lock_path = sidecar_lock_path(&config.runtime.health_path);
        assert!(config.validate().is_err());
    }

    #[test]
    fn rechaza_pesos_fuera_de_rango() {
        let mut config = AppConfig::default();
        config.resources.background_cpu_weight = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn modelo_estricto_requiere_ruta() {
        let mut config = AppConfig::default();
        config.classifier.strategy = "model".into();
        config.classifier.model_path = None;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rechaza_claves_toml_desconocidas() {
        let invalid =
            "apply = false\ninterval_secs = 3\ntop_n = 10\nonce = false\nclave_inventada = true\n";
        assert!(toml::from_str::<AppConfig>(invalid).is_err());
    }

    #[test]
    fn valida_combinaciones_de_comandos() {
        let invalid_output = CliOptions {
            model_output: Some(PathBuf::from("model.toml")),
            ..Default::default()
        };
        assert!(invalid_output.validate_commands().is_err());

        let invalid_modes = CliOptions {
            label: Some(SystemMode::Idle),
            train_model_dataset: Some(PathBuf::from("data.jsonl")),
            ..Default::default()
        };
        assert!(invalid_modes.validate_commands().is_err());

        let invalid_doctor_shutdown = CliOptions {
            doctor: true,
            shutdown: true,
            ..Default::default()
        };
        assert!(invalid_doctor_shutdown.validate_commands().is_err());

        let invalid_diagnostics = CliOptions {
            doctor: true,
            analyze: true,
            ..Default::default()
        };
        assert!(invalid_diagnostics.validate_commands().is_err());

        let valid_analyze = CliOptions {
            analyze: true,
            analyze_json: true,
            export_report: Some(PathBuf::from("report.json")),
            ..Default::default()
        };
        assert!(valid_analyze.validate_commands().is_ok());

        let valid_doctor = CliOptions {
            doctor: true,
            ..Default::default()
        };
        assert!(valid_doctor.validate_commands().is_ok());
    }
}
