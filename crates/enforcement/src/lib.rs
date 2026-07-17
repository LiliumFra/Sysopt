use anyhow::Result;
use policy::Action;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct EnforcementConfig {
    pub resource_groups_enabled: bool,
    /// Raíz administrada por sysopt. En Linux debe estar dentro de una
    /// jerarquía cgroup v2 delegada al usuario/servicio que ejecuta sysopt.
    pub linux_cgroup_root: Option<PathBuf>,
    /// Journal durable de cambios reversibles. Es obligatorio en modo apply.
    pub journal_path: Option<PathBuf>,
    /// Journal independiente para controles compartidos cgroup v2.
    pub cgroup_control_journal_path: Option<PathBuf>,
    /// Tiempo durante el cual una sesión activa renueva sus entradas.
    pub journal_lease_secs: u64,
}

impl Default for EnforcementConfig {
    fn default() -> Self {
        Self {
            resource_groups_enabled: false,
            linux_cgroup_root: None,
            journal_path: None,
            cgroup_control_journal_path: None,
            journal_lease_secs: 120,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnforcementFailure {
    pub action: Action,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
pub struct EnforcementReport {
    pub succeeded: Vec<Action>,
    pub failures: Vec<EnforcementFailure>,
}

impl EnforcementReport {
    pub fn success(actions: &[Action]) -> Self {
        Self {
            succeeded: actions.to_vec(),
            failures: Vec::new(),
        }
    }

    pub fn push(&mut self, action: &Action, outcome: Result<()>) {
        match outcome {
            Ok(()) => self.succeeded.push(action.clone()),
            Err(error) => self.failures.push(EnforcementFailure {
                action: action.clone(),
                error: error.to_string(),
            }),
        }
    }

    pub fn is_success(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn failure_summary(&self) -> Option<String> {
        if self.failures.is_empty() {
            return None;
        }
        Some(format!(
            "{} de {} acción(es) fallaron:\n  {}",
            self.failures.len(),
            self.succeeded.len() + self.failures.len(),
            self.failures
                .iter()
                .map(|failure| failure.error.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        ))
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecoveryReport {
    pub restored: usize,
    pub discarded: usize,
    pub failures: Vec<String>,
}

impl RecoveryReport {
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "recuperación: {} restaurada(s), {} descartada(s), {} fallo(s)",
            self.restored,
            self.discarded,
            self.failures.len()
        )
    }
}

/// Abstracción sobre el mecanismo de enforcement del SO.
///
/// El informe por acción permite confirmar únicamente los cambios que el SO
/// aceptó. Esto evita que el rastreador interno diverja ante éxitos parciales.
pub trait SystemEnforcer {
    /// Recupera entradas persistentes de una ejecución anterior. Debe llamarse
    /// antes de aplicar cambios nuevos.
    fn recover(&mut self) -> RecoveryReport {
        RecoveryReport::default()
    }

    fn apply(&mut self, actions: &[Action]) -> EnforcementReport;

    /// Renueva los leases de cambios todavía activos.
    fn heartbeat(&mut self) -> Result<()> {
        Ok(())
    }

    fn supports_priority_changes(&self) -> bool {
        true
    }

    fn supports_power_policy_changes(&self) -> bool {
        false
    }
}

#[cfg(target_os = "linux")]
mod cgroup_journal;
mod identity;
mod journal;

#[cfg(target_os = "linux")]
mod cgroup_v2;
#[cfg(target_os = "linux")]
mod priority_helper;
#[cfg(target_os = "linux")]
pub use priority_helper::main_from_env as run_linux_priority_helper;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use unix::UnixEnforcer as PlatformEnforcer;

#[cfg(target_os = "windows")]
mod windows_impl;
#[cfg(target_os = "windows")]
pub use windows_impl::WindowsEnforcer as PlatformEnforcer;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub use unsupported::UnsupportedEnforcer as PlatformEnforcer;

pub struct DryRunEnforcer;

impl SystemEnforcer for DryRunEnforcer {
    fn apply(&mut self, actions: &[Action]) -> EnforcementReport {
        for action in actions {
            println!("[dry-run] {action:?}");
        }
        EnforcementReport::success(actions)
    }
}
