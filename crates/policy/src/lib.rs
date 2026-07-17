mod features;
mod learning;
mod model;
mod signatures;
mod tracker;

pub use features::{FeatureVector, FEATURE_NAMES};
pub use learning::{append_learning_record, load_learning_records, LearningRecord};
pub use model::TrainedModel;
pub use signatures::SignatureDb;
pub use tracker::PriorityTracker;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use telemetry::SystemSnapshot;

/// Nivel de prioridad abstracto, independiente del SO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessPowerPolicy {
    /// Devuelve la decisión de QoS al sistema operativo.
    SystemManaged,
    /// Solicita EcoQoS/ejecución eficiente para trabajo no interactivo.
    Eco,
    /// Desactiva explícitamente EcoQoS para trabajo sensible a latencia.
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerResolutionPolicy {
    /// No controla la política de resolución de temporizador.
    SystemManaged,
    /// Permite al sistema ignorar solicitudes de alta resolución del proceso.
    Ignore,
    /// Exige respetar las solicitudes de resolución del proceso.
    Respect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceGroup {
    Foreground,
    Development,
    Containers,
    Background,
}

impl ResourceGroup {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Development => "development",
            Self::Containers => "containers",
            Self::Background => "background",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub cpu_weight: Option<u16>,
    pub io_weight: Option<u16>,
    pub memory_high_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    SetProcessPriority {
        pid: u32,
        start_time: u64,
        name: String,
        priority: Priority,
    },
    SetProcessPowerPolicy {
        pid: u32,
        start_time: u64,
        name: String,
        power: ProcessPowerPolicy,
        timer_resolution: TimerResolutionPolicy,
    },
    AssignProcessGroup {
        pid: u32,
        start_time: u64,
        name: String,
        group: ResourceGroup,
        limits: ResourceLimits,
    },
    ResetProcessGroup {
        pid: u32,
        start_time: u64,
        name: String,
    },
}

impl Action {
    pub fn pid(&self) -> u32 {
        match self {
            Self::SetProcessPriority { pid, .. }
            | Self::SetProcessPowerPolicy { pid, .. }
            | Self::AssignProcessGroup { pid, .. }
            | Self::ResetProcessGroup { pid, .. } => *pid,
        }
    }

    pub fn start_time(&self) -> u64 {
        match self {
            Self::SetProcessPriority { start_time, .. }
            | Self::SetProcessPowerPolicy { start_time, .. }
            | Self::AssignProcessGroup { start_time, .. }
            | Self::ResetProcessGroup { start_time, .. } => *start_time,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemMode {
    Idle,
    Interactive,
    Developing,
    MobileDevelopment,
    Gaming,
    Creative,
    Streaming,
    Containerized,
    HeavyForeground,
    Unknown,
}

impl SystemMode {
    pub const ALL: [Self; 10] = [
        Self::Idle,
        Self::Interactive,
        Self::Developing,
        Self::MobileDevelopment,
        Self::Gaming,
        Self::Creative,
        Self::Streaming,
        Self::Containerized,
        Self::HeavyForeground,
        Self::Unknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Interactive => "interactive",
            Self::Developing => "developing",
            Self::MobileDevelopment => "mobile_development",
            Self::Gaming => "gaming",
            Self::Creative => "creative",
            Self::Streaming => "streaming",
            Self::Containerized => "containerized",
            Self::HeavyForeground => "heavy_foreground",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for SystemMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SystemMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let normalized = value.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == normalized)
            .ok_or_else(|| anyhow::anyhow!("modo inválido: {value}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassificationSource {
    Rules,
    Model,
    RulesFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierStrategy {
    Rules,
    Model,
    Hybrid,
}

impl FromStr for ClassifierStrategy {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "rules" | "reglas" => Ok(Self::Rules),
            "model" | "modelo" => Ok(Self::Model),
            "hybrid" | "hibrido" | "híbrido" => Ok(Self::Hybrid),
            _ => bail!("clasificador inválido: {value}; usa rules, model o hybrid"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Prediction {
    pub mode: SystemMode,
    pub confidence: f32,
    pub source: ClassificationSource,
}

pub struct Classifier {
    strategy: ClassifierStrategy,
    model: Option<TrainedModel>,
    min_confidence: f32,
}

impl Classifier {
    pub fn rules() -> Self {
        Self {
            strategy: ClassifierStrategy::Rules,
            model: None,
            min_confidence: 0.0,
        }
    }

    pub fn new(
        strategy: ClassifierStrategy,
        model_path: Option<&Path>,
        min_confidence: f32,
    ) -> Result<Self> {
        anyhow::ensure!(
            min_confidence.is_finite() && (0.0..=1.0).contains(&min_confidence),
            "min_confidence debe estar entre 0 y 1"
        );
        let model = if strategy == ClassifierStrategy::Rules {
            None
        } else {
            model_path.map(TrainedModel::load).transpose()?
        };
        if strategy == ClassifierStrategy::Model && model.is_none() {
            bail!("classifier=model requiere model_path o --model");
        }
        Ok(Self {
            strategy,
            model,
            min_confidence,
        })
    }

    pub fn predict(&self, snapshot: &SystemSnapshot, sigs: &SignatureDb) -> Result<Prediction> {
        let rule_mode = classify(snapshot, sigs);
        if self.strategy == ClassifierStrategy::Rules {
            return Ok(Prediction {
                mode: rule_mode,
                confidence: 1.0,
                source: ClassificationSource::Rules,
            });
        }

        let Some(model) = &self.model else {
            return Ok(Prediction {
                mode: rule_mode,
                confidence: 1.0,
                source: ClassificationSource::RulesFallback,
            });
        };

        let features = FeatureVector::from_snapshot(snapshot, sigs);
        let (model_mode, confidence) = model.predict(&features)?;
        let specialized_rule = matches!(
            rule_mode,
            SystemMode::Gaming | SystemMode::Creative | SystemMode::Streaming
        );
        if self.strategy == ClassifierStrategy::Hybrid
            && specialized_rule
            && model_mode != rule_mode
        {
            return Ok(Prediction {
                mode: rule_mode,
                confidence: confidence.max(0.80),
                source: ClassificationSource::RulesFallback,
            });
        }
        if self.strategy == ClassifierStrategy::Model || confidence >= self.min_confidence {
            Ok(Prediction {
                mode: model_mode,
                confidence,
                source: ClassificationSource::Model,
            })
        } else {
            Ok(Prediction {
                mode: rule_mode,
                confidence,
                source: ClassificationSource::RulesFallback,
            })
        }
    }
}

/// Clasificador determinista de respaldo.
pub fn classify(snapshot: &SystemSnapshot, sigs: &SignatureDb) -> SystemMode {
    if snapshot.global_cpu_percent < 5.0 {
        return SystemMode::Idle;
    }

    let mut has_ide = false;
    let mut has_language_server = false;
    let mut build_tool_count = 0u32;
    let mut has_mobile_tooling = false;
    let mut has_container_vm = false;
    let mut has_game = false;
    let mut has_creative = false;
    let mut has_streaming = false;

    for process in &snapshot.top_processes {
        let categories = sigs.categorize(&process.name);
        has_ide |= categories.contains(&"ide");
        has_language_server |= categories.contains(&"language_server");
        has_mobile_tooling |= categories.contains(&"mobile_tooling");
        has_container_vm |= categories.contains(&"container_vm");
        has_game |= categories.contains(&"game");
        has_creative |= categories.contains(&"creative") || categories.contains(&"media_encoder");
        has_streaming |= categories.contains(&"streaming");
        if categories.contains(&"build_tool") {
            build_tool_count += 1;
        }
    }

    if has_streaming {
        return SystemMode::Streaming;
    }
    if has_game {
        return SystemMode::Gaming;
    }
    if has_creative {
        return SystemMode::Creative;
    }
    if has_mobile_tooling {
        return SystemMode::MobileDevelopment;
    }
    if build_tool_count >= 2 || (has_ide && (build_tool_count >= 1 || has_language_server)) {
        return SystemMode::Developing;
    }
    if has_container_vm && snapshot.global_cpu_percent > 30.0 {
        return SystemMode::Containerized;
    }
    if snapshot
        .top_processes
        .first()
        .is_some_and(|top| top.cpu_percent > 60.0)
    {
        return SystemMode::HeavyForeground;
    }
    if snapshot.global_cpu_percent > 10.0 {
        return SystemMode::Interactive;
    }
    SystemMode::Unknown
}

#[derive(Debug, Clone)]
pub struct ResourcePolicyConfig {
    pub enabled: bool,
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

impl Default for ResourcePolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            foreground_cpu_weight: 500,
            development_cpu_weight: 300,
            containers_cpu_weight: 200,
            background_cpu_weight: 50,
            foreground_io_weight: 500,
            development_io_weight: 300,
            containers_io_weight: 200,
            background_io_weight: 50,
            containers_memory_high_percent: 75,
            background_memory_high_percent: 35,
        }
    }
}

fn is_manageable_process(process: &&telemetry::ProcessSample) -> bool {
    process.pid > 4 && process.pid != std::process::id()
}

const MAX_DETERMINISTIC_DEMOTIONS: usize = 6;

fn has_category(categories: &[&str], expected: &str) -> bool {
    categories.contains(&expected)
}

fn is_protected_process(process: &telemetry::ProcessSample, sigs: &SignatureDb) -> bool {
    let categories = sigs.categorize(&process.name);
    if has_category(&categories, "protected") {
        return true;
    }
    let normalized = process.name.trim().to_ascii_lowercase();
    [
        "system",
        "registry",
        "idle",
        "init",
        "launchd",
        "kernel_task",
        "wininit",
        "winlogon",
        "csrss",
        "services",
        "lsass",
        "smss",
        "dwm",
        "explorer",
        "systemd",
        "dbus-daemon",
        "xorg",
        "wayland",
        "windowserver",
    ]
    .iter()
    .any(|pattern| normalized == *pattern || normalized == format!("{pattern}.exe"))
}

fn is_safe_background_process(process: &telemetry::ProcessSample, sigs: &SignatureDb) -> bool {
    if is_protected_process(process, sigs) {
        return false;
    }
    let categories = sigs.categorize(&process.name);
    ["background", "updater", "indexer", "game_launcher"]
        .iter()
        .any(|category| has_category(&categories, category))
}

fn is_known_foreground_process(process: &telemetry::ProcessSample, sigs: &SignatureDb) -> bool {
    if is_protected_process(process, sigs) || is_safe_background_process(process, sigs) {
        return false;
    }
    let categories = sigs.categorize(&process.name);
    [
        "ide",
        "language_server",
        "build_tool",
        "mobile_tooling",
        "container_vm",
        "browser",
        "game",
        "creative",
        "media_encoder",
        "streaming",
        "communication",
    ]
    .iter()
    .any(|category| has_category(&categories, category))
}

pub struct PolicyEngine;

impl PolicyEngine {
    pub fn decide(mode: SystemMode, snapshot: &SystemSnapshot, sigs: &SignatureDb) -> Vec<Action> {
        Self::decide_with_config(mode, snapshot, sigs, &ResourcePolicyConfig::default())
    }

    pub fn decide_with_config(
        mode: SystemMode,
        snapshot: &SystemSnapshot,
        sigs: &SignatureDb,
        resources: &ResourcePolicyConfig,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut add_priority = |process: &telemetry::ProcessSample, priority| {
            actions.push(Action::SetProcessPriority {
                pid: process.pid,
                start_time: process.start_time,
                name: process.name.clone(),
                priority,
            });
        };

        match mode {
            SystemMode::Developing => {
                for process in snapshot
                    .top_processes
                    .iter()
                    .filter(is_manageable_process)
                    .filter(|process| {
                        let categories = sigs.categorize(&process.name);
                        categories.contains(&"ide")
                            || categories.contains(&"build_tool")
                            || categories.contains(&"language_server")
                    })
                {
                    add_priority(process, Priority::AboveNormal);
                }
            }
            SystemMode::MobileDevelopment => {
                for process in snapshot
                    .top_processes
                    .iter()
                    .filter(is_manageable_process)
                    .filter(|process| sigs.is_category(&process.name, "mobile_tooling"))
                {
                    add_priority(process, Priority::AboveNormal);
                }
            }
            SystemMode::Gaming => {
                for process in snapshot
                    .top_processes
                    .iter()
                    .filter(is_manageable_process)
                    .filter(|process| sigs.is_category(&process.name, "game"))
                    .take(2)
                {
                    add_priority(process, Priority::AboveNormal);
                }
            }
            SystemMode::Creative => {
                for process in snapshot
                    .top_processes
                    .iter()
                    .filter(is_manageable_process)
                    .filter(|process| {
                        sigs.is_category(&process.name, "creative")
                            || sigs.is_category(&process.name, "media_encoder")
                    })
                    .take(3)
                {
                    add_priority(process, Priority::AboveNormal);
                }
            }
            SystemMode::Streaming => {
                for process in snapshot
                    .top_processes
                    .iter()
                    .filter(is_manageable_process)
                    .filter(|process| sigs.is_category(&process.name, "streaming"))
                    .take(2)
                {
                    add_priority(process, Priority::AboveNormal);
                }
            }
            SystemMode::Containerized => {
                for process in snapshot
                    .top_processes
                    .iter()
                    .filter(is_manageable_process)
                    .filter(|process| sigs.is_category(&process.name, "container_vm"))
                {
                    add_priority(process, Priority::AboveNormal);
                }
            }
            SystemMode::HeavyForeground => {
                // El fallback determinista es deliberadamente conservador: nunca
                // eleva a High, nunca toca procesos protegidos o desconocidos y
                // solo degrada categorías explícitamente seguras de segundo plano.
                let foreground = snapshot
                    .top_processes
                    .iter()
                    .filter(is_manageable_process)
                    .find(|process| is_known_foreground_process(process, sigs));
                if let Some(process) = foreground {
                    add_priority(process, Priority::AboveNormal);
                }
                for process in snapshot
                    .top_processes
                    .iter()
                    .filter(is_manageable_process)
                    .filter(|process| foreground.is_none_or(|top| top.pid != process.pid))
                    .filter(|process| is_safe_background_process(process, sigs))
                    .take(MAX_DETERMINISTIC_DEMOTIONS)
                {
                    add_priority(process, Priority::BelowNormal);
                }
            }
            SystemMode::Idle | SystemMode::Interactive | SystemMode::Unknown => {}
        }
        let _ = add_priority;

        // Windows filtra y aplica estas acciones mediante Power Throttling nativo.
        // En otras plataformas el backend las omite de forma explícita. El
        // temporizador queda bajo control del sistema por defecto: ignorarlo es
        // una optimización más agresiva y no se activa automáticamente.
        let foreground = snapshot
            .top_processes
            .iter()
            .filter(|process| is_manageable_process(process))
            .find(|process| is_known_foreground_process(process, sigs));
        if let Some(process) = foreground {
            actions.push(Action::SetProcessPowerPolicy {
                pid: process.pid,
                start_time: process.start_time,
                name: process.name.clone(),
                power: ProcessPowerPolicy::High,
                timer_resolution: TimerResolutionPolicy::SystemManaged,
            });
        }
        for process in snapshot
            .top_processes
            .iter()
            .filter(|process| is_manageable_process(process))
            .filter(|process| foreground.is_none_or(|active| active.pid != process.pid))
            .filter(|process| is_safe_background_process(process, sigs))
            .take(MAX_DETERMINISTIC_DEMOTIONS)
        {
            actions.push(Action::SetProcessPowerPolicy {
                pid: process.pid,
                start_time: process.start_time,
                name: process.name.clone(),
                power: ProcessPowerPolicy::Eco,
                timer_resolution: TimerResolutionPolicy::SystemManaged,
            });
        }

        if resources.enabled {
            actions.extend(resource_actions(mode, snapshot, sigs, resources));
        }
        actions
    }
}

fn resource_actions(
    mode: SystemMode,
    snapshot: &SystemSnapshot,
    sigs: &SignatureDb,
    config: &ResourcePolicyConfig,
) -> Vec<Action> {
    let memory_limit = |percent: u8| {
        snapshot
            .total_memory_bytes
            .checked_mul(u64::from(percent))
            .map(|bytes| bytes / 100)
            .filter(|bytes| *bytes > 0)
    };
    let limits_for = |group| match group {
        ResourceGroup::Foreground => ResourceLimits {
            cpu_weight: Some(config.foreground_cpu_weight),
            io_weight: Some(config.foreground_io_weight),
            memory_high_bytes: None,
        },
        ResourceGroup::Development => ResourceLimits {
            cpu_weight: Some(config.development_cpu_weight),
            io_weight: Some(config.development_io_weight),
            memory_high_bytes: None,
        },
        ResourceGroup::Containers => ResourceLimits {
            cpu_weight: Some(config.containers_cpu_weight),
            io_weight: Some(config.containers_io_weight),
            memory_high_bytes: memory_limit(config.containers_memory_high_percent),
        },
        ResourceGroup::Background => ResourceLimits {
            cpu_weight: Some(config.background_cpu_weight),
            io_weight: Some(config.background_io_weight),
            memory_high_bytes: memory_limit(config.background_memory_high_percent),
        },
    };
    let assign = |process: &telemetry::ProcessSample, group| Action::AssignProcessGroup {
        pid: process.pid,
        start_time: process.start_time,
        name: process.name.clone(),
        group,
        limits: limits_for(group),
    };

    match mode {
        SystemMode::Developing => snapshot
            .top_processes
            .iter()
            .filter(is_manageable_process)
            .filter(|process| {
                let categories = sigs.categorize(&process.name);
                categories.contains(&"ide")
                    || categories.contains(&"build_tool")
                    || categories.contains(&"language_server")
            })
            .map(|process| assign(process, ResourceGroup::Development))
            .collect(),
        SystemMode::MobileDevelopment => snapshot
            .top_processes
            .iter()
            .filter(is_manageable_process)
            .filter(|process| sigs.is_category(&process.name, "mobile_tooling"))
            .map(|process| assign(process, ResourceGroup::Development))
            .collect(),
        SystemMode::Gaming => snapshot
            .top_processes
            .iter()
            .filter(is_manageable_process)
            .filter(|process| sigs.is_category(&process.name, "game"))
            .take(2)
            .map(|process| assign(process, ResourceGroup::Foreground))
            .collect(),
        SystemMode::Creative => snapshot
            .top_processes
            .iter()
            .filter(is_manageable_process)
            .filter(|process| {
                sigs.is_category(&process.name, "creative")
                    || sigs.is_category(&process.name, "media_encoder")
            })
            .take(3)
            .map(|process| assign(process, ResourceGroup::Foreground))
            .collect(),
        SystemMode::Streaming => snapshot
            .top_processes
            .iter()
            .filter(is_manageable_process)
            .filter(|process| sigs.is_category(&process.name, "streaming"))
            .take(2)
            .map(|process| assign(process, ResourceGroup::Foreground))
            .collect(),
        SystemMode::Containerized => snapshot
            .top_processes
            .iter()
            .filter(is_manageable_process)
            .filter(|process| sigs.is_category(&process.name, "container_vm"))
            .map(|process| assign(process, ResourceGroup::Containers))
            .collect(),
        SystemMode::HeavyForeground => {
            let foreground = snapshot
                .top_processes
                .iter()
                .filter(is_manageable_process)
                .find(|process| is_known_foreground_process(process, sigs));
            let mut actions = Vec::new();
            if let Some(process) = foreground {
                actions.push(assign(process, ResourceGroup::Foreground));
            }
            actions.extend(
                snapshot
                    .top_processes
                    .iter()
                    .filter(is_manageable_process)
                    .filter(|process| foreground.is_none_or(|top| top.pid != process.pid))
                    .filter(|process| is_safe_background_process(process, sigs))
                    .take(MAX_DETERMINISTIC_DEMOTIONS)
                    .map(|process| assign(process, ResourceGroup::Background)),
            );
            actions
        }
        SystemMode::Idle | SystemMode::Interactive | SystemMode::Unknown => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::ProcessSample;

    fn snapshot_with(procs: Vec<(&str, f32)>, global_cpu: f32) -> SystemSnapshot {
        SystemSnapshot {
            global_cpu_percent: global_cpu,
            used_memory_bytes: 0,
            available_memory_bytes: 0,
            total_memory_bytes: 16 * 1024 * 1024 * 1024,
            process_count: procs.len(),
            top_processes: procs
                .into_iter()
                .enumerate()
                .map(|(index, (name, cpu_percent))| ProcessSample {
                    pid: 100_000 + index as u32,
                    start_time: 100 + index as u64,
                    name: name.to_owned(),
                    cpu_percent,
                    memory_bytes: 0,
                    read_bytes: 0,
                    written_bytes: 0,
                    cwd: None,
                    executable: None,
                })
                .collect(),
            io_processes: Vec::new(),
            pressure: Default::default(),
            power: Default::default(),
        }
    }

    #[test]
    fn detecta_modos_principales() {
        let sigs = SignatureDb::embedded_default();
        assert_eq!(
            classify(&snapshot_with(vec![], 1.0), &sigs),
            SystemMode::Idle
        );
        assert_eq!(
            classify(
                &snapshot_with(vec![("rustc", 40.0), ("cc1plus", 30.0)], 70.0),
                &sigs
            ),
            SystemMode::Developing
        );
        assert_eq!(
            classify(&snapshot_with(vec![("dockerd", 40.0)], 65.0), &sigs),
            SystemMode::Containerized
        );
        assert_eq!(
            classify(&snapshot_with(vec![("juego.exe", 85.0)], 90.0), &sigs),
            SystemMode::HeavyForeground
        );
        assert_eq!(
            classify(&snapshot_with(vec![("cs2", 45.0)], 55.0), &sigs),
            SystemMode::Gaming
        );
        assert_eq!(
            classify(&snapshot_with(vec![("blender", 45.0)], 55.0), &sigs),
            SystemMode::Creative
        );
        assert_eq!(
            classify(&snapshot_with(vec![("obs64", 35.0)], 45.0), &sigs),
            SystemMode::Streaming
        );
    }

    #[test]
    fn developing_no_prioriza_navegador() {
        let sigs = SignatureDb::embedded_default();
        let snapshot = snapshot_with(
            vec![("code", 15.0), ("rustc", 35.0), ("chrome.exe", 10.0)],
            60.0,
        );
        let actions = PolicyEngine::decide(SystemMode::Developing, &snapshot, &sigs);
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(action, Action::SetProcessPriority { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn rechaza_confianza_invalida() {
        assert!(Classifier::new(ClassifierStrategy::Rules, None, f32::NAN).is_err());
        assert!(Classifier::new(ClassifierStrategy::Rules, None, 1.1).is_err());
    }

    #[test]
    fn no_administra_pid_reservado() {
        let sigs = SignatureDb::embedded_default();
        let snapshot = SystemSnapshot {
            global_cpu_percent: 90.0,
            used_memory_bytes: 0,
            available_memory_bytes: 0,
            total_memory_bytes: 8 * 1024 * 1024 * 1024,
            process_count: 2,
            top_processes: vec![
                ProcessSample {
                    pid: 1,
                    start_time: 1,
                    name: "init".into(),
                    cpu_percent: 90.0,
                    memory_bytes: 0,
                    read_bytes: 0,
                    written_bytes: 0,
                    cwd: None,
                    executable: None,
                },
                ProcessSample {
                    pid: 100_000,
                    start_time: 2,
                    name: "backup".into(),
                    cpu_percent: 5.0,
                    memory_bytes: 0,
                    read_bytes: 0,
                    written_bytes: 0,
                    cwd: None,
                    executable: None,
                },
            ],
            io_processes: Vec::new(),
            pressure: Default::default(),
            power: Default::default(),
        };
        let actions = PolicyEngine::decide(SystemMode::HeavyForeground, &snapshot, &sigs);
        assert!(actions.iter().all(|action| action.pid() != 1));
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::SetProcessPriority {
                pid: 100_000,
                priority: Priority::BelowNormal,
                ..
            }
        )));
    }

    #[test]
    fn limites_por_defecto_no_imponen_controles() {
        let limits = ResourceLimits::default();
        assert_eq!(limits.cpu_weight, None);
        assert_eq!(limits.io_weight, None);
        assert_eq!(limits.memory_high_bytes, None);
    }

    #[test]
    fn heavy_foreground_no_toca_procesos_criticos_o_desconocidos() {
        let sigs = SignatureDb::embedded_default();
        let snapshot = snapshot_with(
            vec![
                ("systemd", 90.0),
                ("lsass.exe", 60.0),
                ("proceso-desconocido", 40.0),
            ],
            95.0,
        );
        let actions = PolicyEngine::decide(SystemMode::HeavyForeground, &snapshot, &sigs);
        assert!(actions.is_empty());
    }

    #[test]
    fn heavy_foreground_es_conservador_y_limita_degradaciones() {
        let sigs = SignatureDb::embedded_default();
        let mut processes = vec![("code", 90.0)];
        processes.extend(std::iter::repeat_n(("backup", 10.0), 10));
        let snapshot = snapshot_with(processes, 95.0);
        let actions = PolicyEngine::decide(SystemMode::HeavyForeground, &snapshot, &sigs);
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::SetProcessPriority {
                name,
                priority: Priority::AboveNormal,
                ..
            } if name == "code"
        )));
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(
                    action,
                    Action::SetProcessPriority {
                        priority: Priority::BelowNormal,
                        ..
                    }
                ))
                .count(),
            MAX_DETERMINISTIC_DEMOTIONS
        );
        assert!(actions.iter().all(|action| !matches!(
            action,
            Action::SetProcessPriority {
                priority: Priority::High,
                ..
            }
        )));
    }

    #[test]
    fn recursos_son_opt_in() {
        let sigs = SignatureDb::embedded_default();
        let snapshot = snapshot_with(vec![("juego.exe", 85.0), ("backup", 10.0)], 90.0);
        let default_actions = PolicyEngine::decide(SystemMode::HeavyForeground, &snapshot, &sigs);
        assert!(default_actions.iter().all(|action| !matches!(
            action,
            Action::AssignProcessGroup { .. } | Action::ResetProcessGroup { .. }
        )));

        let config = ResourcePolicyConfig {
            enabled: true,
            ..Default::default()
        };
        let actions = PolicyEngine::decide_with_config(
            SystemMode::HeavyForeground,
            &snapshot,
            &sigs,
            &config,
        );
        assert!(actions
            .iter()
            .any(|action| matches!(action, Action::AssignProcessGroup { .. })));
        assert!(actions.iter().all(|action| match action {
            Action::AssignProcessGroup { group, limits, .. } => match group {
                ResourceGroup::Foreground =>
                    limits.cpu_weight == Some(config.foreground_cpu_weight)
                        && limits.io_weight == Some(config.foreground_io_weight)
                        && limits.memory_high_bytes.is_none(),
                ResourceGroup::Background =>
                    limits.cpu_weight == Some(config.background_cpu_weight)
                        && limits.io_weight == Some(config.background_io_weight)
                        && limits.memory_high_bytes
                            == Some(
                                snapshot.total_memory_bytes
                                    * u64::from(config.background_memory_high_percent)
                                    / 100
                            ),
                ResourceGroup::Development | ResourceGroup::Containers => true,
            },
            Action::SetProcessPriority { .. }
            | Action::SetProcessPowerPolicy { .. }
            | Action::ResetProcessGroup { .. } => true,
        }));
    }
}
