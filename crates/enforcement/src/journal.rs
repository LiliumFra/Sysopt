use crate::identity::NativeProcessIdentity;
use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const JOURNAL_SCHEMA_VERSION: u32 = 1;
const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalState {
    Prepared,
    Applied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerThrottlingValue {
    pub control_mask: u32,
    pub state_mask: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resource", rename_all = "snake_case")]
pub enum ResourceChange {
    Priority {
        original: i64,
        applied: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous_applied: Option<i64>,
    },
    Cgroup {
        original: PathBuf,
        applied: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous_applied: Option<PathBuf>,
    },
    PowerThrottling {
        original: PowerThrottlingValue,
        applied: PowerThrottlingValue,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous_applied: Option<PowerThrottlingValue>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: u64,
    pub pid: u32,
    pub observed_start_time_secs: u64,
    pub name: String,
    pub native_identity: NativeProcessIdentity,
    pub backend: String,
    pub state: JournalState,
    pub change: ResourceChange,
    pub created_at_unix_secs: u64,
    pub last_confirmed_at_unix_secs: u64,
    pub lease_expires_at_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalFile {
    schema_version: u32,
    next_id: u64,
    entries: Vec<JournalEntry>,
}

impl Default for JournalFile {
    fn default() -> Self {
        Self {
            schema_version: JOURNAL_SCHEMA_VERSION,
            next_id: 1,
            entries: Vec::new(),
        }
    }
}

pub struct ActionJournal {
    path: PathBuf,
    lease_secs: u64,
    data: JournalFile,
    _lock: File,
}

impl ActionJournal {
    pub fn open(path: PathBuf, lease_secs: u64) -> Result<Self> {
        anyhow::ensure!(
            lease_secs >= 30,
            "el lease del journal debe ser de al menos 30 segundos"
        );
        ensure_private_parent(&path)?;
        let lock_path = sidecar_lock_path(&path);
        validate_optional_regular_file(&lock_path)?;
        let lock = open_private_lock(&lock_path)
            .with_context(|| format!("no se pudo abrir el lock {}", lock_path.display()))?;
        validate_open_lock(&lock, &lock_path)?;
        FileExt::try_lock_exclusive(&lock).with_context(|| {
            format!(
                "otro proceso ya administra el journal de enforcement {}",
                path.display()
            )
        })?;
        let data = match fs::symlink_metadata(&path) {
            Ok(_) => read_journal(&path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => JournalFile::default(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("no se pudo inspeccionar {}", path.display()));
            }
        };
        anyhow::ensure!(
            data.schema_version == JOURNAL_SCHEMA_VERSION,
            "versión de journal incompatible en {}: {}",
            path.display(),
            data.schema_version
        );
        validate_loaded_journal(&data)?;
        Ok(Self {
            path,
            lease_secs,
            data,
            _lock: lock,
        })
    }

    pub fn entries(&self) -> Vec<JournalEntry> {
        self.data.entries.clone()
    }

    pub fn prepare(
        &mut self,
        pid: u32,
        observed_start_time_secs: u64,
        name: &str,
        native_identity: NativeProcessIdentity,
        backend: &str,
        change: ResourceChange,
    ) -> Result<u64> {
        let now = unix_now_secs();
        let mut next = self.data.clone();
        anyhow::ensure!(
            !next.entries.iter().any(|entry| {
                entry.pid == pid
                    && entry.backend == backend
                    && entry.native_identity == native_identity
            }),
            "ya existe una transacción pendiente para pid {pid} en backend {backend}"
        );
        let id = next.next_id;
        next.next_id = next.next_id.saturating_add(1).max(1);
        next.entries.push(JournalEntry {
            id,
            pid,
            observed_start_time_secs,
            name: name.to_owned(),
            native_identity,
            backend: backend.to_owned(),
            state: JournalState::Prepared,
            change,
            created_at_unix_secs: now,
            last_confirmed_at_unix_secs: now,
            lease_expires_at_unix_secs: now.saturating_add(self.lease_secs),
        });
        self.commit(next)?;
        Ok(id)
    }

    pub fn update_change(&mut self, id: u64, change: ResourceChange) -> Result<()> {
        let now = unix_now_secs();
        let mut next = self.data.clone();
        let entry = next
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .with_context(|| format!("entrada {id} no encontrada en el journal"))?;
        entry.change = change;
        entry.state = JournalState::Prepared;
        entry.last_confirmed_at_unix_secs = now;
        entry.lease_expires_at_unix_secs = now.saturating_add(self.lease_secs);
        self.commit(next)
    }

    pub fn mark_applied(&mut self, id: u64) -> Result<()> {
        let now = unix_now_secs();
        let mut next = self.data.clone();
        let entry = next
            .entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .with_context(|| format!("entrada {id} no encontrada en el journal"))?;
        entry.state = JournalState::Applied;
        match &mut entry.change {
            ResourceChange::Priority {
                previous_applied, ..
            } => *previous_applied = None,
            ResourceChange::Cgroup {
                previous_applied, ..
            } => *previous_applied = None,
            ResourceChange::PowerThrottling {
                previous_applied, ..
            } => *previous_applied = None,
        }
        entry.last_confirmed_at_unix_secs = now;
        entry.lease_expires_at_unix_secs = now.saturating_add(self.lease_secs);
        self.commit(next)
    }

    pub fn renew_all(&mut self) -> Result<()> {
        if self.data.entries.is_empty() {
            return Ok(());
        }
        let now = unix_now_secs();
        let renew_before = now.saturating_add((self.lease_secs / 2).max(1));
        if self
            .data
            .entries
            .iter()
            .all(|entry| entry.lease_expires_at_unix_secs > renew_before)
        {
            return Ok(());
        }
        let mut next = self.data.clone();
        for entry in &mut next.entries {
            entry.last_confirmed_at_unix_secs = now;
            entry.lease_expires_at_unix_secs = now.saturating_add(self.lease_secs);
        }
        self.commit(next)
    }

    pub fn remove(&mut self, id: u64) -> Result<()> {
        let mut next = self.data.clone();
        let previous_len = next.entries.len();
        next.entries.retain(|entry| entry.id != id);
        if next.entries.len() == previous_len {
            return Ok(());
        }
        self.commit(next)
    }

    pub fn remove_many(&mut self, ids: &[u64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut next = self.data.clone();
        let previous_len = next.entries.len();
        next.entries.retain(|entry| !ids.contains(&entry.id));
        let removed = previous_len.saturating_sub(next.entries.len());
        if removed == 0 {
            return Ok(0);
        }
        self.commit(next)?;
        Ok(removed)
    }

    fn commit(&mut self, next: JournalFile) -> Result<()> {
        validate_loaded_journal(&next)?;
        write_json_atomic(&self.path, &next)?;
        self.data = next;
        Ok(())
    }
}

fn read_journal(path: &Path) -> Result<JournalFile> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("no se pudo inspeccionar {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "se rechazó un journal indirecto o no regular: {}",
        path.display()
    );
    validate_platform_metadata(&metadata, path, false)?;
    anyhow::ensure!(
        metadata.len() <= MAX_JOURNAL_BYTES,
        "journal demasiado grande: {}",
        path.display()
    );
    let mut file =
        open_private_read(path).with_context(|| format!("no se pudo abrir {}", path.display()))?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    if content.trim().is_empty() {
        return Ok(JournalFile::default());
    }
    serde_json::from_str(&content)
        .with_context(|| format!("journal inválido o corrupto en {}", path.display()))
}

fn write_json_atomic(path: &Path, value: &JournalFile) -> Result<()> {
    ensure_private_parent(path)?;
    validate_optional_regular_file(path)?;
    let temporary = temporary_path(path);
    validate_optional_regular_file(&temporary)?;
    let bytes = serde_json::to_vec_pretty(value)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_JOURNAL_BYTES,
        "journal excede el límite de seguridad"
    );

    let result = (|| -> Result<()> {
        let mut file = open_private_write(&temporary)
            .with_context(|| format!("no se pudo crear {}", temporary.display()))?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        replace_file(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn sidecar_lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map_or_else(
        || "enforcement-journal.json".into(),
        |value| value.to_os_string(),
    );
    name.push(".lock");
    path.with_file_name(name)
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map_or_else(
        || "enforcement-journal.json".into(),
        |value| value.to_os_string(),
    );
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    name.push(format!(".{}.{nonce}.tmp", std::process::id()));
    path.with_file_name(name)
}

fn validate_loaded_journal(data: &JournalFile) -> Result<()> {
    let mut ids = std::collections::HashSet::with_capacity(data.entries.len());
    let mut max_id = 0u64;
    for entry in &data.entries {
        anyhow::ensure!(entry.id > 0, "journal contiene id cero");
        anyhow::ensure!(
            ids.insert(entry.id),
            "journal contiene id duplicado {}",
            entry.id
        );
        anyhow::ensure!(
            entry.name.len() <= 512,
            "nombre excesivo en entrada {}",
            entry.id
        );
        anyhow::ensure!(entry.pid > 0, "pid inválido en entrada {}", entry.id);
        anyhow::ensure!(
            entry.observed_start_time_secs > 0,
            "hora de inicio inválida en entrada {}",
            entry.id
        );
        anyhow::ensure!(
            !entry.backend.trim().is_empty(),
            "backend vacío en entrada {}",
            entry.id
        );
        anyhow::ensure!(
            entry.backend.len() <= 128,
            "backend excesivo en entrada {}",
            entry.id
        );
        validate_native_identity(
            &entry.native_identity,
            entry.observed_start_time_secs,
            entry.id,
        )?;
        anyhow::ensure!(
            !data.entries.iter().any(|other| {
                other.id != entry.id
                    && other.pid == entry.pid
                    && other.backend == entry.backend
                    && other.native_identity == entry.native_identity
            }),
            "journal contiene transacciones duplicadas para pid {} en backend {}",
            entry.pid,
            entry.backend
        );
        anyhow::ensure!(
            entry.last_confirmed_at_unix_secs >= entry.created_at_unix_secs,
            "confirmación anterior a creación en entrada {}",
            entry.id
        );
        anyhow::ensure!(
            entry.lease_expires_at_unix_secs >= entry.last_confirmed_at_unix_secs,
            "lease inválido en entrada {}",
            entry.id
        );
        if entry.state == JournalState::Applied {
            let has_previous = match &entry.change {
                ResourceChange::Priority {
                    previous_applied, ..
                } => previous_applied.is_some(),
                ResourceChange::Cgroup {
                    previous_applied, ..
                } => previous_applied.is_some(),
                ResourceChange::PowerThrottling {
                    previous_applied, ..
                } => previous_applied.is_some(),
            };
            anyhow::ensure!(
                !has_previous,
                "entrada applied {} conserva previous_applied",
                entry.id
            );
        }
        max_id = max_id.max(entry.id);
    }
    anyhow::ensure!(
        data.next_id > max_id,
        "next_id del journal no avanza respecto de sus entradas"
    );
    Ok(())
}

fn validate_native_identity(
    identity: &NativeProcessIdentity,
    observed_start_time_secs: u64,
    entry_id: u64,
) -> Result<()> {
    match identity {
        NativeProcessIdentity::Linux {
            boot_id,
            start_ticks,
        } => {
            anyhow::ensure!(
                valid_linux_boot_id(boot_id),
                "boot_id Linux inválido en entrada {entry_id}"
            );
            anyhow::ensure!(
                *start_ticks > 0,
                "start_ticks Linux inválido en entrada {entry_id}"
            );
        }
        NativeProcessIdentity::Windows {
            creation_time_100ns,
        } => {
            const WINDOWS_TO_UNIX_EPOCH_SECS: u64 = 11_644_473_600;
            const TICKS_PER_SEC: u64 = 10_000_000;
            let start_time_secs = creation_time_100ns
                .checked_div(TICKS_PER_SEC)
                .and_then(|seconds| seconds.checked_sub(WINDOWS_TO_UNIX_EPOCH_SECS));
            anyhow::ensure!(
                start_time_secs == Some(observed_start_time_secs),
                "FILETIME Windows incoherente en entrada {entry_id}"
            );
        }
        NativeProcessIdentity::Macos {
            start_sec,
            start_usec,
        } => {
            anyhow::ensure!(
                *start_sec == observed_start_time_secs,
                "inicio macOS incoherente en entrada {entry_id}"
            );
            anyhow::ensure!(
                *start_usec < 1_000_000,
                "microsegundos macOS inválidos en entrada {entry_id}"
            );
        }
        NativeProcessIdentity::Fallback { start_time_secs } => anyhow::ensure!(
            *start_time_secs == observed_start_time_secs,
            "identidad fallback incoherente en entrada {entry_id}"
        ),
    }
    Ok(())
}

fn valid_linux_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn ensure_private_parent(path: &Path) -> Result<()> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    match fs::symlink_metadata(parent) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "directorio inseguro para journal: {}",
                parent.display()
            );
            validate_platform_metadata(&metadata, parent, true)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(parent)
                .with_context(|| format!("no se pudo crear {}", parent.display()))?;
            #[cfg(unix)]
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            let metadata = fs::symlink_metadata(parent)
                .with_context(|| format!("no se pudo verificar {}", parent.display()))?;
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "el directorio creado para el journal no es seguro: {}",
                parent.display()
            );
            validate_platform_metadata(&metadata, parent, true)?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("no se pudo inspeccionar {}", parent.display()))
        }
    }
    Ok(())
}

fn validate_optional_regular_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "se rechazó un archivo indirecto o no regular: {}",
                path.display()
            );
            validate_platform_metadata(&metadata, path, false)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("no se pudo inspeccionar {}", path.display()))
        }
    }
    Ok(())
}

fn validate_platform_metadata(metadata: &fs::Metadata, path: &Path, directory: bool) -> Result<()> {
    validate_unix_owner_and_mode(metadata, path, directory)?;
    validate_windows_reparse_point(metadata, path)
}

#[cfg(windows)]
fn validate_windows_reparse_point(metadata: &fs::Metadata, path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    anyhow::ensure!(
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "se rechazó un reparse point en la ruta del journal: {}",
        path.display()
    );
    Ok(())
}

#[cfg(not(windows))]
fn validate_windows_reparse_point(_metadata: &fs::Metadata, _path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_unix_owner_and_mode(
    metadata: &fs::Metadata,
    path: &Path,
    directory: bool,
) -> Result<()> {
    let effective_uid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        metadata.uid() == effective_uid,
        "propietario inseguro para {}: {}",
        if directory {
            "directorio de journal"
        } else {
            "journal"
        },
        path.display()
    );
    let forbidden_mode = if directory { 0o022 } else { 0o077 };
    anyhow::ensure!(
        metadata.mode() & forbidden_mode == 0,
        "{} tiene permisos inseguros: {}",
        if directory {
            "el directorio del journal"
        } else {
            "el archivo del journal"
        },
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_unix_owner_and_mode(
    _metadata: &fs::Metadata,
    _path: &Path,
    _directory: bool,
) -> Result<()> {
    Ok(())
}

fn validate_open_lock(file: &File, path: &Path) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("no se pudo inspeccionar el lock {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "el lock del journal no es un archivo regular: {}",
        path.display()
    );
    validate_platform_metadata(&metadata, path, false)
}

#[cfg(unix)]
fn open_private_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_private_lock(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_private_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
}

#[cfg(unix)]
fn open_private_read(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_private_read(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_private_read(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(unix)]
fn open_private_write(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_private_write(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_private_write(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create_new(true).write(true).open(path)
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

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_prepara_confirma_y_elimina() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "sysopt-journal-test-{}-{nonce}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("journal.json");
        let mut journal = ActionJournal::open(path.clone(), 120).unwrap();
        assert!(ActionJournal::open(path.clone(), 120).is_err());
        let id = journal
            .prepare(
                7,
                10,
                "test",
                NativeProcessIdentity::Fallback {
                    start_time_secs: 10,
                },
                "test",
                ResourceChange::Priority {
                    original: 0,
                    applied: 10,
                    previous_applied: None,
                },
            )
            .unwrap();
        assert!(journal
            .prepare(
                7,
                10,
                "duplicate",
                NativeProcessIdentity::Fallback {
                    start_time_secs: 10,
                },
                "test",
                ResourceChange::Priority {
                    original: 0,
                    applied: 19,
                    previous_applied: None,
                },
            )
            .is_err());
        journal
            .update_change(
                id,
                ResourceChange::Priority {
                    original: 0,
                    applied: 19,
                    previous_applied: Some(10),
                },
            )
            .unwrap();
        journal.mark_applied(id).unwrap();
        let before_heartbeat = journal.entries()[0].clone();
        journal.renew_all().unwrap();
        let after_early_heartbeat = journal.entries()[0].clone();
        assert_eq!(
            before_heartbeat.last_confirmed_at_unix_secs,
            after_early_heartbeat.last_confirmed_at_unix_secs
        );
        assert_eq!(
            before_heartbeat.lease_expires_at_unix_secs,
            after_early_heartbeat.lease_expires_at_unix_secs
        );
        journal.data.entries[0].lease_expires_at_unix_secs = 0;
        journal.renew_all().unwrap();
        assert!(journal.entries()[0].lease_expires_at_unix_secs > 0);
        assert_eq!(journal.entries().len(), 1);
        let entries = journal.entries();
        assert!(matches!(
            &entries[0].change,
            ResourceChange::Priority {
                previous_applied: None,
                ..
            }
        ));
        let second_id = journal
            .prepare(
                8,
                11,
                "second",
                NativeProcessIdentity::Fallback {
                    start_time_secs: 11,
                },
                "test",
                ResourceChange::Priority {
                    original: 0,
                    applied: 10,
                    previous_applied: None,
                },
            )
            .unwrap();
        assert_eq!(journal.remove_many(&[id, second_id]).unwrap(), 2);
        assert!(journal.entries().is_empty());
        drop(journal);
        let reopened = ActionJournal::open(path, 120).unwrap();
        assert!(reopened.entries().is_empty());
        drop(reopened);
        let _ = fs::remove_dir_all(root);
    }
}
