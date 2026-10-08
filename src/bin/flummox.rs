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
    if flummox::windows::coordinator::entrypoint()? {
        return Ok(());
    }
    flummox::windows::run()
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    flummox::macos::run()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn main() -> anyhow::Result<()> {
    let _matches = clap::Command::new("flummox")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Flummox desktop shell; storage backend unavailable on this platform")
        .get_matches();
    anyhow::bail!("Flummox does not have a storage backend for this platform yet")
}
