//! The `flummox` command.

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    if flummox::jobs::entrypoint()? {
        return Ok(());
    }
    flummox::cli::run()
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    flummox::windows::run()
}

#[cfg(not(any(target_os = "linux", windows)))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("Flummox does not have a storage backend for this platform yet")
}
