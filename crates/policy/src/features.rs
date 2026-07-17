use crate::SignatureDb;
use serde::{Deserialize, Serialize};
use telemetry::SystemSnapshot;

pub const FEATURE_NAMES: [&str; 12] = [
    "global_cpu",
    "ram_used_ratio",
    "top_cpu",
    "top3_cpu",
    "top_memory_ratio",
    "process_count",
    "ide_count",
    "language_server_count",
    "build_tool_count",
    "mobile_tooling_count",
    "container_vm_count",
    "browser_count",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FeatureVector {
    pub values: Vec<f32>,
}

impl FeatureVector {
    pub fn from_snapshot(snapshot: &SystemSnapshot, sigs: &SignatureDb) -> Self {
        let total_memory = snapshot.total_memory_bytes.max(1) as f32;
        let top_cpu = snapshot
            .top_processes
            .first()
            .map_or(0.0, |process| process.cpu_percent);
        let top3_cpu = snapshot
            .top_processes
            .iter()
            .take(3)
            .map(|process| process.cpu_percent)
            .sum::<f32>();
        let top_memory = snapshot
            .top_processes
            .iter()
            .map(|process| process.memory_bytes)
            .max()
            .unwrap_or(0) as f32;

        let mut ide = 0.0;
        let mut language_server = 0.0;
        let mut build_tool = 0.0;
        let mut mobile_tooling = 0.0;
        let mut container_vm = 0.0;
        let mut browser = 0.0;

        for process in &snapshot.top_processes {
            let categories = sigs.categorize(&process.name);
            ide += if categories.contains(&"ide") {
                1.0
            } else {
                0.0
            };
            language_server += if categories.contains(&"language_server") {
                1.0
            } else {
                0.0
            };
            build_tool += if categories.contains(&"build_tool") {
                1.0
            } else {
                0.0
            };
            mobile_tooling += if categories.contains(&"mobile_tooling") {
                1.0
            } else {
                0.0
            };
            container_vm += if categories.contains(&"container_vm") {
                1.0
            } else {
                0.0
            };
            browser += if categories.contains(&"browser") {
                1.0
            } else {
                0.0
            };
        }

        let normalize_count = |value: f32| (value / 5.0).clamp(0.0, 1.0);

        Self {
            values: vec![
                (snapshot.global_cpu_percent / 100.0).clamp(0.0, 1.0),
                (snapshot.used_memory_bytes as f32 / total_memory).clamp(0.0, 1.0),
                (top_cpu / 100.0).clamp(0.0, 1.0),
                (top3_cpu / 300.0).clamp(0.0, 1.0),
                (top_memory / total_memory).clamp(0.0, 1.0),
                (snapshot.process_count as f32 / 500.0).clamp(0.0, 1.0),
                normalize_count(ide),
                normalize_count(language_server),
                normalize_count(build_tool),
                normalize_count(mobile_tooling),
                normalize_count(container_vm),
                normalize_count(browser),
            ],
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.values.len() == FEATURE_NAMES.len(),
            "se esperaban {} features y se recibieron {}",
            FEATURE_NAMES.len(),
            self.values.len()
        );
        anyhow::ensure!(
            self.values.iter().all(|value| value.is_finite()),
            "el vector contiene valores no finitos"
        );
        anyhow::ensure!(
            self.values.iter().all(|value| (0.0..=1.0).contains(value)),
            "el vector contiene valores fuera del rango normalizado 0..=1"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valida_dimension_y_rango_normalizado() {
        assert!(FeatureVector {
            values: vec![0.5; FEATURE_NAMES.len()]
        }
        .validate()
        .is_ok());
        assert!(FeatureVector {
            values: vec![0.5; FEATURE_NAMES.len() - 1]
        }
        .validate()
        .is_err());

        let mut out_of_range = vec![0.5; FEATURE_NAMES.len()];
        out_of_range[0] = 1.1;
        assert!(FeatureVector {
            values: out_of_range
        }
        .validate()
        .is_err());
    }
}
