#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    enforcement::run_linux_priority_helper()
}

#[cfg(not(target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("sysopt-priority-helper solo está disponible en Linux")
}
