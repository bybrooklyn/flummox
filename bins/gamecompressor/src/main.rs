//! `gamecompressor`: the command-line front end.
//!
//! Every command follows the same shape: find the games, probe the filesystem
//! they live on, then hand the work to a `gc_core` backend. The GUI and the
//! daemon will call the same functions.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use humansize::{DECIMAL, format_size};

use gc_core::backend::{self, Backend, CompressOpts, Event, EventSink, JobCtx, Preset};
use gc_core::busy::{self, ProcFs};
use gc_core::estimate::{self, EstimateOpts};
use gc_core::fsprobe::{self, FsInfo, Tier};
use gc_core::inventory::{self, Inventory};
use gc_core::model::Game;
use gc_launchers::{Env, Scan};

#[derive(Debug, Parser)]
#[command(
    name = "gamecompressor",
    version,
    about = "Compress installed games with zstd, keeping them playable"
)]
struct Cli {
    /// Log what is happening to stderr; repeat for more detail.
    ///
    /// `RUST_LOG` overrides this when set, for the usual per-module filters.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List the games found on this machine.
    Scan {
        /// Include runtimes and redistributables such as Proton.
        #[arg(long)]
        tools: bool,
    },
    /// Estimate how much space compressing a game would save.
    Estimate {
        /// Game to estimate: an appid, `steam:105600`, or part of the title.
        selector: String,
        #[command(flatten)]
        level: LevelArgs,
    },
    /// Compress a game in place.
    Compress {
        /// Game to compress.
        selector: String,
        #[command(flatten)]
        level: LevelArgs,
        /// Files to work on at once.
        #[arg(long, default_value_t = 2)]
        threads: usize,
        /// Compress even when the game looks busy.
        #[arg(long)]
        force: bool,
        /// Estimate and show what would happen, without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Return a game to uncompressed storage.
    Decompress {
        /// Game to decompress.
        selector: String,
        /// Decompress even when the game looks busy.
        #[arg(long)]
        force: bool,
    },
    /// Show how much of a game is currently stored compressed.
    Status {
        /// Game to inspect.
        selector: String,
    },
    /// List the drives holding games, and how each one can be compressed.
    Drives,
    /// Check this machine for anything that would stop the tool working.
    Doctor,
}

#[derive(Debug, Args)]
struct LevelArgs {
    /// Compression preset.
    #[arg(long, value_enum, default_value_t = PresetArg::Balanced)]
    preset: PresetArg,
    /// Explicit zstd level, overriding the preset (-15 to 15 on btrfs).
    #[arg(long)]
    level: Option<i32>,
}

impl LevelArgs {
    fn opts(&self, threads: usize) -> CompressOpts {
        CompressOpts { preset: self.preset.into(), level: self.level, threads }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PresetArg {
    /// Quick, smaller gain.
    Fast,
    /// The default.
    Balanced,
    /// Slowest, best ratio.
    Max,
}

impl From<PresetArg> for Preset {
    fn from(p: PresetArg) -> Self {
        match p {
            PresetArg::Fast => Self::Fast,
            PresetArg::Balanced => Self::Balanced,
            PresetArg::Max => Self::Max,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let cancel = install_signal_handler()?;
    let env = Env::current().context("HOME is not set")?;
    match cli.command {
        Command::Scan { tools } => cmd_scan(&env, tools),
        Command::Estimate { selector, level } => cmd_estimate(&env, &selector, &level),
        Command::Compress { selector, level, threads, force, dry_run } => {
            cmd_compress(&env, &selector, &level, threads, force, dry_run, &cancel)
        }
        Command::Decompress { selector, force } => cmd_decompress(&env, &selector, force, &cancel),
        Command::Status { selector } => cmd_status(&env, &selector),
        Command::Drives => cmd_drives(&env),
        Command::Doctor => cmd_doctor(&env),
    }
}

/// Sends `tracing` output to stderr, quiet unless asked.
///
/// Results go to stdout and stay parseable; this stream is for the story of
/// what the tool did, which matters most when a job on someone's game library
/// did something unexpected.
fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    // Failure here means a subscriber is already installed, which is not
    // worth refusing to run over.
    let _started = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Makes Ctrl-C stop a job cleanly instead of killing it mid-file.
///
/// The flag is polled between files, so the current file always finishes:
/// btrfs rewrites a file's extents atomically, and stopping between files
/// leaves the game in a state that is simply "partly compressed", which is
/// valid and can be resumed by running the command again. Killing the process
/// outright would be safe too, but the user would lose the summary of what
/// had already been done.
fn install_signal_handler() -> Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let _id = signal_hook::flag::register(signal, Arc::clone(&flag))
            .with_context(|| format!("installing the handler for signal {signal}"))?;
    }
    Ok(flag)
}

fn size(bytes: u64) -> String {
    format_size(bytes, DECIMAL)
}

fn scan(env: &Env) -> Scan {
    let scan = gc_launchers::scan_all(env);
    for warning in &scan.warnings {
        eprintln!("warning: {warning}");
    }
    scan
}

/// Finds the one game a selector names.
fn find_game(env: &Env, selector: &str) -> Result<Game> {
    let scan = scan(env);
    let matches: Vec<Game> = scan.games.into_iter().filter(|g| g.matches(selector)).collect();
    match matches.len() {
        0 => bail!("no game matches {selector:?}; try `gamecompressor scan`"),
        1 => matches.into_iter().next().context("no game"),
        _ => {
            let titles: Vec<String> = matches
                .iter()
                .map(|g| format!("{} ({})", g.title, g.id))
                .collect();
            bail!("{selector:?} matches several games:\n  {}", titles.join("\n  "))
        }
    }
}

/// Probes the filesystem and returns the backend for it.
fn backend_for(install_dir: &Path) -> Result<(FsInfo, Box<dyn Backend>)> {
    let fs = fsprobe::probe(install_dir)
        .with_context(|| format!("probing {}", install_dir.display()))?;
    let tier = fsprobe::tier_for(&fs);
    let kind = match &tier {
        Tier::Unsupported(why) => {
            bail!("{} is on {}, which is not supported: {why}", install_dir.display(), fs.fstype)
        }
        Tier::Pack => bail!(
            "{} is on {}, which needs the pack + FUSE tier. That is not built yet; \
             today only btrfs is supported.",
            install_dir.display(),
            fs.fstype
        ),
        Tier::Native(kind) => *kind,
    };
    let backend = backend::for_kind(kind)
        .with_context(|| format!("no backend for {}", kind.label()))?;
    Ok((fs, backend))
}

/// Refuses to touch a game that is running, updating or in use.
fn check_idle(game: &Game, force: bool) -> Result<()> {
    let process = busy::process_using(&game.install_dir, &ProcFs::new());
    if let Some(p) = &process
        && !force
    {
        bail!("{} is in use by {p}; close it first, or pass --force", game.title);
    }
    if !game.state.is_idle() && !force {
        bail!(
            "{} is {} (Steam's own state). Wait for it to finish, or pass --force.",
            game.title,
            game.state
        );
    }
    if !game.state.is_idle() || process.is_some() {
        eprintln!("warning: --force: working on {} while it is {}", game.title, game.state);
    }
    Ok(())
}

fn walk(game: &Game, backend: &dyn Backend) -> Result<Inventory> {
    let inv = inventory::walk(&game.install_dir, &backend.walk_opts())
        .with_context(|| format!("reading {}", game.install_dir.display()))?;
    for w in &inv.warnings {
        eprintln!("warning: {w}");
    }
    Ok(inv)
}

fn cmd_scan(env: &Env, tools: bool) -> Result<()> {
    let scan = scan(env);
    let games: Vec<&Game> = scan.games.iter().filter(|g| tools || !g.is_tool).collect();
    if games.is_empty() {
        println!("No games found. Is Steam installed for this user?");
        return Ok(());
    }
    let width = games.iter().map(|g| g.title.chars().count()).max().unwrap_or(10).min(48);
    for game in &games {
        let fs = fsprobe::probe(&game.install_dir).ok();
        let tier = fs.as_ref().map_or("?".to_owned(), |f| match fsprobe::tier_for(f) {
            Tier::Native(kind) => kind.label().to_owned(),
            Tier::Pack => format!("{} (pack, not built yet)", f.fstype),
            Tier::Unsupported(why) => format!("unsupported: {why}"),
        });
        println!(
            "{:<width$}  {:>10}  {:<22}  {:<12}  {}",
            game.title.chars().take(width).collect::<String>(),
            game.size_hint.map(size).unwrap_or_default(),
            game.state.to_string(),
            game.id.to_string(),
            tier,
            width = width
        );
    }
    println!("\n{} games", games.len());
    Ok(())
}

fn cmd_estimate(env: &Env, selector: &str, level: &LevelArgs) -> Result<()> {
    let game = find_game(env, selector)?;
    let opts = level.opts(1);
    let (fs, backend) = backend_for(&game.install_dir)?;
    let inv = walk(&game, backend.as_ref())?;
    println!(
        "{}: {} in {} files on {}",
        game.title,
        size(inv.total_bytes()),
        inv.files.len(),
        fs.fstype
    );
    eprintln!("sampling...");
    let model = backend.model(&opts);
    let est_opts = EstimateOpts::new(opts.btrfs_level(), &fs);
    let probe = backend.disk_probe();
    let est = estimate::estimate_game_with(
        &game.install_dir,
        &inv,
        model.as_ref(),
        &est_opts,
        probe.as_ref(),
    );
    print_estimate(&est, &est_opts);
    Ok(())
}

fn print_estimate(est: &estimate::Estimate, opts: &EstimateOpts) {
    println!("  install    : {}", size(est.install_bytes));
    println!(
        "  will rewrite: {} files (everything a compress pass would touch)",
        est.rewrite_files
    );
    println!("  of those, expected to shrink: {} files, {}", est.files, size(est.bytes));
    println!(
        "  skipped    : {} files (too small, already compressed, or not worth it)",
        est.skipped_files
    );
    println!("  on disk now: ~{} (those files)", size(est.disk_now));
    println!("  after      : ~{}", size(est.disk_after));
    let of_install = if est.install_bytes == 0 {
        0.0
    } else {
        est.saving() as f64 / est.install_bytes as f64 * 100.0
    };
    println!(
        "  saving     : ~{} at zstd level {} ({:.0}% of the files it touches, \
         {of_install:.0}% of the whole game)",
        size(est.saving()),
        opts.level,
        est.saving_ratio() * 100.0,
    );
    if let Some(mount) = opts.mount_level {
        println!(
            "\n  Note: this drive already compresses everything at zstd:{mount}, so the\n  \
             figures above are the *extra* saving on top of what you already have."
        );
    }
    println!("  Estimates are sampled, so the real result will differ a little.");
}

/// Prints job progress: a redrawn line on a terminal, occasional whole lines
/// otherwise.
///
/// The redraw uses `\r`, which a pipe or log file records as one enormous
/// line — a 1190-file job wrote 94 KB of it — so anything that is not a
/// terminal gets one line per tenth of the work instead.
struct Progress {
    total_files: std::sync::atomic::AtomicU64,
    total_bytes: std::sync::atomic::AtomicU64,
    /// Whether stderr is a terminal that can handle a redrawn line.
    tty: bool,
    /// The last tenth of the work already reported, when not on a terminal.
    last_tenth: std::sync::atomic::AtomicU64,
}

impl EventSink for Progress {
    fn event(&self, event: Event) {
        use std::sync::atomic::Ordering::Relaxed;
        match event {
            Event::Started { files, bytes } => {
                self.total_files.store(files, Relaxed);
                self.total_bytes.store(bytes, Relaxed);
                eprintln!("{files} files, {} to process", size(bytes));
            }
            Event::Progress { files_done, bytes_done, current } => {
                let total_files = self.total_files.load(Relaxed).max(1);
                let total_bytes = self.total_bytes.load(Relaxed).max(1);
                if self.tty {
                    // Keep the tail of a long path: the file name is the part
                    // worth seeing.
                    let tail: Vec<char> = current.chars().rev().take(48).collect();
                    let name: String = tail.into_iter().rev().collect();
                    eprint!(
                        "\r[{files_done}/{total_files}] {} / {}  {name:<48}",
                        size(bytes_done),
                        size(total_bytes)
                    );
                    // A dropped flush only means the line redraws late.
                    let _flushed = std::io::stderr().flush();
                    return;
                }
                let tenth = bytes_done.saturating_mul(10) / total_bytes;
                if tenth > self.last_tenth.swap(tenth, Relaxed) {
                    eprintln!(
                        "[{files_done}/{total_files}] {} / {}",
                        size(bytes_done),
                        size(total_bytes)
                    );
                }
            }
            Event::Warning(msg) => eprintln!("{}warning: {msg}", if self.tty { "\r" } else { "" }),
            Event::Finished(_) => {
                if self.tty {
                    eprintln!();
                }
            }
        }
    }
}

impl Progress {
    fn new() -> Self {
        use std::io::IsTerminal;
        Self {
            total_files: std::sync::atomic::AtomicU64::new(0),
            total_bytes: std::sync::atomic::AtomicU64::new(0),
            tty: std::io::stderr().is_terminal(),
            last_tenth: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_compress(
    env: &Env,
    selector: &str,
    level: &LevelArgs,
    threads: usize,
    force: bool,
    dry_run: bool,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let game = find_game(env, selector)?;
    let opts = level.opts(threads);
    let (fs, backend) = backend_for(&game.install_dir)?;
    check_idle(&game, force)?;
    if let Some(why) = fsprobe::snapshot_risk(&fs) {
        eprintln!(
            "warning: {why}.\n         Compressing rewrites every extent, which unshares it \
             from existing snapshots,\n         so the drive can end up fuller until those \
             snapshots expire."
        );
    }
    let inv = walk(&game, backend.as_ref())?;

    if dry_run {
        let model = backend.model(&opts);
        let est_opts = EstimateOpts::new(opts.btrfs_level(), &fs);
        let probe = backend.disk_probe();
        let est = estimate::estimate_game_with(
            &game.install_dir,
            &inv,
            model.as_ref(),
            &est_opts,
            probe.as_ref(),
        );
        println!("{} (dry run, nothing written)", game.title);
        print_estimate(&est, &est_opts);
        return Ok(());
    }

    println!(
        "Compressing {} with {} at zstd level {}",
        game.title,
        backend.kind().label(),
        opts.btrfs_level()
    );
    let progress = Progress::new();
    let ctx = JobCtx { events: &progress, cancel: cancel.as_ref() };
    tracing::info!(game = %game.title, level = opts.btrfs_level(), files = inv.files.len(), "compressing");
    let outcome = backend
        .compress(&game.install_dir, &inv, &opts, &ctx)
        .with_context(|| format!("compressing {}", game.title))?;

    if outcome.cancelled {
        println!(
            "Stopped early. What was already compressed stays compressed; \
             run the same command again to finish the rest."
        );
    }
    println!(
        "Done: {} files, {} processed, {} skipped",
        outcome.files,
        size(outcome.bytes),
        outcome.skipped
    );
    let freed = outcome.freed();
    if freed > 0 {
        println!("Freed about {} (measured from free space, so approximate).", size(freed as u64));
    } else {
        println!(
            "Free space did not go up. On a drive already mounted with compression, \
             most of the gain was already there."
        );
    }
    if !outcome.errors.is_empty() {
        println!("{} files failed; the first few:", outcome.errors.len());
        for e in outcome.errors.iter().take(5) {
            println!("  {e}");
        }
    }
    Ok(())
}

fn cmd_decompress(
    env: &Env,
    selector: &str,
    force: bool,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let game = find_game(env, selector)?;
    let (_fs, backend) = backend_for(&game.install_dir)?;
    check_idle(&game, force)?;
    let inv = walk(&game, backend.as_ref())?;
    println!("Decompressing {}", game.title);
    let progress = Progress::new();
    let ctx = JobCtx { events: &progress, cancel: cancel.as_ref() };
    let outcome = backend
        .decompress(&game.install_dir, &inv, &ctx)
        .with_context(|| format!("decompressing {}", game.title))?;
    println!("Done: {} files rewritten", outcome.files);
    let freed = outcome.freed();
    if freed < 0 {
        println!("Uses about {} more space now.", size(freed.unsigned_abs()));
    }
    Ok(())
}

fn cmd_status(env: &Env, selector: &str) -> Result<()> {
    let game = find_game(env, selector)?;
    let (fs, backend) = backend_for(&game.install_dir)?;
    let inv = walk(&game, backend.as_ref())?;
    let status = backend.status(&game.install_dir, &inv)?;
    println!("{} on {} ({})", game.title, fs.fstype, backend.kind().label());
    println!("  files      : {}", status.files);
    println!("  mapped     : {}", size(status.total_bytes));
    println!(
        "  compressed : {} ({:.0}% of mapped data is in compressed extents)",
        size(status.compressed_bytes),
        status.ratio() * 100.0
    );
    if let Some((algo, level)) = fs.mount_compression() {
        println!(
            "  drive default: {algo}{}",
            level.map(|l| format!(":{l}")).unwrap_or_default()
        );
    }
    println!(
        "\n  Exact compressed sizes need root, so this shows how much data is stored\n  \
         compressed rather than how many bytes it takes up."
    );
    Ok(())
}

fn cmd_drives(env: &Env) -> Result<()> {
    let scan = scan(env);
    let mut seen: Vec<(PathBuf, FsInfo, u64, usize)> = Vec::new();
    for game in &scan.games {
        let Ok(fs) = fsprobe::probe(&game.install_dir) else { continue };
        match seen.iter_mut().find(|(mp, _, _, _)| *mp == fs.mountpoint) {
            Some(entry) => {
                entry.2 = entry.2.saturating_add(game.size_hint.unwrap_or(0));
                entry.3 += 1;
            }
            None => seen.push((fs.mountpoint.clone(), fs, game.size_hint.unwrap_or(0), 1)),
        }
    }
    if seen.is_empty() {
        println!("No game drives found.");
        return Ok(());
    }
    for (mountpoint, fs, bytes, games) in seen {
        let free = backend::free_bytes(&mountpoint).unwrap_or(0);
        println!("{} ({})", mountpoint.display(), fs.fstype);
        println!("  games      : {games}, {}", size(bytes));
        println!("  free       : {}", size(free));
        match fsprobe::tier_for(&fs) {
            Tier::Native(kind) => println!("  support    : {} compression, in place", kind.label()),
            Tier::Pack => println!("  support    : needs the pack + FUSE tier (not built yet)"),
            Tier::Unsupported(why) => println!("  support    : none ({why})"),
        }
        if let Some((algo, level)) = fs.mount_compression() {
            println!(
                "  mounted    : compress={algo}{}",
                level.map(|l| format!(":{l}")).unwrap_or_default()
            );
        }
    }
    Ok(())
}

fn cmd_doctor(env: &Env) -> Result<()> {
    println!("gamecompressor doctor\n");

    let roots = gc_launchers::steam::roots(env);
    if roots.is_empty() {
        println!("[!] No Steam installation found under {}", env.home.display());
    } else {
        for root in &roots {
            println!("[ok] Steam root: {}", root.display());
            match gc_launchers::steam::libraries(root) {
                Ok(libs) => {
                    for lib in libs {
                        println!("     library: {}", lib.display());
                    }
                }
                Err(e) => println!("[!]  cannot read its libraries: {e}"),
            }
        }
    }

    match std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        Ok(release) => {
            let release = release.trim();
            let (major, minor) = parse_kernel_version(release);
            println!("[ok] kernel {release}");
            if (major, minor) < (6, 15) {
                println!(
                    "[!]  btrfs compression levels need kernel 6.15+; on this kernel the \
                     drive's default level is used instead"
                );
            }
        }
        Err(e) => println!("[!] cannot read the kernel version: {e}"),
    }

    let fuse = Path::new("/dev/fuse").exists();
    println!(
        "{} /dev/fuse {}",
        if fuse { "[ok]" } else { "[!] " },
        if fuse { "present (needed later for ext4/xfs drives)" } else { "missing" }
    );

    let games = scan(env).games;
    let idle = games.iter().filter(|g| g.state.is_idle() && !g.is_tool).count();
    println!("[ok] {} games found, {idle} ready to compress", games.len());

    let mut checked: Vec<PathBuf> = Vec::new();
    for game in &games {
        let Ok(fs) = fsprobe::probe(&game.install_dir) else { continue };
        if checked.contains(&fs.mountpoint) {
            continue;
        }
        checked.push(fs.mountpoint.clone());
        match fsprobe::snapshot_risk(&fs) {
            Some(why) => println!(
                "[!]  {}: {why}; compressing unshares extents from snapshots, \
                 so space may not drop until they expire",
                fs.mountpoint.display()
            ),
            None => println!("[ok] {}: no snapshots in the way", fs.mountpoint.display()),
        }
    }
    Ok(())
}

/// Parses `major.minor` out of a kernel release string such as `7.2.3-1-x`.
fn parse_kernel_version(release: &str) -> (u32, u32) {
    let mut parts = release.split(['.', '-']);
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor)
}

#[cfg(test)]
mod tests {
    use gc_testutil::{TestResult, check_eq};

    use super::*;

    #[test]
    fn parses_kernel_versions() -> TestResult {
        check_eq(parse_kernel_version("7.2.3-1-cachyos"), (7, 2), "a distro kernel release")?;
        check_eq(parse_kernel_version("6.15.0"), (6, 15), "a plain version")?;
        check_eq(parse_kernel_version("nonsense"), (0, 0), "a release with no numbers")
    }

    #[test]
    fn cli_parses_every_command() -> TestResult {
        use clap::CommandFactory;
        // clap's own consistency check over the derived command tree.
        Cli::command().debug_assert();
        Ok(())
    }
}
