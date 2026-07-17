use crate::{EnforcementConfig, EnforcementReport, SystemEnforcer};
use anyhow::{bail, Result};
use policy::Action;

pub struct UnsupportedEnforcer;

impl UnsupportedEnforcer {
    pub fn new(_config: EnforcementConfig) -> Result<Self> {
        bail!("enforcement no implementado para este sistema operativo; usa dry-run")
    }
}

impl SystemEnforcer for UnsupportedEnforcer {
    fn apply(&mut self, actions: &[Action]) -> EnforcementReport {
        let mut report = EnforcementReport::default();
        for action in actions {
            report.push(
                action,
                Err(anyhow::anyhow!(
                    "enforcement no implementado para este sistema operativo; usa dry-run"
                )),
            );
        }
        report
    }
}
