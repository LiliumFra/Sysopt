use crate::{FeatureVector, LearningRecord, SystemMode, FEATURE_NAMES};
use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassCentroid {
    pub mode: SystemMode,
    pub sample_count: usize,
    pub values: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainedModel {
    pub format_version: u32,
    pub feature_names: Vec<String>,
    pub distance_temperature: f32,
    pub classes: Vec<ClassCentroid>,
}

impl TrainedModel {
    pub fn train(records: &[LearningRecord]) -> Result<Self> {
        let mut grouped: BTreeMap<SystemMode, Vec<&FeatureVector>> = BTreeMap::new();
        for record in records {
            record.validate()?;
            if let Some(label) = record.user_label {
                grouped.entry(label).or_default().push(&record.features);
            }
        }

        anyhow::ensure!(
            grouped.len() >= 2,
            "se necesitan muestras etiquetadas de al menos 2 modos distintos"
        );

        let mut classes = Vec::with_capacity(grouped.len());
        for (mode, samples) in grouped {
            let mut centroid = vec![0.0; FEATURE_NAMES.len()];
            for sample in &samples {
                for (target, value) in centroid.iter_mut().zip(&sample.values) {
                    *target += *value;
                }
            }
            let divisor = samples.len() as f32;
            for value in &mut centroid {
                *value /= divisor;
            }
            classes.push(ClassCentroid {
                mode,
                sample_count: samples.len(),
                values: centroid,
            });
        }

        Ok(Self {
            format_version: 1,
            feature_names: FEATURE_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            distance_temperature: 0.08,
            classes,
        })
    }

    pub fn predict(&self, features: &FeatureVector) -> Result<(SystemMode, f32)> {
        self.validate()?;
        features.validate()?;

        let temperature = self.distance_temperature.max(0.001);
        let logits: Vec<f32> = self
            .classes
            .iter()
            .map(|class| {
                let distance = class
                    .values
                    .iter()
                    .zip(&features.values)
                    .map(|(a, b)| {
                        let delta = a - b;
                        delta * delta
                    })
                    .sum::<f32>()
                    / FEATURE_NAMES.len() as f32;
                -distance / temperature
            })
            .collect();

        let max_logit = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let probabilities: Vec<f32> = logits
            .iter()
            .map(|logit| (*logit - max_logit).exp())
            .collect();
        let total = probabilities.iter().sum::<f32>().max(f32::EPSILON);

        let (index, probability) = probabilities
            .iter()
            .enumerate()
            .map(|(index, value)| (index, *value / total))
            .max_by(|left, right| {
                left.1
                    .partial_cmp(&right.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .context("el modelo no contiene clases")?;

        Ok((self.classes[index].mode, probability))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let metadata = regular_file_metadata(path)?;
        anyhow::ensure!(
            metadata.len() <= 64 * 1024 * 1024,
            "modelo entrenado demasiado grande"
        );
        let lock_path = sidecar_lock_path(path);
        let lock = open_sidecar_lock(&lock_path)?;
        FileExt::lock_shared(&lock)
            .with_context(|| format!("no se pudo bloquear el modelo {}", path.display()))?;
        let result = (|| -> Result<Self> {
            let metadata = regular_file_metadata(path)?;
            anyhow::ensure!(
                metadata.len() <= 64 * 1024 * 1024,
                "modelo entrenado demasiado grande"
            );
            let mut file = open_regular_read(path)?;
            let mut content = String::with_capacity(metadata.len() as usize);
            file.read_to_string(&mut content)
                .with_context(|| format!("no se pudo leer el modelo {}", path.display()))?;
            let model: Self = toml::from_str(&content)
                .with_context(|| format!("modelo TOML inválido: {}", path.display()))?;
            model.validate()?;
            Ok(model)
        })();
        FileExt::unlock(&lock)
            .with_context(|| format!("no se pudo liberar {}", lock_path.display()))?;
        result
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        ensure_private_parent(path)?;
        let content = toml::to_string_pretty(self)?;
        let lock_path = sidecar_lock_path(path);
        let lock = open_sidecar_lock(&lock_path)?;
        FileExt::lock_exclusive(&lock)
            .with_context(|| format!("no se pudo bloquear {}", path.display()))?;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let temp = path.with_extension(format!("tmp-{}-{unique}", std::process::id()));
        let result = (|| -> Result<()> {
            validate_optional_regular_file(path)?;
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options
                .open(&temp)
                .with_context(|| format!("no se pudo crear {}", temp.display()))?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            replace_file(&temp, path)?;
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            Ok(())
        })();
        let _ = fs::remove_file(&temp);
        FileExt::unlock(&lock)
            .with_context(|| format!("no se pudo liberar {}", lock_path.display()))?;
        result
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.format_version == 1, "versión de modelo no soportada");
        anyhow::ensure!(
            self.feature_names
                .iter()
                .map(String::as_str)
                .eq(FEATURE_NAMES),
            "el modelo usa un esquema de features incompatible"
        );
        anyhow::ensure!(
            self.distance_temperature.is_finite() && self.distance_temperature > 0.0,
            "distance_temperature debe ser finita y mayor que 0"
        );
        anyhow::ensure!(
            self.classes.len() >= 2,
            "el modelo necesita al menos 2 clases"
        );
        anyhow::ensure!(
            self.classes.iter().all(|class| {
                class.sample_count > 0
                    && class.values.len() == FEATURE_NAMES.len()
                    && class
                        .values
                        .iter()
                        .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
            }),
            "el modelo contiene una clase o centroide inválido"
        );
        let unique_modes = self
            .classes
            .iter()
            .map(|class| class.mode)
            .collect::<std::collections::BTreeSet<_>>();
        anyhow::ensure!(
            unique_modes.len() == self.classes.len(),
            "el modelo contiene clases duplicadas"
        );
        Ok(())
    }
}

fn regular_file_metadata(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "se rechazó una ruta de modelo que no es un archivo regular: {}",
        path.display()
    );
    Ok(metadata)
}

fn validate_optional_regular_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink() && metadata.is_file(),
            "se rechazó reemplazar una ruta de modelo que no es un archivo regular: {}",
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

fn open_regular_read(path: &Path) -> Result<File> {
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

fn open_sidecar_lock(path: &Path) -> Result<File> {
    validate_optional_regular_file(path)?;
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

fn sidecar_lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "model".into(), |value| value.to_os_string());
    name.push(".lock");
    path.with_file_name(name)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
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
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)
        .with_context(|| format!("no se pudo reemplazar {}", destination.display()))?;
    if let Some(parent) = destination.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("no se pudo sincronizar {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClassificationSource;

    fn record(value: f32, label: SystemMode) -> LearningRecord {
        LearningRecord {
            schema_version: 1,
            timestamp_unix_ms: 0,
            features: FeatureVector {
                values: vec![value; FEATURE_NAMES.len()],
            },
            predicted_mode: label,
            prediction_confidence: 1.0,
            prediction_source: ClassificationSource::Rules,
            user_label: Some(label),
        }
    }

    #[test]
    fn aprende_centroides_y_predice_la_clase_cercana() {
        let model = TrainedModel::train(&[
            record(0.0, SystemMode::Idle),
            record(0.1, SystemMode::Idle),
            record(0.9, SystemMode::HeavyForeground),
            record(1.0, SystemMode::HeavyForeground),
        ])
        .unwrap();

        let (mode, confidence) = model
            .predict(&FeatureVector {
                values: vec![0.95; FEATURE_NAMES.len()],
            })
            .unwrap();
        assert_eq!(mode, SystemMode::HeavyForeground);
        assert!(confidence > 0.5);
    }
}
