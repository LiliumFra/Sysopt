use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum NativeProcessIdentity {
    Linux { boot_id: String, start_ticks: u64 },
    Windows { creation_time_100ns: u64 },
    Macos { start_sec: u64, start_usec: u64 },
    Fallback { start_time_secs: u64 },
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(crate) fn belongs_to_current_platform(identity: &NativeProcessIdentity) -> bool {
    #[cfg(target_os = "windows")]
    {
        matches!(identity, NativeProcessIdentity::Windows { .. })
    }
    #[cfg(target_os = "macos")]
    {
        matches!(identity, NativeProcessIdentity::Macos { .. })
    }
}

#[cfg(target_os = "linux")]
pub struct ProcessGuard {
    pid: u32,
    identity: NativeProcessIdentity,
    pidfd: Option<std::os::fd::RawFd>,
}

#[cfg(target_os = "linux")]
impl ProcessGuard {
    pub fn acquire(pid: u32, expected_start_time_secs: u64) -> Result<Self> {
        let identity = capture(pid, expected_start_time_secs)?;
        let pidfd = open_pidfd(pid)?;
        let second = capture(pid, expected_start_time_secs)?;
        anyhow::ensure!(
            identity == second,
            "pid {pid} cambió de identidad mientras se adquiría pidfd"
        );
        Ok(Self {
            pid,
            identity,
            pidfd,
        })
    }

    pub fn identity(&self) -> &NativeProcessIdentity {
        &self.identity
    }

    pub fn still_same(&self) -> Result<bool> {
        if let Some(fd) = self.pidfd {
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
            if result < 0 {
                return Err(std::io::Error::last_os_error()).context("poll(pidfd) falló");
            }
            if result > 0 && pollfd.revents != 0 {
                return Ok(false);
            }
        }
        matches_identity(self.pid, &self.identity)
    }
}

#[cfg(target_os = "linux")]
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(fd) = self.pidfd.take() {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub struct ProcessGuard {
    pid: u32,
    identity: NativeProcessIdentity,
}

#[cfg(target_os = "macos")]
impl ProcessGuard {
    pub fn acquire(pid: u32, expected_start_time_secs: u64) -> Result<Self> {
        let identity = capture(pid, expected_start_time_secs)?;
        Ok(Self { pid, identity })
    }

    pub fn identity(&self) -> &NativeProcessIdentity {
        &self.identity
    }

    pub fn still_same(&self) -> Result<bool> {
        matches_identity(self.pid, &self.identity)
    }
}

#[cfg(target_os = "linux")]
pub fn capture(pid: u32, expected_start_time_secs: u64) -> Result<NativeProcessIdentity> {
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("no se pudo leer el boot_id de Linux")?
        .trim()
        .to_owned();
    anyhow::ensure!(!boot_id.is_empty(), "boot_id de Linux vacío");
    let start_ticks = linux_start_ticks(pid)?;
    let actual_start_time_secs = linux_start_time_secs(start_ticks)?;
    anyhow::ensure!(
        actual_start_time_secs == expected_start_time_secs,
        "pid {pid} fue reutilizado: inicio esperado {expected_start_time_secs}, actual {actual_start_time_secs}"
    );
    Ok(NativeProcessIdentity::Linux {
        boot_id,
        start_ticks,
    })
}

#[cfg(target_os = "linux")]
pub fn matches_identity(pid: u32, expected: &NativeProcessIdentity) -> Result<bool> {
    let NativeProcessIdentity::Linux {
        boot_id,
        start_ticks,
    } = expected
    else {
        return Ok(false);
    };
    let current_boot_id = match std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        Ok(value) => value.trim().to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("no se pudo leer el boot_id de Linux"),
    };
    if &current_boot_id != boot_id {
        return Ok(false);
    }
    match linux_start_ticks(pid) {
        Ok(current) => Ok(current == *start_ticks),
        Err(error) if process_missing_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn linux_start_ticks(pid: u32) -> Result<u64> {
    let path = format!("/proc/{pid}/stat");
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("no se pudo leer {path}"))?;
    let command_end = content
        .rfind(')')
        .context("formato inesperado de /proc/<pid>/stat")?;
    let remainder = content
        .get(command_end + 1..)
        .context("formato incompleto de /proc/<pid>/stat")?
        .trim_start();
    let start_ticks = remainder
        .split_whitespace()
        .nth(19)
        .context("/proc/<pid>/stat no contiene starttime")?
        .parse::<u64>()
        .context("starttime inválido en /proc/<pid>/stat")?;
    Ok(start_ticks)
}

#[cfg(target_os = "linux")]
fn linux_start_time_secs(start_ticks: u64) -> Result<u64> {
    let stat = std::fs::read_to_string("/proc/stat").context("no se pudo leer /proc/stat")?;
    let boot_time = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .context("/proc/stat no contiene btime")?
        .trim()
        .parse::<u64>()
        .context("btime inválido en /proc/stat")?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    anyhow::ensure!(ticks_per_second > 0, "sysconf(_SC_CLK_TCK) falló");
    Ok(boot_time.saturating_add(start_ticks / ticks_per_second as u64))
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> Result<Option<std::os::fd::RawFd>> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd >= 0 {
        return Ok(Some(fd as std::os::fd::RawFd));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EPERM) => Ok(None),
        Some(libc::ESRCH) => anyhow::bail!("pid {pid} ya no existe"),
        _ => Err(error).with_context(|| format!("pidfd_open falló para pid {pid}")),
    }
}

#[cfg(target_os = "macos")]
pub fn capture(pid: u32, expected_start_time_secs: u64) -> Result<NativeProcessIdentity> {
    let info = macos_bsd_info(pid)?;
    anyhow::ensure!(
        info.pbi_start_tvsec == expected_start_time_secs,
        "pid {pid} fue reutilizado: inicio esperado {expected_start_time_secs}, actual {}",
        info.pbi_start_tvsec
    );
    Ok(NativeProcessIdentity::Macos {
        start_sec: info.pbi_start_tvsec,
        start_usec: info.pbi_start_tvusec,
    })
}

#[cfg(target_os = "macos")]
pub fn matches_identity(pid: u32, expected: &NativeProcessIdentity) -> Result<bool> {
    let NativeProcessIdentity::Macos {
        start_sec,
        start_usec,
    } = expected
    else {
        return Ok(false);
    };
    match macos_bsd_info(pid) {
        Ok(info) => Ok(info.pbi_start_tvsec == *start_sec && info.pbi_start_tvusec == *start_usec),
        Err(error) if process_missing_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "macos")]
const PROC_PIDTBSDINFO: i32 = 3;
#[cfg(target_os = "macos")]
const MAXCOMLEN: usize = 16;

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    rfu_1: u32,
    pbi_comm: [u8; MAXCOMLEN],
    pbi_name: [u8; 2 * MAXCOMLEN],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
extern "C" {
    fn proc_pidinfo(
        pid: i32,
        flavor: i32,
        arg: u64,
        buffer: *mut libc::c_void,
        buffersize: i32,
    ) -> i32;
}

#[cfg(target_os = "macos")]
fn macos_bsd_info(pid: u32) -> Result<ProcBsdInfo> {
    let mut info = std::mem::MaybeUninit::<ProcBsdInfo>::zeroed();
    let expected = std::mem::size_of::<ProcBsdInfo>();
    let written = unsafe {
        proc_pidinfo(
            pid as i32,
            PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            expected as i32,
        )
    };
    if written != expected as i32 {
        let error = std::io::Error::last_os_error();
        return Err(error).with_context(|| format!("proc_pidinfo falló para pid {pid}"));
    }
    Ok(unsafe { info.assume_init() })
}

#[cfg(target_os = "windows")]
pub fn matches_handle_identity(
    handle: windows_sys::Win32::Foundation::HANDLE,
    expected: &NativeProcessIdentity,
) -> Result<bool> {
    let NativeProcessIdentity::Windows {
        creation_time_100ns,
    } = expected
    else {
        return Ok(false);
    };
    Ok(process_creation_time_100ns(handle)? == *creation_time_100ns)
}

#[cfg(target_os = "windows")]
pub fn identity_from_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
    expected_start_time_secs: u64,
) -> Result<NativeProcessIdentity> {
    let creation_time_100ns = process_creation_time_100ns(handle)?;
    let actual_start_time_secs = filetime_to_unix_secs(creation_time_100ns)
        .context("hora de creación de proceso anterior a Unix epoch")?;
    anyhow::ensure!(
        actual_start_time_secs == expected_start_time_secs,
        "identidad Windows no coincide: inicio esperado {expected_start_time_secs}, actual {actual_start_time_secs}"
    );
    Ok(NativeProcessIdentity::Windows {
        creation_time_100ns,
    })
}

#[cfg(target_os = "windows")]
fn process_creation_time_100ns(handle: windows_sys::Win32::Foundation::HANDLE) -> Result<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    let result =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    if result == 0 {
        anyhow::bail!("GetProcessTimes falló: {}", std::io::Error::last_os_error());
    }
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

#[cfg(target_os = "windows")]
fn filetime_to_unix_secs(ticks: u64) -> Option<u64> {
    const WINDOWS_TO_UNIX_EPOCH_SECS: u64 = 11_644_473_600;
    const TICKS_PER_SEC: u64 = 10_000_000;
    ticks
        .checked_div(TICKS_PER_SEC)?
        .checked_sub(WINDOWS_TO_UNIX_EPOCH_SECS)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn process_missing_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(io.kind(), std::io::ErrorKind::NotFound)
                || io.raw_os_error() == Some(libc::ESRCH)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::NativeProcessIdentity;

    #[test]
    fn identidad_fallback_es_estable() {
        assert_eq!(
            NativeProcessIdentity::Fallback { start_time_secs: 7 },
            NativeProcessIdentity::Fallback { start_time_secs: 7 }
        );
    }
}
