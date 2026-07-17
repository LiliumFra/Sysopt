use crate::cgroup_journal::{GroupControlJournal, GroupControlRecord};
use crate::identity::{self, NativeProcessIdentity, ProcessGuard};
use crate::journal::{ActionJournal, JournalEntry, ResourceChange};
use anyhow::{bail, Context, Result};
use policy::{ResourceGroup, ResourceLimits};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
struct Membership {
    start_time: u64,
    original_cgroup: PathBuf,
    applied_cgroup: PathBuf,
    native_identity: NativeProcessIdentity,
    journal_id: u64,
}

pub struct CgroupV2 {
    root: PathBuf,
    mount_root: PathBuf,
    memberships: HashMap<u32, Membership>,
    control_journal: GroupControlJournal,
    initialized: bool,
}

impl CgroupV2 {
    pub fn new(root: PathBuf, control_journal_path: PathBuf) -> Result<Self> {
        anyhow::ensure!(root.is_absolute(), "la raíz cgroup debe ser absoluta");
        anyhow::ensure!(
            root.components().all(|component| {
                matches!(component, Component::RootDir | Component::Normal(_))
            }),
            "la raíz cgroup contiene componentes inseguros: {}",
            root.display()
        );
        let parent = root
            .parent()
            .context("la raíz cgroup configurada no tiene directorio padre")?
            .to_path_buf();
        ensure_real_directory(&parent)?;
        ensure_regular_control_file(&parent.join("cgroup.controllers"))
            .with_context(|| format!("{} no parece una jerarquía cgroup v2", parent.display()))?;

        let mut mount_root = parent.clone();
        while let Some(ancestor) = mount_root.parent().map(Path::to_path_buf) {
            if ensure_real_directory(&ancestor).is_ok()
                && ensure_regular_control_file(&ancestor.join("cgroup.controllers")).is_ok()
            {
                mount_root = ancestor;
            } else {
                break;
            }
        }

        Ok(Self {
            root,
            mount_root,
            memberships: HashMap::new(),
            control_journal: GroupControlJournal::open(control_journal_path)?,
            initialized: false,
        })
    }

    pub fn assign(
        &mut self,
        pid: u32,
        start_time: u64,
        name: &str,
        group: ResourceGroup,
        limits: ResourceLimits,
        journal: &mut ActionJournal,
    ) -> Result<()> {
        validate_limits(limits)?;
        self.ensure_root()?;
        let guard = ProcessGuard::acquire(pid, start_time)
            .with_context(|| format!("no se pudo fijar identidad nativa de pid {pid} ({name})"))?;
        let native_identity = guard.identity().clone();
        let group_path = self.root.join(group.as_str());
        self.ensure_controllers(limits)?;
        ensure_cgroup_directory(&group_path)?;

        let current_cgroup = self.current_cgroup(pid)?;
        let existing = self.memberships.get(&pid).cloned();
        let existing = match existing {
            Some(membership)
                if membership.start_time == start_time
                    && membership.native_identity == native_identity =>
            {
                if current_cgroup != membership.applied_cgroup {
                    journal.remove(membership.journal_id)?;
                    self.memberships.remove(&pid);
                    bail!(
                        "pid {pid} ({name}) fue movido externamente desde {}; SysOpt abandona su restauración",
                        membership.applied_cgroup.display()
                    );
                }
                Some(membership)
            }
            Some(membership) => {
                journal.remove(membership.journal_id)?;
                self.memberships.remove(&pid);
                None
            }
            None => None,
        };

        if current_cgroup == group_path && existing.is_none() {
            bail!(
                "pid {pid} ({name}) ya pertenece a {} sin una acción activa de SysOpt; no se reclamará una pertenencia externa",
                group_path.display()
            );
        }

        if current_cgroup == group_path {
            self.ensure_group_controls(&group_path, limits)?;
            return Ok(());
        }

        let original_cgroup = existing.as_ref().map_or_else(
            || current_cgroup.clone(),
            |membership| membership.original_cgroup.clone(),
        );
        let journal_id = if let Some(membership) = &existing {
            journal.update_change(
                membership.journal_id,
                ResourceChange::Cgroup {
                    original: original_cgroup.clone(),
                    applied: group_path.clone(),
                    previous_applied: Some(membership.applied_cgroup.clone()),
                },
            )?;
            membership.journal_id
        } else {
            journal.prepare(
                pid,
                start_time,
                name,
                native_identity.clone(),
                "linux_cgroup_v2",
                ResourceChange::Cgroup {
                    original: original_cgroup.clone(),
                    applied: group_path.clone(),
                    previous_applied: None,
                },
            )?
        };

        anyhow::ensure!(
            guard.still_same()?,
            "pid {pid} cambió antes de migrarlo de cgroup"
        );
        let latest_cgroup = self.current_cgroup(pid)?;
        if latest_cgroup != current_cgroup {
            let cleanup_error = if let Some(membership) = &existing {
                match journal.remove(membership.journal_id) {
                    Ok(()) => {
                        self.memberships.remove(&pid);
                        None
                    }
                    Err(error) => Some(error.context(format!(
                        "no se pudo descartar el journal de pid {pid} después de detectar una migración externa"
                    ))),
                }
            } else {
                journal.remove(journal_id).err().map(|error| {
                    error.context(format!(
                        "no se pudo descartar la entrada preparada de pid {pid} después de detectar una migración externa"
                    ))
                })
            };
            let mut error = anyhow::anyhow!(
                "pid {pid} ({name}) fue movido externamente de {} a {} durante la preparación; SysOpt no lo sobrescribirá",
                current_cgroup.display(),
                latest_cgroup.display()
            );
            if let Some(cleanup_error) = cleanup_error {
                error = error.context(format!("además falló la limpieza durable: {cleanup_error}"));
            }
            return Err(error);
        }

        let group_procs = group_path.join("cgroup.procs");
        if let Err(error) = ensure_regular_control_file(&group_procs) {
            return Err(cleanup_new_prepared_entry(
                journal,
                journal_id,
                existing.as_ref(),
                error,
                pid,
            ));
        }
        if let Err(write_error) = fs::write(&group_procs, pid.to_string()).with_context(|| {
            format!(
                "no se pudo mover pid {pid} al cgroup {}",
                group_path.display()
            )
        }) {
            let confirmed_target = match guard.still_same() {
                Ok(false) => false,
                Ok(true) => match self.current_cgroup(pid) {
                    Ok(current) => current == group_path,
                    Err(observe_error) => {
                        return Err(write_error.context(format!(
                            "no se pudo determinar el estado final de pid {pid}; se conserva el journal preparado: {observe_error}"
                        )));
                    }
                },
                Err(identity_error) => {
                    return Err(write_error.context(format!(
                        "no se pudo volver a validar la identidad de pid {pid}; se conserva el journal preparado: {identity_error}"
                    )));
                }
            };
            if !confirmed_target {
                return Err(cleanup_new_prepared_entry(
                    journal,
                    journal_id,
                    existing.as_ref(),
                    write_error,
                    pid,
                ));
            }
        }

        if let Err(control_error) = self.ensure_group_controls(&group_path, limits) {
            let rollback_group = existing
                .as_ref()
                .map_or(&original_cgroup, |membership| &membership.applied_cgroup);
            let rollback_result = self.move_to(pid, rollback_group);
            return match rollback_result {
                Ok(()) => {
                    let journal_result = reconcile_membership_journal(
                        journal,
                        journal_id,
                        existing.as_ref(),
                    );
                    let controls_result = self.restore_group_if_unused(&group_path);
                    match (journal_result, controls_result) {
                        (Ok(()), Ok(())) => Err(control_error).context(format!(
                            "no se pudieron aplicar controles al cgroup {}; la migración fue revertida",
                            group_path.display()
                        )),
                        (journal_error, controls_error) => Err(anyhow::anyhow!(
                            "fallaron controles de {}: {control_error}; la migración fue revertida, pero la reconciliación durable quedó incompleta (journal: {}; controles: {})",
                            group_path.display(),
                            journal_error
                                .err()
                                .map_or_else(|| "ok".to_owned(), |error| error.to_string()),
                            controls_error
                                .err()
                                .map_or_else(|| "ok".to_owned(), |error| error.to_string())
                        )),
                    }
                }
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "fallaron controles de {}: {control_error}; también falló rollback a {} y se conserva el journal preparado: {rollback_error}",
                    group_path.display(),
                    rollback_group.display()
                )),
            };
        }

        if !guard.still_same()? {
            journal.remove(journal_id)?;
            self.memberships.remove(&pid);
            self.restore_group_if_unused(&group_path)?;
            bail!("pid {pid} terminó o cambió de identidad antes de confirmar su cgroup");
        }
        let confirmed_group = self.current_cgroup(pid)?;
        if confirmed_group != group_path {
            journal.remove(journal_id)?;
            self.memberships.remove(&pid);
            self.restore_group_if_unused(&group_path)?;
            bail!(
                "pid {pid} ({name}) fue movido externamente a {} antes de la confirmación durable",
                confirmed_group.display()
            );
        }

        let previous_group = existing
            .as_ref()
            .map(|membership| membership.applied_cgroup.clone());
        let result = self.confirm_assignment(
            pid,
            start_time,
            original_cgroup,
            group_path.clone(),
            native_identity,
            journal_id,
            existing.as_ref(),
            journal,
        );
        if result.is_ok() {
            if let Some(previous_group) = previous_group {
                if previous_group != group_path {
                    self.restore_group_if_unused(&previous_group)?;
                }
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn confirm_assignment(
        &mut self,
        pid: u32,
        start_time: u64,
        original_cgroup: PathBuf,
        group_path: PathBuf,
        native_identity: NativeProcessIdentity,
        journal_id: u64,
        existing: Option<&Membership>,
        journal: &mut ActionJournal,
    ) -> Result<()> {
        if let Err(journal_error) = journal.mark_applied(journal_id) {
            let rollback_group =
                existing.map_or(&original_cgroup, |membership| &membership.applied_cgroup);
            return match self.move_to(pid, rollback_group) {
                Ok(()) => match reconcile_membership_journal(journal, journal_id, existing) {
                    Ok(()) => Err(journal_error).context(format!(
                        "falló la confirmación durable de pid {pid}; la migración cgroup fue revertida"
                    )),
                    Err(reconcile_error) => Err(anyhow::anyhow!(
                        "falló la confirmación durable de pid {pid}: {journal_error}; la migración fue revertida, pero no se pudo reconciliar el journal: {reconcile_error}"
                    )),
                },
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "falló la confirmación durable de pid {pid}: {journal_error}; también falló el rollback a {} y se conserva el journal preparado: {rollback_error}",
                    rollback_group.display()
                )),
            };
        }

        self.memberships.insert(
            pid,
            Membership {
                start_time,
                original_cgroup,
                applied_cgroup: group_path,
                native_identity,
                journal_id,
            },
        );
        Ok(())
    }

    pub fn reset(&mut self, pid: u32, start_time: u64, journal: &mut ActionJournal) -> Result<()> {
        let Some(membership) = self.memberships.get(&pid).cloned() else {
            return Ok(());
        };
        if membership.start_time != start_time {
            journal.remove(membership.journal_id)?;
            self.memberships.remove(&pid);
            self.restore_group_if_unused(&membership.applied_cgroup)?;
            return Ok(());
        }

        let guard = ProcessGuard::acquire(pid, start_time)?;
        if guard.identity() != &membership.native_identity {
            journal.remove(membership.journal_id)?;
            self.memberships.remove(&pid);
            self.restore_group_if_unused(&membership.applied_cgroup)?;
            return Ok(());
        }
        let current_group = self.current_cgroup(pid)?;
        if current_group != membership.applied_cgroup {
            // Respeta una decisión externa posterior.
            journal.remove(membership.journal_id)?;
            self.memberships.remove(&pid);
            self.restore_group_if_unused(&membership.applied_cgroup)?;
            return Ok(());
        }

        anyhow::ensure!(
            guard.still_same()?,
            "pid {pid} cambió antes de restaurar su cgroup"
        );
        let latest_group = self.current_cgroup(pid)?;
        if latest_group != current_group {
            journal.remove(membership.journal_id)?;
            self.memberships.remove(&pid);
            self.restore_group_if_unused(&membership.applied_cgroup)?;
            return Ok(());
        }
        self.move_to(pid, &membership.original_cgroup)?;
        journal.remove(membership.journal_id)?;
        self.memberships.remove(&pid);
        self.restore_group_if_unused(&membership.applied_cgroup)?;
        Ok(())
    }

    /// Devuelve true si se restauró; false si la entrada ya no corresponde al
    /// estado aplicado por SysOpt y debe descartarse sin tocar el proceso.
    pub fn recover_entry(&self, entry: &JournalEntry) -> Result<bool> {
        if entry.backend != "linux_cgroup_v2" {
            return Ok(false);
        }
        if !identity::matches_identity(entry.pid, &entry.native_identity)? {
            return Ok(false);
        }
        let ResourceChange::Cgroup {
            original,
            applied,
            previous_applied,
        } = &entry.change
        else {
            bail!("entrada de recuperación no corresponde a cgroup");
        };
        validate_safe_absolute_path(original)?;
        validate_safe_absolute_path(applied)?;
        if let Some(previous) = previous_applied {
            validate_safe_absolute_path(previous)?;
            anyhow::ensure!(
                previous.starts_with(&self.root),
                "cgroup aplicado anterior fuera de la raíz SysOpt: {}",
                previous.display()
            );
        }
        anyhow::ensure!(
            original.starts_with(&self.mount_root),
            "cgroup original fuera del mount administrable: {}",
            original.display()
        );
        anyhow::ensure!(
            applied.starts_with(&self.root),
            "cgroup aplicado fuera de la raíz SysOpt: {}",
            applied.display()
        );

        let guard = match ProcessGuard::acquire(entry.pid, entry.observed_start_time_secs) {
            Ok(guard) => guard,
            Err(error) if identity::process_missing_error(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        if guard.identity() != &entry.native_identity {
            return Ok(false);
        }
        let current = match self.current_cgroup(entry.pid) {
            Ok(current) => current,
            Err(_) if !guard.still_same()? => return Ok(false),
            Err(error) => return Err(error),
        };
        if &current != applied && previous_applied.as_ref() != Some(&current) {
            // Una escritura que falló antes de mover el proceso puede dejar un
            // destino preparado que nunca llegó a existir. Eso es descartable,
            // no un bloqueo de recuperación.
            return Ok(false);
        }
        if !guard.still_same()? {
            return Ok(false);
        }
        let latest = match self.current_cgroup(entry.pid) {
            Ok(latest) => latest,
            Err(_) if !guard.still_same()? => return Ok(false),
            Err(error) => return Err(error),
        };
        if latest != current {
            return Ok(false);
        }
        ensure_real_directory(original)?;
        ensure_regular_control_file(&original.join("cgroup.procs"))?;
        if let Err(error) = self.move_to(entry.pid, original) {
            if !guard.still_same()? {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(true)
    }

    pub fn discard_journal_ids(&mut self, ids: &[u64]) -> Result<()> {
        let affected = self
            .memberships
            .values()
            .filter(|membership| ids.contains(&membership.journal_id))
            .map(|membership| membership.applied_cgroup.clone())
            .collect::<Vec<_>>();
        self.memberships
            .retain(|_, membership| !ids.contains(&membership.journal_id));
        for group in affected {
            self.restore_group_if_unused(&group)?;
        }
        Ok(())
    }

    pub fn recover_group_controls(&mut self) -> (usize, usize, Vec<String>) {
        let mut restored = 0usize;
        let mut discarded = 0usize;
        let mut failures = Vec::new();
        for record in self.control_journal.records() {
            match self.restore_control_record(&record) {
                Ok(true) => {
                    if let Err(error) = self.control_journal.remove(&record.group_path) {
                        failures.push(format!(
                            "controles {} restaurados pero journal no eliminado: {error}",
                            record.group_path.display()
                        ));
                    } else {
                        restored += 1;
                    }
                }
                Ok(false) => {
                    if let Err(error) = self.control_journal.remove(&record.group_path) {
                        failures.push(format!(
                            "controles {} descartados pero journal no eliminado: {error}",
                            record.group_path.display()
                        ));
                    } else {
                        discarded += 1;
                    }
                }
                Err(error) => failures.push(format!(
                    "controles {}: {error}",
                    record.group_path.display()
                )),
            }
        }
        (restored, discarded, failures)
    }

    fn ensure_controllers(&self, limits: ResourceLimits) -> Result<()> {
        let mut required = Vec::new();
        if limits.cpu_weight.is_some() {
            required.push("cpu");
        }
        if limits.io_weight.is_some() {
            required.push("io");
        }
        if limits.memory_high_bytes.is_some() {
            required.push("memory");
        }
        if required.is_empty() {
            return Ok(());
        }
        let available_path = self.root.join("cgroup.controllers");
        let subtree_path = self.root.join("cgroup.subtree_control");
        ensure_regular_control_file(&available_path)?;
        ensure_regular_control_file(&subtree_path)?;
        let available = fs::read_to_string(&available_path)?;
        for controller in &required {
            anyhow::ensure!(
                available
                    .split_whitespace()
                    .any(|value| value == *controller),
                "controlador cgroup {controller} no está delegado a {}",
                self.root.display()
            );
        }
        let current = fs::read_to_string(&subtree_path)?;
        let missing = required
            .into_iter()
            .filter(|controller| !current.split_whitespace().any(|value| value == *controller))
            .map(|controller| format!("+{controller}"))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            fs::write(&subtree_path, missing.join(" ")).with_context(|| {
                format!(
                    "no se pudieron habilitar controladores en {}; la jerarquía debe estar delegada y sin procesos internos",
                    self.root.display()
                )
            })?;
            let confirmed = fs::read_to_string(&subtree_path)?;
            for controller in available.split_whitespace().filter(|controller| {
                missing
                    .iter()
                    .any(|value| value.strip_prefix('+') == Some(*controller))
            }) {
                anyhow::ensure!(
                    confirmed
                        .split_whitespace()
                        .any(|value| value == controller),
                    "el kernel no confirmó el controlador cgroup {controller} en {}",
                    subtree_path.display()
                );
            }
        }
        Ok(())
    }

    fn ensure_group_controls(&mut self, group_path: &Path, limits: ResourceLimits) -> Result<()> {
        let mut desired = desired_controls(limits);
        let existing_record = self.control_journal.record(group_path);
        if desired.is_empty() && existing_record.is_none() {
            return Ok(());
        }
        if let Some(record) = &existing_record {
            for (name, transition) in &record.controls {
                desired
                    .entry(name.clone())
                    .or_insert_with(|| transition.original.clone());
            }
        }
        let current = read_controls(group_path, desired.keys())?;
        if let Some(record) = &existing_record {
            for (name, transition) in &record.controls {
                let observed = current
                    .get(name)
                    .with_context(|| format!("falta control {name}"))?;
                let accepted = observed == &transition.applied
                    || transition.previous_applied.as_ref() == Some(observed);
                if !accepted {
                    self.control_journal.remove(group_path)?;
                    bail!(
                        "{} cambió externamente de {}; SysOpt abandona su restauración",
                        group_path.join(name).display(),
                        transition.applied
                    );
                }
            }
            if desired.iter().all(|(name, value)| {
                record
                    .controls
                    .get(name)
                    .is_some_and(|transition| &transition.applied == value)
                    && current.get(name) == Some(value)
            }) {
                return Ok(());
            }
        }

        self.control_journal
            .prepare(group_path, &current, &desired)?;
        let previous = current.clone();
        if let Err(error) = write_controls(group_path, &desired) {
            let rollback = write_controls(group_path, &previous);
            return match rollback {
                Ok(()) => match self.reconcile_control_journal(group_path, existing_record.as_ref()) {
                    Ok(()) => Err(error).context("controles cgroup revertidos"),
                    Err(journal_error) => Err(anyhow::anyhow!(
                        "falló aplicar controles cgroup: {error}; los controles fueron revertidos, pero no se pudo reconciliar el journal: {journal_error}"
                    )),
                },
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "falló aplicar controles cgroup: {error}; también falló rollback y se conserva el journal preparado: {rollback_error}"
                )),
            };
        }
        if let Err(error) = self.control_journal.mark_applied(group_path) {
            let rollback = write_controls(group_path, &previous);
            return match rollback {
                Ok(()) => match self.reconcile_control_journal(group_path, existing_record.as_ref()) {
                    Ok(()) => Err(error).context("falló confirmación durable; controles revertidos"),
                    Err(journal_error) => Err(anyhow::anyhow!(
                        "falló confirmación durable: {error}; los controles fueron revertidos, pero no se pudo reconciliar el journal: {journal_error}"
                    )),
                },
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "falló confirmación durable: {error}; también falló rollback y se conserva el journal preparado: {rollback_error}"
                )),
            };
        }
        Ok(())
    }

    fn reconcile_control_journal(
        &mut self,
        group_path: &Path,
        previous: Option<&GroupControlRecord>,
    ) -> Result<()> {
        match previous {
            Some(record) => self.control_journal.restore_record(record),
            None => self.control_journal.remove(group_path),
        }
    }

    fn restore_group_if_unused(&mut self, group_path: &Path) -> Result<()> {
        if self
            .memberships
            .values()
            .any(|membership| membership.applied_cgroup == group_path)
        {
            return Ok(());
        }
        let Some(record) = self.control_journal.record(group_path) else {
            return Ok(());
        };
        match self.restore_control_record(&record)? {
            true => self.control_journal.remove(group_path),
            false => {
                self.control_journal.remove(group_path)?;
                Ok(())
            }
        }
    }

    fn restore_control_record(&self, record: &GroupControlRecord) -> Result<bool> {
        let metadata = match fs::symlink_metadata(&record.group_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Ok(false);
        }
        let current = read_controls(&record.group_path, record.controls.keys())?;
        for (name, transition) in &record.controls {
            let observed = current
                .get(name)
                .with_context(|| format!("falta control {name}"))?;
            if observed != &transition.applied
                && transition.previous_applied.as_ref() != Some(observed)
            {
                return Ok(false);
            }
        }
        let originals = record
            .controls
            .iter()
            .map(|(name, transition)| (name.clone(), transition.original.clone()))
            .collect::<BTreeMap<_, _>>();
        write_controls(&record.group_path, &originals)?;
        Ok(true)
    }

    fn move_to(&self, pid: u32, cgroup: &Path) -> Result<()> {
        let procs = cgroup.join("cgroup.procs");
        ensure_regular_control_file(&procs)?;
        fs::write(&procs, pid.to_string())
            .with_context(|| format!("no se pudo mover pid {pid} al cgroup {}", cgroup.display()))
    }

    fn ensure_root(&mut self) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        ensure_cgroup_directory(&self.root)?;
        self.initialized = true;
        Ok(())
    }

    fn current_cgroup(&self, pid: u32) -> Result<PathBuf> {
        let proc_path = PathBuf::from(format!("/proc/{pid}/cgroup"));
        let content = fs::read_to_string(&proc_path)
            .with_context(|| format!("no se pudo leer {}", proc_path.display()))?;
        let relative = unified_cgroup_path(&content)
            .context("no se encontró la entrada unificada cgroup v2 del proceso")?;

        let relative = Path::new(relative.trim_start_matches('/'));
        anyhow::ensure!(
            relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
            "ruta cgroup v2 inválida para pid {pid}"
        );
        let cgroup = self.mount_root.join(relative);
        ensure_real_directory(&cgroup).with_context(|| {
            format!(
                "el cgroup original de pid {pid} no es accesible: {}",
                cgroup.display()
            )
        })?;
        ensure_regular_control_file(&cgroup.join("cgroup.procs")).with_context(|| {
            format!(
                "el cgroup original de pid {pid} no es accesible: {}",
                cgroup.display()
            )
        })?;
        Ok(cgroup)
    }
}

fn cleanup_new_prepared_entry(
    journal: &mut ActionJournal,
    journal_id: u64,
    existing: Option<&Membership>,
    error: anyhow::Error,
    pid: u32,
) -> anyhow::Error {
    match reconcile_membership_journal(journal, journal_id, existing) {
        Ok(()) => error,
        Err(cleanup_error) => error.context(format!(
            "además no se pudo reconciliar la entrada preparada de pid {pid}: {cleanup_error}"
        )),
    }
}

fn reconcile_membership_journal(
    journal: &mut ActionJournal,
    journal_id: u64,
    existing: Option<&Membership>,
) -> Result<()> {
    if let Some(membership) = existing {
        journal.update_change(
            membership.journal_id,
            ResourceChange::Cgroup {
                original: membership.original_cgroup.clone(),
                applied: membership.applied_cgroup.clone(),
                previous_applied: None,
            },
        )?;
        journal.mark_applied(membership.journal_id)
    } else {
        journal.remove(journal_id)
    }
}

fn validate_safe_absolute_path(path: &Path) -> Result<()> {
    anyhow::ensure!(
        path.is_absolute(),
        "ruta cgroup no absoluta: {}",
        path.display()
    );
    anyhow::ensure!(
        path.components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
        "ruta cgroup insegura: {}",
        path.display()
    );
    Ok(())
}

fn ensure_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "se rechazó un directorio cgroup indirecto o inválido: {}",
        path.display()
    );
    Ok(())
}

fn ensure_regular_control_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "se rechazó un archivo de control cgroup indirecto o inválido: {}",
        path.display()
    );
    Ok(())
}

fn ensure_cgroup_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "se rechazó un cgroup que no es un directorio real: {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let parent = path
                .parent()
                .context("el cgroup a crear no tiene directorio padre")?;
            // No se usa create_dir_all: todos los ancestros deben existir y ser
            // directorios reales antes de crear el único nivel administrado.
            ensure_real_directory(parent)?;
            fs::create_dir(path).with_context(|| format!("no se pudo crear {}", path.display()))?;
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("no se pudo verificar {}", path.display()))?;
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "el cgroup creado no es un directorio real: {}",
                path.display()
            );
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("no se pudo inspeccionar {}", path.display()));
        }
    }
    Ok(())
}

fn unified_cgroup_path(content: &str) -> Option<&str> {
    content.lines().find_map(|line| {
        let mut parts = line.splitn(3, ':');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("0"), Some(""), Some(path)) => Some(path),
            _ => None,
        }
    })
}

fn validate_limits(limits: ResourceLimits) -> Result<()> {
    if let Some(value) = limits.cpu_weight {
        anyhow::ensure!((1..=10_000).contains(&value), "cpu.weight fuera de rango");
    }
    if let Some(value) = limits.io_weight {
        anyhow::ensure!((1..=10_000).contains(&value), "io.weight fuera de rango");
    }
    if let Some(value) = limits.memory_high_bytes {
        anyhow::ensure!(
            value >= 16 * 1024 * 1024,
            "memory.high debe ser al menos 16 MiB"
        );
    }
    Ok(())
}

fn desired_controls(limits: ResourceLimits) -> BTreeMap<String, String> {
    let mut controls = BTreeMap::new();
    if let Some(value) = limits.cpu_weight {
        controls.insert("cpu.weight".into(), value.to_string());
    }
    if let Some(value) = limits.io_weight {
        controls.insert("io.weight".into(), format!("default {value}"));
    }
    if let Some(value) = limits.memory_high_bytes {
        controls.insert("memory.high".into(), value.to_string());
    }
    controls
}

fn read_controls<'a>(
    group_path: &Path,
    names: impl Iterator<Item = &'a String>,
) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for name in names {
        let path = group_path.join(name);
        ensure_regular_control_file(&path)?;
        let raw = fs::read_to_string(&path)?;
        values.insert(name.clone(), normalize_control_value(name, &raw)?);
    }
    Ok(values)
}

fn normalize_control_value(name: &str, raw: &str) -> Result<String> {
    let raw = raw.trim();
    match name {
        "io.weight" => {
            // El kernel devuelve una línea default y opcionalmente overrides por
            // MAJ:MIN. SysOpt solo administra default y nunca reclama overrides.
            if let Some(value) = raw.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                match (fields.next(), fields.next(), fields.next()) {
                    (Some("default"), Some(value), None) => Some(value),
                    _ => None,
                }
            }) {
                let parsed = value.parse::<u16>().context("io.weight default inválido")?;
                anyhow::ensure!((1..=10_000).contains(&parsed), "io.weight fuera de rango");
                return Ok(format!("default {parsed}"));
            }
            if let Ok(parsed) = raw.parse::<u16>() {
                anyhow::ensure!((1..=10_000).contains(&parsed), "io.weight fuera de rango");
                return Ok(format!("default {parsed}"));
            }
            bail!("io.weight no contiene una línea default válida")
        }
        "cpu.weight" => {
            let parsed = raw.parse::<u16>().context("cpu.weight inválido")?;
            anyhow::ensure!((1..=10_000).contains(&parsed), "cpu.weight fuera de rango");
            Ok(parsed.to_string())
        }
        "memory.high" => {
            if raw == "max" {
                return Ok(raw.to_owned());
            }
            let parsed = raw.parse::<u64>().context("memory.high inválido")?;
            Ok(parsed.to_string())
        }
        _ => bail!("control cgroup no permitido: {name}"),
    }
}

fn write_controls(group_path: &Path, values: &BTreeMap<String, String>) -> Result<()> {
    for (name, value) in values {
        let path = group_path.join(name);
        ensure_regular_control_file(&path)?;
        fs::write(&path, value)
            .with_context(|| format!("no se pudo escribir {}={value}", path.display()))?;
        let observed_raw = fs::read_to_string(&path)?;
        let observed = normalize_control_value(name, &observed_raw)?;
        let expected = normalize_control_value(name, value)?;
        anyhow::ensure!(
            observed == expected,
            "el kernel no confirmó {}={expected}; devolvió {observed}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cgroup_membership_journal_reconciles_prepared_entry() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "sysopt-cgroup-membership-journal-{}-{nonce}",
            std::process::id()
        ));
        let path = root.join("journal.json");
        let mut journal = ActionJournal::open(path.clone(), 120).expect("abre journal");
        let id = journal
            .prepare(
                42,
                100,
                "test",
                NativeProcessIdentity::Fallback {
                    start_time_secs: 100,
                },
                "linux_cgroup_v2",
                ResourceChange::Cgroup {
                    original: PathBuf::from("/original"),
                    applied: PathBuf::from("/managed"),
                    previous_applied: None,
                },
            )
            .expect("prepara membresía");
        assert_eq!(journal.entries().len(), 1);
        reconcile_membership_journal(&mut journal, id, None).expect("reconcilia entrada");
        assert!(journal.entries().is_empty());
        drop(journal);
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn extrae_ruta_de_cgroup_v2_unificado() {
        let content = "5:cpu:/legacy\n0::/user.slice/app.scope\n";
        assert_eq!(unified_cgroup_path(content), Some("/user.slice/app.scope"));
        assert_eq!(unified_cgroup_path("2:cpu:/legacy\n"), None);
    }

    #[test]
    fn rechaza_componentes_de_travesia() {
        assert!(validate_safe_absolute_path(Path::new("/sys/fs/cgroup/../etc")).is_err());
    }

    #[test]
    fn valida_y_serializa_controles_compartidos() {
        let limits = ResourceLimits {
            cpu_weight: Some(300),
            io_weight: Some(200),
            memory_high_bytes: Some(512 * 1024 * 1024),
        };
        assert!(validate_limits(limits).is_ok());
        let controls = desired_controls(limits);
        assert_eq!(controls.get("cpu.weight").map(String::as_str), Some("300"));
        assert_eq!(
            controls.get("io.weight").map(String::as_str),
            Some("default 200")
        );
    }

    #[test]
    fn io_weight_preserva_overrides_y_normaliza_solo_default() {
        let raw = "default 175\n8:16 900\n8:0 50\n";
        assert_eq!(
            normalize_control_value("io.weight", raw).unwrap(),
            "default 175"
        );
        assert_eq!(
            normalize_control_value("io.weight", "200").unwrap(),
            "default 200"
        );
    }

    #[test]
    fn cpu_weight_rechaza_cero_segun_cgroup_v2() {
        assert!(normalize_control_value("cpu.weight", "0").is_err());
        assert_eq!(normalize_control_value("cpu.weight", "100").unwrap(), "100");
    }

    #[test]
    fn memory_high_acepta_max_y_valores_numericos() {
        assert_eq!(
            normalize_control_value("memory.high", "max").unwrap(),
            "max"
        );
        assert_eq!(
            normalize_control_value("memory.high", "536870912\n").unwrap(),
            "536870912"
        );
    }
}
