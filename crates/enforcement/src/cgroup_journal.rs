use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 1;
const MAX_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupJournalState {
    Prepared,
    Applied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlTransition {
    pub original: String,
    pub applied: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_applied: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupControlRecord {
    pub group_path: PathBuf,
    pub state: GroupJournalState,
    pub controls: BTreeMap<String, ControlTransition>,
    pub updated_at_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupJournalFile {
    schema_version: u32,
    records: Vec<GroupControlRecord>,
}

impl Default for GroupJournalFile {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            records: Vec::new(),
        }
    }
}

pub struct GroupControlJournal {
    path: PathBuf,
    data: GroupJournalFile,
    _lock: File,
}

impl GroupControlJournal {
    pub fn open(path: PathBuf) -> Result<Self> {
        ensure_private_parent(&path)?;
        let lock_path = sidecar_lock_path(&path);
        validate_optional_regular_file(&lock_path)?;
        let lock = open_private_file(&lock_path, true)?;
        FileExt::try_lock_exclusive(&lock).with_context(|| {
            format!(
                "otro proceso ya administra el journal de controles cgroup {}",
                path.display()
            )
        })?;
        let data = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "journal cgroup inválido: {}",
                    path.display()
                );
                anyhow::ensure!(
                    metadata.len() <= MAX_BYTES,
                    "journal cgroup demasiado grande"
                );
                let mut file = open_private_read(&path)?;
                let mut text = String::new();
                file.read_to_string(&mut text)?;
                serde_json::from_str::<GroupJournalFile>(&text)
                    .with_context(|| format!("journal cgroup corrupto: {}", path.display()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                GroupJournalFile::default()
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            data.schema_version == SCHEMA_VERSION,
            "versión de journal cgroup incompatible"
        );
        let mut seen = std::collections::BTreeSet::new();
        for record in &data.records {
            validate_record(record)?;
            anyhow::ensure!(
                seen.insert(record.group_path.clone()),
                "journal cgroup contiene grupos duplicados"
            );
        }
        Ok(Self {
            path,
            data,
            _lock: lock,
        })
    }

    pub fn records(&self) -> Vec<GroupControlRecord> {
        self.data.records.clone()
    }

    pub fn record(&self, path: &Path) -> Option<GroupControlRecord> {
        self.data
            .records
            .iter()
            .find(|record| record.group_path == path)
            .cloned()
    }

    pub fn prepare(
        &mut self,
        group_path: &Path,
        current: &BTreeMap<String, String>,
        desired: &BTreeMap<String, String>,
    ) -> Result<()> {
        anyhow::ensure!(!desired.is_empty(), "transacción cgroup sin controles");
        let now = unix_now_secs();
        let mut next = self.data.clone();
        if let Some(record) = next
            .records
            .iter_mut()
            .find(|record| record.group_path == group_path)
        {
            let mut controls = record.controls.clone();
            for (name, desired_value) in desired {
                let current_value = current
                    .get(name)
                    .with_context(|| format!("falta valor actual de {name}"))?;
                if let Some(transition) = controls.get_mut(name) {
                    transition.previous_applied = Some(current_value.clone());
                    transition.applied = desired_value.clone();
                } else {
                    controls.insert(
                        name.clone(),
                        ControlTransition {
                            original: current_value.clone(),
                            applied: desired_value.clone(),
                            previous_applied: None,
                        },
                    );
                }
            }
            record.controls = controls;
            record.state = GroupJournalState::Prepared;
            record.updated_at_unix_secs = now;
        } else {
            let controls = desired
                .iter()
                .map(|(name, desired_value)| {
                    let original = current
                        .get(name)
                        .cloned()
                        .with_context(|| format!("falta valor original de {name}"))?;
                    Ok((
                        name.clone(),
                        ControlTransition {
                            original,
                            applied: desired_value.clone(),
                            previous_applied: None,
                        },
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            next.records.push(GroupControlRecord {
                group_path: group_path.to_path_buf(),
                state: GroupJournalState::Prepared,
                controls,
                updated_at_unix_secs: now,
            });
        }
        self.commit(next)
    }

    pub fn mark_applied(&mut self, group_path: &Path) -> Result<()> {
        let mut next = self.data.clone();
        let record = next
            .records
            .iter_mut()
            .find(|record| record.group_path == group_path)
            .with_context(|| format!("grupo {} no encontrado", group_path.display()))?;
        record.state = GroupJournalState::Applied;
        record.updated_at_unix_secs = unix_now_secs();
        for transition in record.controls.values_mut() {
            transition.previous_applied = None;
        }
        self.commit(next)
    }

    pub fn restore_record(&mut self, record: &GroupControlRecord) -> Result<()> {
        validate_record(record)?;
        let mut next = self.data.clone();
        if let Some(existing) = next
            .records
            .iter_mut()
            .find(|existing| existing.group_path == record.group_path)
        {
            *existing = record.clone();
        } else {
            next.records.push(record.clone());
        }
        self.commit(next)
    }

    pub fn remove(&mut self, group_path: &Path) -> Result<()> {
        let mut next = self.data.clone();
        let before = next.records.len();
        next.records
            .retain(|record| record.group_path != group_path);
        if next.records.len() == before {
            return Ok(());
        }
        self.commit(next)
    }

    fn commit(&mut self, next: GroupJournalFile) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&next)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "journal cgroup excede el límite"
        );
        let temporary = self.path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        let result = (|| -> Result<()> {
            let mut file = open_private_temp(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            replace_file(&temporary, &self.path)?;
            #[cfg(unix)]
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
            if let Some(parent) = self.path.parent() {
                File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        result?;
        self.data = next;
        Ok(())
    }
}

fn validate_record(record: &GroupControlRecord) -> Result<()> {
    anyhow::ensure!(record.group_path.is_absolute(), "ruta cgroup no absoluta");
    anyhow::ensure!(
        record
            .group_path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
        "ruta cgroup insegura"
    );
    anyhow::ensure!(!record.controls.is_empty(), "registro cgroup sin controles");
    for (name, transition) in &record.controls {
        anyhow::ensure!(
            matches!(name.as_str(), "cpu.weight" | "io.weight" | "memory.high"),
            "control cgroup no permitido: {name}"
        );
        anyhow::ensure!(
            !transition.original.contains('\n')
                && !transition.applied.contains('\n')
                && transition
                    .previous_applied
                    .as_ref()
                    .is_none_or(|value| !value.contains('\n')),
            "valor cgroup inválido"
        );
    }
    Ok(())
}

fn ensure_private_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("journal cgroup sin directorio padre")?;
    ensure_private_directory_tree(parent)
}

fn ensure_private_directory_tree(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata_is_safe_directory(&metadata),
                "directorio de journal cgroup inseguro: {}",
                directory.display()
            );
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("no se pudo inspeccionar {}", directory.display()));
        }
    }
    if let Some(parent) = directory
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty() && *parent != directory)
    {
        ensure_private_directory_tree(parent)?;
    }
    match fs::create_dir(directory) {
        Ok(()) => {
            #[cfg(unix)]
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| format!("no se pudo crear {}", directory.display()));
        }
    }
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("no se pudo verificar {}", directory.display()))?;
    anyhow::ensure!(
        metadata_is_safe_directory(&metadata),
        "directorio de journal cgroup creado de forma insegura: {}",
        directory.display()
    );
    Ok(())
}

fn metadata_is_safe_directory(metadata: &fs::Metadata) -> bool {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

fn validate_optional_regular_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "archivo indirecto rechazado: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn open_private_file(path: &Path, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(create);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    Ok(options.open(path)?)
}

fn open_private_read(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    Ok(options.open(path)?)
}

fn open_private_temp(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    Ok(options.open(path)?)
}

fn sidecar_lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

#[cfg(unix)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(source, destination)?;
    Ok(())
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        std::env::temp_dir().join(format!("sysopt-{name}-{}-{nonce}", std::process::id()))
    }

    fn cleanup_journal(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(sidecar_lock_path(path));
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }

    #[test]
    fn recupera_transaccion_preparada_y_confirma_aplicacion() {
        let directory = unique_test_path("cgroup-journal");
        let path = directory.join("controls.json");
        let group = directory.join("managed/background");
        let current = BTreeMap::from([("cpu.weight".into(), "100".into())]);
        let desired = BTreeMap::from([("cpu.weight".into(), "25".into())]);

        {
            let mut journal = GroupControlJournal::open(path.clone()).expect("abre journal");
            journal
                .prepare(&group, &current, &desired)
                .expect("prepara transacción");
            let record = journal.record(&group).expect("registro preparado");
            assert_eq!(record.state, GroupJournalState::Prepared);
            assert_eq!(record.controls["cpu.weight"].original, "100");
            assert_eq!(record.controls["cpu.weight"].applied, "25");
        }

        {
            let mut recovered = GroupControlJournal::open(path.clone()).expect("reabre journal");
            assert_eq!(
                recovered.record(&group).expect("registro recuperado").state,
                GroupJournalState::Prepared
            );
            recovered
                .mark_applied(&group)
                .expect("confirma transacción");
            assert_eq!(
                recovered.record(&group).expect("registro aplicado").state,
                GroupJournalState::Applied
            );
            recovered.remove(&group).expect("elimina registro");
            assert!(recovered.record(&group).is_none());
        }
        cleanup_journal(&path);
    }

    #[test]
    fn conserva_original_y_registra_valor_aplicado_anterior() {
        let directory = unique_test_path("cgroup-journal-update");
        let path = directory.join("controls.json");
        let group = directory.join("managed/background");
        let original = BTreeMap::from([("io.weight".into(), "default 100".into())]);
        let first = BTreeMap::from([("io.weight".into(), "default 200".into())]);
        let current = first.clone();
        let second = BTreeMap::from([("io.weight".into(), "default 300".into())]);

        {
            let mut journal = GroupControlJournal::open(path.clone()).expect("abre journal");
            journal
                .prepare(&group, &original, &first)
                .expect("primer prepare");
            journal.mark_applied(&group).expect("primer commit");
            journal
                .prepare(&group, &current, &second)
                .expect("segundo prepare");
            let transition = &journal.record(&group).expect("registro").controls["io.weight"];
            assert_eq!(transition.original, "default 100");
            assert_eq!(transition.previous_applied.as_deref(), Some("default 200"));
            assert_eq!(transition.applied, "default 300");
        }
        cleanup_journal(&path);
    }

    #[test]
    fn valida_controles_permitidos() {
        let record = GroupControlRecord {
            group_path: PathBuf::from("/sys/fs/cgroup/sysopt/background"),
            state: GroupJournalState::Applied,
            controls: BTreeMap::from([(
                "cpu.weight".into(),
                ControlTransition {
                    original: "100".into(),
                    applied: "25".into(),
                    previous_applied: None,
                },
            )]),
            updated_at_unix_secs: 1,
        };
        assert!(validate_record(&record).is_ok());
    }
}
