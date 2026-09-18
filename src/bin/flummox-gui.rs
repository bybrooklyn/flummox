//! The `flummox-gui` window.

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    if flummox::jobs::entrypoint()? {
        return Ok(());
    }
    flummox::gui::run()
}
