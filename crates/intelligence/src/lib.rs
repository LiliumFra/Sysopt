mod semantic;

pub use semantic::{
    prefetch_model as prefetch_semantic_model, resolve_model_id as resolve_semantic_model_id,
    resolve_model_spec as resolve_semantic_model_spec, ResolvedModelSpec, SemanticHint,
    SemanticModelInfo,
};

use anyhow::{Context, Result};
use fs2::FileExt;
use policy::{Action, PolicyEngine, Priority, ResourcePolicyConfig, SignatureDb, SystemMode};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use telemetry::{ProcessSample, SystemSnapshot};

const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntelligenceConfig {
    pub enabled: bool,
    pub persist_state: bool,
    pub state_path: Option<PathBuf>,
    pub learning_rate: f32,
    pub history_decay: f32,
    pub min_confidence: f32,
    pub max_boosted_processes: usize,
    pub max_demoted_processes: usize,
    pub allow_high_priority: bool,
    pub high_priority_min_score: f32,
    pub background_demotion_cpu_threshold: f32,
    pub background_demotion_ram_percent: u8,
    pub max_process_profiles: usize,
    pub max_root_profiles: usize,
    pub persist_interval_secs: u64,
    pub semantic_enabled: bool,
    pub semantic_auto_download: bool,
    pub semantic_retry_initial_secs: u64,
    pub semantic_retry_max_secs: u64,
    pub semantic_download_timeout_secs: u64,
    pub semantic_model_id: String,
    pub semantic_cache_dir: Option<PathBuf>,
    pub semantic_only_unknown: bool,
    pub semantic_min_similarity: f32,
    pub semantic_weight: f32,
    pub semantic_max_cache_entries: usize,
    pub semantic_max_processes_per_cycle: usize,
}

impl Default for IntelligenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            persist_state: true,
            state_path: None,
            learning_rate: 0.18,
            history_decay: 0.985,
            min_confidence: 0.52,
            max_boosted_processes: 4,
            max_demoted_processes: 6,
            allow_high_priority: false,
            high_priority_min_score: 0.94,
            background_demotion_cpu_threshold: 82.0,
            background_demotion_ram_percent: 12,
            max_process_profiles: 1024,
            max_root_profiles: 128,
            persist_interval_secs: 60,
            semantic_enabled: true,
            semantic_auto_download: true,
            semantic_retry_initial_secs: 30,
            semantic_retry_max_secs: 3600,
            semantic_download_timeout_secs: 300,
            semantic_model_id: "auto".into(),
            semantic_cache_dir: Some(PathBuf::from("./ai-models")),
            semantic_only_unknown: true,
            semantic_min_similarity: 0.36,
            semantic_weight: 0.18,
            semantic_max_cache_entries: 2048,
            semantic_max_processes_per_cycle: 4,
        }
    }
}

impl IntelligenceConfig {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("learning_rate", self.learning_rate),
            ("history_decay", self.history_decay),
            ("min_confidence", self.min_confidence),
            ("high_priority_min_score", self.high_priority_min_score),
            ("semantic_min_similarity", self.semantic_min_similarity),
            ("semantic_weight", self.semantic_weight),
        ] {
            anyhow::ensure!(
                value.is_finite() && (0.0..=1.0).contains(&value),
                "intelligence.{name} debe estar entre 0 y 1"
            );
        }
        anyhow::ensure!(
            self.learning_rate > 0.0,
            "intelligence.learning_rate debe ser mayor que 0"
        );
        anyhow::ensure!(
            self.history_decay >= 0.8,
            "intelligence.history_decay debe ser al menos 0.8"
        );
        anyhow::ensure!(
            (1..=32).contains(&self.max_boosted_processes),
            "intelligence.max_boosted_processes debe estar entre 1 y 32"
        );
        anyhow::ensure!(
            self.max_demoted_processes <= 64,
            "intelligence.max_demoted_processes debe ser como máximo 64"
        );
        anyhow::ensure!(
            self.background_demotion_cpu_threshold.is_finite()
                && (50.0..=100.0).contains(&self.background_demotion_cpu_threshold),
            "intelligence.background_demotion_cpu_threshold debe estar entre 50 y 100"
        );
        anyhow::ensure!(
            (1..=50).contains(&self.background_demotion_ram_percent),
            "intelligence.background_demotion_ram_percent debe estar entre 1 y 50"
        );
        anyhow::ensure!(
            (64..=100_000).contains(&self.max_process_profiles),
            "intelligence.max_process_profiles fuera de rango"
        );
        anyhow::ensure!(
            (8..=4096).contains(&self.max_root_profiles),
            "intelligence.max_root_profiles fuera de rango"
        );
        anyhow::ensure!(
            (10..=3600).contains(&self.persist_interval_secs),
            "intelligence.persist_interval_secs debe estar entre 10 y 3600"
        );
        if let Some(path) = &self.state_path {
            anyhow::ensure!(
                !path.as_os_str().is_empty(),
                "intelligence.state_path no puede estar vacío"
            );
        }
        if self.semantic_enabled {
            anyhow::ensure!(
                !self.semantic_model_id.trim().is_empty(),
                "intelligence.semantic_model_id no puede estar vacío"
            );
            anyhow::ensure!(
                self.semantic_cache_dir
                    .as_ref()
                    .is_some_and(|path| !path.as_os_str().is_empty()),
                "intelligence.semantic_cache_dir es obligatorio cuando la IA semántica está activa"
            );
            anyhow::ensure!(
                (64..=100_000).contains(&self.semantic_max_cache_entries),
                "intelligence.semantic_max_cache_entries fuera de rango"
            );
            anyhow::ensure!(
                (1..=32).contains(&self.semantic_max_processes_per_cycle),
                "intelligence.semantic_max_processes_per_cycle debe estar entre 1 y 32"
            );
            anyhow::ensure!(
                (5..=3600).contains(&self.semantic_retry_initial_secs),
                "intelligence.semantic_retry_initial_secs debe estar entre 5 y 3600"
            );
            anyhow::ensure!(
                self.semantic_retry_max_secs >= self.semantic_retry_initial_secs
                    && self.semantic_retry_max_secs <= 86_400,
                "intelligence.semantic_retry_max_secs debe ser mayor o igual al intervalo inicial y como máximo 86400"
            );
            anyhow::ensure!(
                (30..=3600).contains(&self.semantic_download_timeout_secs),
                "intelligence.semantic_download_timeout_secs debe estar entre 30 y 3600"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProcessMemory {
    observations: u32,
    ema_cpu: f32,
    ema_io: f32,
    ema_relevance: f32,
    persistence: f32,
    last_seen_unix_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RootMemory {
    observations: u32,
    active_streak: u32,
    utility: f32,
    last_seen_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedState {
    schema_version: u32,
    cycles: u64,
    process_profiles: HashMap<String, ProcessMemory>,
    root_profiles: HashMap<String, RootMemory>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            cycles: 0,
            process_profiles: HashMap::new(),
            root_profiles: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessDecision {
    pub pid: u32,
    pub name: String,
    pub score: f32,
    pub confidence: f32,
    pub priority: Priority,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_category: Option<String>,
    #[serde(default)]
    pub semantic_similarity: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheAdvice {
    pub allow_warmup: bool,
    pub budget_multiplier: f32,
    pub scan_interval_multiplier: f32,
    pub confidence: f32,
    pub root_scores: BTreeMap<PathBuf, f32>,
    pub extension_scores: BTreeMap<String, f32>,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntelligenceReport {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_error: Option<String>,
    #[serde(default)]
    pub semantic_downloading: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_retry_in_secs: Option<u64>,
    pub summary: String,
    pub process_decisions: Vec<ProcessDecision>,
    pub cache: CacheAdvice,
}

#[derive(Debug, Clone)]
pub struct IntelligentDecision {
    pub actions: Vec<Action>,
    pub cache: CacheAdvice,
    pub report: IntelligenceReport,
}

pub struct AdaptiveIntelligence {
    config: IntelligenceConfig,
    state: PersistedState,
    last_persist: Option<Instant>,
    active_roots_last_cycle: HashSet<PathBuf>,
    semantic: Option<semantic::SemanticProcessClassifier>,
    semantic_download_rx: Option<std::sync::mpsc::Receiver<std::result::Result<(), String>>>,
    semantic_download_cancel: Option<Arc<AtomicBool>>,
    semantic_download_handle: Option<std::thread::JoinHandle<()>>,
    semantic_next_retry: Option<Instant>,
    semantic_retry_delay_secs: u64,
    semantic_error: Option<String>,
    semantic_runtime_allowed: bool,
}

impl AdaptiveIntelligence {
    pub fn new(config: IntelligenceConfig) -> Result<Self> {
        config.validate()?;
        let state = if config.persist_state {
            config
                .state_path
                .as_deref()
                .and_then(|path| load_state(path).ok())
                .filter(|state| state.schema_version == STATE_SCHEMA_VERSION)
                .unwrap_or_default()
        } else {
            PersistedState::default()
        };
        let semantic_retry_delay_secs = config.semantic_retry_initial_secs;
        Ok(Self {
            config,
            state,
            last_persist: None,
            active_roots_last_cycle: HashSet::new(),
            semantic: None,
            semantic_download_rx: None,
            semantic_download_cancel: None,
            semantic_download_handle: None,
            semantic_next_retry: None,
            semantic_retry_delay_secs,
            semantic_error: None,
            semantic_runtime_allowed: true,
        })
    }

    pub fn config(&self) -> &IntelligenceConfig {
        &self.config
    }

    pub fn set_semantic_runtime_allowed(&mut self, allowed: bool) {
        if self.semantic_runtime_allowed && !allowed {
            if let Some(cancel) = &self.semantic_download_cancel {
                cancel.store(true, AtomicOrdering::SeqCst);
            }
        }
        self.semantic_runtime_allowed = allowed;
    }

    pub fn decide(
        &mut self,
        mode: SystemMode,
        snapshot: &SystemSnapshot,
        signatures: &SignatureDb,
        resources: &ResourcePolicyConfig,
        active_roots: &[PathBuf],
        foreground_pid: Option<u32>,
    ) -> Result<IntelligentDecision> {
        if !self.config.enabled {
            let actions = PolicyEngine::decide_with_config(mode, snapshot, signatures, resources)
                .into_iter()
                .filter(|action| {
                    !matches!(
                        action,
                        Action::SetProcessPriority {
                            pid,
                            priority: Priority::BelowNormal,
                            ..
                        } if foreground_pid == Some(*pid)
                    )
                })
                .collect();
            let cache = fallback_cache_advice(mode, snapshot, active_roots);
            return Ok(IntelligentDecision {
                actions,
                cache: cache.clone(),
                report: IntelligenceReport {
                    enabled: false,
                    confidence: 1.0,
                    summary: "IA híbrida desactivada; se usan reglas deterministas".into(),
                    cache,
                    ..IntelligenceReport::default()
                },
            });
        }

        self.state.cycles = self.state.cycles.saturating_add(1);
        self.decay_memories();
        if self.semantic_runtime_allowed {
            self.ensure_semantic_model(snapshot.total_memory_bytes);
        }

        let available_ram_percent =
            percent(snapshot.available_memory_bytes, snapshot.total_memory_bytes);
        let under_pressure = snapshot.global_cpu_percent
            >= self.config.background_demotion_cpu_threshold
            || available_ram_percent <= f32::from(self.config.background_demotion_ram_percent);

        // Se combinan los líderes por CPU y por E/S. Esto evita ignorar procesos
        // que cargan muchos datos pero consumen poca CPU (IDE, juegos y editores).
        let mut seen_processes = HashSet::new();
        let candidates = snapshot
            .top_processes
            .iter()
            .chain(snapshot.io_processes.iter())
            .filter(|process| {
                manageable(process) && seen_processes.insert((process.pid, process.start_time))
            })
            .collect::<Vec<_>>();

        let mut scored = Vec::with_capacity(candidates.len());
        let mut semantic_attempts = 0usize;
        let mut semantic_hits = 0usize;
        let mut semantic_roots = Vec::new();
        for (rank, process) in candidates.into_iter().enumerate() {
            let mut categories = signatures
                .categorize(&process.name)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let rules_matched = !categories.is_empty();
            let should_try_semantic = self.semantic_runtime_allowed
                && self.config.semantic_enabled
                && semantic_attempts < self.config.semantic_max_processes_per_cycle
                && (!self.config.semantic_only_unknown || !rules_matched);
            let semantic_hint = if should_try_semantic {
                semantic_attempts += 1;
                self.semantic
                    .as_mut()
                    .and_then(|semantic| semantic.classify_process(process))
                    .filter(|hint| hint.confidence >= 0.12)
            } else {
                None
            };
            if let Some(hint) = &semantic_hint {
                semantic_hits += 1;
                if !categories.iter().any(|category| category == &hint.category) {
                    categories.push(hint.category.clone());
                }
                if semantic_category_supports_cache(&hint.category) {
                    if let Some(root) = process_root(process) {
                        semantic_roots.push(root);
                    }
                }
            }

            let is_foreground = foreground_pid == Some(process.pid);
            let score = self.score_process(
                process,
                rank,
                mode,
                snapshot,
                &categories,
                is_foreground,
                semantic_hint.as_ref(),
                rules_matched,
            );
            let confidence = self.process_confidence(
                process,
                &categories,
                is_foreground,
                semantic_hint.as_ref(),
                rules_matched,
            );
            scored.push((
                process,
                categories,
                score,
                confidence,
                is_foreground,
                semantic_hint,
            ));
        }

        let mut all_active_roots = active_roots.to_vec();
        all_active_roots.extend(semantic_roots);
        deduplicate_paths(&mut all_active_roots);
        self.observe_roots(&all_active_roots);

        for (process, categories, score, _, _, semantic_hint) in &scored {
            self.learn_process(process, mode, categories, *score, semantic_hint.as_ref());
        }

        if self
            .semantic
            .as_ref()
            .is_some_and(|model| !model.is_healthy())
        {
            self.semantic = None;
            self.schedule_semantic_retry(
                "el runtime semántico detectó un fallo interno y fue desactivado de forma segura"
                    .into(),
            );
        }

        scored.sort_by(|left, right| right.2.partial_cmp(&left.2).unwrap_or(Ordering::Equal));

        let mut process_decisions = Vec::new();
        let mut priority_actions = Vec::new();
        let boost_limit = self
            .config
            .max_boosted_processes
            .min(hardware_boost_limit());
        let mut boosted = 0usize;
        let mut demoted = 0usize;
        let mut boosted_families = HashSet::new();

        for (index, (process, categories, score, confidence, is_foreground, semantic_hint)) in
            scored.iter().enumerate()
        {
            if is_protected_process(&process.name, categories) {
                continue;
            }

            let family = process_key(process);
            let can_boost_family = !boosted_families.contains(&family);
            let priority = if *confidence < self.config.min_confidence {
                Priority::Normal
            } else if *score >= self.config.high_priority_min_score
                && self.config.allow_high_priority
                && index == 0
                && can_boost_family
            {
                boosted_families.insert(family);
                boosted += 1;
                Priority::High
            } else if *score >= 0.62 && boosted < boost_limit && can_boost_family {
                boosted_families.insert(family);
                boosted += 1;
                Priority::AboveNormal
            } else if under_pressure
                && !*is_foreground
                && *score <= 0.18
                && demoted < self.config.max_demoted_processes
                && is_safe_background_candidate(categories)
            {
                demoted += 1;
                Priority::BelowNormal
            } else {
                Priority::Normal
            };

            let reason = explain_process_decision(
                process,
                mode,
                categories,
                *score,
                *confidence,
                priority,
                under_pressure,
                *is_foreground,
                semantic_hint.as_ref(),
            );
            process_decisions.push(ProcessDecision {
                pid: process.pid,
                name: process.name.clone(),
                score: *score,
                confidence: *confidence,
                priority,
                reason,
                semantic_category: semantic_hint.as_ref().map(|hint| hint.category.clone()),
                semantic_similarity: semantic_hint.as_ref().map_or(0.0, |hint| hint.similarity),
            });
            if priority != Priority::Normal {
                priority_actions.push(Action::SetProcessPriority {
                    pid: process.pid,
                    start_time: process.start_time,
                    name: process.name.clone(),
                    priority,
                });
            }
        }

        // Los grupos de recursos siguen siendo deterministas y auditables. La IA híbrida
        // solo reemplaza la elección de prioridad para mantener límites duros.
        let mut actions = PolicyEngine::decide_with_config(mode, snapshot, signatures, resources)
            .into_iter()
            .filter(|action| !matches!(action, Action::SetProcessPriority { .. }))
            .collect::<Vec<_>>();
        actions.extend(priority_actions);

        let cache = self.cache_advice(mode, snapshot, &all_active_roots);
        let confidence = if process_decisions.is_empty() {
            cache.confidence
        } else {
            (process_decisions
                .iter()
                .map(|decision| decision.confidence)
                .sum::<f32>()
                / process_decisions.len() as f32
                * 0.7)
                + cache.confidence * 0.3
        }
        .clamp(0.0, 1.0);

        self.trim_state();
        self.persist_if_due();

        let semantic_model = self
            .semantic
            .as_ref()
            .map(|semantic| semantic.model_id().to_owned());
        let semantic_downloading = self.semantic_download_rx.is_some();
        let semantic_retry_in_secs = self.semantic_retry_in_secs();
        let semantic_state = if semantic_model.is_some() {
            format!("semántica {} acierto(s)", semantic_hits)
        } else if semantic_downloading {
            "descarga automática de IA en curso".into()
        } else if let Some(seconds) = semantic_retry_in_secs {
            format!("fallback; reintento automático en {seconds}s")
        } else if self.config.semantic_enabled {
            "semántica en fallback".into()
        } else {
            "semántica desactivada".into()
        };
        let summary = format!(
            "IA híbrida: {} proceso(s) priorizados, {} reducido(s), {} raíz/raíces candidatas, {}; confianza {:.0}%",
            boosted,
            demoted,
            cache.root_scores.len(),
            semantic_state,
            confidence * 100.0
        );
        let report = IntelligenceReport {
            enabled: true,
            confidence,
            boosted_processes: boosted,
            demoted_processes: demoted,
            learned_processes: self.state.process_profiles.len(),
            learned_roots: self.state.root_profiles.len(),
            semantic_model_loaded: semantic_model.is_some(),
            semantic_hits,
            semantic_model,
            semantic_error: self.semantic_error.clone(),
            semantic_downloading,
            semantic_retry_in_secs,
            summary,
            process_decisions,
            cache: cache.clone(),
        };

        Ok(IntelligentDecision {
            actions,
            cache,
            report,
        })
    }

    pub fn flush(&mut self) -> Result<()> {
        self.persist(true)
    }

    fn ensure_semantic_model(&mut self, total_memory_bytes: u64) {
        if !self.semantic_runtime_allowed
            || !self.config.semantic_enabled
            || self.semantic.is_some()
        {
            return;
        }

        self.poll_semantic_download(total_memory_bytes);
        if self.semantic.is_some() || self.semantic_download_rx.is_some() {
            return;
        }
        if self
            .semantic_next_retry
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return;
        }

        if self.try_load_cached_semantic_model(total_memory_bytes) {
            return;
        }
        if !self.config.semantic_auto_download {
            self.semantic_error = Some(
                "modelo semántico no disponible en caché y la descarga automática está desactivada"
                    .into(),
            );
            return;
        }

        self.start_semantic_download(total_memory_bytes);
    }

    fn try_load_cached_semantic_model(&mut self, total_memory_bytes: u64) -> bool {
        let Some(cache_dir) = self.config.semantic_cache_dir.as_deref() else {
            self.semantic_error = Some("directorio de modelo semántico no configurado".into());
            return false;
        };
        match semantic::SemanticProcessClassifier::load_cached(
            &self.config.semantic_model_id,
            cache_dir,
            total_memory_bytes,
            self.config.semantic_min_similarity,
            self.config.semantic_max_cache_entries,
        ) {
            Ok(model) => {
                self.semantic = Some(model);
                self.semantic_error = None;
                self.semantic_next_retry = None;
                self.semantic_retry_delay_secs = self.config.semantic_retry_initial_secs;
                true
            }
            Err(error) => {
                self.semantic_error = Some(format!(
                    "modelo semántico todavía no disponible en caché; se mantienen reglas y aprendizaje online: {error}"
                ));
                false
            }
        }
    }

    fn start_semantic_download(&mut self, total_memory_bytes: u64) {
        let Some(cache_dir) = self.config.semantic_cache_dir.clone() else {
            self.semantic_error = Some("directorio de modelo semántico no configurado".into());
            return;
        };
        let configured_model_id = self.config.semantic_model_id.clone();
        let min_similarity = self.config.semantic_min_similarity;
        let max_cache_entries = self.config.semantic_max_cache_entries;
        let timeout = Duration::from_secs(self.config.semantic_download_timeout_secs);
        let (sender, receiver) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        match std::thread::Builder::new()
            .name("sysopt-ai-download".into())
            .spawn(move || {
                let result = run_semantic_download_worker(
                    &configured_model_id,
                    &cache_dir,
                    total_memory_bytes,
                    min_similarity,
                    max_cache_entries,
                    timeout,
                    &worker_cancel,
                );
                let _ = sender.send(result);
            }) {
            Ok(handle) => {
                self.semantic_download_rx = Some(receiver);
                self.semantic_download_cancel = Some(cancel);
                self.semantic_download_handle = Some(handle);
                self.semantic_next_retry = None;
                self.semantic_error = Some(
                    "descarga y validación automática del modelo de IA en segundo plano".into(),
                );
            }
            Err(error) => self.schedule_semantic_retry(format!(
                "no se pudo iniciar el trabajador de descarga de IA: {error}"
            )),
        }
    }

    fn poll_semantic_download(&mut self, total_memory_bytes: u64) {
        let result = match self.semantic_download_rx.as_ref() {
            Some(receiver) => match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(
                    "el trabajador de descarga de IA terminó sin informar un resultado".into(),
                )),
            },
            None => None,
        };
        let Some(result) = result else {
            return;
        };
        self.semantic_download_rx = None;
        self.semantic_download_cancel = None;
        if let Some(handle) = self.semantic_download_handle.take() {
            let _ = handle.join();
        }
        match result {
            Ok(_) => {
                self.semantic_retry_delay_secs = self.config.semantic_retry_initial_secs;
                self.semantic_next_retry = None;
                if !self.try_load_cached_semantic_model(total_memory_bytes) {
                    self.schedule_semantic_retry(
                        "el modelo se descargó, pero no pudo cargarse desde la caché validada"
                            .into(),
                    );
                }
            }
            Err(error) => self.schedule_semantic_retry(format!(
                "falló la descarga o validación automática del modelo de IA: {error}"
            )),
        }
    }

    fn schedule_semantic_retry(&mut self, error: String) {
        let (delay, next_delay) = bounded_retry_schedule(
            self.semantic_retry_delay_secs,
            self.config.semantic_retry_initial_secs,
            self.config.semantic_retry_max_secs,
        );
        self.semantic_next_retry = Some(Instant::now() + Duration::from_secs(delay));
        self.semantic_retry_delay_secs = next_delay;
        self.semantic_error = Some(format!(
            "{error}; se mantienen reglas y aprendizaje local; reintento automático en {delay}s"
        ));
    }

    fn semantic_retry_in_secs(&self) -> Option<u64> {
        self.semantic_next_retry
            .map(|deadline| deadline.saturating_duration_since(Instant::now()).as_secs())
    }

    #[allow(clippy::too_many_arguments)]
    fn score_process(
        &self,
        process: &ProcessSample,
        rank: usize,
        mode: SystemMode,
        snapshot: &SystemSnapshot,
        categories: &[String],
        is_foreground: bool,
        semantic_hint: Option<&SemanticHint>,
        rules_matched: bool,
    ) -> f32 {
        let key = process_key(process);
        let memory = self.state.process_profiles.get(&key);
        let total_memory = snapshot.total_memory_bytes.max(1) as f32;
        let cpu = (process.cpu_percent / 100.0).clamp(0.0, 1.0);
        let io = ((process.read_bytes.saturating_add(process.written_bytes)) as f64
            / (64.0 * 1024.0 * 1024.0))
            .min(1.0) as f32;
        let memory_ratio = (process.memory_bytes as f32 / total_memory).clamp(0.0, 1.0);
        let rank_bonus = match rank {
            0 => 0.18,
            1 => 0.11,
            2 => 0.07,
            _ => 0.0,
        };
        let category_prior = category_relevance(mode, categories);
        let rule_category_signal = if rules_matched { category_prior } else { 0.0 };
        let semantic_category_signal =
            semantic_hint.map_or(0.0, |hint| category_prior * hint.confidence.clamp(0.0, 1.0));
        let history = memory.map_or(0.35, |memory| {
            (memory.ema_relevance * 0.55
                + memory.persistence * 0.25
                + memory.ema_cpu * 0.10
                + memory.ema_io * 0.10)
                .clamp(0.0, 1.0)
        });
        let foreground_bonus = if is_foreground { 0.24 } else { 0.0 };
        let pressure_penalty =
            if percent(snapshot.available_memory_bytes, snapshot.total_memory_bytes) < 12.0 {
                memory_ratio * 0.22
            } else {
                memory_ratio * 0.08
            };

        (0.28 * cpu
            + 0.20 * io
            + 0.24 * rule_category_signal
            + self.config.semantic_weight * semantic_category_signal
            + 0.20 * history
            + rank_bonus
            + foreground_bonus
            - pressure_penalty)
            .clamp(0.0, 1.0)
    }

    fn process_confidence(
        &self,
        process: &ProcessSample,
        categories: &[String],
        is_foreground: bool,
        semantic_hint: Option<&SemanticHint>,
        rules_matched: bool,
    ) -> f32 {
        let observations = self
            .state
            .process_profiles
            .get(&process_key(process))
            .map_or(0, |memory| memory.observations);
        let history_confidence = (observations as f32 / 24.0).min(1.0);
        let category_confidence = if rules_matched {
            0.75
        } else if let Some(hint) = semantic_hint {
            0.25 + hint.confidence * 0.55
        } else if categories.is_empty() {
            0.15
        } else {
            0.35
        };
        let foreground_confidence = if is_foreground { 0.12 } else { 0.0 };
        (0.30 + history_confidence * 0.45 + category_confidence * 0.25 + foreground_confidence)
            .clamp(0.0, 1.0)
    }

    fn learn_process(
        &mut self,
        process: &ProcessSample,
        mode: SystemMode,
        categories: &[String],
        predicted_score: f32,
        semantic_hint: Option<&SemanticHint>,
    ) {
        let learning_rate = self.config.learning_rate;
        let key = process_key(process);
        let io = ((process.read_bytes.saturating_add(process.written_bytes)) as f64
            / (64.0 * 1024.0 * 1024.0))
            .min(1.0) as f32;
        let cpu = (process.cpu_percent / 100.0).clamp(0.0, 1.0);
        let semantic_factor =
            semantic_hint.map_or(1.0, |hint| (0.45 + hint.confidence * 0.55).clamp(0.0, 1.0));
        let observed_relevance = (cpu * 0.34
            + io * 0.32
            + category_relevance(mode, categories) * 0.24 * semantic_factor
            + if predicted_score > 0.55 { 0.10 } else { 0.0 })
        .clamp(0.0, 1.0);
        let memory = self.state.process_profiles.entry(key).or_default();
        memory.observations = memory.observations.saturating_add(1);
        memory.ema_cpu = ema(memory.ema_cpu, cpu, learning_rate);
        memory.ema_io = ema(memory.ema_io, io, learning_rate);
        memory.ema_relevance = ema(memory.ema_relevance, observed_relevance, learning_rate);
        memory.persistence = ema(memory.persistence, 1.0, learning_rate * 0.6);
        memory.last_seen_unix_secs = unix_now_secs();
    }

    fn observe_roots(&mut self, roots: &[PathBuf]) {
        let now = unix_now_secs();
        let active = roots
            .iter()
            .filter_map(|path| fs::canonicalize(path).ok())
            .collect::<HashSet<_>>();

        for path in &active {
            let key = path.to_string_lossy().into_owned();
            let was_active = self.active_roots_last_cycle.contains(path);
            let memory = self.state.root_profiles.entry(key).or_default();
            memory.observations = memory.observations.saturating_add(1);
            memory.active_streak = if was_active {
                memory.active_streak.saturating_add(1)
            } else {
                1
            };
            let reward = if was_active { 1.0 } else { 0.62 };
            memory.utility = ema(memory.utility, reward, self.config.learning_rate);
            memory.last_seen_unix_secs = now;
        }

        for (key, memory) in &mut self.state.root_profiles {
            if !active
                .iter()
                .any(|path| path.to_string_lossy() == key.as_str())
            {
                memory.active_streak = 0;
                memory.utility *= self.config.history_decay;
            }
        }
        self.active_roots_last_cycle = active;
    }

    fn cache_advice(
        &self,
        mode: SystemMode,
        snapshot: &SystemSnapshot,
        roots: &[PathBuf],
    ) -> CacheAdvice {
        let available = percent(snapshot.available_memory_bytes, snapshot.total_memory_bytes);
        let mut allow = !matches!(
            mode,
            SystemMode::HeavyForeground | SystemMode::Containerized
        );
        let mut reason = if mode == SystemMode::Gaming {
            "precarga breve de recursos del juego mientras hay margen".to_owned()
        } else {
            "precarga adaptativa habilitada".to_owned()
        };
        if snapshot.global_cpu_percent >= 88.0 {
            allow = false;
            reason = "CPU muy alta; la IA reserva recursos para la carga actual".into();
        } else if available <= 10.0 {
            allow = false;
            reason = "RAM libre crítica; la IA pausa SmartCache".into();
        } else if mode == SystemMode::Gaming && snapshot.global_cpu_percent >= 45.0 {
            allow = false;
            reason = "el juego ya está bajo carga; SmartCache libera el disco".into();
        }

        let pressure = ((snapshot.global_cpu_percent / 100.0) * 0.55
            + ((100.0 - available) / 100.0) * 0.45)
            .clamp(0.0, 1.0);
        let mode_multiplier = match mode {
            SystemMode::Idle => 1.35,
            SystemMode::Developing | SystemMode::MobileDevelopment => 1.10,
            SystemMode::Creative => 1.05,
            SystemMode::Streaming => 0.65,
            SystemMode::Interactive | SystemMode::Unknown => 0.85,
            SystemMode::Gaming | SystemMode::HeavyForeground | SystemMode::Containerized => 0.35,
        };
        let mut budget_multiplier = (mode_multiplier * (1.15 - pressure)).clamp(0.20, 1.40);
        let scan_interval_multiplier = (0.65 + pressure * 1.85).clamp(0.60, 2.50);

        let mut root_scores = BTreeMap::new();
        for root in roots {
            let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            let key = canonical.to_string_lossy();
            let memory = self.state.root_profiles.get(key.as_ref());
            let score = memory.map_or(0.50, |memory| {
                let observations = (memory.observations as f32 / 20.0).min(1.0);
                let streak = (memory.active_streak as f32 / 8.0).min(1.0);
                (memory.utility * 0.55 + observations * 0.25 + streak * 0.20).clamp(0.0, 1.0)
            });
            root_scores.insert(canonical, score);
        }

        let extension_scores = extension_preferences(mode);
        let confidence = if root_scores.is_empty() {
            0.35
        } else {
            let learned = root_scores
                .keys()
                .filter(|path| {
                    self.state
                        .root_profiles
                        .contains_key(path.to_string_lossy().as_ref())
                })
                .count() as f32
                / root_scores.len() as f32;
            (0.45 + learned * 0.45 + (1.0 - pressure) * 0.10).clamp(0.0, 1.0)
        };
        // Durante el aprendizaje inicial se usa una fracción conservadora del
        // presupuesto. La capacidad aumenta sola al crecer la confianza.
        budget_multiplier = (budget_multiplier * (0.55 + confidence * 0.45)).clamp(0.20, 1.40);
        if allow && confidence < 0.55 {
            reason = "la IA está aprendiendo; precarga conservadora".into();
        }

        CacheAdvice {
            allow_warmup: allow,
            budget_multiplier,
            scan_interval_multiplier,
            confidence,
            root_scores,
            extension_scores,
            reason,
        }
    }

    fn decay_memories(&mut self) {
        let decay = self.config.history_decay;
        for memory in self.state.process_profiles.values_mut() {
            memory.persistence *= decay;
            memory.ema_relevance *= 0.9995;
        }
        for memory in self.state.root_profiles.values_mut() {
            memory.utility *= 0.9995;
        }
    }

    fn trim_state(&mut self) {
        trim_map_by_last_seen(
            &mut self.state.process_profiles,
            self.config.max_process_profiles,
            |memory| memory.last_seen_unix_secs,
        );
        trim_map_by_last_seen(
            &mut self.state.root_profiles,
            self.config.max_root_profiles,
            |memory| memory.last_seen_unix_secs,
        );
    }

    fn persist_if_due(&mut self) {
        let due = self.last_persist.is_none_or(|last| {
            last.elapsed() >= Duration::from_secs(self.config.persist_interval_secs)
        });
        if due {
            let _ = self.persist(false);
        }
    }

    fn persist(&mut self, force: bool) -> Result<()> {
        if !self.config.persist_state {
            return Ok(());
        }
        if !force
            && self.last_persist.is_some_and(|last| {
                last.elapsed() < Duration::from_secs(self.config.persist_interval_secs)
            })
        {
            return Ok(());
        }
        let Some(path) = self.config.state_path.as_deref() else {
            return Ok(());
        };
        write_json_atomic(path, &self.state)?;
        self.last_persist = Some(Instant::now());
        Ok(())
    }
}

impl Drop for AdaptiveIntelligence {
    fn drop(&mut self) {
        if let Some(cancel) = self.semantic_download_cancel.take() {
            cancel.store(true, AtomicOrdering::SeqCst);
        }
        self.semantic_download_rx = None;
        if let Some(handle) = self.semantic_download_handle.take() {
            let _ = handle.join();
        }
        let _ = self.persist(true);
    }
}

fn fallback_cache_advice(
    mode: SystemMode,
    _snapshot: &SystemSnapshot,
    roots: &[PathBuf],
) -> CacheAdvice {
    let allow = !matches!(
        mode,
        SystemMode::HeavyForeground | SystemMode::Containerized | SystemMode::Gaming
    );
    CacheAdvice {
        allow_warmup: allow,
        budget_multiplier: 1.0,
        scan_interval_multiplier: 1.0,
        confidence: 1.0,
        root_scores: roots.iter().cloned().map(|path| (path, 0.5)).collect(),
        extension_scores: extension_preferences(mode),
        reason: if allow {
            "reglas deterministas permiten precarga".into()
        } else {
            format!("modo {mode} reserva el disco")
        },
    }
}

fn extension_preferences(mode: SystemMode) -> BTreeMap<String, f32> {
    let groups: &[(&[&str], f32)] = match mode {
        SystemMode::Developing => &[
            (
                &[
                    "rs", "toml", "lock", "json", "ts", "tsx", "js", "py", "go", "c", "cpp", "h",
                ],
                1.35,
            ),
            (&["md", "yaml", "yml", "xml"], 1.12),
        ],
        SystemMode::MobileDevelopment => &[
            (
                &["kt", "kts", "java", "gradle", "xml", "jar", "class"],
                1.40,
            ),
            (&["json", "yaml", "toml"], 1.10),
        ],
        SystemMode::Creative => &[
            (
                &[
                    "blend", "psd", "aep", "prproj", "drp", "wav", "mp3", "png", "jpg", "jpeg",
                ],
                1.35,
            ),
            (&["dll", "so", "dylib", "pak", "dat"], 1.12),
        ],
        SystemMode::Streaming => &[
            (&["json", "yaml", "ini", "dll", "so", "dylib"], 1.10),
            (&["mp4", "mkv", "mov", "wav"], 0.72),
        ],
        SystemMode::Gaming => &[
            (
                &[
                    "exe", "dll", "so", "dylib", "pak", "bin", "dat", "shader", "cache",
                ],
                1.28,
            ),
            (&["mp4", "mkv", "mov"], 0.75),
        ],
        _ => &[(
            &["exe", "dll", "so", "dylib", "json", "db", "sqlite", "dat"],
            1.08,
        )],
    };
    let mut preferences = BTreeMap::new();
    for (extensions, weight) in groups {
        for extension in *extensions {
            preferences.insert((*extension).to_owned(), *weight);
        }
    }
    preferences
}

fn has_category(categories: &[String], category: &str) -> bool {
    categories.iter().any(|value| value == category)
}

fn semantic_category_supports_cache(category: &str) -> bool {
    matches!(
        category,
        "ide"
            | "language_server"
            | "build_tool"
            | "mobile_tooling"
            | "container_vm"
            | "game"
            | "creative"
            | "media_encoder"
            | "streaming"
    )
}

fn process_root(process: &ProcessSample) -> Option<PathBuf> {
    if let Some(cwd) = process.cwd.as_ref().filter(|path| path.is_dir()) {
        return Some(cwd.clone());
    }
    process
        .executable
        .as_ref()
        .and_then(|path| path.parent())
        .filter(|path| path.is_dir())
        .map(Path::to_path_buf)
}

fn deduplicate_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|path| {
        let normalized = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        seen.insert(normalized)
    });
}

fn category_relevance(mode: SystemMode, categories: &[String]) -> f32 {
    let has = |category| has_category(categories, category);
    let score: f32 = match mode {
        SystemMode::Developing => {
            if has("ide") || has("build_tool") || has("language_server") {
                1.0
            } else if has("browser") {
                0.58
            } else {
                0.28
            }
        }
        SystemMode::MobileDevelopment => {
            if has("mobile_tooling") || has("ide") || has("build_tool") {
                1.0
            } else {
                0.25
            }
        }
        SystemMode::Containerized => {
            if has("container_vm") {
                1.0
            } else {
                0.25
            }
        }
        SystemMode::Gaming => {
            if has("game") {
                1.0
            } else if has("game_launcher") {
                0.72
            } else {
                0.22
            }
        }
        SystemMode::Creative => {
            if has("creative") {
                1.0
            } else if has("media_encoder") {
                0.85
            } else {
                0.25
            }
        }
        SystemMode::Streaming => {
            if has("streaming") {
                1.0
            } else if has("communication") {
                0.68
            } else {
                0.25
            }
        }
        SystemMode::HeavyForeground => 0.55,
        SystemMode::Interactive => {
            if has("browser") || has("communication") {
                0.72
            } else {
                0.45
            }
        }
        SystemMode::Idle | SystemMode::Unknown => 0.30,
    };
    score.clamp(0.0, 1.0)
}

fn is_safe_background_candidate(categories: &[String]) -> bool {
    has_category(categories, "background")
        || has_category(categories, "updater")
        || has_category(categories, "indexer")
        || has_category(categories, "game_launcher")
}

fn is_protected_process(name: &str, categories: &[String]) -> bool {
    if has_category(categories, "protected") {
        return true;
    }
    let normalized = name.to_ascii_lowercase();
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

#[allow(clippy::too_many_arguments)]
fn explain_process_decision(
    process: &ProcessSample,
    mode: SystemMode,
    categories: &[String],
    score: f32,
    confidence: f32,
    priority: Priority,
    under_pressure: bool,
    is_foreground: bool,
    semantic_hint: Option<&SemanticHint>,
) -> String {
    let mut factors = Vec::new();
    if is_foreground {
        factors.push("aplicación en primer plano".into());
    }
    if process.cpu_percent >= 25.0 {
        factors.push(format!("CPU {:.0}%", process.cpu_percent));
    }
    let io_mib = process.read_bytes.saturating_add(process.written_bytes) as f64 / 1_048_576.0;
    if io_mib >= 1.0 {
        factors.push(format!("E/S {:.1} MiB", io_mib));
    }
    if !categories.is_empty() {
        factors.push(format!("categorías {}", categories.join(",")));
    }
    if let Some(hint) = semantic_hint {
        factors.push(format!(
            "semántica {} {:.0}%",
            hint.category,
            hint.similarity * 100.0
        ));
    }
    if under_pressure {
        factors.push("sistema bajo presión".into());
    }
    if factors.is_empty() {
        factors.push("actividad moderada".into());
    }
    format!(
        "modo {mode}; {}; puntuación {:.0}%, confianza {:.0}%, prioridad {:?}",
        factors.join("; "),
        score * 100.0,
        confidence * 100.0,
        priority
    )
}

fn hardware_boost_limit() -> usize {
    let threads = std::thread::available_parallelism().map_or(4, |value| value.get());
    match threads {
        0..=4 => 1,
        5..=8 => 2,
        9..=16 => 3,
        _ => 4,
    }
}

fn manageable(process: &ProcessSample) -> bool {
    process.pid > 4 && process.pid != std::process::id()
}

fn process_key(process: &ProcessSample) -> String {
    let executable = process
        .executable
        .as_ref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or(&process.name);
    executable.trim().to_ascii_lowercase()
}

fn ema(previous: f32, observed: f32, alpha: f32) -> f32 {
    if previous == 0.0 {
        observed
    } else {
        previous * (1.0 - alpha) + observed * alpha
    }
}

fn percent(part: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (part as f64 * 100.0 / total as f64) as f32
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn trim_map_by_last_seen<T>(
    map: &mut HashMap<String, T>,
    max_entries: usize,
    last_seen: impl Fn(&T) -> u64,
) {
    if map.len() <= max_entries {
        return;
    }
    let mut ranked = map
        .iter()
        .map(|(key, value)| (key.clone(), last_seen(value)))
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(_, seen)| std::cmp::Reverse(*seen));
    let keep = ranked
        .into_iter()
        .take(max_entries)
        .map(|(key, _)| key)
        .collect::<HashSet<_>>();
    map.retain(|key, _| keep.contains(key));
}

fn run_semantic_download_worker(
    configured_model_id: &str,
    cache_dir: &Path,
    total_memory_bytes: u64,
    min_similarity: f32,
    max_cache_entries: usize,
    timeout: Duration,
    cancel: &AtomicBool,
) -> std::result::Result<(), String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("no se pudo localizar el ejecutable actual: {error}"))?;
    let mut child = Command::new(executable)
        .arg("--internal-ai-prefetch")
        .arg("--internal-ai-model-id")
        .arg(configured_model_id)
        .arg("--internal-ai-cache-dir")
        .arg(cache_dir)
        .arg("--internal-ai-total-memory")
        .arg(total_memory_bytes.to_string())
        .arg("--internal-ai-min-similarity")
        .arg(min_similarity.to_string())
        .arg("--internal-ai-max-cache-entries")
        .arg(max_cache_entries.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("no se pudo iniciar el subproceso de descarga: {error}"))?;

    // Drenar stderr en paralelo evita que un mensaje largo de TLS/Hugging Face
    // llene el pipe y bloquee al hijo antes de que `try_wait` observe su salida.
    // Solo se conserva una cola acotada para no convertir errores remotos en un
    // crecimiento de memoria no limitado.
    let mut stderr_reader = child
        .stderr
        .take()
        .map(|pipe| thread::spawn(move || collect_bounded_stderr(pipe)));

    let started = Instant::now();
    loop {
        if cancel.load(AtomicOrdering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = join_stderr_reader(&mut stderr_reader);
            return Err("descarga de IA cancelada durante el apagado".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let stderr = join_stderr_reader(&mut stderr_reader);
                if status.success() {
                    return Ok(());
                }
                let detail = stderr.trim();
                return Err(if detail.is_empty() {
                    format!("el subproceso de IA terminó con {status}")
                } else {
                    format!("el subproceso de IA terminó con {status}: {detail}")
                });
            }
            Ok(None) if started.elapsed() < timeout => {
                thread::sleep(Duration::from_millis(250));
            }
            Ok(None) => {
                let kill_result = child.kill();
                let wait_result = child.wait();
                let stderr = join_stderr_reader(&mut stderr_reader);
                // El hijo puede terminar por sí mismo entre `try_wait` y
                // `kill`. Si terminó correctamente, no se convierte esa carrera
                // en un fallo falso. Si el kill tuvo éxito, el timeout sigue
                // siendo la causa reportada.
                if kill_result.is_err() && wait_result.as_ref().is_ok_and(|status| status.success())
                {
                    return Ok(());
                }
                if let Err(error) = wait_result {
                    return Err(format!(
                        "la descarga superó {timeout:?}; no se pudo esperar al subproceso: {error}"
                    ));
                }
                if let Err(error) = kill_result {
                    let detail = stderr.trim();
                    return Err(if detail.is_empty() {
                        format!("la descarga superó {timeout:?} y no pudo cancelarse: {error}")
                    } else {
                        format!("la descarga superó {timeout:?} y no pudo cancelarse: {error}; {detail}")
                    });
                }
                return Err(format!(
                    "la descarga superó el límite de {} segundos y fue cancelada",
                    timeout.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_stderr_reader(&mut stderr_reader);
                return Err(format!("no se pudo consultar el subproceso de IA: {error}"));
            }
        }
    }
}

fn collect_bounded_stderr(mut pipe: impl Read) -> String {
    const MAX_CAPTURE_BYTES: usize = 64 * 1024;
    let mut captured = Vec::with_capacity(4096);
    let mut buffer = [0u8; 4096];
    loop {
        let read = match pipe.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        captured.extend_from_slice(&buffer[..read]);
        if captured.len() > MAX_CAPTURE_BYTES {
            let excess = captured.len() - MAX_CAPTURE_BYTES;
            captured.drain(..excess);
        }
    }
    String::from_utf8_lossy(&captured).into_owned()
}

fn join_stderr_reader(reader: &mut Option<std::thread::JoinHandle<String>>) -> String {
    let Some(reader) = reader.take() else {
        return String::new();
    };
    match reader.join() {
        Ok(stderr) => stderr,
        Err(_) => "no se pudo recopilar stderr del subproceso".into(),
    }
}

fn bounded_retry_schedule(current: u64, initial: u64, maximum: u64) -> (u64, u64) {
    let delay = current.max(initial).min(maximum);
    let next = delay.saturating_mul(2).min(maximum);
    (delay, next)
}

fn load_state(path: &Path) -> Result<PersistedState> {
    let metadata = regular_state_metadata(path)?;
    anyhow::ensure!(
        metadata.len() <= 16 * 1024 * 1024,
        "estado de inteligencia demasiado grande"
    );
    let lock_path = sidecar_lock_path(path);
    let lock = open_state_lock(&lock_path)?;
    FileExt::lock_shared(&lock)
        .with_context(|| format!("no se pudo bloquear {}", path.display()))?;
    let result = (|| -> Result<PersistedState> {
        let metadata = regular_state_metadata(path)?;
        anyhow::ensure!(
            metadata.len() <= 16 * 1024 * 1024,
            "estado de inteligencia demasiado grande"
        );
        let mut file = open_state_read(path)?;
        let mut content = String::with_capacity(metadata.len() as usize);
        file.read_to_string(&mut content)
            .with_context(|| format!("no se pudo leer {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("estado de inteligencia inválido: {}", path.display()))
    })();
    FileExt::unlock(&lock)
        .with_context(|| format!("no se pudo liberar {}", lock_path.display()))?;
    result
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    ensure_private_parent(path)?;
    let lock_path = sidecar_lock_path(path);
    let lock = open_state_lock(&lock_path)?;
    FileExt::lock_exclusive(&lock)
        .with_context(|| format!("no se pudo bloquear {}", path.display()))?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let temp = path.with_extension(format!("tmp-{}-{unique}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(value)?;
    let result = (|| -> Result<()> {
        validate_optional_state_file(path)?;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temp)
            .with_context(|| format!("no se pudo crear {}", temp.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        replace_state_file(&temp, path)?;
        set_private_file_permissions(path)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    FileExt::unlock(&lock)
        .with_context(|| format!("no se pudo liberar {}", lock_path.display()))?;
    result
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

fn regular_state_metadata(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "se rechazó una ruta de estado que no es un archivo regular: {}",
        path.display()
    );
    Ok(metadata)
}

fn validate_optional_state_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink() && metadata.is_file(),
            "se rechazó reemplazar una ruta de estado que no es un archivo regular: {}",
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

fn open_state_read(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    Ok(options.open(path)?)
}

fn open_state_lock(path: &Path) -> Result<File> {
    validate_optional_state_file(path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
        .open(path)
        .with_context(|| format!("no se pudo abrir {}", path.display()))
}

fn sidecar_lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "state".into(), |value| value.to_os_string());
    name.push(".lock");
    path.with_file_name(name)
}

fn ensure_private_parent(path: &Path) -> Result<()> {
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

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, cpu: f32, io: u64) -> ProcessSample {
        ProcessSample {
            pid: 100,
            start_time: 1,
            name: name.into(),
            cpu_percent: cpu,
            memory_bytes: 128 * 1024 * 1024,
            read_bytes: io,
            written_bytes: 0,
            cwd: None,
            executable: None,
        }
    }

    #[test]
    fn config_default_es_valida() {
        IntelligenceConfig::default().validate().unwrap();
    }

    #[test]
    fn backoff_semantico_respeta_inicio_y_maximo() {
        assert_eq!(bounded_retry_schedule(0, 30, 3600), (30, 60));
        assert_eq!(bounded_retry_schedule(60, 30, 3600), (60, 120));
        assert_eq!(bounded_retry_schedule(2400, 30, 3600), (2400, 3600));
        assert_eq!(bounded_retry_schedule(3600, 30, 3600), (3600, 3600));
    }

    #[test]
    fn stderr_del_hijo_se_acota_y_conserva_la_cola() {
        let mut bytes = vec![b'a'; 70 * 1024];
        bytes.extend_from_slice(b"-final");
        let captured = collect_bounded_stderr(std::io::Cursor::new(bytes));
        assert!(captured.len() <= 64 * 1024);
        assert!(captured.ends_with("-final"));
    }

    #[test]
    fn ema_converge_hacia_observacion() {
        assert!((ema(0.5, 1.0, 0.2) - 0.6).abs() < 0.001);
    }

    #[test]
    fn proceso_activo_supera_proceso_inactivo() {
        let engine = AdaptiveIntelligence::new(IntelligenceConfig::default()).unwrap();
        let snapshot = SystemSnapshot {
            global_cpu_percent: 50.0,
            used_memory_bytes: 4,
            available_memory_bytes: 4,
            total_memory_bytes: 8,
            process_count: 2,
            top_processes: vec![],
            io_processes: vec![],
            pressure: Default::default(),
            power: Default::default(),
        };
        let sigs = SignatureDb::embedded_default();
        let active = sample("cargo", 70.0, 32 * 1024 * 1024);
        let quiet = sample("unknown", 1.0, 0);
        let active_categories = sigs
            .categorize(&active.name)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let quiet_categories = sigs
            .categorize(&quiet.name)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(
            engine.score_process(
                &active,
                0,
                SystemMode::Developing,
                &snapshot,
                &active_categories,
                false,
                None,
                !active_categories.is_empty(),
            ) > engine.score_process(
                &quiet,
                5,
                SystemMode::Developing,
                &snapshot,
                &quiet_categories,
                false,
                None,
                !quiet_categories.is_empty(),
            )
        );
    }
    #[test]
    fn incluye_procesos_intensivos_en_io_y_limita_por_familia() {
        let config = IntelligenceConfig {
            semantic_enabled: false,
            ..IntelligenceConfig::default()
        };
        let mut engine = AdaptiveIntelligence::new(config).unwrap();
        let mut first = sample("cs2", 8.0, 64 * 1024 * 1024);
        first.pid = 101;
        let mut second = first.clone();
        second.pid = 102;
        second.start_time = 2;
        let snapshot = SystemSnapshot {
            global_cpu_percent: 30.0,
            used_memory_bytes: 4 * 1024 * 1024 * 1024,
            available_memory_bytes: 12 * 1024 * 1024 * 1024,
            total_memory_bytes: 16 * 1024 * 1024 * 1024,
            process_count: 2,
            top_processes: vec![],
            io_processes: vec![first, second],
            pressure: Default::default(),
            power: Default::default(),
        };
        let signatures = SignatureDb::embedded_default();
        let resources = ResourcePolicyConfig::default();
        let mut decision = engine
            .decide(
                SystemMode::Gaming,
                &snapshot,
                &signatures,
                &resources,
                &[],
                None,
            )
            .unwrap();
        for _ in 0..3 {
            decision = engine
                .decide(
                    SystemMode::Gaming,
                    &snapshot,
                    &signatures,
                    &resources,
                    &[],
                    None,
                )
                .unwrap();
        }
        let boosted = decision
            .actions
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    Action::SetProcessPriority {
                        priority: Priority::AboveNormal | Priority::High,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(boosted, 1);
        assert!(decision
            .report
            .process_decisions
            .iter()
            .any(|item| item.name == "cs2"));
    }

    #[test]
    fn smartcache_de_juego_solo_actua_con_margen() {
        let engine = AdaptiveIntelligence::new(IntelligenceConfig::default()).unwrap();
        let base = SystemSnapshot {
            global_cpu_percent: 20.0,
            used_memory_bytes: 4,
            available_memory_bytes: 12,
            total_memory_bytes: 16,
            process_count: 0,
            top_processes: vec![],
            io_processes: vec![],
            pressure: Default::default(),
            power: Default::default(),
        };
        assert!(
            engine
                .cache_advice(SystemMode::Gaming, &base, &[])
                .allow_warmup
        );
        let busy = SystemSnapshot {
            global_cpu_percent: 60.0,
            ..base
        };
        assert!(
            !engine
                .cache_advice(SystemMode::Gaming, &busy, &[])
                .allow_warmup
        );
    }

    #[test]
    fn primer_plano_aumenta_score_y_confianza() {
        let engine = AdaptiveIntelligence::new(IntelligenceConfig::default()).unwrap();
        let snapshot = SystemSnapshot {
            global_cpu_percent: 25.0,
            used_memory_bytes: 4,
            available_memory_bytes: 12,
            total_memory_bytes: 16,
            process_count: 1,
            top_processes: vec![],
            io_processes: vec![],
            pressure: Default::default(),
            power: Default::default(),
        };
        let process = sample("unknown-app", 5.0, 0);
        let categories = Vec::new();
        let background = engine.score_process(
            &process,
            4,
            SystemMode::Interactive,
            &snapshot,
            &categories,
            false,
            None,
            false,
        );
        let foreground = engine.score_process(
            &process,
            4,
            SystemMode::Interactive,
            &snapshot,
            &categories,
            true,
            None,
            false,
        );
        assert!(foreground > background);
        assert!(
            engine.process_confidence(&process, &categories, true, None, false)
                > engine.process_confidence(&process, &categories, false, None, false)
        );
    }
}
