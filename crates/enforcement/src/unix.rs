use crate::identity::{self, NativeProcessIdentity, ProcessGuard};
use crate::journal::{ActionJournal, JournalEntry, ResourceChange};
use crate::{EnforcementConfig, EnforcementReport, RecoveryReport, SystemEnforcer};
use anyhow::{bail, Context, Result};
use policy::{Action, Priority};
use std::collections::HashMap;

#[cfg(target_os = "linux")]
use crate::cgroup_v2::CgroupV2;

fn priority_to_nice(priority: Priority) -> i32 {
    match priority {
        Priority::Idle => 19,
        Priority::BelowNormal => 10,
        Priority::Normal => 0,
        Priority::AboveNormal => -5,
        Priority::High => -10,
    }
}

#[derive(Debug, Clone)]
struct OriginalNice {
    start_time: u64,
    original_nice: i32,
    applied_nice: i32,
    native_identity: NativeProcessIdentity,
    journal_id: u64,
}

pub struct UnixEnforcer {
    original_priorities: HashMap<u32, OriginalNice>,
    can_restore_priorities: bool,
    journal: ActionJournal,
    #[cfg(target_os = "linux")]
    resource_groups_enabled: bool,
    #[cfg(target_os = "linux")]
    linux_cgroup_root: std::path::PathBuf,
    #[cfg(target_os = "linux")]
    cgroup_control_journal_path: std::path::PathBuf,
    #[cfg(target_os = "linux")]
    cgroups: Option<CgroupV2>,
}

impl UnixEnforcer {
    pub fn new(config: EnforcementConfig) -> Result<Self> {
        let journal_path = config
            .journal_path
            .clone()
            .context("journal_path es obligatorio para enforcement real")?;
        let journal = ActionJournal::open(journal_path, config.journal_lease_secs)?;

        #[cfg(target_os = "linux")]
        {
            let linux_cgroup_root = config
                .linux_cgroup_root
                .unwrap_or_else(|| "/sys/fs/cgroup/sysopt".into());
            let cgroup_control_journal_path = config
                .cgroup_control_journal_path
                .context("cgroup_control_journal_path es obligatorio en Linux")?;
            // La recuperación inicializa cgroup de forma perezosa. Así, una
            // entrada de un boot anterior puede descartarse por boot_id aunque
            // la jerarquía cgroup de esa sesión ya no exista.
            let cgroups = None;
            Ok(Self {
                original_priorities: HashMap::new(),
                can_restore_priorities: has_priority_restore_privilege(),
                journal,
                resource_groups_enabled: config.resource_groups_enabled,
                linux_cgroup_root,
                cgroup_control_journal_path,
                cgroups,
            })
        }

        #[cfg(target_os = "macos")]
        {
            if config.resource_groups_enabled {
                bail!("resource groups no están soportados en macOS");
            }
            Ok(Self {
                original_priorities: HashMap::new(),
                can_restore_priorities: has_priority_restore_privilege(),
                journal,
            })
        }
    }

    #[cfg(target_os = "linux")]
    fn ensure_cgroups(&mut self) -> Result<()> {
        if self.cgroups.is_none() {
            self.cgroups = Some(CgroupV2::new(
                self.linux_cgroup_root.clone(),
                self.cgroup_control_journal_path.clone(),
            )?);
        }
        Ok(())
    }

    fn set_priority(
        &mut self,
        pid: u32,
        start_time: u64,
        name: &str,
        priority: Priority,
    ) -> Result<()> {
        if priority != Priority::Normal && !self.can_restore_priorities {
            bail!(
                "pid {pid} ({name}): cambio omitido porque esta sesión no posee privilegios para restaurar la prioridad original de forma garantizada"
            );
        }

        let guard = ProcessGuard::acquire(pid, start_time)
            .with_context(|| format!("no se pudo fijar identidad nativa de pid {pid} ({name})"))?;
        let native_identity = guard.identity().clone();
        let current = get_nice(pid).with_context(|| {
            format!("no se pudo leer la prioridad actual de pid {pid} ({name})")
        })?;

        let existing = self.original_priorities.get(&pid).cloned();
        let existing = match existing {
            Some(original)
                if original.start_time == start_time
                    && original.native_identity == native_identity =>
            {
                if current != original.applied_nice {
                    self.journal.remove(original.journal_id)?;
                    self.original_priorities.remove(&pid);
                    bail!(
                        "pid {pid} ({name}) cambió externamente de nice {} a {current}; SysOpt abandona su restauración",
                        original.applied_nice
                    );
                }
                Some(original)
            }
            Some(original) => {
                self.journal.remove(original.journal_id)?;
                self.original_priorities.remove(&pid);
                None
            }
            None => None,
        };

        if priority == Priority::Normal {
            let Some(original) = existing else {
                // No normalizar arbitrariamente procesos que SysOpt no cambió.
                return Ok(());
            };
            anyhow::ensure!(
                guard.still_same()?,
                "pid {pid} cambió antes de restaurar su prioridad"
            );
            let latest = get_nice(pid).with_context(|| {
                format!("no se pudo volver a leer la prioridad de pid {pid} ({name})")
            })?;
            if latest != current {
                self.journal.remove(original.journal_id)?;
                self.original_priorities.remove(&pid);
                bail!(
                    "pid {pid} ({name}) cambió externamente de nice {current} a {latest} durante la restauración; SysOpt no lo sobrescribirá"
                );
            }
            set_nice(pid, start_time, name, original.original_nice)
                .with_context(|| format!("no se pudo restaurar pid {pid} ({name})"))?;
            self.journal.remove(original.journal_id)?;
            self.original_priorities.remove(&pid);
            return Ok(());
        }

        let target_nice = priority_to_nice(priority);
        if current == target_nice {
            // Si no existía una acción previa, SysOpt no reclama una prioridad
            // que ya había sido elegida por el usuario u otra herramienta.
            return Ok(());
        }

        let (original_nice, journal_id) = if let Some(original) = &existing {
            self.journal.update_change(
                original.journal_id,
                ResourceChange::Priority {
                    original: i64::from(original.original_nice),
                    applied: i64::from(target_nice),
                    previous_applied: Some(i64::from(original.applied_nice)),
                },
            )?;
            (original.original_nice, original.journal_id)
        } else {
            let id = self.journal.prepare(
                pid,
                start_time,
                name,
                native_identity.clone(),
                unix_priority_backend(),
                ResourceChange::Priority {
                    original: i64::from(current),
                    applied: i64::from(target_nice),
                    previous_applied: None,
                },
            )?;
            (current, id)
        };

        anyhow::ensure!(
            guard.still_same()?,
            "pid {pid} cambió antes de aplicar su prioridad"
        );
        let latest = get_nice(pid).with_context(|| {
            format!("no se pudo volver a leer la prioridad de pid {pid} ({name})")
        })?;
        if latest != current {
            if let Some(original) = &existing {
                self.journal.remove(original.journal_id)?;
                self.original_priorities.remove(&pid);
            } else {
                let _ = self.journal.remove(journal_id);
            }
            bail!(
                "pid {pid} ({name}) cambió externamente de nice {current} a {latest} durante la preparación; SysOpt no lo sobrescribirá"
            );
        }
        if let Err(error) = set_nice(pid, start_time, name, target_nice) {
            // Para una acción nueva no existe estado aplicado que conservar. En una
            // transición, en cambio, el journal preparado conserva previous_applied
            // y no debe eliminarse: el proceso continúa bajo la acción anterior.
            if existing.is_none() {
                let _ = self.journal.remove(journal_id);
            }
            return Err(error).with_context(|| format!("pid {pid} ({name})"));
        }
        if let Err(journal_error) = self.journal.mark_applied(journal_id) {
            let rollback_nice = existing
                .as_ref()
                .map_or(original_nice, |original| original.applied_nice);
            return match set_nice(pid, start_time, name, rollback_nice) {
                Ok(()) => {
                    if existing.is_none() {
                        if let Err(cleanup_error) = self.journal.remove(journal_id) {
                            return Err(anyhow::anyhow!(
                                "falló la confirmación durable de pid {pid}: {journal_error}; la prioridad fue revertida, pero no se pudo limpiar la entrada preparada: {cleanup_error}"
                            ));
                        }
                    }
                    Err(journal_error).context(format!(
                        "falló la confirmación durable de pid {pid}; la prioridad fue revertida"
                    ))
                }
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "falló la confirmación durable de pid {pid}: {journal_error}; también falló el rollback a nice {rollback_nice}: {rollback_error}"
                )),
            };
        }
        self.original_priorities.insert(
            pid,
            OriginalNice {
                start_time,
                original_nice,
                applied_nice: target_nice,
                native_identity,
                journal_id,
            },
        );
        Ok(())
    }

    fn recover_priority(&self, entry: &JournalEntry) -> Result<bool> {
        if !identity::matches_identity(entry.pid, &entry.native_identity)? {
            return Ok(false);
        }
        anyhow::ensure!(
            entry.backend == unix_priority_backend(),
            "backend de prioridad incompatible: {}",
            entry.backend
        );
        let ResourceChange::Priority {
            original,
            applied,
            previous_applied,
        } = &entry.change
        else {
            bail!("entrada de recuperación no corresponde a prioridad");
        };
        let original = i32::try_from(*original).context("nice original fuera de rango")?;
        let applied = i32::try_from(*applied).context("nice aplicado fuera de rango")?;
        let previous_applied = previous_applied
            .as_ref()
            .copied()
            .map(i32::try_from)
            .transpose()
            .context("nice aplicado anterior fuera de rango")?;
        anyhow::ensure!(
            (-20..=19).contains(&original),
            "nice original inválido: {original}"
        );
        anyhow::ensure!(
            (-20..=19).contains(&applied),
            "nice aplicado inválido: {applied}"
        );
        anyhow::ensure!(
            previous_applied.is_none_or(|nice| (-20..=19).contains(&nice)),
            "nice aplicado anterior inválido"
        );

        let guard = match ProcessGuard::acquire(entry.pid, entry.observed_start_time_secs) {
            Ok(guard) => guard,
            Err(error) if identity::process_missing_error(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        if guard.identity() != &entry.native_identity {
            return Ok(false);
        }
        let current = match get_nice(entry.pid) {
            Ok(current) => current,
            Err(_) if !guard.still_same()? => return Ok(false),
            Err(error) => return Err(error),
        };
        if current != applied && previous_applied != Some(current) {
            return Ok(false);
        }
        if !guard.still_same()? {
            return Ok(false);
        }
        let latest = match get_nice(entry.pid) {
            Ok(latest) => latest,
            Err(_) if !guard.still_same()? => return Ok(false),
            Err(error) => return Err(error),
        };
        if latest != current {
            return Ok(false);
        }
        if let Err(error) = set_nice(
            entry.pid,
            entry.observed_start_time_secs,
            &entry.name,
            original,
        ) {
            if !guard.still_same()? {
                return Ok(false);
            }
            return Err(error)
                .with_context(|| format!("no se pudo recuperar prioridad de pid {}", entry.pid));
        }
        Ok(true)
    }
}

impl SystemEnforcer for UnixEnforcer {
    fn recover(&mut self) -> RecoveryReport {
        let mut report = RecoveryReport::default();
        // Primero se restauran pertenencias/prioridades por proceso. Los
        // controles compartidos cgroup se revierten después, cuando ningún
        // proceso administrado debería seguir dentro de los grupos SysOpt.
        for entry in self.journal.entries() {
            let outcome = match &entry.change {
                ResourceChange::Priority { .. } => self.recover_priority(&entry),
                ResourceChange::Cgroup { .. } => {
                    #[cfg(target_os = "linux")]
                    {
                        match identity::matches_identity(entry.pid, &entry.native_identity) {
                            Ok(false) => Ok(false),
                            Err(error) => Err(error),
                            Ok(true) => match self.ensure_cgroups() {
                                Ok(()) => self
                                    .cgroups
                                    .as_ref()
                                    .context("backend cgroup no disponible")
                                    .and_then(|cgroups| cgroups.recover_entry(&entry)),
                                Err(error) => Err(error
                                    .context("no se pudo inicializar cgroup para recuperación")),
                            },
                        }
                    }
                    #[cfg(target_os = "macos")]
                    {
                        if identity::belongs_to_current_platform(&entry.native_identity) {
                            Err(anyhow::anyhow!(
                                "journal contiene una entrada cgroup incompatible con macOS"
                            ))
                        } else {
                            Ok(false)
                        }
                    }
                }
                ResourceChange::PowerThrottling { .. } => {
                    if identity::belongs_to_current_platform(&entry.native_identity) {
                        Err(anyhow::anyhow!(
                            "journal contiene una entrada PowerThrottling incompatible con Unix"
                        ))
                    } else {
                        Ok(false)
                    }
                }
            };

            match outcome {
                Ok(restored) => {
                    if let Err(error) = self.journal.remove(entry.id) {
                        report.failures.push(format!(
                            "entrada {} restaurada/descartada pero no eliminada: {error}",
                            entry.id
                        ));
                    } else if restored {
                        report.restored += 1;
                    } else {
                        report.discarded += 1;
                    }
                }
                Err(error) => report.failures.push(format!(
                    "entrada {} pid {} ({}): {error}",
                    entry.id, entry.pid, entry.name
                )),
            }
        }

        #[cfg(target_os = "linux")]
        {
            let controls_exist = self.cgroup_control_journal_path.exists();
            if controls_exist {
                match self.ensure_cgroups() {
                    Ok(()) => {
                        if let Some(cgroups) = self.cgroups.as_mut() {
                            let (restored, discarded, failures) = cgroups.recover_group_controls();
                            report.restored = report.restored.saturating_add(restored);
                            report.discarded = report.discarded.saturating_add(discarded);
                            report.failures.extend(failures);
                        }
                    }
                    Err(error) => report.failures.push(format!(
                        "no se pudo recuperar journal de controles cgroup: {error}"
                    )),
                }
            }
        }
        report
    }

    fn apply(&mut self, actions: &[Action]) -> EnforcementReport {
        let mut report = EnforcementReport::default();

        for action in actions {
            let outcome = match action {
                Action::SetProcessPriority {
                    pid,
                    start_time,
                    name,
                    priority,
                } => self.set_priority(*pid, *start_time, name, *priority),
                Action::SetProcessPowerPolicy {
                    power,
                    timer_resolution,
                    ..
                } => {
                    if *power == policy::ProcessPowerPolicy::SystemManaged
                        && *timer_resolution == policy::TimerResolutionPolicy::SystemManaged
                    {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!(
                            "Power Throttling por proceso solo está disponible en Windows"
                        ))
                    }
                }
                Action::AssignProcessGroup {
                    pid,
                    start_time,
                    name,
                    group,
                    limits,
                } => {
                    #[cfg(target_os = "linux")]
                    {
                        if !self.resource_groups_enabled {
                            Err(anyhow::anyhow!("resource groups están desactivados"))
                        } else if let Err(error) = self.ensure_cgroups() {
                            Err(error.context("no se pudo inicializar el backend cgroup"))
                        } else {
                            match self.cgroups.as_mut() {
                                Some(cgroups) => cgroups.assign(
                                    *pid,
                                    *start_time,
                                    name,
                                    *group,
                                    *limits,
                                    &mut self.journal,
                                ),
                                None => Err(anyhow::anyhow!("backend cgroup no disponible")),
                            }
                        }
                    }
                    #[cfg(target_os = "macos")]
                    {
                        let _ = (pid, start_time, name, group, limits);
                        Err(anyhow::anyhow!(
                            "resource groups no están soportados en macOS"
                        ))
                    }
                }
                Action::ResetProcessGroup {
                    pid, start_time, ..
                } => {
                    #[cfg(target_os = "linux")]
                    {
                        match self.cgroups.as_mut() {
                            Some(cgroups) => cgroups.reset(*pid, *start_time, &mut self.journal),
                            None => Ok(()),
                        }
                    }
                    #[cfg(target_os = "macos")]
                    {
                        let _ = (pid, start_time);
                        Ok(())
                    }
                }
            };
            report.push(action, outcome);
        }

        report
    }

    fn heartbeat(&mut self) -> Result<()> {
        let mut stale_ids = Vec::new();
        for entry in self.journal.entries() {
            if !identity::matches_identity(entry.pid, &entry.native_identity)? {
                stale_ids.push(entry.id);
            }
        }
        if !stale_ids.is_empty() {
            self.journal.remove_many(&stale_ids)?;
            self.original_priorities
                .retain(|_, original| !stale_ids.contains(&original.journal_id));
            #[cfg(target_os = "linux")]
            if let Some(cgroups) = self.cgroups.as_mut() {
                cgroups.discard_journal_ids(&stale_ids)?;
            }
        }
        self.journal.renew_all()
    }

    fn supports_priority_changes(&self) -> bool {
        self.can_restore_priorities
    }
}

fn unix_priority_backend() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "linux_nice"
    }
    #[cfg(target_os = "macos")]
    {
        "macos_nice"
    }
}

fn get_nice(pid: u32) -> Result<i32> {
    clear_errno();
    let value = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid) };
    let errno = current_errno();
    if value == -1 && errno != 0 {
        return Err(std::io::Error::from_raw_os_error(errno).into());
    }
    Ok(value)
}

fn set_nice(pid: u32, start_time: u64, name: &str, nice: i32) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if crate::priority_helper::direct_priority_privilege() {
            let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) };
            if result != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            return Ok(());
        }
        crate::priority_helper::set_nice_via_helper(pid, start_time, name, nice)
    }
    #[cfg(target_os = "macos")]
    {
        let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn has_priority_restore_privilege() -> bool {
    crate::priority_helper::direct_priority_privilege()
        || crate::priority_helper::helper_available()
}

#[cfg(target_os = "macos")]
fn has_priority_restore_privilege() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(target_os = "linux")]
fn clear_errno() {
    unsafe {
        *libc::__errno_location() = 0;
    }
}

#[cfg(target_os = "linux")]
fn current_errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

#[cfg(target_os = "macos")]
fn clear_errno() {
    unsafe {
        *libc::__error() = 0;
    }
}

#[cfg(target_os = "macos")]
fn current_errno() -> i32 {
    unsafe { *libc::__error() }
}
