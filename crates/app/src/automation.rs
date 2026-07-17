use anyhow::Result;
use policy::SystemMode;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerformanceProfile {
    Smart,
    Eco,
    Balanced,
    Performance,
    Gaming,
    Development,
    Creator,
    Streaming,
    Quiet,
}

impl PerformanceProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Smart => "smart",
            Self::Eco => "eco",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
            Self::Gaming => "gaming",
            Self::Development => "development",
            Self::Creator => "creator",
            Self::Streaming => "streaming",
            Self::Quiet => "quiet",
        }
    }

    pub fn forced_mode(self) -> Option<SystemMode> {
        match self {
            Self::Gaming => Some(SystemMode::Gaming),
            Self::Development => Some(SystemMode::Developing),
            Self::Creator => Some(SystemMode::Creative),
            Self::Streaming => Some(SystemMode::Streaming),
            _ => None,
        }
    }
}

impl FromStr for PerformanceProfile {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "smart" | "intelligent" | "inteligente" | "automatic" | "automatico" | "automático" => Ok(Self::Smart),
            "eco" | "ahorro" | "battery" | "bateria" | "batería" => Ok(Self::Eco),
            "balanced" | "equilibrado" => Ok(Self::Balanced),
            "performance" | "rendimiento" | "max" => Ok(Self::Performance),
            "gaming" | "game" | "juegos" => Ok(Self::Gaming),
            "development" | "developing" | "desarrollo" | "dev" => Ok(Self::Development),
            "creator" | "creative" | "creador" | "creacion" | "creación" => Ok(Self::Creator),
            "streaming" | "stream" | "directo" => Ok(Self::Streaming),
            "quiet" | "silent" | "silencioso" => Ok(Self::Quiet),
            _ => anyhow::bail!(
                "perfil inválido: {value}; usa smart, eco, balanced, performance, gaming, development, creator, streaming o quiet"
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutomationConfig {
    pub enabled: bool,
    pub profile: String,
    pub mode_stability_cycles: u8,
    pub idle_interval_secs: u64,
    pub active_interval_secs: u64,
    pub busy_interval_secs: u64,
    pub heavy_foreground_immediate: bool,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            profile: "smart".into(),
            mode_stability_cycles: 2,
            idle_interval_secs: 8,
            active_interval_secs: 3,
            busy_interval_secs: 1,
            heavy_foreground_immediate: true,
        }
    }
}

impl AutomationConfig {
    pub fn validate(&self) -> Result<()> {
        let _ = self.profile()?;
        anyhow::ensure!(
            (1..=10).contains(&self.mode_stability_cycles),
            "automation.mode_stability_cycles debe estar entre 1 y 10"
        );
        for (name, value) in [
            ("idle_interval_secs", self.idle_interval_secs),
            ("active_interval_secs", self.active_interval_secs),
            ("busy_interval_secs", self.busy_interval_secs),
        ] {
            anyhow::ensure!(
                (1..=300).contains(&value),
                "automation.{name} debe estar entre 1 y 300"
            );
        }
        Ok(())
    }

    pub fn profile(&self) -> Result<PerformanceProfile> {
        PerformanceProfile::from_str(&self.profile)
    }

    pub fn apply_profile(&mut self, profile: PerformanceProfile) {
        self.profile = profile.as_str().into();
        match profile {
            PerformanceProfile::Smart => {
                self.mode_stability_cycles = 2;
                self.idle_interval_secs = 8;
                self.active_interval_secs = 3;
                self.busy_interval_secs = 1;
            }
            PerformanceProfile::Eco => {
                self.mode_stability_cycles = 3;
                self.idle_interval_secs = 15;
                self.active_interval_secs = 5;
                self.busy_interval_secs = 2;
            }
            PerformanceProfile::Balanced => {
                self.mode_stability_cycles = 2;
                self.idle_interval_secs = 8;
                self.active_interval_secs = 3;
                self.busy_interval_secs = 1;
            }
            PerformanceProfile::Performance => {
                self.mode_stability_cycles = 1;
                self.idle_interval_secs = 4;
                self.active_interval_secs = 2;
                self.busy_interval_secs = 1;
            }
            PerformanceProfile::Gaming => {
                self.mode_stability_cycles = 1;
                self.idle_interval_secs = 4;
                self.active_interval_secs = 1;
                self.busy_interval_secs = 1;
            }
            PerformanceProfile::Development => {
                self.mode_stability_cycles = 1;
                self.idle_interval_secs = 6;
                self.active_interval_secs = 2;
                self.busy_interval_secs = 1;
            }
            PerformanceProfile::Creator => {
                self.mode_stability_cycles = 1;
                self.idle_interval_secs = 5;
                self.active_interval_secs = 2;
                self.busy_interval_secs = 1;
            }
            PerformanceProfile::Streaming => {
                self.mode_stability_cycles = 1;
                self.idle_interval_secs = 6;
                self.active_interval_secs = 2;
                self.busy_interval_secs = 1;
            }
            PerformanceProfile::Quiet => {
                self.mode_stability_cycles = 4;
                self.idle_interval_secs = 20;
                self.active_interval_secs = 7;
                self.busy_interval_secs = 3;
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AutomationDecision {
    pub effective_mode: SystemMode,
    pub interval_secs: u64,
    pub changed: bool,
}

pub struct AutomationController {
    config: AutomationConfig,
    profile: PerformanceProfile,
    effective_mode: SystemMode,
    candidate_mode: SystemMode,
    candidate_cycles: u8,
}

impl AutomationController {
    pub fn new(config: AutomationConfig) -> Result<Self> {
        config.validate()?;
        let profile = config.profile()?;
        Ok(Self {
            config,
            profile,
            effective_mode: SystemMode::Unknown,
            candidate_mode: SystemMode::Unknown,
            candidate_cycles: 0,
        })
    }

    pub fn set_profile(&mut self, profile: PerformanceProfile) {
        self.config.apply_profile(profile);
        self.profile = profile;
        self.candidate_mode = self.effective_mode;
        self.candidate_cycles = 0;
        if let Some(mode) = profile.forced_mode() {
            self.effective_mode = mode;
            self.candidate_mode = mode;
        }
    }

    pub fn profile_name(&self) -> &str {
        &self.config.profile
    }

    pub fn observe(&mut self, observed: SystemMode, fallback_interval: u64) -> AutomationDecision {
        let previous_mode = self.effective_mode;
        if let Some(forced) = self.profile.forced_mode() {
            self.effective_mode = forced;
            return AutomationDecision {
                effective_mode: forced,
                interval_secs: self.interval_for(forced),
                changed: forced != previous_mode,
            };
        }

        if !self.config.enabled {
            let changed = self.effective_mode != observed;
            self.effective_mode = observed;
            return AutomationDecision {
                effective_mode: observed,
                interval_secs: fallback_interval,
                changed,
            };
        }

        let immediate = self.config.heavy_foreground_immediate
            && matches!(
                observed,
                SystemMode::HeavyForeground | SystemMode::Gaming | SystemMode::Streaming
            );
        if observed == self.effective_mode {
            self.candidate_mode = observed;
            self.candidate_cycles = 0;
        } else if immediate {
            self.effective_mode = observed;
            self.candidate_mode = observed;
            self.candidate_cycles = 0;
        } else {
            if observed == self.candidate_mode {
                self.candidate_cycles = self.candidate_cycles.saturating_add(1);
            } else {
                self.candidate_mode = observed;
                self.candidate_cycles = 1;
            }
            if self.candidate_cycles >= self.config.mode_stability_cycles {
                self.effective_mode = observed;
                self.candidate_cycles = 0;
            }
        }

        AutomationDecision {
            effective_mode: self.effective_mode,
            interval_secs: self.interval_for(self.effective_mode),
            changed: self.effective_mode != previous_mode,
        }
    }

    fn interval_for(&self, mode: SystemMode) -> u64 {
        match mode {
            SystemMode::Idle | SystemMode::Unknown => self.config.idle_interval_secs,
            SystemMode::HeavyForeground
            | SystemMode::Containerized
            | SystemMode::Gaming
            | SystemMode::Streaming => self.config.busy_interval_secs,
            SystemMode::Interactive
            | SystemMode::Developing
            | SystemMode::MobileDevelopment
            | SystemMode::Creative => self.config.active_interval_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estabiliza_cambios_de_modo() {
        let config = AutomationConfig {
            mode_stability_cycles: 2,
            ..AutomationConfig::default()
        };
        let mut controller = AutomationController::new(config).unwrap();
        let first = controller.observe(SystemMode::Developing, 3);
        assert_eq!(first.effective_mode, SystemMode::Unknown);
        let second = controller.observe(SystemMode::Developing, 3);
        assert_eq!(second.effective_mode, SystemMode::Developing);
    }

    #[test]
    fn gaming_es_inmediato() {
        let mut controller = AutomationController::new(AutomationConfig::default()).unwrap();
        let decision = controller.observe(SystemMode::Gaming, 3);
        assert_eq!(decision.effective_mode, SystemMode::Gaming);
        assert_eq!(decision.interval_secs, 1);
    }

    #[test]
    fn perfil_development_fuerza_modo() {
        let mut controller = AutomationController::new(AutomationConfig::default()).unwrap();
        controller.set_profile(PerformanceProfile::Development);
        let decision = controller.observe(SystemMode::Interactive, 3);
        assert_eq!(decision.effective_mode, SystemMode::Developing);
    }
}
