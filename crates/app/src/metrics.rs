use crate::runtime::RuntimeStatus;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub bind: SocketAddr,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: SocketAddr::from(([127, 0, 0, 1], 9898)),
        }
    }
}

impl MetricsConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.bind.ip().is_loopback(),
            "metrics.bind debe usar loopback; se rechazó {}",
            self.bind
        );
        anyhow::ensure!(
            self.bind.port() > 0,
            "metrics.bind requiere un puerto no cero"
        );
        Ok(())
    }
}

pub struct MetricsServer {
    shared: Arc<RwLock<RuntimeStatus>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MetricsServer {
    pub fn start(config: &MetricsConfig, initial: RuntimeStatus) -> Result<Option<Self>> {
        config.validate()?;
        if !config.enabled {
            return Ok(None);
        }
        let listener = TcpListener::bind(config.bind)
            .with_context(|| format!("no se pudo abrir OpenMetrics en {}", config.bind))?;
        listener.set_nonblocking(true)?;
        let shared = Arc::new(RwLock::new(initial));
        let worker_state = Arc::clone(&shared);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("sysopt-openmetrics".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, peer)) if peer.ip().is_loopback() => {
                            let _ = serve(stream, &worker_state);
                        }
                        Ok((_stream, _peer)) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(100))
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Some(Self {
            shared,
            stop,
            worker: Some(worker),
        }))
    }

    pub fn update(&self, status: &RuntimeStatus) {
        if let Ok(mut current) = self.shared.write() {
            *current = status.clone();
        }
    }
}

impl Drop for MetricsServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve(mut stream: TcpStream, shared: &Arc<RwLock<RuntimeStatus>>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut request = [0u8; 2048];
    let bytes = stream.read(&mut request)?;
    if !valid_metrics_request(&request[..bytes]) {
        stream.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        return Ok(());
    }
    let status = shared
        .read()
        .map_err(|_| anyhow::anyhow!("estado de métricas envenenado"))?;
    let body = render(&status);
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/openmetrics-text; version=1.0.0; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)?;
    Ok(())
}

fn valid_metrics_request(request: &[u8]) -> bool {
    let Some(first_line) = request.split(|byte| *byte == b'\n').next() else {
        return false;
    };
    let first_line = first_line.strip_suffix(b"\r").unwrap_or(first_line);
    matches!(
        first_line,
        b"GET /metrics HTTP/1.0" | b"GET /metrics HTTP/1.1"
    )
}

fn render(status: &RuntimeStatus) -> String {
    let mut body = format!(
        "# HELP sysopt_cycle_duration_milliseconds Duracion del ultimo ciclo.\n# TYPE sysopt_cycle_duration_milliseconds gauge\nsysopt_cycle_duration_milliseconds {}\n# TYPE sysopt_overhead_level gauge\nsysopt_overhead_level {}\n# TYPE sysopt_overhead_p95_milliseconds gauge\nsysopt_overhead_p95_milliseconds {}\n# TYPE sysopt_actions_planned gauge\nsysopt_actions_planned {}\n# TYPE sysopt_actions_applied gauge\nsysopt_actions_applied {}\n# TYPE sysopt_action_failures gauge\nsysopt_action_failures {}\n# TYPE sysopt_managed_changes gauge\nsysopt_managed_changes {}\n# TYPE sysopt_cpu_percent gauge\nsysopt_cpu_percent {:.3}\n# TYPE sysopt_memory_available_bytes gauge\nsysopt_memory_available_bytes {}\n# TYPE sysopt_safety_circuit_open gauge\nsysopt_safety_circuit_open {}\n# TYPE sysopt_power_telemetry_supported gauge\nsysopt_power_telemetry_supported {}\n# TYPE sysopt_battery_present gauge\nsysopt_battery_present {}\n# TYPE sysopt_battery_saver gauge\nsysopt_battery_saver {}\n# TYPE sysopt_thermal_state gauge\nsysopt_thermal_state {}\n",
        status.cycle_duration_ms,
        status.overhead_level,
        status.overhead_p95_ms,
        status.actions_planned,
        status.actions_applied,
        status.action_failures,
        status.managed_changes,
        status.global_cpu_percent,
        status.available_memory_bytes,
        u8::from(status.safety.circuit_open),
        u8::from(status.power_supported),
        u8::from(status.battery_percent.is_some()),
        u8::from(status.battery_saver),
        thermal_state_value(&status.thermal_state),
    );
    if let Some(percent) = status.battery_percent {
        body.push_str("# TYPE sysopt_battery_percent gauge\n");
        body.push_str(&format!("sysopt_battery_percent {}\n", percent));
    }
    if status.pressure_supported {
        body.push_str("# TYPE sysopt_psi_cpu_some_avg10 gauge\n");
        body.push_str(&format!(
            "sysopt_psi_cpu_some_avg10 {:.3}\n",
            status.cpu_some_pressure_avg10
        ));
        body.push_str("# TYPE sysopt_psi_memory_some_avg10 gauge\n");
        body.push_str(&format!(
            "sysopt_psi_memory_some_avg10 {:.3}\n",
            status.memory_some_pressure_avg10
        ));
        body.push_str("# TYPE sysopt_psi_io_some_avg10 gauge\n");
        body.push_str(&format!(
            "sysopt_psi_io_some_avg10 {:.3}\n",
            status.io_some_pressure_avg10
        ));
    }
    body.push_str("# EOF\n");
    body
}

fn thermal_state_value(state: &str) -> u8 {
    match state {
        "nominal" => 1,
        "fair" => 2,
        "serious" => 3,
        "critical" => 4,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rechaza_bind_publico() {
        let config = MetricsConfig {
            enabled: true,
            bind: "0.0.0.0:9898".parse().unwrap(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn renderiza_openmetrics() {
        let body = render(&RuntimeStatus::default());
        assert!(body.contains("sysopt_cycle_duration_milliseconds"));
        assert!(body.contains("sysopt_battery_present 0"));
        assert!(!body.contains("sysopt_battery_percent 0"));
        assert!(body.ends_with("# EOF\n"));
    }

    #[test]
    fn exige_ruta_http_exacta() {
        assert!(valid_metrics_request(
            b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n"
        ));
        assert!(!valid_metrics_request(b"GET /metrics?x=1 HTTP/1.1\r\n\r\n"));
        assert!(!valid_metrics_request(
            b"GET /metrics/extra HTTP/1.1\r\n\r\n"
        ));
        assert!(!valid_metrics_request(b"POST /metrics HTTP/1.1\r\n\r\n"));
    }
}
