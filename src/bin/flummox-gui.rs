//! The `flummox-gui` window.

fn main() -> anyhow::Result<()> {
    if std::env::args_os().any(|argument| argument == "--version") {
        println!("flummox {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if flummox::jobs::entrypoint()? {
        return Ok(());
    }
    flummox::gui::run()
}
