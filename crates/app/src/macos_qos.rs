use anyhow::Result;
#[cfg(target_os = "macos")]
use std::ffi::c_void;

#[cfg(target_os = "macos")]
const QOS_CLASS_UTILITY: u32 = 0x11;
#[cfg(target_os = "macos")]
const IOPOL_TYPE_DISK: i32 = 0;
#[cfg(target_os = "macos")]
const IOPOL_SCOPE_THREAD: i32 = 1;
// Política pública documentada por setiopolicy_np(3) para trabajo de fondo.
// Se usa la política pública IOPOL_THROTTLE; no se depende de constantes privadas o ausentes en SDK públicos.
#[cfg(target_os = "macos")]
const IOPOL_THROTTLE: i32 = 2;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_self() -> *mut c_void;
    // qos_class_t pthread_get_qos_class_np(pthread_t, int *);
    fn pthread_get_qos_class_np(thread: *mut c_void, relative_priority: *mut i32) -> u32;
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    fn getiopolicy_np(policy_type: i32, scope: i32) -> i32;
    fn setiopolicy_np(policy_type: i32, scope: i32, policy: i32) -> i32;
}

#[cfg(target_os = "macos")]
fn current_qos() -> (u32, i32) {
    let mut relative_priority = 0i32;
    let qos_class = unsafe { pthread_get_qos_class_np(pthread_self(), &mut relative_priority) };
    (qos_class, relative_priority)
}

#[cfg(target_os = "macos")]
fn current_io_policy() -> Result<i32> {
    let policy = unsafe { getiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD) };
    anyhow::ensure!(
        policy >= 0,
        "getiopolicy_np falló: {}",
        std::io::Error::last_os_error()
    );
    Ok(policy)
}

/// Reduce la prioridad energética del trabajo interno de SysOpt. En macOS la
/// guarda restaura QoS e I/O policy al salir; en otras plataformas es un no-op.
pub struct WorkerQosGuard {
    #[cfg(target_os = "macos")]
    original_qos: u32,
    #[cfg(target_os = "macos")]
    original_relative_priority: i32,
    #[cfg(target_os = "macos")]
    original_io_policy: i32,
}

impl WorkerQosGuard {
    pub fn utility_io() -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let (original_qos, original_relative_priority) = current_qos();
            let original_io_policy = current_io_policy()?;

            let qos_set = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0) };
            anyhow::ensure!(
                qos_set == 0,
                "pthread_set_qos_class_self_np falló: {qos_set}"
            );
            let io_set =
                unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, IOPOL_THROTTLE) };
            if io_set != 0 {
                unsafe {
                    let _ = pthread_set_qos_class_self_np(original_qos, original_relative_priority);
                }
                anyhow::bail!("setiopolicy_np falló: {}", std::io::Error::last_os_error());
            }
            Ok(Self {
                original_qos,
                original_relative_priority,
                original_io_policy,
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            Ok(Self {})
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for WorkerQosGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, self.original_io_policy);
            let _ =
                pthread_set_qos_class_self_np(self.original_qos, self.original_relative_priority);
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requiere macOS nativo; CI de release lo ejecuta explícitamente"]
    fn macos_worker_qos_roundtrip() {
        let original_qos = current_qos();
        let original_io = current_io_policy().expect("se puede leer la política de E/S");
        {
            let _guard = WorkerQosGuard::utility_io().expect("se puede aplicar QoS utility");
            assert_eq!(current_qos().0, QOS_CLASS_UTILITY);
            assert_eq!(
                current_io_policy().expect("se puede verificar la política de E/S"),
                IOPOL_THROTTLE
            );
        }
        assert_eq!(current_qos(), original_qos);
        assert_eq!(
            current_io_policy().expect("se puede verificar la restauración de E/S"),
            original_io
        );
    }
}
