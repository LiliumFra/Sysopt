use crate::identity::{self, NativeProcessIdentity};
use crate::journal::{ActionJournal, JournalEntry, PowerThrottlingValue, ResourceChange};
use crate::{EnforcementConfig, EnforcementReport, RecoveryReport, SystemEnforcer};
use anyhow::{bail, Context, Result};
use policy::{Action, Priority, ProcessPowerPolicy, TimerResolutionPolicy};
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetPriorityClass, GetProcessInformation, OpenProcess, ProcessPowerThrottling, SetPriorityClass,
    SetProcessInformation, WaitForSingleObject, ABOVE_NORMAL_PRIORITY_CLASS,
    BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, REALTIME_PRIORITY_CLASS,
};

#[derive(Debug, Clone)]
struct OriginalPowerThrottling {
    start_time: u64,
    original: PowerThrottlingValue,
    applied: PowerThrottlingValue,
    native_identity: NativeProcessIdentity,
    journal_id: u64,
}

#[derive(Debug, Clone)]
struct OriginalPriorityClass {
    start_time: u64,
    original_class: u32,
    applied_class: u32,
    native_identity: NativeProcessIdentity,
    journal_id: u64,
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn open(pid: u32) -> Result<Self> {
        const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
        let access =
            PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_ACCESS;
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("OpenProcess falló para pid {pid}"));
        }
        Ok(Self(handle))
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub struct WindowsEnforcer {
    original_priorities: HashMap<u32, OriginalPriorityClass>,
    original_power: HashMap<u32, OriginalPowerThrottling>,
    journal: ActionJournal,
    power_throttling_supported: bool,
}

impl WindowsEnforcer {
    pub fn new(config: EnforcementConfig) -> Result<Self> {
        if config.resource_groups_enabled {
            bail!(
                "resource groups están deshabilitados en Windows: Job Objects no permiten desasociar de forma reversible un proceso vivo"
            );
        }
        let journal_path = config
            .journal_path
            .context("journal_path es obligatorio para enforcement real")?;
        let power_throttling_supported = probe_power_throttling_support();
        Ok(Self {
            original_priorities: HashMap::new(),
            original_power: HashMap::new(),
            journal: ActionJournal::open(journal_path, config.journal_lease_secs)?,
            power_throttling_supported,
        })
    }

    fn set_priority(
        &mut self,
        pid: u32,
        start_time: u64,
        name: &str,
        priority: Priority,
    ) -> Result<()> {
        let handle = OwnedHandle::open(pid)?;
        let native_identity = identity::identity_from_handle(handle.0, start_time)?;
        let current = get_priority_class(handle.0, pid)?;

        let existing = self.original_priorities.get(&pid).cloned();
        let existing = match existing {
            Some(original)
                if original.start_time == start_time
                    && original.native_identity == native_identity =>
            {
                if current != original.applied_class {
                    self.journal.remove(original.journal_id)?;
                    self.original_priorities.remove(&pid);
                    bail!(
                        "pid {pid} ({name}) cambió externamente de clase 0x{:x} a 0x{current:x}; SysOpt abandona su restauración",
                        original.applied_class
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
                return Ok(());
            };
            let latest = get_priority_class(handle.0, pid)?;
            if latest != current {
                self.journal.remove(original.journal_id)?;
                self.original_priorities.remove(&pid);
                bail!(
                    "pid {pid} ({name}) cambió externamente de clase 0x{current:x} a 0x{latest:x} durante la restauración; SysOpt no lo sobrescribirá"
                );
            }
            set_priority_class(handle.0, pid, current, original.original_class)?;
            self.journal.remove(original.journal_id)?;
            self.original_priorities.remove(&pid);
            return Ok(());
        }

        let target = priority_to_class(priority);
        if current == target {
            return Ok(());
        }

        let (original_class, journal_id) = if let Some(original) = &existing {
            self.journal.update_change(
                original.journal_id,
                ResourceChange::Priority {
                    original: i64::from(original.original_class),
                    applied: i64::from(target),
                    previous_applied: Some(i64::from(original.applied_class)),
                },
            )?;
            (original.original_class, original.journal_id)
        } else {
            let id = self.journal.prepare(
                pid,
                start_time,
                name,
                native_identity.clone(),
                "windows_priority_class",
                ResourceChange::Priority {
                    original: i64::from(current),
                    applied: i64::from(target),
                    previous_applied: None,
                },
            )?;
            (current, id)
        };

        let latest = get_priority_class(handle.0, pid)?;
        if latest != current {
            if let Some(original) = &existing {
                self.journal.remove(original.journal_id)?;
                self.original_priorities.remove(&pid);
            } else {
                let _ = self.journal.remove(journal_id);
            }
            bail!(
                "pid {pid} ({name}) cambió externamente de clase 0x{current:x} a 0x{latest:x} durante la preparación; SysOpt no lo sobrescribirá"
            );
        }
        if let Err(error) = set_priority_class(handle.0, pid, current, target) {
            // Una transición fallida deja vigente la clase anterior administrada.
            // Conservamos el journal preparado con previous_applied para no perder
            // la restauración durable de esa acción previa.
            if existing.is_none() {
                let _ = self.journal.remove(journal_id);
            }
            return Err(error);
        }
        if let Err(journal_error) = self.journal.mark_applied(journal_id) {
            let rollback_class = existing
                .as_ref()
                .map_or(original_class, |original| original.applied_class);
            return match set_priority_class(handle.0, pid, target, rollback_class) {
                Ok(()) => {
                    if existing.is_none() {
                        if let Err(cleanup_error) = self.journal.remove(journal_id) {
                            return Err(anyhow::anyhow!(
                                "falló la confirmación durable de pid {pid}: {journal_error}; la clase fue revertida, pero no se pudo limpiar la entrada preparada: {cleanup_error}"
                            ));
                        }
                    }
                    Err(journal_error).context(format!(
                        "falló la confirmación durable de pid {pid}; la clase fue revertida"
                    ))
                }
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "falló la confirmación durable de pid {pid}: {journal_error}; también falló el rollback a 0x{rollback_class:x}: {rollback_error}"
                )),
            };
        }
        self.original_priorities.insert(
            pid,
            OriginalPriorityClass {
                start_time,
                original_class,
                applied_class: target,
                native_identity,
                journal_id,
            },
        );
        Ok(())
    }

    fn set_power_policy(
        &mut self,
        pid: u32,
        start_time: u64,
        name: &str,
        power: ProcessPowerPolicy,
        timer_resolution: TimerResolutionPolicy,
    ) -> Result<()> {
        anyhow::ensure!(
            self.power_throttling_supported,
            "Power Throttling/EcoQoS no está disponible o no puede leerse de forma reversible en esta versión de Windows"
        );
        let handle = OwnedHandle::open(pid)?;
        let native_identity = identity::identity_from_handle(handle.0, start_time)?;
        let current = get_power_throttling(handle.0, pid)?;

        let existing = self.original_power.get(&pid).cloned();
        let existing = match existing {
            Some(original)
                if original.start_time == start_time
                    && original.native_identity == native_identity =>
            {
                if current != original.applied {
                    self.journal.remove(original.journal_id)?;
                    self.original_power.remove(&pid);
                    bail!(
                        "pid {pid} ({name}) cambió externamente Power Throttling; SysOpt abandona su restauración"
                    );
                }
                Some(original)
            }
            Some(original) => {
                self.journal.remove(original.journal_id)?;
                self.original_power.remove(&pid);
                None
            }
            None => None,
        };

        if power == ProcessPowerPolicy::SystemManaged
            && timer_resolution == TimerResolutionPolicy::SystemManaged
        {
            let Some(original) = existing else {
                return Ok(());
            };
            let latest = get_power_throttling(handle.0, pid)?;
            if latest != current {
                self.journal.remove(original.journal_id)?;
                self.original_power.remove(&pid);
                bail!(
                    "pid {pid} ({name}) cambió externamente Power Throttling durante la restauración"
                );
            }
            set_power_throttling(handle.0, pid, original.original)?;
            self.journal.remove(original.journal_id)?;
            self.original_power.remove(&pid);
            return Ok(());
        }

        let target = compose_power_target(current, power, timer_resolution);
        if current == target {
            return Ok(());
        }

        let (original_value, journal_id) = if let Some(original) = &existing {
            self.journal.update_change(
                original.journal_id,
                ResourceChange::PowerThrottling {
                    original: original.original,
                    applied: target,
                    previous_applied: Some(original.applied),
                },
            )?;
            (original.original, original.journal_id)
        } else {
            let id = self.journal.prepare(
                pid,
                start_time,
                name,
                native_identity.clone(),
                "windows_power_throttling",
                ResourceChange::PowerThrottling {
                    original: current,
                    applied: target,
                    previous_applied: None,
                },
            )?;
            (current, id)
        };

        let latest = get_power_throttling(handle.0, pid)?;
        if latest != current {
            if let Some(original) = &existing {
                self.journal.remove(original.journal_id)?;
                self.original_power.remove(&pid);
            } else {
                let _ = self.journal.remove(journal_id);
            }
            bail!("pid {pid} ({name}) cambió externamente Power Throttling durante la preparación");
        }
        if let Err(apply_error) = set_power_throttling(handle.0, pid, target) {
            // SetProcessInformation puede haber aceptado la escritura y fallar
            // después durante la verificación. Nunca descartamos el journal
            // hasta confirmar que el estado anterior fue restaurado.
            let rollback = set_power_throttling(handle.0, pid, current);
            return match rollback {
                Ok(()) => {
                    let journal_cleanup = if let Some(original) = &existing {
                        self.journal
                            .update_change(
                                original.journal_id,
                                ResourceChange::PowerThrottling {
                                    original: original.original,
                                    applied: original.applied,
                                    previous_applied: None,
                                },
                            )
                            .and_then(|()| self.journal.mark_applied(original.journal_id))
                    } else {
                        self.journal.remove(journal_id)
                    };
                    match journal_cleanup {
                        Ok(()) => Err(apply_error).context(format!(
                            "falló la transición de Power Throttling para pid {pid}; se restauró el estado anterior"
                        )),
                        Err(journal_error) => Err(anyhow::anyhow!(
                            "falló la transición de Power Throttling para pid {pid}: {apply_error}; se restauró el estado anterior, pero no se pudo reconciliar el journal: {journal_error}"
                        )),
                    }
                }
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "falló la transición de Power Throttling para pid {pid}: {apply_error}; también falló el rollback al estado anterior y se conservó el journal preparado: {rollback_error}"
                )),
            };
        }
        if let Err(journal_error) = self.journal.mark_applied(journal_id) {
            let rollback = existing
                .as_ref()
                .map_or(original_value, |value| value.applied);
            return match set_power_throttling(handle.0, pid, rollback) {
                Ok(()) => {
                    if existing.is_none() {
                        if let Err(cleanup_error) = self.journal.remove(journal_id) {
                            return Err(anyhow::anyhow!(
                                "falló la confirmación durable de EcoQoS para pid {pid}: {journal_error}; el estado fue revertido, pero no se pudo limpiar la entrada preparada: {cleanup_error}"
                            ));
                        }
                    }
                    Err(journal_error).context(format!(
                        "falló la confirmación durable de EcoQoS para pid {pid}; se revirtió"
                    ))
                }
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "falló la confirmación durable de EcoQoS para pid {pid}: {journal_error}; también falló rollback: {rollback_error}"
                )),
            };
        }
        self.original_power.insert(
            pid,
            OriginalPowerThrottling {
                start_time,
                original: original_value,
                applied: target,
                native_identity,
                journal_id,
            },
        );
        Ok(())
    }

    fn recover_power(&self, entry: &JournalEntry) -> Result<bool> {
        if !identity::belongs_to_current_platform(&entry.native_identity) {
            return Ok(false);
        }
        anyhow::ensure!(
            entry.backend == "windows_power_throttling",
            "backend Power Throttling incompatible: {}",
            entry.backend
        );
        let ResourceChange::PowerThrottling {
            original,
            applied,
            previous_applied,
        } = &entry.change
        else {
            bail!("entrada de recuperación no corresponde a Power Throttling");
        };
        validate_power_value(*original)?;
        validate_power_value(*applied)?;
        if let Some(previous) = previous_applied {
            validate_power_value(*previous)?;
        }

        let handle = match OwnedHandle::open(entry.pid) {
            Ok(handle) => handle,
            Err(error) if process_is_gone(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        let actual = identity::identity_from_handle(handle.0, entry.observed_start_time_secs)?;
        if actual != entry.native_identity || !process_is_active(handle.0)? {
            return Ok(false);
        }
        let current = get_power_throttling(handle.0, entry.pid)?;
        if current != *applied && *previous_applied != Some(current) {
            return Ok(false);
        }
        let latest = get_power_throttling(handle.0, entry.pid)?;
        if latest != current || !process_is_active(handle.0)? {
            return Ok(false);
        }
        set_power_throttling(handle.0, entry.pid, *original)?;
        Ok(true)
    }

    fn recover_priority(&self, entry: &JournalEntry) -> Result<bool> {
        if !identity::belongs_to_current_platform(&entry.native_identity) {
            return Ok(false);
        }
        anyhow::ensure!(
            entry.backend == "windows_priority_class",
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
        let original = u32::try_from(*original).context("clase original fuera de rango")?;
        let applied = u32::try_from(*applied).context("clase aplicada fuera de rango")?;
        let previous_applied = previous_applied
            .as_ref()
            .copied()
            .map(u32::try_from)
            .transpose()
            .context("clase aplicada anterior fuera de rango")?;
        anyhow::ensure!(
            valid_priority_class(original),
            "clase original inválida: 0x{original:x}"
        );
        anyhow::ensure!(
            valid_priority_class(applied),
            "clase aplicada inválida: 0x{applied:x}"
        );
        anyhow::ensure!(
            previous_applied.is_none_or(valid_priority_class),
            "clase aplicada anterior inválida"
        );

        let handle = match OwnedHandle::open(entry.pid) {
            Ok(handle) => handle,
            Err(error) if process_is_gone(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        let actual = identity::identity_from_handle(handle.0, entry.observed_start_time_secs)?;
        if actual != entry.native_identity || !process_is_active(handle.0)? {
            return Ok(false);
        }
        let current = match get_priority_class(handle.0, entry.pid) {
            Ok(current) => current,
            Err(_) if !process_is_active(handle.0)? => return Ok(false),
            Err(error) => return Err(error),
        };
        if current != applied && previous_applied != Some(current) {
            return Ok(false);
        }
        if !process_is_active(handle.0)? {
            return Ok(false);
        }
        let latest = match get_priority_class(handle.0, entry.pid) {
            Ok(latest) => latest,
            Err(_) if !process_is_active(handle.0)? => return Ok(false),
            Err(error) => return Err(error),
        };
        if latest != current {
            return Ok(false);
        }
        if let Err(error) = set_priority_class(handle.0, entry.pid, current, original) {
            if !process_is_active(handle.0)? {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(true)
    }
}

impl SystemEnforcer for WindowsEnforcer {
    fn recover(&mut self) -> RecoveryReport {
        let mut report = RecoveryReport::default();
        for entry in self.journal.entries() {
            let outcome = match &entry.change {
                ResourceChange::Priority { .. } => self.recover_priority(&entry),
                ResourceChange::PowerThrottling { .. } => self.recover_power(&entry),
                ResourceChange::Cgroup { .. } => {
                    if identity::belongs_to_current_platform(&entry.native_identity) {
                        Err(anyhow::anyhow!(
                            "journal contiene una entrada cgroup incompatible con Windows"
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
                    pid,
                    start_time,
                    name,
                    power,
                    timer_resolution,
                } => self.set_power_policy(*pid, *start_time, name, *power, *timer_resolution),
                Action::AssignProcessGroup { .. } | Action::ResetProcessGroup { .. } => {
                    Err(anyhow::anyhow!(
                        "resource groups no son reversibles en Windows y permanecen deshabilitados"
                    ))
                }
            };
            report.push(action, outcome);
        }
        report
    }

    fn supports_power_policy_changes(&self) -> bool {
        self.power_throttling_supported
    }

    fn heartbeat(&mut self) -> Result<()> {
        let mut stale_ids = Vec::new();
        for entry in self.journal.entries() {
            if !identity::belongs_to_current_platform(&entry.native_identity) {
                stale_ids.push(entry.id);
                continue;
            }
            let handle = match OwnedHandle::open(entry.pid) {
                Ok(handle) => handle,
                Err(error) if process_is_gone(&error) => {
                    stale_ids.push(entry.id);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !process_is_active(handle.0)?
                || !identity::matches_handle_identity(handle.0, &entry.native_identity)?
            {
                stale_ids.push(entry.id);
            }
        }
        if !stale_ids.is_empty() {
            self.journal.remove_many(&stale_ids)?;
            self.original_priorities
                .retain(|_, original| !stale_ids.contains(&original.journal_id));
            self.original_power
                .retain(|_, original| !stale_ids.contains(&original.journal_id));
        }
        self.journal.renew_all()
    }
}

fn probe_power_throttling_support() -> bool {
    let pid = std::process::id();
    OwnedHandle::open(pid)
        .and_then(|handle| get_power_throttling(handle.0, pid))
        .is_ok()
}

fn get_power_throttling(handle: HANDLE, pid: u32) -> Result<PowerThrottlingValue> {
    let mut value = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: 0,
        StateMask: 0,
    };
    let result = unsafe {
        GetProcessInformation(
            handle,
            ProcessPowerThrottling,
            (&mut value as *mut PROCESS_POWER_THROTTLING_STATE).cast::<c_void>(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if result == 0 {
        bail!(
            "GetProcessInformation(ProcessPowerThrottling) falló para pid {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
    let value = PowerThrottlingValue {
        control_mask: value.ControlMask,
        state_mask: value.StateMask,
    };
    validate_power_value(value)?;
    Ok(value)
}

fn set_power_throttling(handle: HANDLE, pid: u32, value: PowerThrottlingValue) -> Result<()> {
    validate_power_value(value)?;
    let native = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: value.control_mask,
        StateMask: value.state_mask,
    };
    let result = unsafe {
        SetProcessInformation(
            handle,
            ProcessPowerThrottling,
            (&native as *const PROCESS_POWER_THROTTLING_STATE).cast::<c_void>(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if result == 0 {
        bail!(
            "SetProcessInformation(ProcessPowerThrottling) falló para pid {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
    let verified = get_power_throttling(handle, pid)?;
    anyhow::ensure!(
        verified == value,
        "Windows no confirmó Power Throttling para pid {pid}: esperado {value:?}, observado {verified:?}"
    );
    Ok(())
}

fn compose_power_target(
    current: PowerThrottlingValue,
    power: ProcessPowerPolicy,
    timer: TimerResolutionPolicy,
) -> PowerThrottlingValue {
    let mut target = current;
    match power {
        ProcessPowerPolicy::SystemManaged => {
            target.control_mask &= !PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
            target.state_mask &= !PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
        }
        ProcessPowerPolicy::Eco => {
            target.control_mask |= PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
            target.state_mask |= PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
        }
        ProcessPowerPolicy::High => {
            target.control_mask |= PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
            target.state_mask &= !PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
        }
    }
    match timer {
        TimerResolutionPolicy::SystemManaged => {
            target.control_mask &= !PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
            target.state_mask &= !PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
        }
        TimerResolutionPolicy::Ignore => {
            target.control_mask |= PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
            target.state_mask |= PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
        }
        TimerResolutionPolicy::Respect => {
            target.control_mask |= PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
            target.state_mask &= !PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
        }
    }
    target
}

fn validate_power_value(value: PowerThrottlingValue) -> Result<()> {
    anyhow::ensure!(
        value.state_mask & !value.control_mask == 0,
        "Power Throttling tiene estado activo fuera de ControlMask"
    );
    Ok(())
}

fn process_is_active(handle: HANDLE) -> Result<bool> {
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;
    const WAIT_FAILED: u32 = u32::MAX;
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_TIMEOUT => Ok(true),
        WAIT_OBJECT_0 => Ok(false),
        WAIT_FAILED => Err(std::io::Error::last_os_error()).context("WaitForSingleObject falló"),
        other => bail!("WaitForSingleObject devolvió un estado inesperado: {other}"),
    }
}

fn get_priority_class(handle: HANDLE, pid: u32) -> Result<u32> {
    let current = unsafe { GetPriorityClass(handle) };
    if current == 0 {
        bail!(
            "GetPriorityClass falló para pid {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(current)
}

fn set_priority_class(handle: HANDLE, pid: u32, current: u32, target: u32) -> Result<()> {
    anyhow::ensure!(
        valid_priority_class(target),
        "clase objetivo inválida: 0x{target:x}"
    );
    let result = unsafe { SetPriorityClass(handle, target) };
    if result == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(5) && current == REALTIME_PRIORITY_CLASS {
            bail!("pid {pid} usa prioridad realtime y Windows denegó modificarla");
        }
        bail!("SetPriorityClass falló para pid {pid}: {error}");
    }
    let observed = get_priority_class(handle, pid)?;
    anyhow::ensure!(
        observed == target,
        "Windows no confirmó la clase de prioridad para pid {pid}: esperado 0x{target:x}, observado 0x{observed:x}"
    );
    Ok(())
}

fn process_is_gone(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(|code| matches!(code, 87 | 1168))
    })
}

fn valid_priority_class(value: u32) -> bool {
    matches!(
        value,
        IDLE_PRIORITY_CLASS
            | BELOW_NORMAL_PRIORITY_CLASS
            | NORMAL_PRIORITY_CLASS
            | ABOVE_NORMAL_PRIORITY_CLASS
            | HIGH_PRIORITY_CLASS
            | REALTIME_PRIORITY_CLASS
    )
}

fn priority_to_class(priority: Priority) -> u32 {
    match priority {
        Priority::Idle => IDLE_PRIORITY_CLASS,
        Priority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        Priority::Normal => NORMAL_PRIORITY_CLASS,
        Priority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        Priority::High => HIGH_PRIORITY_CLASS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valida_clases_conocidas() {
        assert!(valid_priority_class(NORMAL_PRIORITY_CLASS));
        assert!(!valid_priority_class(0));
    }

    #[test]
    fn compone_ecoqos_y_temporizadores_sin_perder_bits_ajenos() {
        let unknown = 1u32 << 29;
        let current = PowerThrottlingValue {
            control_mask: unknown,
            state_mask: unknown,
        };
        let eco = compose_power_target(
            current,
            ProcessPowerPolicy::Eco,
            TimerResolutionPolicy::Ignore,
        );
        assert_ne!(
            eco.control_mask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            0
        );
        assert_ne!(eco.state_mask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED, 0);
        assert_ne!(
            eco.state_mask & PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            0
        );
        assert_eq!(eco.control_mask & unknown, unknown);
        assert_eq!(eco.state_mask & unknown, unknown);

        let high = compose_power_target(
            eco,
            ProcessPowerPolicy::High,
            TimerResolutionPolicy::Respect,
        );
        assert_ne!(
            high.control_mask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            0
        );
        assert_eq!(
            high.state_mask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            0
        );
        assert_ne!(
            high.control_mask & PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            0
        );
        assert_eq!(
            high.state_mask & PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            0
        );
    }

    #[test]
    #[ignore = "requiere Windows nativo; CI de release lo ejecuta explícitamente"]
    fn ecoqos_roundtrip_current_process() {
        struct Restore<'a> {
            handle: HANDLE,
            pid: u32,
            original: PowerThrottlingValue,
            restored: &'a mut bool,
        }
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                if set_power_throttling(self.handle, self.pid, self.original).is_ok() {
                    *self.restored = true;
                }
            }
        }

        let pid = std::process::id();
        let handle = OwnedHandle::open(pid).expect("se puede abrir el proceso de prueba");
        let original = get_power_throttling(handle.0, pid).expect("se puede leer Power Throttling");
        let target = compose_power_target(
            original,
            ProcessPowerPolicy::Eco,
            TimerResolutionPolicy::SystemManaged,
        );
        let mut restored = false;
        {
            let _restore = Restore {
                handle: handle.0,
                pid,
                original,
                restored: &mut restored,
            };
            set_power_throttling(handle.0, pid, target).expect("se puede aplicar EcoQoS");
            assert_eq!(
                get_power_throttling(handle.0, pid).expect("se puede verificar EcoQoS"),
                target
            );
        }
        assert!(restored, "el estado original debe restaurarse");
        assert_eq!(
            get_power_throttling(handle.0, pid).expect("se puede verificar la restauración"),
            original
        );
    }

    #[test]
    fn system_managed_libera_solo_los_bits_administrados() {
        let unknown = 1u32 << 29;
        let current = PowerThrottlingValue {
            control_mask: unknown
                | PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            state_mask: unknown
                | PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        };
        let target = compose_power_target(
            current,
            ProcessPowerPolicy::SystemManaged,
            TimerResolutionPolicy::SystemManaged,
        );
        assert_eq!(target.control_mask, unknown);
        assert_eq!(target.state_mask, unknown);
        assert!(validate_power_value(target).is_ok());
    }
}
