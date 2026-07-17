use crate::{ClassificationSource, FeatureVector, SignatureDb, SystemMode};
use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use telemetry::SystemSnapshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningRecord {
    pub schema_version: u32,
    pub timestamp_unix_ms: u64,
    pub features: FeatureVector,
    pub predicted_mode: SystemMode,
    pub prediction_confidence: f32,
    pub prediction_source: ClassificationSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_label: Option<SystemMode>,
}

impl LearningRecord {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.schema_version == 1, "versión de dataset no soportada");
        anyhow::ensure!(
            self.prediction_confidence.is_finite()
                && (0.0..=1.0).contains(&self.prediction_confidence),
            "prediction_confidence debe estar entre 0 y 1"
        );
        self.features.validate()
    }

    pub fn new(
        snapshot: &SystemSnapshot,
        sigs: &SignatureDb,
        predicted_mode: SystemMode,
        prediction_confidence: f32,
        prediction_source: ClassificationSource,
        user_label: Option<SystemMode>,
    ) -> Self {
        let timestamp_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            });

        Self {
            schema_version: 1,
            timestamp_unix_ms,
            features: FeatureVector::from_snapshot(snapshot, sigs),
            predicted_mode,
            prediction_confidence,
            prediction_source,
            user_label,
        }
    }
}

pub fn append_learning_record(path: &Path, record: &LearningRecord) -> Result<()> {
    record.validate()?;
    ensure_private_parent(path)?;

    let mut file = open_learning_append(path)?;
    FileExt::lock_exclusive(&file)
        .with_context(|| format!("no se pudo bloquear {}", path.display()))?;
    let result = (|| -> Result<()> {
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_data()?;
        Ok(())
    })();
    FileExt::unlock(&file).with_context(|| format!("no se pudo liberar {}", path.display()))?;
    result?;
    Ok(())
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

pub fn load_learning_records(path: &Path) -> Result<Vec<LearningRecord>> {
    let metadata = regular_file_metadata(path)?;
    anyhow::ensure!(
        metadata.len() <= 256 * 1024 * 1024,
        "dataset de aprendizaje demasiado grande"
    );
    let file = open_learning_read(path)
        .with_context(|| format!("no se pudo abrir el dataset {}", path.display()))?;
    FileExt::lock_shared(&file)
        .with_context(|| format!("no se pudo bloquear el dataset {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("error leyendo línea {}", index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let record: LearningRecord = serde_json::from_str(&line)
            .with_context(|| format!("JSON inválido en línea {}", index + 1))?;
        record
            .validate()
            .with_context(|| format!("registro inválido en línea {}", index + 1))?;
        records.push(record);
    }

    Ok(records)
}

fn regular_file_metadata(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "se rechazó una ruta de aprendizaje que no es un archivo regular: {}",
        path.display()
    );
    Ok(metadata)
}

fn open_learning_append(path: &Path) -> Result<File> {
    let existed = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.file_type().is_symlink() && metadata.is_file(),
                "se rechazó una ruta de aprendizaje que no es un archivo regular: {}",
                path.display()
            );
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("no se pudo inspeccionar {}", path.display()));
        }
    };
    let mut options = OpenOptions::new();
    options.create(true).append(true).read(true);
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
    if !existed {
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn open_learning_read(path: &Path) -> Result<File> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_jsonl_hace_roundtrip() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "sysopt-learning-{}-{unique}.jsonl",
            std::process::id()
        ));
        let record = LearningRecord {
            schema_version: 1,
            timestamp_unix_ms: 123,
            features: FeatureVector {
                values: vec![0.25; crate::FEATURE_NAMES.len()],
            },
            predicted_mode: SystemMode::Developing,
            prediction_confidence: 0.8,
            prediction_source: ClassificationSource::Rules,
            user_label: Some(SystemMode::Developing),
        };

        append_learning_record(&path, &record).unwrap();
        let loaded = load_learning_records(&path).unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].user_label, Some(SystemMode::Developing));
        assert_eq!(loaded[0].features, record.features);
    }
}
