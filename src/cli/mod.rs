//! The command line front end.
//!
//! Every command has the same shape: find the games, probe the filesystem they
//! live on, then hand the work to a backend. The GUI runs these same commands
//! as child processes, so anything it can do is available here too.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use humansize::{DECIMAL, format_size};

use crate::backend::{self, Backend, CompressOpts, Event, EventSink, JobCtx, Preset};
use crate::busy::{self, ProcFs};
use crate::db::{Activity, ActivityLevel, Db, GameRecord};
use crate::estimate::DiskProbe;
use crate::sandbox::{self, SandboxPlan};
use crate::estimate::{self, EstimateOpts};
use crate::fsprobe::{self, FsInfo, Tier};
use crate::inventory::{self, Inventory};
use crate::model::Game;
use crate::launchers::{Env, Scan};

#[derive(Debug, Parser)]
#[command(
    name = "flummox",
    version,
    about = "Compress installed games with zstd, keeping them playable"
)]
struct Cli {
    /// Log what is happening to stderr; repeat for more detail.
    ///
    /// `RUST_LOG` overrides this when set, for the usual per-module filters.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Print results as JSON, for scripts and the GUI.
    #[arg(long, global = true)]
    json: bool,
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
        /// Stop if the game is launched, instead of waiting for it to close.
        #[arg(long)]
        no_pause: bool,
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
    /// Show what this tool has done recently.
    Log {
        /// How many entries to show.
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Compress new Steam downloads as they are written.
    ///
    /// Sets a property on each Steam library so future downloads and patches
    /// land compressed, with no second pass over them afterwards.
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
    /// Hide something that is not a game, or that you never want touched.
    Exclude {
        #[command(subcommand)]
        action: ExcludeAction,
    },
    /// List the drives holding games, and how each one can be compressed.
    Drives,
    /// Check this machine for anything that would stop the tool working.
    Doctor,
}

#[derive(Debug, Subcommand)]
enum ExcludeAction {
    /// Hide a game, by app ID or part of its title.
    Add {
        /// What to hide.
        selector: String,
    },
    /// Show a hidden game again.
    Remove {
        /// What to stop hiding.
        selector: String,
    },
    /// What is currently hidden.
    List,
}

#[derive(Debug, Subcommand)]
enum HookAction {
    /// Turn it on for every Steam library found.
    On,
    /// Turn it off. Games already compressed stay compressed.
    Off,
    /// Show which libraries have it.
    Status,
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

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let cancel = install_signal_handler()?;
    let env = Env::current().context("HOME is not set")?;
    let out = Output { json: cli.json };
    match cli.command {
        Command::Scan { tools } => cmd_scan(&env, out, tools),
        Command::Estimate { selector, level } => {
            cmd_estimate(&env, out, &selector, &level, &cancel)
        }
        Command::Compress { selector, level, threads, force, no_pause, dry_run } => {
            cmd_compress(&env, &selector, &level, threads, force, no_pause, dry_run, &cancel)
        }
        Command::Decompress { selector, force } => cmd_decompress(&env, &selector, force, &cancel),
        Command::Status { selector } => cmd_status(&env, out, &selector, &cancel),
        Command::Log { limit } => cmd_log(out, limit),
        Command::Hook { action } => cmd_hook(&env, out, action),
        Command::Exclude { action } => cmd_exclude(&env, out, action),
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

/// Where a command's results go.
///
/// Text is for a person reading a terminal; JSON is for a script, and for the
/// GUI, which drives these same commands instead of keeping a second copy of
/// this logic. Progress and warnings always go to stderr, so JSON on stdout
/// stays parseable even mid-job.
#[derive(Debug, Clone, Copy)]
struct Output {
    json: bool,
}

impl Output {
    /// Prints `value` as JSON, or runs `text` to print it for a human.
    fn emit<T: serde::Serialize>(self, value: &T, text: impl FnOnce()) -> Result<()> {
        if !self.json {
            text();
            return Ok(());
        }
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, value).context("writing JSON")?;
        writeln!(stdout).context("writing JSON")?;
        Ok(())
    }
}

/// One game in `scan --json`.
#[derive(serde::Serialize)]
struct ScanRow {
    title: String,
    id: String,
    launcher: &'static str,
    path: PathBuf,
    size: Option<u64>,
    state: String,
    idle: bool,
    is_tool: bool,
    filesystem: String,
    backend: Option<&'static str>,
    supported: bool,
    note: Option<String>,
}

/// Pauses a job while the game is being played.
///
/// Scans this user's processes for anything with a file open inside the
/// install directory, which is the same check that decides whether a job may
/// start at all.
struct GameInUse {
    install_dir: PathBuf,
}

impl backend::BusyCheck for GameInUse {
    fn in_use_by(&self) -> Option<String> {
        busy::process_using(&self.install_dir, &ProcFs::new())
    }
}

/// Roughly how long ago something happened, for the activity log.
///
/// The log answers "what has this been doing lately", and a Unix timestamp
/// answers that badly.
fn ago(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let seconds = now.saturating_sub(ts).max(0);
    match seconds {
        s if s < 60 => "just now".to_owned(),
        s if s < 3_600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3_600),
        s => format!("{} days ago", s / 86_400),
    }
}

/// Opens the state database, explaining rather than failing if it cannot.
///
/// Nothing here needs the database to do its job: without it a pass simply is
/// not recorded, which costs the user an accurate estimate next time but not
/// the compression itself.
fn open_db() -> Option<Db> {
    let path = Db::default_path()?;
    match Db::open(&path) {
        Ok(db) => Some(db),
        Err(e) => {
            eprintln!(
                "warning: cannot use the state database at {} ({e}).\n         \
                 This pass will not be recorded, so the next estimate will not know \
                 what was already done.",
                path.display()
            );
            None
        }
    }
}

/// An estimator probe that also knows what earlier passes did.
///
/// The backend measures what is compressed on disk today; the database
/// remembers the level each file was last processed at. The second half is
/// what stops an estimate from promising space that a previous pass already
/// proved is not there.
struct RecordedProbe<'a> {
    measured: &'a dyn DiskProbe,
    levels: HashMap<PathBuf, i32>,
}

impl DiskProbe for RecordedProbe<'_> {
    fn measure(&self, path: &Path) -> Option<(u64, u64)> {
        self.measured.measure(path)
    }

    fn attempted_level(&self, path: &Path) -> Option<i32> {
        self.levels.get(path).copied()
    }
}

/// The level each of a game's files was last compressed at, by absolute path.
///
/// The database stores paths relative to the install directory, while the
/// estimator asks about absolute ones, so they are joined here.
fn recorded_levels(db: Option<&Db>, game: &Game) -> HashMap<PathBuf, i32> {
    let Some(db) = db else { return HashMap::new() };
    match db.fingerprints(&game.id) {
        Ok(prints) => prints
            .into_iter()
            .map(|(rel, fp)| (game.install_dir.join(rel), fp.level_applied))
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not read what earlier passes did");
            HashMap::new()
        }
    }
}

fn scan(env: &Env) -> Scan {
    let scan = crate::launchers::scan_all(env);
    tracing::info!(games = scan.games.len(), warnings = scan.warnings.len(), "scanned launchers");
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
        0 => bail!("no game matches {selector:?}; try `flummox scan`"),
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
/// Refuses to start when something is using the game.
///
/// Call this again after the walk. Walking a 64 GB install takes seconds, and
/// a game launched in that window would otherwise have its files rewritten
/// underneath it.
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

fn walk(game: &Game, backend: &dyn Backend, cancel: Option<&AtomicBool>) -> Result<Inventory> {
    let inv = inventory::walk_cancellable(&game.install_dir, &backend.walk_opts(), cancel)
        .with_context(|| format!("reading {}", game.install_dir.display()))?;
    for w in &inv.warnings {
        eprintln!("warning: {w}");
    }
    Ok(inv)
}

fn cmd_scan(env: &Env, out: Output, tools: bool) -> Result<()> {
    let scan = scan(env);
    let hidden = excluded_ids(open_db().as_ref());
    let games: Vec<&Game> = scan
        .games
        .iter()
        .filter(|g| tools || !g.is_tool)
        .filter(|g| !g.ids().any(|id| hidden.contains(id)))
        .collect();
    if games.is_empty() && !out.json {
        println!("No games found. Is Steam installed for this user?");
        return Ok(());
    }
    let rows: Vec<ScanRow> = games
        .iter()
        .map(|game| {
            let fs = fsprobe::probe(&game.install_dir).ok();
            let tier = fs.as_ref().map(fsprobe::tier_for);
            let (backend, supported, note) = match &tier {
                Some(Tier::Native(kind)) => (Some(kind.label()), true, None),
                Some(Tier::Pack) => (None, false, Some("needs the pack tier, not built yet".to_owned())),
                Some(Tier::Unsupported(why)) => (None, false, Some((*why).to_owned())),
                None => (None, false, Some("could not probe the filesystem".to_owned())),
            };
            ScanRow {
                title: game.title.clone(),
                id: game.id.to_string(),
                launcher: game.id.launcher.slug(),
                path: game.install_dir.clone(),
                size: game.size_hint,
                state: game.state.to_string(),
                idle: game.state.is_idle(),
                is_tool: game.is_tool,
                filesystem: fs.map_or_else(|| "?".to_owned(), |f| f.fstype),
                backend,
                supported,
                note,
            }
        })
        .collect();

    out.emit(&rows, || {
        let width = rows.iter().map(|r| r.title.chars().count()).max().unwrap_or(10).min(48);
        for row in &rows {
            let support = match (&row.backend, &row.note) {
                (Some(kind), _) => (*kind).to_owned(),
                (None, Some(why)) => why.clone(),
                (None, None) => "?".to_owned(),
            };
            println!(
                "{:<width$}  {:>10}  {:<22}  {:<12}  {}",
                row.title.chars().take(width).collect::<String>(),
                row.size.map(size).unwrap_or_default(),
                row.state,
                row.id,
                support,
                width = width
            );
        }
        println!("\n{} games", rows.len());
    })
}

fn cmd_estimate(
    env: &Env,
    out: Output,
    selector: &str,
    level: &LevelArgs,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let game = find_game(env, selector)?;
    let opts = level.opts(1);
    let (fs, backend) = backend_for(&game.install_dir)?;
    let inv = walk(&game, backend.as_ref(), Some(cancel.as_ref()))?;
    if !out.json {
        println!(
            "{}: {} in {} files on {}",
            game.title,
            size(inv.total_bytes()),
            inv.files.len(),
            fs.fstype
        );
    }
    eprintln!("sampling...");
    tracing::info!(
        game = %game.title,
        files = inv.files.len(),
        candidates = inv.to_compress().count(),
        level = opts.btrfs_level(),
        "estimating"
    );
    let model = backend.model(&opts);
    let est_opts = EstimateOpts::new(opts.btrfs_level(), &fs);
    let measured = backend.disk_probe();
    let probe = RecordedProbe {
        measured: measured.as_ref(),
        levels: recorded_levels(open_db().as_ref(), &game),
    };
    let est = estimate::estimate_game_cancellable(
        &game.install_dir,
        &inv,
        model.as_ref(),
        &est_opts,
        &probe,
        Some(cancel.as_ref()),
    );
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("Stopped early, so this estimate covers only part of the game.");
    }

    #[derive(serde::Serialize)]
    struct EstimateOut<'a> {
        game: &'a str,
        id: String,
        level: i32,
        mount_level: Option<i32>,
        #[serde(flatten)]
        estimate: estimate::Estimate,
    }
    let payload = EstimateOut {
        game: &game.title,
        id: game.id.to_string(),
        level: est_opts.level,
        mount_level: est_opts.mount_level,
        estimate: est,
    };
    out.emit(&payload, || print_estimate(&est, &est_opts))
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
/// line. A 1190-file job wrote 94 KB of it, so anything that is not a
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
                    // Keep the tail of a long path, which is where the file
                    // name is.
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
            Event::Paused { by } => {
                eprintln!("{}paused: {by} is using the game, waiting", if self.tty { "\r" } else { "" });
            }
            Event::Resumed => {
                eprintln!("{}resumed", if self.tty { "\r" } else { "" });
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
    no_pause: bool,
    dry_run: bool,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let game = find_game(env, selector)?;
    let opts = level.opts(threads);
    let (fs, backend) = backend_for(&game.install_dir)?;
    check_idle(&game, force)?;
    // Open the database before the sandbox goes up: creating its directory is
    // simpler to do now than to grant a sandboxed process.
    let mut db = open_db();
    // Naming a hidden game directly should not get around hiding it.
    if let Some(open) = db.as_ref()
        && game.ids().any(|id| open.is_excluded(id).unwrap_or(false))
    {
        bail!(
            "{} is on the exclusion list. Run `flummox exclude remove {}` first.",
            game.title,
            game.id
        );
    }
    if let Some(why) = fsprobe::snapshot_risk(&fs) {
        eprintln!(
            "warning: {why}.\n         Compressing rewrites every extent, which unshares it \
             from existing snapshots,\n         so the drive can end up fuller until those \
             snapshots expire."
        );
    }
    let full_inv = walk(&game, backend.as_ref(), Some(cancel.as_ref()))?;
    // The walk took time. Anything could have started in it.
    check_idle(&game, force)?;

    // After a game update most of an install is byte-identical to what was
    // compressed last time. Where an earlier pass already ran at this level or
    // higher, only the files that actually changed need rewriting,
    // otherwise a small patch costs a rewrite of the whole install.
    let previous = db.as_ref().and_then(|db| db.game(&game.id).ok().flatten());
    let reuse = previous.as_ref().is_some_and(|prev| prev.level >= opts.btrfs_level());
    let mut unchanged = 0usize;
    let inv = match (reuse, db.as_ref()) {
        (true, Some(open)) => match open.changed_since(&game.id, &full_inv) {
            Ok(changed) => {
                unchanged = full_inv.files.len().saturating_sub(changed.len());
                Inventory { files: changed, warnings: Vec::new() }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not tell which files changed; doing all of them");
                full_inv.clone()
            }
        },
        _ => full_inv.clone(),
    };
    if unchanged > 0 {
        println!(
            "{unchanged} files are unchanged since the last pass at level {}; skipping them.",
            previous.as_ref().map_or(0, |p| p.level)
        );
    }
    // Running a job over nothing would still print a free-space delta, and on
    // a busy filesystem that delta is somebody else's writes.
    if !dry_run && inv.to_compress().next().is_none() {
        println!("Nothing to do: {} is already compressed and nothing has changed.", game.title);
        return Ok(());
    }

    if dry_run {
        let model = backend.model(&opts);
        let est_opts = EstimateOpts::new(opts.btrfs_level(), &fs);
        let measured = backend.disk_probe();
        let probe = RecordedProbe {
            measured: measured.as_ref(),
            levels: recorded_levels(db.as_ref(), &game),
        };
        let est = estimate::estimate_game_cancellable(
            &game.install_dir,
            &full_inv,
            model.as_ref(),
            &est_opts,
            &probe,
            Some(cancel.as_ref()),
        );
        println!("{} (dry run, nothing written)", game.title);
        print_estimate(&est, &est_opts);
        return Ok(());
    }

    // Sampled before the pass runs. Compressing changes what the probe sees,
    // so an estimate taken afterwards reports a saving that has already been
    // taken, which is why this cannot be deferred to the end.
    let pass_saving = {
        let model = backend.model(&opts);
        let est_opts = EstimateOpts::new(opts.btrfs_level(), &fs);
        let measured = backend.disk_probe();
        let probe = RecordedProbe {
            measured: measured.as_ref(),
            levels: recorded_levels(db.as_ref(), &game),
        };
        estimate::estimate_game_cancellable(
            &game.install_dir,
            &inv,
            model.as_ref(),
            &est_opts,
            &probe,
            Some(cancel.as_ref()),
        )
        .saving()
    };
    println!("Estimated saving: {}", size(pass_saving));

    println!(
        "Compressing {} with {} at zstd level {}",
        game.title,
        backend.kind().label(),
        opts.btrfs_level()
    );
    // Drop this process's access to everything except the game itself, before
    // any worker thread exists. Discovery is already finished, so nothing
    // further needs to read Steam's configuration.
    let plan = SandboxPlan::for_job(&game.install_dir, Db::default_path().as_deref().and_then(Path::parent));
    let sandboxed = sandbox::restrict(&plan);
    tracing::info!(status = %sandboxed.describe(), "sandbox");
    if !sandboxed.is_active() {
        tracing::warn!("{}", sandboxed.describe());
    }

    let progress = Progress::new();
    // Pausing needs somewhere to look, so it is built here and borrowed
    // for the length of the job.
    let in_use = GameInUse { install_dir: game.install_dir.clone() };
    let ctx = JobCtx {
        events: &progress,
        cancel: cancel.as_ref(),
        busy: (!no_pause).then_some(&in_use as &dyn backend::BusyCheck),
    };
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
    // This figure is the filesystem's free space before and after, so anything
    // else writing to the same drive lands in it too. It is worth showing, but
    // not worth dressing up as a measurement of this job alone.
    let freed = outcome.freed();
    match (outcome.files, freed) {
        (0, _) => println!("No files were rewritten."),
        (_, None) => println!("Could not read free space, so there is no figure to report."),
        (_, Some(n)) if n > 0 => {
            println!("Freed about {} (from free space, so approximate).", size(n.unsigned_abs()));
        }
        _ => println!(
            "Free space did not go up. On a drive already mounted with compression, \
             most of the gain was already there."
        ),
    }
    if !outcome.errors.is_empty() {
        println!("{} files failed; the first few:", outcome.errors.len());
        for e in outcome.errors.iter().take(5) {
            println!("  {e}");
        }
    }

    if let Some(open) = db.as_mut() {
        let mut record = GameRecord::new(
            game.id.clone(),
            game.title.clone(),
            game.install_dir.clone(),
            backend.kind(),
            &opts,
        );
        record.build = game.build.clone();
        record.install_bytes = full_inv.total_bytes();
        record.disk_before = outcome.free_before.unwrap_or_default();
        record.disk_after = outcome.free_after.unwrap_or_default();
        // Added to what earlier passes predicted. A later pass over the same
        // game only has whatever is left to take, so replacing the figure
        // would make a game's saving fall every time it is recompressed.
        record.est_saving = previous
            .as_ref()
            .map_or(0, |p| p.est_saving)
            .saturating_add(i64::try_from(pass_saving).unwrap_or(i64::MAX));
        // When files were skipped as unchanged, they are still compressed at
        // whatever the earlier, higher level was; recording the lower level of
        // this pass would make a later run redo them for nothing.
        // The level the kernel applied, which is lower than the one asked for
        // on a kernel too old to accept a level at all.
        record.level = outcome.effective_level.unwrap_or(record.level);
        record.level = previous.as_ref().map_or(record.level, |p| p.level.max(record.level));
        if outcome.effective_level.is_some_and(|applied| applied < opts.btrfs_level()) {
            println!(
                "Note: this kernel does not accept a compression level, so the files were \
                 compressed at the filesystem default rather than {}.",
                opts.btrfs_level()
            );
        }
        // Fingerprints are stored for the whole install, not just the files
        // this pass touched, or the skipped ones would look new next time.
        if let Err(e) = open.record_compression(&record, &full_inv) {
            eprintln!("warning: could not record this pass: {e}");
        }
        let entry = Activity::new(
            ActivityLevel::Info,
            "compress",
            format!(
                "{} at zstd {} ({} files, {})",
                game.title,
                record.level,
                outcome.files,
                size(outcome.bytes)
            ),
        )
        .for_game(&game.id)
        // Zero when free space could not be read, so the log records "no
        // figure" instead of an invented one.
        .with_bytes(-freed.unwrap_or_default());
        if let Err(e) = open.log_activity(&entry) {
            tracing::warn!(error = %e, "could not write to the activity log");
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
    let mut db = open_db();
    let inv = walk(&game, backend.as_ref(), Some(cancel.as_ref()))?;
    println!("Decompressing {}", game.title);
    // Same restriction as a compress job: by this point every path the work
    // needs is known, so the process has no business reaching anything else.
    let plan =
        SandboxPlan::for_job(&game.install_dir, Db::default_path().as_deref().and_then(Path::parent));
    tracing::info!(status = %sandbox::restrict(&plan).describe(), "sandbox");
    let progress = Progress::new();
    // No pause on the way back out. Decompress is what someone runs to
    // undo, and it should not sit waiting on a game.
    let ctx = JobCtx { events: &progress, cancel: cancel.as_ref(), busy: None };
    let outcome = backend
        .decompress(&game.install_dir, &inv, &ctx)
        .with_context(|| format!("decompressing {}", game.title))?;
    println!("Done: {} files rewritten", outcome.files);
    let freed = outcome.freed();
    if let Some(n) = freed
        && n < 0
    {
        println!("Uses about {} more space now.", size(n.unsigned_abs()));
    }

    if let Some(open) = db.as_mut() {
        // Forget first: the stored fingerprints describe a compressed install
        // that no longer exists, and `forget` also clears this game's log
        // entries, so the entry below has to come after it.
        if let Err(e) = open.forget(&game.id) {
            eprintln!("warning: could not clear the record for {}: {e}", game.title);
        }
        let entry = Activity::new(
            ActivityLevel::Info,
            "decompress",
            format!("{} back to uncompressed ({} files)", game.title, outcome.files),
        )
        .for_game(&game.id)
        // Zero when free space could not be read, so the log records "no
        // figure" instead of an invented one.
        .with_bytes(-freed.unwrap_or_default());
        if let Err(e) = open.log_activity(&entry) {
            tracing::warn!(error = %e, "could not write to the activity log");
        }
    }
    Ok(())
}

fn cmd_status(
    env: &Env,
    out: Output,
    selector: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let game = find_game(env, selector)?;
    let (fs, backend) = backend_for(&game.install_dir)?;
    let inv = walk(&game, backend.as_ref(), Some(cancel.as_ref()))?;
    let status = backend.status(&game.install_dir, &inv)?;

    #[derive(serde::Serialize)]
    struct StatusOut<'a> {
        game: &'a str,
        id: String,
        filesystem: &'a str,
        backend: &'static str,
        mount_compression: Option<String>,
        #[serde(flatten)]
        status: backend::CompressionStatus,
        compressed_fraction: f64,
    }
    if out.json {
        let payload = StatusOut {
            game: &game.title,
            id: game.id.to_string(),
            filesystem: &fs.fstype,
            backend: backend.kind().label(),
            mount_compression: fs
                .mount_compression()
                .map(|(a, l)| l.map_or_else(|| a.clone(), |l| format!("{a}:{l}"))),
            status,
            compressed_fraction: status.ratio(),
        };
        return out.emit(&payload, || {});
    }
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

fn cmd_log(out: Output, limit: u32) -> Result<()> {
    let Some(path) = Db::default_path() else {
        bail!("cannot work out where the state database lives; is HOME set?");
    };
    let db = Db::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let entries = db.recent_activity(limit).context("reading the activity log")?;

    if out.json {
        #[derive(serde::Serialize)]
        struct LogRow {
            ts: i64,
            level: &'static str,
            kind: String,
            game: Option<String>,
            message: String,
            free_space_delta: i64,
        }
        let rows: Vec<LogRow> = entries
            .iter()
            .map(|e| LogRow {
                ts: e.ts,
                level: e.level.as_str(),
                kind: e.kind.clone(),
                game: e.game_id.as_ref().map(ToString::to_string),
                message: e.message.clone(),
                free_space_delta: -e.bytes_delta,
            })
            .collect();
        return out.emit(&rows, || {});
    }
    if entries.is_empty() {
        println!("Nothing recorded yet. Compress a game and it will show up here.");
        return Ok(());
    }
    for entry in entries {
        // The byte figure comes from the drive's free space, which moves for
        // reasons that have nothing to do with us, so it is labelled as the
        // rough thing it is.
        let delta = if entry.bytes_delta == 0 {
            String::new()
        } else if entry.bytes_delta < 0 {
            format!("  (free space +{})", size(entry.bytes_delta.unsigned_abs()))
        } else {
            format!("  (free space -{})", size(entry.bytes_delta.unsigned_abs()))
        };
        println!(
            "{:>12}  {:<5}  {:<12}  {}{delta}",
            ago(entry.ts),
            entry.level.as_str(),
            entry.kind,
            entry.message
        );
    }
    Ok(())
}

/// The directories in a Steam library whose contents should inherit
/// compression.
///
/// `downloading` and `temp` are where Steam stages a download before moving it
/// into place, so covering them is what makes the saving cost nothing: the
/// bytes are compressed the first and only time they are written.
fn hook_dirs(library: &Path) -> Vec<PathBuf> {
    let steamapps = library.join("steamapps");
    ["", "common", "downloading", "temp"]
        .iter()
        .map(|sub| if sub.is_empty() { steamapps.clone() } else { steamapps.join(sub) })
        .filter(|p| p.is_dir())
        .collect()
}

/// Every Steam library on this machine, with duplicates removed.
fn steam_libraries(env: &Env) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in crate::launchers::steam::roots(env) {
        let Ok(libraries) = crate::launchers::steam::libraries(&root) else { continue };
        for library in libraries {
            if !out.contains(&library) {
                out.push(library);
            }
        }
    }
    out
}

fn cmd_hook(env: &Env, out: Output, action: HookAction) -> Result<()> {
    let libraries = steam_libraries(env);
    if libraries.is_empty() {
        bail!("no Steam libraries found; try `flummox doctor`");
    }

    #[derive(serde::Serialize)]
    struct HookRow {
        library: PathBuf,
        filesystem: String,
        supported: bool,
        directories: usize,
        compression: Option<String>,
    }

    let mut rows = Vec::new();
    for library in libraries {
        let fs = fsprobe::probe(&library).ok();
        // The property only means anything on a filesystem that compresses.
        let supported = fs
            .as_ref()
            .is_some_and(|f| matches!(fsprobe::tier_for(f), Tier::Native(_)));
        let dirs = hook_dirs(&library);

        if supported {
            for dir in &dirs {
                let result = match action {
                    HookAction::On => backend::btrfs::set_dir_property(dir, true),
                    HookAction::Off => backend::btrfs::set_dir_property(dir, false),
                    HookAction::Status => Ok(()),
                };
                if let Err(e) = result {
                    eprintln!("warning: {}: {e}", dir.display());
                }
            }
        }

        let compression = dirs
            .first()
            .and_then(|dir| backend::btrfs::dir_property(dir).ok().flatten());
        rows.push(HookRow {
            library,
            filesystem: fs.map_or_else(|| "unknown".to_owned(), |f| f.fstype),
            supported,
            directories: dirs.len(),
            compression,
        });
    }

    out.emit(&rows, || {
        for row in &rows {
            let state = match (&row.compression, row.supported) {
                (Some(algo), _) => format!("on ({algo})"),
                (None, true) => "off".to_owned(),
                (None, false) => format!("not possible on {}", row.filesystem),
            };
            println!("{:<52}  {state}", row.library.display());
        }
        match action {
            HookAction::On => println!(
                "\nNew downloads and patches in these libraries will be compressed as they \
                 are written. Games already installed are untouched; run `flummox compress` \
                 for those."
            ),
            HookAction::Off => {
                println!("\nFuture downloads land uncompressed. Nothing already compressed changed.");
            }
            HookAction::Status => {}
        }
    })
}

/// The games hidden by the exclusion list.
///
/// Returns an empty set when the database is unavailable, so a missing
/// database hides nothing rather than hiding everything.
fn excluded_ids(db: Option<&Db>) -> Vec<crate::model::GameId> {
    db.and_then(|db| db.excluded().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

fn cmd_exclude(env: &Env, out: Output, action: ExcludeAction) -> Result<()> {
    let Some(path) = Db::default_path() else {
        bail!("cannot work out where the state database lives; is HOME set?");
    };
    let db = Db::open(&path).with_context(|| format!("opening {}", path.display()))?;

    match action {
        ExcludeAction::Add { selector } => {
            let game = find_game(env, &selector)?;
            db.exclude(&game.id, &game.title).context("recording the exclusion")?;
            println!("Hidden: {} ({})", game.title, game.id);
            println!("It will not appear in scans and will not be compressed.");
            Ok(())
        }
        ExcludeAction::Remove { selector } => {
            // Matched against the list rather than a scan, because an excluded
            // game no longer turns up in one.
            let hidden = db.excluded().context("reading the exclusion list")?;
            let found = hidden.iter().find(|(id, title)| {
                id.to_string().eq_ignore_ascii_case(selector.trim())
                    || id.key == selector.trim()
                    || title.to_lowercase().contains(&selector.trim().to_lowercase())
            });
            let Some((id, title)) = found else {
                bail!("{selector:?} is not on the exclusion list; try `flummox exclude list`");
            };
            db.unexclude(id).context("removing the exclusion")?;
            println!("Visible again: {title} ({id})");
            Ok(())
        }
        ExcludeAction::List => {
            #[derive(serde::Serialize)]
            struct ExcludedRow {
                id: String,
                title: String,
            }
            let hidden = db.excluded().context("reading the exclusion list")?;
            let rows: Vec<ExcludedRow> = hidden
                .iter()
                .map(|(id, title)| ExcludedRow { id: id.to_string(), title: title.clone() })
                .collect();
            out.emit(&rows, || {
                if rows.is_empty() {
                    println!("Nothing is hidden.");
                    return;
                }
                for row in &rows {
                    println!("{:<44}  {}", row.title, row.id);
                }
            })
        }
    }
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
    println!("flummox doctor\n");

    let roots = crate::launchers::steam::roots(env);
    if roots.is_empty() {
        println!("[!] No Steam installation found under {}", env.home.display());
    } else {
        for root in &roots {
            println!("[ok] Steam root: {}", root.display());
            match crate::launchers::steam::libraries(root) {
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

    for library in steam_libraries(env) {
        let on = backend::btrfs::dir_property(&library.join("steamapps"))
            .ok()
            .flatten();
        match on {
            Some(algo) => println!("[ok] new downloads compress on arrival ({algo}): {}", library.display()),
            None => println!(
                "[ ]  new downloads land uncompressed: {}. Turn it on with `flummox hook on`",
                library.display()
            ),
        }
    }

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
    use crate::testutil::{TestResult, check_eq};

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
