use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CAP_SYS_NICE: u32 = 23;
const DEFAULT_HELPER_PATHS: [&str; 3] = [
    "/usr/lib/sysopt/sysopt-priority-helper",
    "/usr/local/libexec/sysopt-priority-helper",
    "/usr/local/lib/sysopt/sysopt-priority-helper",
];

pub(crate) fn direct_priority_privilege() -> bool {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return false;
    };
    let Some(hex) = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:\t"))
    else {
        return false;
    };
    let Ok(bits) = u64::from_str_radix(hex.trim(), 16) else {
        return false;
    };
    bits & (1_u64 << CAP_SYS_NICE) != 0
}

pub(crate) fn helper_available() -> bool {
    helper_candidates().into_iter().any(|path| {
        Command::new(path)
            .arg("--probe")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear()
            .status()
            .is_ok_and(|status| status.success())
    })
}

pub(crate) fn set_nice_via_helper(pid: u32, start_time: u64, name: &str, nice: i32) -> Result<()> {
    let path = helper_candidates()
        .into_iter()
        .find(|path| path.is_file())
        .context("no se encontró sysopt-priority-helper")?;
    let output = Command::new(&path)
        .arg("--set")
        .arg(pid.to_string())
        .arg(start_time.to_string())
        .arg(nice.to_string())
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env_clear()
        .output()
        .with_context(|| format!("no se pudo ejecutar {}", path.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    bail!(
        "{} rechazó el cambio{}",
        path.display(),
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )
}

fn helper_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("SYSOPT_PRIORITY_HELPER")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        candidates.push(path);
    }
    candidates.extend(DEFAULT_HELPER_PATHS.into_iter().map(PathBuf::from));
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            candidates.push(parent.join("sysopt-priority-helper"));
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

pub fn main_from_env() -> Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [probe] if probe == "--probe" => {
            anyhow::ensure!(
                direct_priority_privilege(),
                "CAP_SYS_NICE no está activa en el helper"
            );
            Ok(())
        }
        [set, pid, start_time, nice, expected_name] if set == "--set" => {
            anyhow::ensure!(
                direct_priority_privilege(),
                "CAP_SYS_NICE no está activa en el helper"
            );
            apply_validated_change(pid, start_time, nice, expected_name)
        }
        _ => bail!("uso interno inválido"),
    }
}

fn apply_validated_change(
    pid: &OsStr,
    expected_start_time: &OsStr,
    requested_nice: &OsStr,
    expected_name: &OsStr,
) -> Result<()> {
    let pid = parse_os::<u32>(pid, "pid")?;
    let expected_start_time = parse_os::<u64>(expected_start_time, "start_time")?;
    let nice = parse_os::<i32>(requested_nice, "nice")?;
    anyhow::ensure!(pid > 4 && pid != std::process::id(), "pid no administrable");
    anyhow::ensure!((-20..=19).contains(&nice), "nice fuera de rango");

    let caller_uid = unsafe { libc::getuid() };
    let target_uid = process_uid(pid)?;
    anyhow::ensure!(
        caller_uid == 0 || target_uid == caller_uid,
        "el proceso objetivo no pertenece al usuario solicitante"
    );

    let start_ticks = process_start_ticks(pid)?;
    let actual_start_time = linux_start_time_secs(start_ticks)?;
    anyhow::ensure!(
        actual_start_time == expected_start_time,
        "identidad temporal del proceso no coincide"
    );
    validate_process_name(pid, expected_name)?;

    let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("setpriority falló");
    }
    anyhow::ensure!(
        process_start_ticks(pid)? == start_ticks,
        "el pid cambió de identidad durante el cambio"
    );
    Ok(())
}

fn parse_os<T: std::str::FromStr>(value: &OsStr, field: &str) -> Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    value
        .to_str()
        .context("argumento no UTF-8")?
        .parse::<T>()
        .with_context(|| format!("{field} inválido"))
}

fn process_uid(pid: u32) -> Result<libc::uid_t> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .with_context(|| format!("no se pudo leer el UID de pid {pid}"))?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|line| line.split_whitespace().next())
        .context("/proc/<pid>/status no contiene Uid")?
        .parse::<libc::uid_t>()
        .context("Uid inválido")?;
    Ok(uid)
}

fn process_start_ticks(pid: u32) -> Result<u64> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .with_context(|| format!("no se pudo leer /proc/{pid}/stat"))?;
    let command_end = content.rfind(')').context("stat de proceso inválido")?;
    content
        .get(command_end + 1..)
        .context("stat de proceso incompleto")?
        .split_whitespace()
        .nth(19)
        .context("stat de proceso sin starttime")?
        .parse::<u64>()
        .context("starttime inválido")
}

fn linux_start_time_secs(start_ticks: u64) -> Result<u64> {
    let stat = std::fs::read_to_string("/proc/stat").context("no se pudo leer /proc/stat")?;
    let boot_time = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .context("/proc/stat no contiene btime")?
        .trim()
        .parse::<u64>()
        .context("btime inválido")?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    anyhow::ensure!(ticks_per_second > 0, "sysconf(_SC_CLK_TCK) falló");
    Ok(boot_time.saturating_add(start_ticks / ticks_per_second as u64))
}

fn validate_process_name(pid: u32, expected: &OsStr) -> Result<()> {
    let expected = expected.to_string_lossy();
    let expected = normalized_name(&expected);
    anyhow::ensure!(!expected.is_empty(), "nombre esperado vacío");

    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|value| normalized_name(value.trim()))
        .unwrap_or_default();
    let executable = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| normalized_name(&name.to_string_lossy()))
        })
        .unwrap_or_default();
    anyhow::ensure!(
        names_compatible(&expected, &comm) || names_compatible(&expected, &executable),
        "el nombre del proceso no coincide"
    );
    Ok(())
}

fn normalized_name(value: &str) -> String {
    Path::new(value)
        .file_name()
        .unwrap_or_else(|| OsStr::new(value))
        .to_string_lossy()
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches(".exe")
        .to_owned()
}

fn names_compatible(expected: &str, actual: &str) -> bool {
    !actual.is_empty()
        && (expected == actual || expected.starts_with(actual) || actual.starts_with(expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normaliza_nombres() {
        assert_eq!(normalized_name("/usr/bin/code"), "code");
        assert_eq!(normalized_name("LSASS.EXE"), "lsass");
    }

    #[test]
    fn compatibilidad_tolera_comm_truncado() {
        assert!(names_compatible("nombre-muy-largo", "nombre-muy-larg"));
        assert!(!names_compatible("code", "cargo"));
    }
}
