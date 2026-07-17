use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const READ_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SmartCacheConfig {
    pub enabled: bool,
    pub roots: Vec<PathBuf>,
    pub auto_process_roots: bool,
    pub restrict_auto_roots_to_home: bool,
    pub persist_state: bool,
    pub state_path: Option<PathBuf>,
    pub max_hot_roots: usize,
    pub scan_interval_secs: u64,
    pub cooldown_secs: u64,
    pub max_depth: usize,
    pub max_scan_entries: usize,
    pub max_file_size_bytes: u64,
    pub bytes_per_cycle: u64,
    pub adaptive_budget: bool,
    pub min_bytes_per_cycle: u64,
    pub min_available_memory_percent: u8,
    pub max_cpu_percent: f32,
    pub pressure_guard_enabled: bool,
    pub max_memory_some_pressure_avg10: f32,
    pub max_io_some_pressure_avg10: f32,
    pub include_extensions: Vec<String>,
    pub exclude_directories: Vec<String>,
}

impl Default for SmartCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            roots: Vec::new(),
            auto_process_roots: true,
            restrict_auto_roots_to_home: true,
            persist_state: true,
            state_path: None,
            max_hot_roots: 32,
            scan_interval_secs: 60,
            cooldown_secs: 900,
            max_depth: 6,
            max_scan_entries: 20_000,
            max_file_size_bytes: 128 * 1024 * 1024,
            bytes_per_cycle: 256 * 1024 * 1024,
            adaptive_budget: true,
            min_bytes_per_cycle: 32 * 1024 * 1024,
            min_available_memory_percent: 25,
            max_cpu_percent: 55.0,
            pressure_guard_enabled: true,
            max_memory_some_pressure_avg10: 6.0,
            max_io_some_pressure_avg10: 15.0,
            include_extensions: default_extensions(),
            exclude_directories: vec![
                ".git".into(),
                ".svn".into(),
                ".hg".into(),
                ".cache".into(),
                ".gradle".into(),
                ".idea".into(),
                ".next".into(),
                ".nuxt".into(),
                ".pytest_cache".into(),
                ".tox".into(),
                ".venv".into(),
                "__pycache__".into(),
                "$recycle.bin".into(),
                "build".into(),
                "coverage".into(),
                "dist".into(),
                "node_modules".into(),
                "out".into(),
                "system volume information".into(),
                "target".into(),
                "tmp".into(),
                "temp".into(),
                "venv".into(),
            ],
        }
    }
}

impl SmartCacheConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=256).contains(&self.max_hot_roots),
            "cache.max_hot_roots debe estar entre 1 y 256"
        );
        if let Some(path) = &self.state_path {
            anyhow::ensure!(
                !path.as_os_str().is_empty(),
                "cache.state_path no puede estar vacío"
            );
        }
        anyhow::ensure!(
            self.scan_interval_secs > 0,
            "cache.scan_interval_secs debe ser mayor que 0"
        );
        anyhow::ensure!(
            self.cooldown_secs > 0,
            "cache.cooldown_secs debe ser mayor que 0"
        );
        anyhow::ensure!(
            (1..=32).contains(&self.max_depth),
            "cache.max_depth debe estar entre 1 y 32"
        );
        anyhow::ensure!(
            self.max_scan_entries > 0,
            "cache.max_scan_entries debe ser mayor que 0"
        );
        anyhow::ensure!(
            self.max_file_size_bytes > 0,
            "cache.max_file_size_bytes debe ser mayor que 0"
        );
        anyhow::ensure!(
            self.bytes_per_cycle > 0,
            "cache.bytes_per_cycle debe ser mayor que 0"
        );
        anyhow::ensure!(
            self.min_bytes_per_cycle > 0,
            "cache.min_bytes_per_cycle debe ser mayor que 0"
        );
        anyhow::ensure!(
            self.min_bytes_per_cycle <= self.bytes_per_cycle,
            "cache.min_bytes_per_cycle no puede superar cache.bytes_per_cycle"
        );
        anyhow::ensure!(
            (1..=90).contains(&self.min_available_memory_percent),
            "cache.min_available_memory_percent debe estar entre 1 y 90"
        );
        anyhow::ensure!(
            self.max_cpu_percent.is_finite() && (1.0..=100.0).contains(&self.max_cpu_percent),
            "cache.max_cpu_percent debe estar entre 1 y 100"
        );
        for (name, value) in [
            (
                "max_memory_some_pressure_avg10",
                self.max_memory_some_pressure_avg10,
            ),
            (
                "max_io_some_pressure_avg10",
                self.max_io_some_pressure_avg10,
            ),
        ] {
            anyhow::ensure!(
                value.is_finite() && (0.0..=100.0).contains(&value),
                "cache.{name} debe estar entre 0 y 100"
            );
        }
        anyhow::ensure!(
            self.include_extensions
                .iter()
                .all(|value| !normalize_extension(value).is_empty()),
            "cache.include_extensions contiene una extensión vacía"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CacheTuning {
    pub budget_multiplier: f32,
    pub scan_interval_multiplier: f32,
    pub root_scores: BTreeMap<PathBuf, f32>,
    pub extension_scores: BTreeMap<String, f32>,
    pub decision_reason: Option<String>,
}

impl Default for CacheTuning {
    fn default() -> Self {
        Self::neutral()
    }
}

impl CacheTuning {
    pub fn neutral() -> Self {
        Self {
            budget_multiplier: 1.0,
            scan_interval_multiplier: 1.0,
            root_scores: BTreeMap::new(),
            extension_scores: BTreeMap::new(),
            decision_reason: None,
        }
    }

    fn normalized(&self) -> Self {
        let mut value = self.clone();
        value.budget_multiplier = if value.budget_multiplier.is_finite() {
            value.budget_multiplier.clamp(0.10, 2.0)
        } else {
            1.0
        };
        value.scan_interval_multiplier = if value.scan_interval_multiplier.is_finite() {
            value.scan_interval_multiplier.clamp(0.25, 4.0)
        } else {
            1.0
        };
        value
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CacheConditions {
    pub global_cpu_percent: f32,
    pub available_memory_bytes: u64,
    pub total_memory_bytes: u64,
    pub allow_warmup: bool,
    pub pressure_supported: bool,
    pub memory_some_pressure_avg10: f32,
    pub io_some_pressure_avg10: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WarmReport {
    pub planned_only: bool,
    pub roots_considered: usize,
    pub entries_scanned: usize,
    pub candidates: usize,
    pub files_warmed: usize,
    pub bytes_warmed: u64,
    pub budget_bytes: u64,
    pub files_skipped: usize,
    pub errors: usize,
    pub elapsed_ms: u128,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
    score: f64,
}

#[derive(Debug, Clone)]
struct WarmState {
    modified: Option<SystemTime>,
    warmed_at: Instant,
}

#[derive(Debug, Clone)]
struct HotRoot {
    path: PathBuf,
    last_seen: Instant,
    heat: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedState {
    schema_version: u32,
    roots: Vec<PersistedRoot>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedRoot {
    path: PathBuf,
    heat: u32,
    last_seen_unix_secs: u64,
}

pub struct SmartCache {
    config: SmartCacheConfig,
    home: Option<PathBuf>,
    last_scan: Option<Instant>,
    warmed: HashMap<PathBuf, WarmState>,
    hot_roots: HashMap<PathBuf, HotRoot>,
    last_persist: Option<Instant>,
}

impl SmartCache {
    pub fn new(config: SmartCacheConfig) -> Result<Self> {
        config.validate()?;
        let mut cache = Self {
            config,
            home: canonical_home(),
            last_scan: None,
            warmed: HashMap::new(),
            hot_roots: HashMap::new(),
            last_persist: None,
        };
        cache.load_persisted_roots();
        Ok(cache)
    }

    pub fn config(&self) -> &SmartCacheConfig {
        &self.config
    }

    pub fn update_operating_limits(
        &mut self,
        scan_interval_secs: u64,
        bytes_per_cycle: u64,
        min_available_memory_percent: u8,
        max_cpu_percent: f32,
    ) -> Result<()> {
        self.config.scan_interval_secs = scan_interval_secs;
        self.config.bytes_per_cycle = bytes_per_cycle;
        self.config.min_bytes_per_cycle = self.config.min_bytes_per_cycle.min(bytes_per_cycle);
        self.config.min_available_memory_percent = min_available_memory_percent;
        self.config.max_cpu_percent = max_cpu_percent;
        self.config.validate()?;
        self.last_scan = None;
        Ok(())
    }

    pub fn remember_process_roots<I>(&mut self, roots: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        if !self.config.auto_process_roots {
            return;
        }
        let now = Instant::now();
        let mut changed = false;
        for root in roots {
            let Some(path) = self.accept_auto_root(&root) else {
                continue;
            };
            match self.hot_roots.entry(path.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let root = entry.get_mut();
                    root.last_seen = now;
                    root.heat = root.heat.saturating_add(1).min(1000);
                    changed = true;
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(HotRoot {
                        path,
                        last_seen: now,
                        heat: 1,
                    });
                    changed = true;
                }
            }
        }
        let before = self.hot_roots.len();
        self.hot_roots
            .retain(|_, root| now.duration_since(root.last_seen) < Duration::from_secs(24 * 3600));
        changed |= self.hot_roots.len() != before;
        self.trim_hot_roots();
        if changed {
            self.persist_roots_if_due();
        }
    }

    pub fn tick(&mut self, conditions: CacheConditions, apply: bool) -> Result<Option<WarmReport>> {
        self.tick_with_tuning(conditions, apply, &CacheTuning::neutral())
    }

    pub fn tick_with_tuning(
        &mut self,
        conditions: CacheConditions,
        apply: bool,
        tuning: &CacheTuning,
    ) -> Result<Option<WarmReport>> {
        if !self.config.enabled {
            return Ok(None);
        }
        let tuning = tuning.normalized();
        let now = Instant::now();
        let adaptive_interval = ((self.config.scan_interval_secs as f32)
            * tuning.scan_interval_multiplier)
            .round()
            .max(1.0) as u64;
        if self
            .last_scan
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(adaptive_interval))
        {
            return Ok(None);
        }

        if !conditions.allow_warmup {
            return Ok(Some(WarmReport {
                planned_only: !apply,
                reason: tuning.decision_reason.clone().or_else(|| {
                    Some(
                        "el modo actual reserva el disco para la aplicación en primer plano".into(),
                    )
                }),
                ..WarmReport::default()
            }));
        }
        if conditions.global_cpu_percent > self.config.max_cpu_percent {
            return Ok(Some(WarmReport {
                planned_only: !apply,
                reason: Some(format!(
                    "CPU {:.1}% supera el límite de {:.1}%",
                    conditions.global_cpu_percent, self.config.max_cpu_percent
                )),
                ..WarmReport::default()
            }));
        }
        if self.config.pressure_guard_enabled && conditions.pressure_supported {
            if conditions.memory_some_pressure_avg10 > self.config.max_memory_some_pressure_avg10 {
                return Ok(Some(WarmReport {
                    planned_only: !apply,
                    reason: Some(format!(
                        "PSI memoria some {:.2}% supera {:.2}%",
                        conditions.memory_some_pressure_avg10,
                        self.config.max_memory_some_pressure_avg10
                    )),
                    ..WarmReport::default()
                }));
            }
            if conditions.io_some_pressure_avg10 > self.config.max_io_some_pressure_avg10 {
                return Ok(Some(WarmReport {
                    planned_only: !apply,
                    reason: Some(format!(
                        "PSI E/S some {:.2}% supera {:.2}%",
                        conditions.io_some_pressure_avg10, self.config.max_io_some_pressure_avg10
                    )),
                    ..WarmReport::default()
                }));
            }
        }
        let available_percent = percent(
            conditions.available_memory_bytes,
            conditions.total_memory_bytes,
        );
        if available_percent < f32::from(self.config.min_available_memory_percent) {
            return Ok(Some(WarmReport {
                planned_only: !apply,
                reason: Some(format!(
                    "RAM disponible {:.1}% por debajo del mínimo {}%",
                    available_percent, self.config.min_available_memory_percent
                )),
                ..WarmReport::default()
            }));
        }

        let mut roots = self.effective_roots();
        roots.sort_by(|left, right| {
            let left_score = tuning.root_scores.get(left).copied().unwrap_or(0.5);
            let right_score = tuning.root_scores.get(right).copied().unwrap_or(0.5);
            right_score
                .partial_cmp(&left_score)
                .unwrap_or(Ordering::Equal)
        });
        if roots.is_empty() {
            return Ok(Some(WarmReport {
                planned_only: !apply,
                reason: Some("aún no se detectaron carpetas de trabajo seguras".into()),
                ..WarmReport::default()
            }));
        }
        // Solo se inicia el cooldown cuando realmente se va a escanear. Así,
        // al desaparecer una carga alta, SmartCache puede reaccionar de inmediato.
        self.last_scan = Some(now);
        let base_budget = self.effective_budget(conditions.available_memory_bytes);
        let budget = ((base_budget as f64) * f64::from(tuning.budget_multiplier))
            .round()
            .clamp(
                self.config.min_bytes_per_cycle as f64,
                self.config.bytes_per_cycle as f64,
            ) as u64;
        self.warm_roots(
            &roots,
            apply,
            false,
            budget,
            &tuning.root_scores,
            &tuning.extension_scores,
        )
        .map(Some)
    }

    pub fn warm_paths_now(&mut self, paths: &[PathBuf], apply: bool) -> Result<WarmReport> {
        anyhow::ensure!(
            !paths.is_empty(),
            "se requiere al menos una ruta para precargar"
        );
        let roots = paths
            .iter()
            .filter_map(|path| canonical_non_root(path))
            .collect::<Vec<_>>();
        anyhow::ensure!(!roots.is_empty(), "ninguna ruta es válida o accesible");
        self.warm_roots(
            &roots,
            apply,
            true,
            self.config.bytes_per_cycle,
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
    }

    fn warm_roots(
        &mut self,
        roots: &[PathBuf],
        apply: bool,
        force: bool,
        budget_bytes: u64,
        root_scores: &BTreeMap<PathBuf, f32>,
        extension_scores: &BTreeMap<String, f32>,
    ) -> Result<WarmReport> {
        let started = Instant::now();
        let mut report = WarmReport {
            planned_only: !apply,
            roots_considered: roots.len(),
            budget_bytes,
            ..WarmReport::default()
        };
        let extension_filter = self
            .config
            .include_extensions
            .iter()
            .map(|value| normalize_extension(value))
            .collect::<HashSet<_>>();
        let exclusions = self
            .config
            .exclude_directories
            .iter()
            .map(|value| value.trim().to_ascii_lowercase())
            .collect::<HashSet<_>>();

        let mut candidates = Vec::new();
        for root in roots {
            let learned_root_bonus = root_scores
                .get(root)
                .copied()
                .unwrap_or(0.5)
                .clamp(0.0, 1.0) as f64
                * 420.0;
            self.scan_root(
                root,
                &extension_filter,
                &exclusions,
                extension_scores,
                learned_root_bonus,
                &mut candidates,
                &mut report,
            );
            if report.entries_scanned >= self.config.max_scan_entries {
                break;
            }
        }
        candidates.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.size.cmp(&right.size))
        });
        report.candidates = candidates.len();

        let now = Instant::now();
        for candidate in candidates {
            if report.bytes_warmed.saturating_add(candidate.size) > budget_bytes {
                report.files_skipped += 1;
                continue;
            }
            if !force && self.was_warmed_recently(&candidate, now) {
                report.files_skipped += 1;
                continue;
            }
            if apply {
                match warm_file(&candidate.path, candidate.size) {
                    Ok(bytes) => {
                        report.files_warmed += 1;
                        report.bytes_warmed = report.bytes_warmed.saturating_add(bytes);
                        self.warmed.insert(
                            candidate.path,
                            WarmState {
                                modified: candidate.modified,
                                warmed_at: now,
                            },
                        );
                    }
                    Err(_) => report.errors += 1,
                }
            } else {
                report.files_warmed += 1;
                report.bytes_warmed = report.bytes_warmed.saturating_add(candidate.size);
            }
        }
        self.warmed.retain(|_, state| {
            now.duration_since(state.warmed_at) < Duration::from_secs(7 * 24 * 3600)
        });
        report.elapsed_ms = started.elapsed().as_millis();
        Ok(report)
    }

    fn effective_budget(&self, available_memory_bytes: u64) -> u64 {
        if !self.config.adaptive_budget {
            return self.config.bytes_per_cycle;
        }
        let memory_based = available_memory_bytes / 32;
        memory_based.clamp(self.config.min_bytes_per_cycle, self.config.bytes_per_cycle)
    }

    fn was_warmed_recently(&self, candidate: &Candidate, now: Instant) -> bool {
        self.warmed.get(&candidate.path).is_some_and(|state| {
            state.modified == candidate.modified
                && now.duration_since(state.warmed_at)
                    < Duration::from_secs(self.config.cooldown_secs)
        })
    }

    fn trim_hot_roots(&mut self) {
        if self.hot_roots.len() <= self.config.max_hot_roots {
            return;
        }
        let mut ranked = self
            .hot_roots
            .values()
            .map(|root| (root.path.clone(), root.heat, root.last_seen))
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.2.cmp(&left.2)));
        let keep = ranked
            .into_iter()
            .take(self.config.max_hot_roots)
            .map(|(path, _, _)| path)
            .collect::<HashSet<_>>();
        self.hot_roots.retain(|path, _| keep.contains(path));
    }

    fn load_persisted_roots(&mut self) {
        if !self.config.persist_state {
            return;
        }
        let Some(path) = &self.config.state_path else {
            return;
        };
        let Ok(state) = load_persisted_state(path) else {
            return;
        };
        if state.schema_version != 1 {
            return;
        }
        let now = Instant::now();
        let unix_now = unix_now_secs();
        for root in state.roots {
            if unix_now.saturating_sub(root.last_seen_unix_secs) > 7 * 24 * 3600 {
                continue;
            }
            let Some(path) = self.accept_auto_root(&root.path) else {
                continue;
            };
            self.hot_roots.insert(
                path.clone(),
                HotRoot {
                    path,
                    last_seen: now,
                    heat: root.heat.clamp(1, 1000),
                },
            );
        }
        self.trim_hot_roots();
    }

    fn persist_roots_if_due(&mut self) {
        if !self.config.persist_state {
            return;
        }
        let now = Instant::now();
        if self
            .last_persist
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(60))
        {
            return;
        }
        let Some(path) = self.config.state_path.clone() else {
            return;
        };
        let state = PersistedState {
            schema_version: 1,
            roots: self
                .hot_roots
                .values()
                .map(|root| PersistedRoot {
                    path: root.path.clone(),
                    heat: root.heat,
                    last_seen_unix_secs: unix_now_secs(),
                })
                .collect(),
        };
        if write_persisted_state(&path, &state).is_ok() {
            self.last_persist = Some(now);
        }
    }

    fn effective_roots(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        let mut roots = Vec::new();
        for root in &self.config.roots {
            if let Some(path) = canonical_non_root(root) {
                if seen.insert(path.clone()) {
                    roots.push(path);
                }
            }
        }
        let mut hot = self.hot_roots.values().collect::<Vec<_>>();
        hot.sort_by_key(|root| std::cmp::Reverse(root.heat));
        for root in hot {
            if seen.insert(root.path.clone()) {
                roots.push(root.path.clone());
            }
        }
        roots
    }

    fn accept_auto_root(&self, path: &Path) -> Option<PathBuf> {
        let mut path = canonical_non_root(path)?;
        if self.config.restrict_auto_roots_to_home {
            let home = self.home.as_ref()?;
            if !path.starts_with(home) || path == *home {
                return None;
            }
            path = discover_project_root(&path, home);
            if path == *home {
                return None;
            }
        }
        Some(path)
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_root(
        &self,
        root: &Path,
        extensions: &HashSet<String>,
        exclusions: &HashSet<String>,
        extension_scores: &BTreeMap<String, f32>,
        root_bonus: f64,
        candidates: &mut Vec<Candidate>,
        report: &mut WarmReport,
    ) {
        let root_metadata = match fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(_) => {
                report.errors += 1;
                return;
            }
        };
        if metadata_is_indirect(&root_metadata) {
            report.files_skipped += 1;
            return;
        }
        if root_metadata.is_file() {
            report.entries_scanned += 1;
            self.consider_metadata_with_bonus(
                root.to_path_buf(),
                root_metadata,
                extensions,
                extension_scores,
                candidates,
                report,
                0,
                1000.0 + root_bonus,
            );
            return;
        }
        if !root_metadata.is_dir() {
            report.files_skipped += 1;
            return;
        }

        let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
        while let Some((directory, depth)) = queue.pop_front() {
            if report.entries_scanned >= self.config.max_scan_entries {
                return;
            }
            let directory_metadata = match fs::symlink_metadata(&directory) {
                Ok(metadata) => metadata,
                Err(_) => {
                    report.errors += 1;
                    continue;
                }
            };
            if metadata_is_indirect(&directory_metadata) || !directory_metadata.is_dir() {
                report.files_skipped += 1;
                continue;
            }
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(_) => {
                    report.errors += 1;
                    continue;
                }
            };
            for entry in entries.flatten() {
                if report.entries_scanned >= self.config.max_scan_entries {
                    return;
                }
                report.entries_scanned += 1;
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(_) => {
                        report.errors += 1;
                        continue;
                    }
                };
                if metadata_is_indirect(&metadata) {
                    report.files_skipped += 1;
                    continue;
                }
                if metadata.is_dir() {
                    if depth < self.config.max_depth && !is_excluded_dir(&path, exclusions) {
                        queue.push_back((path, depth + 1));
                    }
                    continue;
                }
                if metadata.is_file() {
                    self.consider_metadata_with_bonus(
                        path,
                        metadata,
                        extensions,
                        extension_scores,
                        candidates,
                        report,
                        depth,
                        root_bonus,
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn consider_metadata_with_bonus(
        &self,
        path: PathBuf,
        metadata: fs::Metadata,
        extensions: &HashSet<String>,
        extension_scores: &BTreeMap<String, f32>,
        candidates: &mut Vec<Candidate>,
        report: &mut WarmReport,
        depth: usize,
        root_bonus: f64,
    ) {
        let size = metadata.len();
        if size == 0 || size > self.config.max_file_size_bytes {
            report.files_skipped += 1;
            return;
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(normalize_extension)
            .unwrap_or_default();
        if !extensions.is_empty() && !extensions.contains(&extension) {
            report.files_skipped += 1;
            return;
        }
        let modified = metadata.modified().ok();
        let recency = modified
            .and_then(|value| SystemTime::now().duration_since(value).ok())
            .map(recency_score)
            .unwrap_or(0.0);
        let size_score = (self.config.max_file_size_bytes.saturating_sub(size) as f64
            / self.config.max_file_size_bytes as f64)
            * 100.0;
        let depth_penalty = depth as f64 * 4.0;
        let extension_multiplier = extension_scores
            .get(&extension)
            .copied()
            .unwrap_or(1.0)
            .clamp(0.25, 2.0) as f64;
        candidates.push(Candidate {
            path,
            size,
            modified,
            score: root_bonus + (recency + size_score) * extension_multiplier - depth_penalty,
        });
    }
}

fn warm_file(path: &Path, expected_size: u64) -> Result<u64> {
    let file =
        open_sequential(path).with_context(|| format!("no se pudo abrir {}", path.display()))?;
    let opened_metadata = file
        .metadata()
        .with_context(|| format!("no se pudo verificar {} después de abrirlo", path.display()))?;
    anyhow::ensure!(
        opened_metadata.is_file() && !metadata_is_indirect(&opened_metadata),
        "la ruta dejó de ser un archivo regular directo: {}",
        path.display()
    );
    anyhow::ensure!(
        opened_metadata.len() == expected_size,
        "el archivo cambió durante el escaneo (esperado {expected_size}, actual {}): {}",
        opened_metadata.len(),
        path.display()
    );
    advise_will_need(&file, expected_size);
    // Una única memoria intermedia es suficiente: `BufReader` duplicaba el
    // búfer de 1 MiB para una lectura estrictamente secuencial. `take` además
    // limita la lectura frente a cambios posteriores del archivo.
    let mut reader = file.take(expected_size);
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    let mut total = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("no se pudo precargar {}", path.display()))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
    }
    anyhow::ensure!(
        total == expected_size,
        "el archivo se truncó durante la precarga (esperado {expected_size}, leído {total}): {}",
        path.display()
    );
    Ok(total)
}

#[cfg(windows)]
fn open_sequential(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(unix)]
fn open_sequential(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_sequential(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn advise_will_need(file: &File, length: u64) {
    use std::os::fd::AsRawFd;
    let length = length.min(i64::MAX as u64) as libc::off_t;
    // Es solo una sugerencia al kernel. Un fallo no invalida la lectura normal.
    unsafe {
        let _ = libc::posix_fadvise(file.as_raw_fd(), 0, length, libc::POSIX_FADV_WILLNEED);
    }
}

#[cfg(target_os = "macos")]
fn advise_will_need(file: &File, length: u64) {
    use std::os::fd::AsRawFd;
    let advice = libc::radvisory {
        ra_offset: 0,
        ra_count: length.min(i32::MAX as u64) as libc::c_int,
    };
    // F_RDADVISE es no vinculante: la lectura secuencial sigue siendo el fallback.
    unsafe {
        let _ = libc::fcntl(file.as_raw_fd(), libc::F_RDADVISE, &advice);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn advise_will_need(_file: &File, _length: u64) {}

fn canonical_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .and_then(|path| fs::canonicalize(path).ok())
}

fn canonical_non_root(path: &Path) -> Option<PathBuf> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata_is_indirect(&metadata) {
        return None;
    }
    let canonical = fs::canonicalize(path).ok()?;
    if canonical.parent().is_none() || is_sensitive_system_path(&canonical) {
        return None;
    }
    Some(canonical)
}

fn is_sensitive_system_path(path: &Path) -> bool {
    #[cfg(unix)]
    {
        // Los pseudo-sistemas nunca son candidatos válidos. Para directorios
        // amplios del sistema se rechaza la raíz exacta, pero se permite que un
        // usuario seleccione explícitamente un subárbol concreto (p. ej.
        // /usr/local/src/proyecto) bajo su propia responsabilidad.
        if ["/proc", "/sys", "/dev", "/run"]
            .into_iter()
            .any(|root| path.starts_with(root))
        {
            return true;
        }
        if [
            "/etc",
            "/boot",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/usr",
            "/var",
            "/System",
            "/Library",
            "/Applications",
            "/private",
        ]
        .into_iter()
        .any(|root| path == Path::new(root))
        {
            return true;
        }
    }

    #[cfg(windows)]
    {
        let normalized = path
            .to_string_lossy()
            .trim_end_matches(|c| c == '\\' || c == '/')
            .to_ascii_lowercase();
        if let Some(windows) = std::env::var_os("WINDIR") {
            let windows = PathBuf::from(windows);
            if path.starts_with(&windows) {
                return true;
            }
        }
        for variable in ["ProgramData", "ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(value) = std::env::var_os(variable) {
                let candidate = PathBuf::from(value);
                if normalized
                    == candidate
                        .to_string_lossy()
                        .trim_end_matches(|c| c == '\\' || c == '/')
                        .to_ascii_lowercase()
                {
                    return true;
                }
            }
        }
    }

    false
}

fn discover_project_root(path: &Path, home: &Path) -> PathBuf {
    const MARKERS: [&str; 12] = [
        ".git",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "requirements.txt",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "settings.gradle",
        "CMakeLists.txt",
        "composer.json",
        "pubspec.yaml",
    ];
    let mut current = path.to_path_buf();
    let fallback = current.clone();
    for _ in 0..=6 {
        if current == home {
            break;
        }
        if MARKERS.iter().any(|marker| current.join(marker).exists()) {
            return current;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if !parent.starts_with(home) {
            break;
        }
        current = parent.to_path_buf();
    }
    fallback
}

fn is_excluded_dir(path: &Path, exclusions: &HashSet<String>) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| exclusions.contains(&value.to_ascii_lowercase()))
}

fn normalize_extension(value: &str) -> String {
    value.trim().trim_start_matches('.').to_ascii_lowercase()
}

fn recency_score(age: Duration) -> f64 {
    let hours = age.as_secs_f64() / 3600.0;
    if hours <= 1.0 {
        500.0
    } else if hours <= 24.0 {
        350.0
    } else if hours <= 168.0 {
        200.0
    } else if hours <= 720.0 {
        100.0
    } else {
        10.0
    }
}

fn percent(part: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (part as f64 * 100.0 / total as f64) as f32
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn load_persisted_state(path: &Path) -> Result<PersistedState> {
    ensure_private_parent(path)?;
    let metadata = regular_state_metadata(path)?;
    anyhow::ensure!(
        metadata.len() <= 16 * 1024 * 1024,
        "estado de SmartCache demasiado grande"
    );
    let lock_path = sidecar_lock_path(path);
    let lock = open_state_lock(&lock_path)?;
    FileExt::lock_shared(&lock)
        .with_context(|| format!("no se pudo bloquear {}", path.display()))?;
    let result = (|| -> Result<PersistedState> {
        let metadata = regular_state_metadata(path)?;
        anyhow::ensure!(
            metadata.len() <= 16 * 1024 * 1024,
            "estado de SmartCache demasiado grande"
        );
        let mut file = open_state_read(path)?;
        let mut content = String::with_capacity(metadata.len() as usize);
        file.read_to_string(&mut content)
            .with_context(|| format!("no se pudo leer {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("estado de SmartCache inválido: {}", path.display()))
    })();
    FileExt::unlock(&lock)
        .with_context(|| format!("no se pudo liberar {}", lock_path.display()))?;
    result
}

fn write_persisted_state(path: &Path, state: &PersistedState) -> Result<()> {
    ensure_private_parent(path)?;
    let lock_path = sidecar_lock_path(path);
    let lock = open_state_lock(&lock_path)?;
    FileExt::lock_exclusive(&lock)
        .with_context(|| format!("no se pudo bloquear {}", path.display()))?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let temp = path.with_extension(format!("tmp-{}-{unique}", std::process::id()));
    let encoded = serde_json::to_vec_pretty(state)?;
    let result = (|| -> Result<()> {
        validate_optional_state_file(path)?;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temp)
            .with_context(|| format!("no se pudo crear {}", temp.display()))?;
        file.write_all(&encoded)?;
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

fn metadata_is_indirect(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn validate_state_metadata(metadata: &fs::Metadata, path: &Path) -> Result<()> {
    anyhow::ensure!(
        metadata.is_file() && !metadata_is_indirect(metadata),
        "se rechazó una ruta de estado que no es un archivo regular directo: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "el archivo de estado no pertenece al usuario efectivo: {}",
            path.display()
        );
        anyhow::ensure!(
            metadata.mode() & 0o077 == 0,
            "el archivo de estado tiene permisos demasiado amplios: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_open_state_file(file: &File, path: &Path) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("no se pudo verificar el archivo abierto {}", path.display()))?;
    validate_state_metadata(&metadata, path)
}

fn regular_state_metadata(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
    validate_state_metadata(&metadata, path)?;
    Ok(metadata)
}

fn validate_optional_state_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_state_metadata(&metadata, path)?,
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
    let file = options
        .open(path)
        .with_context(|| format!("no se pudo abrir {}", path.display()))?;
    validate_open_state_file(&file, path)?;
    Ok(file)
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
    let file = options
        .open(path)
        .with_context(|| format!("no se pudo abrir {}", path.display()))?;
    validate_open_state_file(&file, path)?;
    Ok(file)
}

fn sidecar_lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "cache-state".into(), |value| value.to_os_string());
    name.push(".lock");
    path.with_file_name(name)
}

fn validate_state_parent_metadata(metadata: &fs::Metadata, path: &Path) -> Result<()> {
    anyhow::ensure!(
        metadata.is_dir() && !metadata_is_indirect(metadata),
        "el directorio padre no es un directorio regular seguro: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "el directorio de estado no pertenece al usuario efectivo: {}",
            path.display()
        );
        anyhow::ensure!(
            metadata.mode() & 0o022 == 0,
            "el directorio de estado es escribible por grupo u otros: {}",
            path.display()
        );
    }
    Ok(())
}

fn ensure_private_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let existed = match fs::symlink_metadata(parent) {
            Ok(metadata) => {
                validate_state_parent_metadata(&metadata, parent)?;
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
            #[cfg(unix)]
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            let metadata = fs::symlink_metadata(parent)
                .with_context(|| format!("no se pudo verificar {}", parent.display()))?;
            validate_state_parent_metadata(&metadata, parent)?;
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

fn default_extensions() -> Vec<String> {
    [
        "rs", "toml", "lock", "json", "yaml", "yml", "xml", "md", "txt", "js", "mjs", "cjs", "ts",
        "tsx", "jsx", "css", "scss", "html", "py", "pyc", "java", "class", "jar", "kt", "kts",
        "gradle", "c", "cc", "cpp", "h", "hpp", "go", "cs", "dll", "so", "dylib", "exe", "bin",
        "wasm", "pak", "db", "sqlite", "sqlite3", "index", "dat", "model", "onnx", "blend", "psd",
        "aep", "prproj", "drp", "wav",
        // Los formatos comprimidos de audio, vídeo e imagen suelen tener poco
        // beneficio al precargarse genéricamente y pueden consumir gran parte
        // del presupuesto. El usuario todavía puede añadirlos explícitamente.
        "ini", "shader", "cache",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn config_default_es_valida() {
        SmartCacheConfig::default().validate().unwrap();
    }

    #[test]
    fn warmup_respeta_presupuesto() {
        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("sysopt-cache-test-{unique}"));
        fs::create_dir_all(&root).unwrap();
        for index in 0..3 {
            let mut file = File::create(root.join(format!("file-{index}.rs"))).unwrap();
            file.write_all(&vec![1u8; 1024]).unwrap();
        }
        let config = SmartCacheConfig {
            roots: vec![root.clone()],
            bytes_per_cycle: 2048,
            min_bytes_per_cycle: 1024,
            adaptive_budget: false,
            max_file_size_bytes: 4096,
            restrict_auto_roots_to_home: false,
            ..SmartCacheConfig::default()
        };
        let mut cache = SmartCache::new(config).unwrap();
        let report = cache
            .warm_paths_now(std::slice::from_ref(&root), false)
            .unwrap();
        let _ = fs::remove_dir_all(root);
        assert!(report.bytes_warmed <= 2048);
        assert_eq!(report.files_warmed, 2);
    }

    #[test]
    fn detecta_raiz_de_proyecto_sin_subir_hasta_home() {
        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!("sysopt-home-test-{unique}"));
        let project = home.join("workspace").join("demo");
        let nested = project.join("src").join("module");
        fs::create_dir_all(&nested).unwrap();
        File::create(project.join("Cargo.toml")).unwrap();

        let detected = discover_project_root(&nested, &home);
        let _ = fs::remove_dir_all(&home);
        assert_eq!(detected, project);
    }

    #[test]
    fn presupuesto_adaptativo_se_limita() {
        let config = SmartCacheConfig {
            min_bytes_per_cycle: 32 * 1024 * 1024,
            bytes_per_cycle: 256 * 1024 * 1024,
            ..SmartCacheConfig::default()
        };
        let cache = SmartCache::new(config).unwrap();
        assert_eq!(cache.effective_budget(64 * 1024 * 1024), 32 * 1024 * 1024);
        assert_eq!(
            cache.effective_budget(64 * 1024 * 1024 * 1024),
            256 * 1024 * 1024
        );
    }

    #[test]
    fn extensiones_comprimidas_no_se_precargan_por_defecto() {
        let extensions = default_extensions();
        for extension in ["png", "jpg", "mp3", "mp4", "mkv", "webm"] {
            assert!(!extensions.iter().any(|value| value == extension));
        }
        assert!(extensions.iter().any(|value| value == "rs"));
        assert!(extensions.iter().any(|value| value == "onnx"));
    }

    #[test]
    fn warm_file_rechaza_tamano_distinto_al_observado() {
        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sysopt-cache-size-{unique}.rs"));
        fs::write(&path, b"contenido").unwrap();
        let result = warm_file(&path, 1);
        let _ = fs::remove_file(path);
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn no_sigue_enlaces_simbolicos_durante_el_escaneo() {
        use std::os::unix::fs::symlink;

        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("sysopt-cache-link-{unique}"));
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.rs"), vec![1u8; 1024]).unwrap();
        symlink(&outside, root.join("junction")).unwrap();

        let config = SmartCacheConfig {
            roots: vec![root.clone()],
            restrict_auto_roots_to_home: false,
            adaptive_budget: false,
            bytes_per_cycle: 4096,
            min_bytes_per_cycle: 1024,
            ..SmartCacheConfig::default()
        };
        let mut cache = SmartCache::new(config).unwrap();
        let report = cache.warm_paths_now(&[root], false).unwrap();

        let _ = fs::remove_dir_all(base);
        assert_eq!(report.files_warmed, 0);
        assert!(report.files_skipped >= 1);
    }

    #[cfg(unix)]
    #[test]
    fn no_enumera_una_raiz_que_es_enlace_simbolico() {
        use std::os::unix::fs::symlink;

        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("sysopt-cache-root-link-{unique}"));
        let outside = base.join("outside");
        let linked_root = base.join("linked-root");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.rs"), vec![1u8; 1024]).unwrap();
        symlink(&outside, &linked_root).unwrap();

        let config = SmartCacheConfig {
            roots: vec![linked_root.clone()],
            restrict_auto_roots_to_home: false,
            adaptive_budget: false,
            bytes_per_cycle: 4096,
            min_bytes_per_cycle: 1024,
            ..SmartCacheConfig::default()
        };
        let mut cache = SmartCache::new(config).unwrap();
        let result = cache.warm_paths_now(&[linked_root], false);

        let _ = fs::remove_dir_all(base);
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rechaza_directorio_de_estado_escribible_por_otros() {
        use std::os::unix::fs::PermissionsExt;

        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = std::env::temp_dir().join(format!("sysopt-cache-parent-{unique}"));
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        let result = ensure_private_parent(&parent.join("state.json"));
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(parent);
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rechaza_estado_con_permisos_amplios() {
        use std::os::unix::fs::PermissionsExt;

        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sysopt-cache-state-{unique}.json"));
        fs::write(&path, b"{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let result = regular_state_metadata(&path);
        let _ = fs::remove_file(path);
        assert!(result.is_err());
    }

    #[test]
    fn pausa_por_presion_de_memoria_psi() {
        let mut cache = SmartCache::new(SmartCacheConfig::default()).unwrap();
        let report = cache
            .tick(
                CacheConditions {
                    global_cpu_percent: 5.0,
                    available_memory_bytes: 12,
                    total_memory_bytes: 16,
                    allow_warmup: true,
                    pressure_supported: true,
                    memory_some_pressure_avg10: 20.0,
                    io_some_pressure_avg10: 0.0,
                },
                false,
            )
            .unwrap()
            .unwrap();
        assert!(report
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("PSI memoria")));
        assert_eq!(report.files_warmed, 0);
    }

    #[test]
    fn usa_la_explicacion_de_la_ia_al_pausar() {
        let mut cache = SmartCache::new(SmartCacheConfig::default()).unwrap();
        let tuning = CacheTuning {
            decision_reason: Some("decisión adaptativa de prueba".into()),
            ..CacheTuning::neutral()
        };
        let report = cache
            .tick_with_tuning(
                CacheConditions {
                    global_cpu_percent: 10.0,
                    available_memory_bytes: 8,
                    total_memory_bytes: 16,
                    allow_warmup: false,
                    pressure_supported: false,
                    memory_some_pressure_avg10: 0.0,
                    io_some_pressure_avg10: 0.0,
                },
                false,
                &tuning,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            report.reason.as_deref(),
            Some("decisión adaptativa de prueba")
        );
    }
}
