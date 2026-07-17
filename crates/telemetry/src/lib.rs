use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::path::{Path, PathBuf};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSample {
    pub pid: u32,
    /// Segundos desde Unix epoch, provistos por `sysinfo`. Se combina con el
    /// PID para evitar confundir un PID reciclado.
    pub start_time: u64,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    #[serde(default)]
    pub read_bytes: u64,
    #[serde(default)]
    pub written_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PressureSnapshot {
    /// Indica si la plataforma expuso métricas PSI compatibles.
    pub supported: bool,
    /// Porcentaje medio de tiempo durante los últimos 10 s con al menos una
    /// tarea esperando CPU.
    pub cpu_some_avg10: f32,
    /// Porcentaje medio de tiempo durante los últimos 10 s con al menos una
    /// tarea bloqueada por recuperación de memoria.
    pub memory_some_avg10: f32,
    /// Porcentaje medio de tiempo durante los últimos 10 s con todas las
    /// tareas no ociosas bloqueadas por memoria.
    pub memory_full_avg10: f32,
    /// Porcentaje medio de tiempo durante los últimos 10 s con al menos una
    /// tarea bloqueada por E/S.
    pub io_some_avg10: f32,
    /// Porcentaje medio de tiempo durante los últimos 10 s con todas las
    /// tareas no ociosas bloqueadas por E/S.
    pub io_full_avg10: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThermalState {
    #[default]
    Unknown,
    Nominal,
    Fair,
    Serious,
    Critical,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PowerSnapshot {
    pub supported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_ac_power: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_percent: Option<u8>,
    #[serde(default)]
    pub battery_saver: bool,
    #[serde(default)]
    pub thermal_state: ThermalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemSnapshot {
    pub global_cpu_percent: f32,
    pub used_memory_bytes: u64,
    #[serde(default)]
    pub available_memory_bytes: u64,
    pub total_memory_bytes: u64,
    pub process_count: usize,
    pub top_processes: Vec<ProcessSample>,
    #[serde(default)]
    pub io_processes: Vec<ProcessSample>,
    #[serde(default)]
    pub pressure: PressureSnapshot,
    #[serde(default)]
    pub power: PowerSnapshot,
}

pub struct Telemetry {
    sys: System,
}

impl Telemetry {
    pub fn new() -> Self {
        // `new_all` ya realiza la carga inicial. Un `refresh_all` inmediato
        // duplicaba lecturas de /proc/Win32 APIs sin mejorar la primera
        // medición de CPU, que de todos modos necesita un intervalo.
        Self {
            sys: System::new_all(),
        }
    }

    /// Toma una foto del estado actual del sistema.
    /// `top_n` controla cuántos procesos (ordenados por CPU) se incluyen,
    /// para no arrastrar cientos de procesos en cada ciclo.
    pub fn snapshot(&mut self, top_n: usize) -> SystemSnapshot {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_memory()
                .with_cpu()
                .with_disk_usage()
                .with_exe(UpdateKind::OnlyIfNotSet)
                .with_cwd(UpdateKind::OnlyIfNotSet)
                .without_tasks(),
        );
        self.sys.refresh_memory();

        let global_cpu_percent = self.sys.global_cpu_usage();
        let used_memory_bytes = self.sys.used_memory();
        let available_memory_bytes = self.sys.available_memory();
        let total_memory_bytes = self.sys.total_memory();
        let process_count = self.sys.processes().len();
        let pressure = system_pressure_snapshot();
        let power = system_power_snapshot();

        // Se ordenan referencias y métricas escalares primero. Obtener `cwd`,
        // `exe` y convertir nombres puede requerir llamadas adicionales al SO;
        // solo se hace para los procesos que realmente entran en los rankings.
        let candidates = self
            .sys
            .processes()
            .values()
            .map(|process| {
                let disk = process.disk_usage();
                (process, disk.read_bytes, disk.written_bytes)
            })
            .collect::<Vec<_>>();

        let mut cpu_indices = (0..candidates.len()).collect::<Vec<_>>();
        cpu_indices.sort_unstable_by(|&left, &right| {
            candidates[right]
                .0
                .cpu_usage()
                .partial_cmp(&candidates[left].0.cpu_usage())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    candidates[right]
                        .0
                        .memory()
                        .cmp(&candidates[left].0.memory())
                })
        });
        let cpu_selected = cpu_indices.into_iter().take(top_n).collect::<Vec<_>>();

        let mut io_indices = (0..candidates.len())
            .filter(|&index| candidates[index].1 > 0 || candidates[index].2 > 0)
            .collect::<Vec<_>>();
        io_indices.sort_unstable_by(|&left, &right| {
            let left_io = candidates[left].1.saturating_add(candidates[left].2);
            let right_io = candidates[right].1.saturating_add(candidates[right].2);
            right_io.cmp(&left_io).then_with(|| {
                candidates[right]
                    .0
                    .cpu_usage()
                    .partial_cmp(&candidates[left].0.cpu_usage())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        let io_selected = io_indices
            .into_iter()
            .take(top_n.min(8))
            .collect::<Vec<_>>();

        // Un proceso puede estar en ambos rankings. Se materializa una sola vez
        // para no repetir consultas de ruta/nombre al sistema operativo.
        let mut samples = HashMap::with_capacity(cpu_selected.len() + io_selected.len());
        for &index in cpu_selected.iter().chain(&io_selected) {
            samples.entry(index).or_insert_with(|| {
                let (process, read_bytes, written_bytes) = candidates[index];
                Self::process_sample(process, read_bytes, written_bytes)
            });
        }
        let top_processes = cpu_selected
            .iter()
            .filter_map(|index| samples.get(index).cloned())
            .collect();
        let io_processes = io_selected
            .iter()
            .filter_map(|index| samples.get(index).cloned())
            .collect();

        SystemSnapshot {
            global_cpu_percent,
            used_memory_bytes,
            available_memory_bytes,
            total_memory_bytes,
            process_count,
            top_processes,
            io_processes,
            pressure,
            power,
        }
    }

    fn process_sample(
        process: &sysinfo::Process,
        read_bytes: u64,
        written_bytes: u64,
    ) -> ProcessSample {
        ProcessSample {
            pid: process.pid().as_u32(),
            start_time: process.start_time(),
            name: process.name().to_string_lossy().into_owned(),
            cpu_percent: process.cpu_usage(),
            memory_bytes: process.memory(),
            read_bytes,
            written_bytes,
            cwd: process.cwd().map(Path::to_path_buf),
            executable: process.exe().map(Path::to_path_buf),
        }
    }

    /// Actualiza la tabla de procesos inmediatamente antes y después de
    /// aplicar acciones. Reduce la ventana en la que un PID podría reciclarse
    /// entre la decisión y el enforcement.
    pub fn refresh_process_identities(&mut self) {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().without_tasks(),
        );
    }

    /// Comprueba que el PID siga apuntando al mismo proceso observado.
    pub fn is_same_process(&self, pid: u32, start_time: u64) -> bool {
        self.sys
            .process(sysinfo::Pid::from_u32(pid))
            .is_some_and(|process| process.start_time() == start_time)
    }

    pub fn is_alive(&self, pid: u32) -> bool {
        self.sys.process(sysinfo::Pid::from_u32(pid)).is_some()
    }

    /// PID de la aplicación que posee la ventana en primer plano cuando la
    /// plataforma ofrece una API estable sin permisos adicionales.
    pub fn foreground_pid(&self) -> Option<u32> {
        foreground_process_id()
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
fn foreground_process_id() -> Option<u32> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    let window = unsafe { GetForegroundWindow() };
    if window == Default::default() {
        return None;
    }
    let mut process_id = 0u32;
    let thread_id = unsafe { GetWindowThreadProcessId(window, &mut process_id) };
    (thread_id != 0 && process_id > 0).then_some(process_id)
}

#[cfg(target_os = "linux")]
fn foreground_process_id() -> Option<u32> {
    linux_x11_foreground_pid()
}

#[cfg(target_os = "macos")]
fn foreground_process_id() -> Option<u32> {
    macos_frontmost_pid()
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn foreground_process_id() -> Option<u32> {
    None
}

#[cfg(target_os = "linux")]
fn system_pressure_snapshot() -> PressureSnapshot {
    let cpu = read_pressure_file("/proc/pressure/cpu");
    let memory = read_pressure_file("/proc/pressure/memory");
    let io = read_pressure_file("/proc/pressure/io");
    // Se anuncia soporte solo con el conjunto completo. Tratar una métrica
    // ausente como cero podría ocultar contención y relajar un guardarraíl.
    let supported = cpu.as_ref().is_some_and(|value| value.some_avg10.is_some())
        && memory
            .as_ref()
            .is_some_and(|value| value.some_avg10.is_some() && value.full_avg10.is_some())
        && io
            .as_ref()
            .is_some_and(|value| value.some_avg10.is_some() && value.full_avg10.is_some());
    let cpu = cpu.unwrap_or_default();
    let memory = memory.unwrap_or_default();
    let io = io.unwrap_or_default();
    PressureSnapshot {
        supported,
        cpu_some_avg10: cpu.some_avg10.unwrap_or_default(),
        memory_some_avg10: memory.some_avg10.unwrap_or_default(),
        memory_full_avg10: memory.full_avg10.unwrap_or_default(),
        io_some_avg10: io.some_avg10.unwrap_or_default(),
        io_full_avg10: io.full_avg10.unwrap_or_default(),
    }
}

#[cfg(not(target_os = "linux"))]
fn system_pressure_snapshot() -> PressureSnapshot {
    PressureSnapshot::default()
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, Default)]
struct ParsedPressure {
    some_avg10: Option<f32>,
    full_avg10: Option<f32>,
}

#[cfg(target_os = "linux")]
fn read_pressure_file(path: &str) -> Option<ParsedPressure> {
    let content = fs::read_to_string(path).ok()?;
    parse_pressure(&content)
}

#[cfg(target_os = "linux")]
fn parse_pressure(content: &str) -> Option<ParsedPressure> {
    let mut parsed = ParsedPressure::default();
    let mut found = false;
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let kind = fields.next()?;
        let avg10 = fields.find_map(|field| {
            field
                .strip_prefix("avg10=")
                .and_then(|value| value.parse::<f32>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0)
        });
        let Some(avg10) = avg10 else {
            continue;
        };
        match kind {
            "some" => {
                parsed.some_avg10 = Some(avg10);
                found = true;
            }
            "full" => {
                parsed.full_avg10 = Some(avg10);
                found = true;
            }
            _ => {}
        }
    }
    found.then_some(parsed)
}

#[cfg(target_os = "windows")]
fn system_power_snapshot() -> PowerSnapshot {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
        return PowerSnapshot::default();
    }
    let on_ac_power = match status.ACLineStatus {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    let battery_percent = (status.BatteryLifePercent <= 100).then_some(status.BatteryLifePercent);
    // SystemStatusFlag == 1 indica Battery Saver activo.
    let battery_saver = status.SystemStatusFlag & 1 != 0;
    PowerSnapshot {
        supported: on_ac_power.is_some() || battery_percent.is_some(),
        on_ac_power,
        battery_percent,
        battery_saver,
        thermal_state: ThermalState::Unknown,
        temperature_c: None,
    }
}

#[cfg(target_os = "linux")]
fn system_power_snapshot() -> PowerSnapshot {
    let mut on_ac_power = None;
    let mut battery_percent = None;
    let power_root = Path::new("/sys/class/power_supply");
    if let Ok(entries) = fs::read_dir(power_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let kind = fs::read_to_string(path.join("type"))
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            if matches!(kind.as_str(), "mains" | "usb" | "usb_c" | "wireless") {
                if let Ok(online) = fs::read_to_string(path.join("online")) {
                    if online.trim() == "1" {
                        on_ac_power = Some(true);
                    } else if on_ac_power.is_none() {
                        on_ac_power = Some(false);
                    }
                }
            } else if kind == "battery" {
                if let Ok(capacity) = fs::read_to_string(path.join("capacity")) {
                    if let Ok(value) = capacity.trim().parse::<u8>() {
                        if value <= 100 {
                            battery_percent = Some(
                                battery_percent.map_or(value, |current: u8| current.min(value)),
                            );
                        }
                    }
                }
                if on_ac_power.is_none() {
                    if let Ok(status) = fs::read_to_string(path.join("status")) {
                        on_ac_power = match status.trim().to_ascii_lowercase().as_str() {
                            "charging" | "full" | "not charging" => Some(true),
                            "discharging" => Some(false),
                            _ => None,
                        };
                    }
                }
            }
        }
    }
    let temperature_c = linux_max_temperature_c();
    let thermal_state = temperature_c.map_or(ThermalState::Unknown, |temp| {
        if temp >= 95.0 {
            ThermalState::Critical
        } else if temp >= 85.0 {
            ThermalState::Serious
        } else if temp >= 75.0 {
            ThermalState::Fair
        } else {
            ThermalState::Nominal
        }
    });
    PowerSnapshot {
        supported: on_ac_power.is_some() || battery_percent.is_some() || temperature_c.is_some(),
        on_ac_power,
        battery_percent,
        // Linux no expone un estado Battery Saver universal. La batería baja se
        // informa por separado y el controlador de seguridad decide la política.
        battery_saver: false,
        thermal_state,
        temperature_c,
    }
}

#[cfg(target_os = "linux")]
fn linux_max_temperature_c() -> Option<f32> {
    let entries = fs::read_dir("/sys/class/thermal").ok()?;
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("thermal_zone")
        })
        .filter_map(|entry| fs::read_to_string(entry.path().join("temp")).ok())
        .filter_map(|value| value.trim().parse::<f32>().ok())
        .map(|value| {
            if value > 1_000.0 {
                value / 1_000.0
            } else {
                value
            }
        })
        .filter(|value| value.is_finite() && (0.0..=150.0).contains(value))
        .max_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal))
}

#[cfg(target_os = "macos")]
fn system_power_snapshot() -> PowerSnapshot {
    let Some((low_power, thermal_raw)) = macos_process_info() else {
        return PowerSnapshot::default();
    };
    let thermal_state = thermal_raw.map_or(ThermalState::Unknown, |value| match value {
        0 => ThermalState::Nominal,
        1 => ThermalState::Fair,
        2 => ThermalState::Serious,
        3 => ThermalState::Critical,
        _ => ThermalState::Unknown,
    });
    PowerSnapshot {
        supported: low_power.is_some() || thermal_raw.is_some(),
        on_ac_power: None,
        battery_percent: None,
        battery_saver: low_power.unwrap_or(false),
        thermal_state,
        temperature_c: None,
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn system_power_snapshot() -> PowerSnapshot {
    PowerSnapshot::default()
}

#[cfg(target_os = "linux")]
fn linux_x11_foreground_pid() -> Option<u32> {
    use libc::{c_char, c_int, c_long, c_uchar, c_ulong, c_void};
    use std::ffi::CString;
    use std::mem::transmute;
    use std::ptr;

    type Display = c_void;
    type Window = c_ulong;
    type Atom = c_ulong;
    type XOpenDisplay = unsafe extern "C" fn(*const c_char) -> *mut Display;
    type XCloseDisplay = unsafe extern "C" fn(*mut Display) -> c_int;
    type XDefaultRootWindow = unsafe extern "C" fn(*mut Display) -> Window;
    type XInternAtom = unsafe extern "C" fn(*mut Display, *const c_char, c_int) -> Atom;
    type XGetWindowProperty = unsafe extern "C" fn(
        *mut Display,
        Window,
        Atom,
        c_long,
        c_long,
        c_int,
        Atom,
        *mut Atom,
        *mut c_int,
        *mut c_ulong,
        *mut c_ulong,
        *mut *mut c_uchar,
    ) -> c_int;
    type XFree = unsafe extern "C" fn(*mut c_void) -> c_int;

    macro_rules! symbol {
        ($lib:expr, $name:expr, $ty:ty) => {{
            let raw = libc::dlsym($lib, $name.as_ptr().cast());
            if raw.is_null() {
                None
            } else {
                Some(transmute::<*mut c_void, $ty>(raw))
            }
        }};
    }

    unsafe {
        let lib = [b"libX11.so.6\0".as_slice(), b"libX11.so\0".as_slice()]
            .into_iter()
            .find_map(|name| {
                let handle = libc::dlopen(name.as_ptr().cast(), libc::RTLD_NOW | libc::RTLD_LOCAL);
                (!handle.is_null()).then_some(handle)
            })?;
        let result = (|| {
            let open: XOpenDisplay = symbol!(lib, b"XOpenDisplay\0", XOpenDisplay)?;
            let close: XCloseDisplay = symbol!(lib, b"XCloseDisplay\0", XCloseDisplay)?;
            let root_window: XDefaultRootWindow =
                symbol!(lib, b"XDefaultRootWindow\0", XDefaultRootWindow)?;
            let intern: XInternAtom = symbol!(lib, b"XInternAtom\0", XInternAtom)?;
            let get_property: XGetWindowProperty =
                symbol!(lib, b"XGetWindowProperty\0", XGetWindowProperty)?;
            let xfree: XFree = symbol!(lib, b"XFree\0", XFree)?;
            let display = open(ptr::null());
            if display.is_null() {
                return None;
            }
            let root = root_window(display);
            let active_name = CString::new("_NET_ACTIVE_WINDOW").ok()?;
            let pid_name = CString::new("_NET_WM_PID").ok()?;
            let active_atom = intern(display, active_name.as_ptr(), 1);
            let pid_atom = intern(display, pid_name.as_ptr(), 1);
            if active_atom == 0 || pid_atom == 0 {
                close(display);
                return None;
            }
            let read_u32 = |window: Window, property: Atom| -> Option<u32> {
                let mut actual_type = 0;
                let mut actual_format = 0;
                let mut item_count = 0;
                let mut bytes_after = 0;
                let mut data: *mut c_uchar = ptr::null_mut();
                let status = get_property(
                    display,
                    window,
                    property,
                    0,
                    1,
                    0,
                    0,
                    &mut actual_type,
                    &mut actual_format,
                    &mut item_count,
                    &mut bytes_after,
                    &mut data,
                );
                if status != 0 || data.is_null() || item_count == 0 || actual_format != 32 {
                    if !data.is_null() {
                        xfree(data.cast());
                    }
                    return None;
                }
                let value = *(data.cast::<c_ulong>()) as u32;
                xfree(data.cast());
                Some(value)
            };
            let window = match read_u32(root, active_atom) {
                Some(window) => window as Window,
                None => {
                    close(display);
                    return None;
                }
            };
            let pid = read_u32(window, pid_atom);
            close(display);
            pid.filter(|value| *value > 0)
        })();
        libc::dlclose(lib);
        result
    }
}

#[cfg(target_os = "macos")]
unsafe fn macos_objc_handles() -> Option<(*mut libc::c_void, *mut libc::c_void)> {
    let objc = libc::dlopen(
        b"/usr/lib/libobjc.A.dylib\0".as_ptr().cast(),
        libc::RTLD_NOW | libc::RTLD_LOCAL,
    );
    if objc.is_null() {
        return None;
    }
    let foundation = libc::dlopen(
        b"/System/Library/Frameworks/Foundation.framework/Foundation\0"
            .as_ptr()
            .cast(),
        libc::RTLD_NOW | libc::RTLD_LOCAL,
    );
    if foundation.is_null() {
        libc::dlclose(objc);
        return None;
    }
    Some((objc, foundation))
}

#[cfg(target_os = "macos")]
fn macos_process_info() -> Option<(Option<bool>, Option<usize>)> {
    use libc::{c_char, c_void};
    use std::mem::transmute;
    type Id = *mut c_void;
    type Sel = *mut c_void;
    type GetClass = unsafe extern "C" fn(*const c_char) -> Id;
    type RegisterSel = unsafe extern "C" fn(*const c_char) -> Sel;
    type MsgId = unsafe extern "C" fn(Id, Sel) -> Id;
    type MsgBool = unsafe extern "C" fn(Id, Sel) -> libc::c_schar;
    type MsgBoolSel = unsafe extern "C" fn(Id, Sel, Sel) -> libc::c_schar;
    type MsgUsize = unsafe extern "C" fn(Id, Sel) -> usize;
    unsafe {
        let (objc, foundation) = macos_objc_handles()?;
        let get_class_raw = libc::dlsym(objc, b"objc_getClass\0".as_ptr().cast());
        let register_raw = libc::dlsym(objc, b"sel_registerName\0".as_ptr().cast());
        let raw_msg = libc::dlsym(objc, b"objc_msgSend\0".as_ptr().cast());
        if get_class_raw.is_null() || register_raw.is_null() || raw_msg.is_null() {
            libc::dlclose(foundation);
            libc::dlclose(objc);
            return None;
        }
        let get_class: GetClass = transmute(get_class_raw);
        let register: RegisterSel = transmute(register_raw);
        let msg_id: MsgId = transmute(raw_msg);
        let msg_bool: MsgBool = transmute(raw_msg);
        let msg_bool_sel: MsgBoolSel = transmute(raw_msg);
        let msg_usize: MsgUsize = transmute(raw_msg);
        let class = get_class(b"NSProcessInfo\0".as_ptr().cast());
        if class.is_null() {
            libc::dlclose(foundation);
            libc::dlclose(objc);
            return None;
        }
        let process_info = msg_id(class, register(b"processInfo\0".as_ptr().cast()));
        if process_info.is_null() {
            libc::dlclose(foundation);
            libc::dlclose(objc);
            return None;
        }
        let responds = register(b"respondsToSelector:\0".as_ptr().cast());
        let low_selector = register(b"isLowPowerModeEnabled\0".as_ptr().cast());
        let thermal_selector = register(b"thermalState\0".as_ptr().cast());
        let low = (msg_bool_sel(process_info, responds, low_selector) != 0)
            .then(|| msg_bool(process_info, low_selector) != 0);
        let thermal = (msg_bool_sel(process_info, responds, thermal_selector) != 0)
            .then(|| msg_usize(process_info, thermal_selector));
        libc::dlclose(foundation);
        libc::dlclose(objc);
        Some((low, thermal))
    }
}

#[cfg(target_os = "macos")]
fn macos_frontmost_pid() -> Option<u32> {
    use libc::{c_char, c_void};
    use std::mem::transmute;
    type Id = *mut c_void;
    type Sel = *mut c_void;
    type GetClass = unsafe extern "C" fn(*const c_char) -> Id;
    type RegisterSel = unsafe extern "C" fn(*const c_char) -> Sel;
    type MsgId = unsafe extern "C" fn(Id, Sel) -> Id;
    type MsgI32 = unsafe extern "C" fn(Id, Sel) -> i32;
    unsafe {
        let (objc, foundation) = macos_objc_handles()?;
        let appkit = libc::dlopen(
            b"/System/Library/Frameworks/AppKit.framework/AppKit\0"
                .as_ptr()
                .cast(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if appkit.is_null() {
            libc::dlclose(foundation);
            libc::dlclose(objc);
            return None;
        }
        let get_class_raw = libc::dlsym(objc, b"objc_getClass\0".as_ptr().cast());
        let register_raw = libc::dlsym(objc, b"sel_registerName\0".as_ptr().cast());
        let raw_msg = libc::dlsym(objc, b"objc_msgSend\0".as_ptr().cast());
        if get_class_raw.is_null() || register_raw.is_null() || raw_msg.is_null() {
            libc::dlclose(appkit);
            libc::dlclose(foundation);
            libc::dlclose(objc);
            return None;
        }
        let get_class: GetClass = transmute(get_class_raw);
        let register: RegisterSel = transmute(register_raw);
        let msg_id: MsgId = transmute(raw_msg);
        let msg_i32: MsgI32 = transmute(raw_msg);
        let workspace_class = get_class(b"NSWorkspace\0".as_ptr().cast());
        if workspace_class.is_null() {
            libc::dlclose(appkit);
            libc::dlclose(foundation);
            libc::dlclose(objc);
            return None;
        }
        let workspace = msg_id(
            workspace_class,
            register(b"sharedWorkspace\0".as_ptr().cast()),
        );
        let app = if workspace.is_null() {
            std::ptr::null_mut()
        } else {
            msg_id(
                workspace,
                register(b"frontmostApplication\0".as_ptr().cast()),
            )
        };
        let pid = if app.is_null() {
            0
        } else {
            msg_i32(app, register(b"processIdentifier\0".as_ptr().cast()))
        };
        libc::dlclose(appkit);
        libc::dlclose(foundation);
        libc::dlclose(objc);
        u32::try_from(pid).ok().filter(|value| *value > 0)
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod pressure_tests {
    use super::*;

    #[test]
    fn parsea_pressure_stall_information() {
        let parsed = parse_pressure(
            "some avg10=1.25 avg60=0.70 avg300=0.11 total=100\nfull avg10=0.08 avg60=0.02 avg300=0.01 total=20\n",
        )
        .expect("PSI válido");
        assert!((parsed.some_avg10.expect("some") - 1.25).abs() < f32::EPSILON);
        assert!((parsed.full_avg10.expect("full") - 0.08).abs() < f32::EPSILON);
    }

    #[test]
    fn rechaza_pressure_sin_avg10_valido() {
        assert!(parse_pressure("some avg60=1.0 total=10\n").is_none());
    }

    #[test]
    fn detecta_pressure_parcial_sin_inventar_full() {
        let parsed = parse_pressure("some avg10=2.0 total=10\n").expect("some válido");
        assert_eq!(parsed.some_avg10, Some(2.0));
        assert_eq!(parsed.full_avg10, None);
    }
}
