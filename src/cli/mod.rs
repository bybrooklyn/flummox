//! The command line front end.
//!
//! Ordinary jobs share the GUI coordinator. Explicit overrides and store
//! experiments run here, with their own cancellation and verification.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use humansize::{DECIMAL, format_size};

use crate::backend::{self, Backend, CompressOpts, Event, EventSink, JobCtx, Preset};
use crate::busy::{self, ProcFs};
use crate::db::{Activity, ActivityLevel, Db, GameRecord};
use crate::estimate::DiskProbe;
use crate::estimate::{self, EstimateOpts};
use crate::fsprobe::{self, FsInfo, Tier};
use crate::inventory::{self, Inventory};
use crate::launchers::{Env, Scan};
use crate::model::Game;
use crate::sandbox::{self, SandboxPlan};

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
        /// Files to work on at once (1 to 32).
        #[arg(long, default_value_t = 2, value_parser = thread_count)]
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
    /// Decompress a game.
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
    /// Compress Steam downloads as they finish.
    ///
    /// Stays running and watches Steam's own manifests. A game is compressed
    /// once its download settles, which recovers what the filesystem's write
    /// heuristic skipped while the files were being written.
    Watch {
        #[command(subcommand)]
        action: Option<WatchAction>,
        #[command(flatten)]
        level: LevelArgs,
        /// Files to work on at once (1 to 32).
        #[arg(long, default_value_t = 2, value_parser = thread_count)]
        threads: usize,
        /// Report what would be compressed, without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Exclude something that is not a game, or that you never want touched.
    Exclude {
        #[command(subcommand)]
        action: ExcludeAction,
    },
    /// Show the free space a job needs, without changing any files.
    Plan {
        folder: PathBuf,
        #[arg(long)]
        restore: bool,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// List the drives holding games, and how each one can be compressed.
    Drives,
    /// Check this machine for anything that would stop the tool working.
    Doctor,
    /// Compare Standard compression with larger-frame Maximum compression, read-only.
    Benchmark {
        folder: PathBuf,
        #[arg(long, default_value_t = 32)]
        budget_mib: u64,
    },
    /// Check and manage compatibility reports.
    Compatibility {
        #[command(subcommand)]
        action: CompatibilityAction,
    },
    /// Maximum: stores, checks, decompressing and mounts.
    Pack {
        #[command(subcommand)]
        action: crate::pack::cli::Command,
    },
    /// Inspect and control the same durable jobs shown in the window.
    Jobs {
        #[command(subcommand)]
        action: Option<JobAction>,
    },
}

#[derive(Debug, Subcommand)]
enum JobAction {
    /// Register a folder whose immediate subfolders are games.
    AddFolder {
        path: String,
        /// Treat the whole folder as one game instead.
        #[arg(long)]
        single_game: bool,
    },
    /// Forget a custom location without deleting its files.
    RemoveFolder {
        path: String,
    },
    /// Restart the background worker when jobs and mounted installs are idle.
    Restart,
    Pause {
        id: i64,
    },
    Resume {
        id: i64,
    },
    Cancel {
        id: i64,
    },
    Retry {
        id: i64,
    },
}

#[derive(Debug, Subcommand)]
enum CompatibilityAction {
    /// Validate a report and add it to the owner-local store.
    Import { report: PathBuf },
    /// List stored reports. Use --json for sanitized export data.
    List,
    /// Report the allocated bytes of files and folders for a compatibility report.
    Measure {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum ExcludeAction {
    /// Exclude a game, by app ID or part of its title.
    Add {
        /// What to exclude.
        selector: String,
    },
    /// Include an excluded game again.
    Remove {
        /// What to include again.
        selector: String,
    },
    /// List the excluded games.
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
    /// Explicit zstd level, overriding the preset (-15 to 15, not 0).
    #[arg(long, value_parser = zstd_level)]
    level: Option<i32>,
}

/// Accepts the levels btrfs can apply. Zero is refused because the database
/// reads a recorded level of 0 as "not compressed".
fn zstd_level(text: &str) -> Result<i32, String> {
    let level: i32 = text
        .parse()
        .map_err(|_| format!("{text:?} is not a whole number"))?;
    if level == 0 || !(-15..=15).contains(&level) {
        return Err("choose a level from -15 to 15, other than 0".to_owned());
    }
    Ok(level)
}

/// Accepts the worker counts the coordinator accepts.
fn thread_count(text: &str) -> Result<usize, String> {
    let threads: usize = text
        .parse()
        .map_err(|_| format!("{text:?} is not a whole number"))?;
    if !(1..=32).contains(&threads) {
        return Err("choose between 1 and 32 threads".to_owned());
    }
    Ok(threads)
}

impl LevelArgs {
    fn opts(&self, threads: usize) -> CompressOpts {
        CompressOpts {
            preset: self.preset.into(),
            level: self.level,
            threads,
        }
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
        Command::Compress {
            selector,
            level,
            threads,
            force,
            no_pause,
            dry_run,
        } => cmd_compress(
            &env, &selector, &level, threads, force, no_pause, dry_run, false, out, &cancel,
        ),
        Command::Decompress { selector, force } => cmd_decompress(&env, &selector, force, &cancel),
        Command::Status { selector } => cmd_status(&env, out, &selector, &cancel),
        Command::Log { limit } => cmd_log(out, limit),
        Command::Hook { action } => cmd_hook(&env, out, action),
        Command::Watch {
            action,
            level,
            threads,
            dry_run,
        } => cmd_watch(&env, out, action, &level, threads, dry_run, &cancel),
        Command::Exclude { action } => cmd_exclude(&env, out, action),
        Command::Plan {
            folder,
            restore,
            store,
        } => {
            let plan = if let Some(store) = store {
                let mut plan = crate::storage::SpacePlan {
                    retained_original: true,
                    ..Default::default()
                };
                plan.add(
                    crate::storage::volume(&store)?,
                    crate::storage::pack_bound(&crate::storage::inventory(&folder)?)?,
                    "New store; the original stays on its drive",
                )?;
                plan
            } else {
                crate::storage::native_plan(&folder, restore)?
            };
            println!("{}", serde_json::to_string_pretty(&plan)?);
            plan.check()
        }
        Command::Drives => cmd_drives(&env, out),
        Command::Doctor => {
            ensure!(!out.json, "doctor has no JSON output yet");
            cmd_doctor(&env)
        }
        Command::Pack { action } => crate::pack::cli::run(action, cli.json, &cancel),
        Command::Benchmark { folder, budget_mib } => {
            let report = crate::benchmark::run(&folder, budget_mib, &cancel)?;
            out.emit(&report, || {
                println!("{} sampled in {} of {} eligible files. Source files unchanged.",size(report.sampled_bytes),report.sampled_files,report.eligible_files);
                for row in &report.candidates {
                    let ratio = if row.input_bytes == 0 { 1. } else { row.output_bytes as f64 / row.input_bytes as f64 };
                    println!("{:<36} {:>6.1}% retained  encode {:.2} ms  decode {:.2} ms",row.name,ratio * 100.,row.compression_ns as f64 / 1e6,row.decompression_ns as f64 / 1e6);
                }
                println!("Native rows model btrfs allocation. Experimental rows measure frame bytes, excluding store metadata. Neither measures space saved on your drive.");
                for warning in &report.warnings { eprintln!("warning: {warning}"); }
            })
        }
        Command::Compatibility { action } => cmd_compatibility(out, action),
        Command::Jobs { action } => {
            use crate::jobs::Command as JobCommand;
            let command = match action {
                None => JobCommand::Snapshot,
                Some(JobAction::Restart) => JobCommand::Restart,
                Some(JobAction::AddFolder { path, single_game }) => {
                    JobCommand::Library(crate::jobs::Library {
                        path: client_path(&path, &env.home)?,
                        automatic: false,
                        custom: true,
                        folder_kind: if single_game {
                            crate::jobs::FolderKind::Game
                        } else {
                            crate::jobs::FolderKind::Collection
                        },
                    })
                }
                Some(JobAction::RemoveFolder { path }) => {
                    JobCommand::RemoveLibrary(client_path(&path, &env.home)?)
                }
                Some(JobAction::Pause { id }) => JobCommand::Pause { id, paused: true },
                Some(JobAction::Resume { id }) => JobCommand::Pause { id, paused: false },
                Some(JobAction::Cancel { id }) => JobCommand::Cancel(id),
                Some(JobAction::Retry { id }) => JobCommand::Retry(id),
            };
            let snapshot = crate::jobs::request(command)?;
            out.emit(&snapshot, || {
                for job in &snapshot.jobs {
                    println!(
                        "{}  {}  {:?}  {}",
                        job.id,
                        job.phase.label(),
                        job.operation,
                        job.game.title
                    );
                }
            })
        }
    }
}

/// The largest report `compatibility import` reads, as in the window's import.
const MAX_REPORT_BYTES: u64 = 1024 * 1024;

/// Reads a report file, refusing one larger than [`MAX_REPORT_BYTES`].
fn read_report(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_REPORT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_REPORT_BYTES,
        "{} is larger than 1 MiB, so it is not a compatibility report",
        path.display()
    );
    Ok(bytes)
}

/// Resolves a typed location against this process's working directory.
///
/// The coordinator keeps the working directory of whichever client started
/// it, so a relative path sent as typed would mean a different folder there.
fn client_path(input: &str, home: &Path) -> Result<PathBuf> {
    let path = crate::jobs::folder_path(input, home);
    std::path::absolute(&path).with_context(|| format!("resolving {}", path.display()))
}

fn compatibility_store() -> Result<crate::compatibility::Store> {
    let database = Db::default_path().context("Cannot locate Flummox's state folder")?;
    let parent = database.parent().context("Invalid Flummox state path")?;
    crate::compatibility::Store::open(parent.join("compatibility"))
}

fn cmd_compatibility(out: Output, action: CompatibilityAction) -> Result<()> {
    if let CompatibilityAction::Measure { paths } = &action {
        let found = crate::allocation::measure(paths)?;
        return out.emit(&found, || {
            println!(
                "{} files  {} logical bytes  {} allocated bytes",
                found.files, found.logical_bytes, found.allocated_bytes
            );
        });
    }
    let store = compatibility_store()?;
    match action {
        CompatibilityAction::Import { report } => {
            let bytes = read_report(&report)?;
            let report: crate::compatibility::Report = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", report.display()))?;
            report.validate()?;
            let path = store.save(&report)?;
            out.emit(&report, || {
                println!("Stored compatibility report at {}", path.display());
            })
        }
        CompatibilityAction::List => {
            let reports = store.load()?;
            out.emit(&reports, || {
                if reports.is_empty() {
                    println!("No compatibility reports recorded.");
                }
                for report in &reports {
                    println!(
                        "{}  build {}  {:?}  {} -> {} allocated bytes",
                        report.game.id(),
                        report.game.build,
                        report.mode,
                        report.storage.allocated_before,
                        report.storage.allocated_after
                    );
                }
            })
        }
        CompatibilityAction::Measure { .. } => Ok(()),
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
/// The flag is polled between files, so the current file finishes and a
/// partly compressed game can be resumed by running the command again. A
/// second signal exits at once, which stops a command that never polls it.
fn install_signal_handler() -> Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        // Registered first, so it sees the flag as it was before this signal.
        // A second signal ends the process with the usual 128 + signal code,
        // for the commands that never poll the flag.
        let _exit = signal_hook::flag::register_conditional_shutdown(
            signal,
            128 + signal,
            Arc::clone(&flag),
        )
        .with_context(|| format!("installing the exit handler for signal {signal}"))?;
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

/// What a running job does when the game is launched.
///
/// Pausing waits for the game to close. Stopping sets the cancel flag, which
/// ends the job at the next file boundary.
struct LaunchGuard<'a> {
    watch: &'a busy::BackgroundScan,
    stop: Option<&'a AtomicBool>,
}

impl backend::BusyCheck for LaunchGuard<'_> {
    fn in_use_by(&self) -> Option<String> {
        let who = self.watch.latest().blocking()?;
        let Some(stop) = self.stop else {
            return Some(who);
        };
        if !stop.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!("\nstopping: {who} is using the game");
        }
        None
    }

    // The answer is already in memory.
    fn check_interval(&self) -> std::time::Duration {
        std::time::Duration::ZERO
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
fn recorded_levels(db: Option<&Db>, game: &Game, inv: &Inventory) -> HashMap<PathBuf, i32> {
    let Some(db) = db else { return HashMap::new() };
    match db.fingerprints(&game.id) {
        Ok(prints) => inv
            .files
            .iter()
            .filter_map(|entry| {
                prints
                    .get(&entry.rel)
                    .filter(|fp| fp.matches(entry))
                    .map(|fp| (game.install_dir.join(&entry.rel), fp.level_applied))
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not read what earlier passes did");
            HashMap::new()
        }
    }
}

fn scan(env: &Env) -> Scan {
    let scan = crate::launchers::scan_all(env);
    tracing::info!(
        games = scan.games.len(),
        warnings = scan.warnings.len(),
        "scanned launchers"
    );
    for warning in &scan.warnings {
        eprintln!("warning: {warning}");
    }
    scan
}

/// The games a selector names.
///
/// An id beats a title, and a whole title beats part of one. Otherwise
/// "Portal" could never be chosen while "Portal 2" is installed, and a number
/// in a title could stand in for another launcher's id.
fn select_games(games: Vec<Game>, selector: &str) -> Vec<Game> {
    let wanted = selector.trim();
    if wanted.is_empty() {
        return Vec::new();
    }
    let by_id = |g: &Game| {
        g.ids()
            .any(|id| id.to_string().eq_ignore_ascii_case(wanted) || id.key == wanted)
    };
    let by_title = |g: &Game| g.title.eq_ignore_ascii_case(wanted);
    if games.iter().any(by_id) {
        games.into_iter().filter(by_id).collect()
    } else if games.iter().any(by_title) {
        games.into_iter().filter(by_title).collect()
    } else {
        games.into_iter().filter(|g| g.matches(wanted)).collect()
    }
}

/// Finds the one game a selector names.
fn find_game(env: &Env, selector: &str) -> Result<Game> {
    ensure!(
        !selector.trim().is_empty(),
        "name a game; try `flummox scan`"
    );
    let matches = select_games(scan(env).games, selector);
    match matches.len() {
        0 => bail!("no game matches {selector:?}; try `flummox scan`"),
        1 => matches.into_iter().next().context("no game"),
        _ => {
            let titles: Vec<String> = matches
                .iter()
                .map(|g| format!("{} ({})", g.title, g.id))
                .collect();
            bail!(
                "{selector:?} matches several games:\n  {}",
                titles.join("\n  ")
            )
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
            bail!(
                "{} is on {}, which is not supported: {why}",
                install_dir.display(),
                fs.fstype
            )
        }
        Tier::Pack => bail!(
            "{} is on {}, which needs Maximum Space. Use `flummox pack create` followed by \
             `flummox pack activate`, or use the desktop app's advanced controls.",
            install_dir.display(),
            fs.fstype
        ),
        Tier::Native(kind) => *kind,
    };
    let backend =
        backend::for_kind(kind).with_context(|| format!("no backend for {}", kind.label()))?;
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
        bail!(
            "{} is in use by {p}; close it first, or pass --force",
            game.title
        );
    }
    if !game.state.is_idle() && !force {
        bail!(
            "{} is {} (Steam's own state). Wait for it to finish, or pass --force.",
            game.title,
            game.state
        );
    }
    if !game.state.is_idle() || process.is_some() {
        eprintln!(
            "warning: --force: working on {} while it is {}",
            game.title, game.state
        );
    }
    Ok(())
}

/// Checks the settings the coordinator accepts for a queued job.
fn check_job_opts(opts: &CompressOpts) -> Result<()> {
    ensure!(
        (1..=32).contains(&opts.threads),
        "Choose between 1 and 32 threads."
    );
    if let Some(level) = opts.level {
        zstd_level(&level.to_string()).map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

/// What the worker checks before a job, for the paths that run in this
/// process: settings in range, enough free space, and a kernel that can keep
/// every path inside the install.
fn preflight(install_dir: &Path, opts: Option<&CompressOpts>, restore: bool) -> Result<()> {
    if let Some(opts) = opts {
        check_job_opts(opts)?;
    }
    crate::storage::native_plan(install_dir, restore)?.recheck()?;
    let anchor = crate::safeio::Anchor::open(install_dir)
        .with_context(|| format!("opening {}", install_dir.display()))?;
    ensure!(
        anchor.fully_resolved(),
        "This kernel cannot safely resolve game paths. Update Linux before using --force."
    );
    Ok(())
}

/// Applies the sandbox a worker applies: the game folder and the database
/// files writable, and no new sockets.
///
/// Call before any thread that touches game files exists, with the database
/// already open so its companion files exist.
fn confine(install_dir: &Path) {
    let database = Db::default_path();
    let plan = match &database {
        Some(database) => SandboxPlan::for_worker(install_dir, database),
        None => SandboxPlan::for_job(install_dir, None),
    };
    let status = sandbox::restrict(&plan);
    tracing::info!(status = %status.describe(), "sandbox");
    if !status.is_active() {
        eprintln!("warning: {}", status.describe());
    }
    if let Err(error) = sandbox::deny_sockets() {
        eprintln!("warning: this job can still open sockets: {error}");
    }
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
        println!(
            "No games found. Add a games location in the app or with flummox jobs add-folder."
        );
        return Ok(());
    }
    let rows: Vec<ScanRow> = games
        .iter()
        .map(|game| {
            let fs = fsprobe::probe(&game.install_dir).ok();
            let tier = fs.as_ref().map(fsprobe::tier_for);
            let (backend, supported, note) = match &tier {
                Some(Tier::Native(kind)) => (
                    Some(kind.label()),
                    backend::for_kind(*kind).is_some(),
                    backend::for_kind(*kind)
                        .is_none()
                        .then(|| format!("{} has no backend yet", kind.label())),
                ),
                Some(Tier::Pack) => (
                    Some("Maximum"),
                    cfg!(feature = "pack-mount") && Path::new("/dev/fuse").exists(),
                    (!cfg!(feature = "pack-mount"))
                        .then(|| "rebuild with --features pack-mount for Maximum".to_owned())
                        .or_else(|| {
                            (!Path::new("/dev/fuse").exists()).then(|| {
                                "Maximum needs FUSE, a Linux feature for custom filesystems, and it is not available here".to_owned()
                            })
                        }),
                ),
                Some(Tier::Unsupported(why)) => (None, false, Some((*why).to_owned())),
                None => (
                    None,
                    false,
                    Some("could not probe the filesystem".to_owned()),
                ),
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
        let width = rows
            .iter()
            .map(|r| r.title.chars().count())
            .max()
            .unwrap_or(10)
            .min(48);
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
    let est_opts = EstimateOpts::new(opts.btrfs_level(), &fs).with_floor(opts.attainable_floor());
    let measured = backend.disk_probe();
    let probe = RecordedProbe {
        measured: measured.as_ref(),
        levels: recorded_levels(open_db().as_ref(), &game, &inv),
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

    out.emit(&EstimateOut::new(&game, &est, &est_opts), || {
        print_estimate(&est, &est_opts)
    })
}

/// One game's estimate in `estimate --json` and `compress --dry-run --json`.
#[derive(serde::Serialize)]
struct EstimateOut<'a> {
    game: &'a str,
    id: String,
    level: i32,
    mount_level: Option<i32>,
    #[serde(flatten)]
    estimate: estimate::Estimate,
}

impl<'a> EstimateOut<'a> {
    fn new(game: &'a Game, estimate: &estimate::Estimate, opts: &EstimateOpts) -> Self {
        Self {
            game: &game.title,
            id: game.id.to_string(),
            level: opts.level,
            mount_level: opts.mount_level,
            estimate: *estimate,
        }
    }
}

fn print_estimate(est: &estimate::Estimate, opts: &EstimateOpts) {
    println!("  install    : {}", size(est.install_bytes));
    println!(
        "  evidence   : {} recognized, {} unknown, {} encoded, {} containers{}",
        est.format_evidence.recognized_files,
        est.format_evidence.unknown_files,
        est.format_evidence.encoded_files,
        est.format_evidence.container_files,
        if est.format_evidence.encrypted_files > 0 {
            format!(
                ", {} encrypted and sampled",
                est.format_evidence.encrypted_files
            )
        } else {
            String::new()
        }
    );
    println!(
        "  will rewrite: {} files (everything a compress pass would touch)",
        est.rewrite_files
    );
    println!(
        "  of those, expected to shrink: {} files, {}",
        est.files,
        size(est.bytes)
    );
    println!(
        "  skipped    : {} files (too small, already compressed, or not worth it)",
        est.skipped_files.saturating_sub(est.unsampled_files)
    );
    if est.unsampled_files > 0 {
        println!(
            "  not sampled: {} smaller files; the sizes here are scaled up from the {} that were",
            est.unsampled_files, est.inspected_files
        );
    }
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
            Event::Progress {
                files_done,
                bytes_done,
                current,
            } => {
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
                eprintln!(
                    "{}paused: {by} is using the game, waiting",
                    if self.tty { "\r" } else { "" }
                );
            }
            Event::Resumed => {
                eprintln!("{}resumed", if self.tty { "\r" } else { "" });
            }
            Event::Warning(msg) => eprintln!("{}warning: {msg}", if self.tty { "\r" } else { "" }),
            Event::FileCompleted { .. } => {}
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
    wait_for_idle: bool,
    out: Output,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let game = find_game(env, selector)?;
    let install = crate::jobs::validate_folder(&game.install_dir)?;
    let opts = level.opts(threads);
    check_job_opts(&opts)?;
    let (fs, backend) = backend_for(&game.install_dir)?;
    let queued = !force && !no_pause && !dry_run;
    // A queued job waits behind the game, so a caller that can retry later
    // (the watcher) leaves the check to the coordinator.
    if !(queued && wait_for_idle) {
        check_idle(&game, force)?;
    }
    // Open the database before the sandbox goes up: creating its directory is
    // simpler to do now than to grant a sandboxed process.
    let mut db = open_db();
    // Naming a hidden game directly should not get around hiding it.
    if let Some(open) = db.as_ref()
        && game.ids().any(|id| open.is_excluded(id).unwrap_or(false))
    {
        bail!(
            "{} is excluded. Run `flummox exclude remove {}` to include it.",
            game.title,
            game.id
        );
    }
    if queued {
        return queued_job(game, crate::jobs::Operation::Compress, opts, cancel);
    }
    let _operation = if dry_run {
        None
    } else {
        Some(crate::jobs::operation_lock()?)
    };
    if !dry_run {
        preflight(&game.install_dir, Some(&opts), false)?;
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
    let reuse = previous
        .as_ref()
        .is_some_and(|prev| prev.level >= opts.attainable_floor());
    let mut unchanged = 0usize;
    let inv = match (reuse, db.as_ref()) {
        (true, Some(open)) => {
            match open.changed_since_floor(&game.id, &full_inv, opts.attainable_floor()) {
                Ok(changed) => {
                    unchanged = full_inv.files.len().saturating_sub(changed.len());
                    Inventory {
                        files: changed,
                        warnings: Vec::new(),
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not tell which files changed; doing all of them");
                    full_inv.clone()
                }
            }
        }
        _ => full_inv.clone(),
    };
    if unchanged > 0 && !out.json {
        println!(
            "{unchanged} files are unchanged since the last pass at level {}; skipping them.",
            previous.as_ref().map_or(0, |p| p.level)
        );
    }
    // Running a job over nothing would still print a free-space delta, and on
    // a busy filesystem that delta is somebody else's writes.
    if !dry_run && inv.to_compress().next().is_none() {
        println!(
            "Nothing to do: {} is already compressed and nothing has changed.",
            game.title
        );
        return Ok(());
    }

    if dry_run {
        let model = backend.model(&opts);
        let est_opts =
            EstimateOpts::new(opts.btrfs_level(), &fs).with_floor(opts.attainable_floor());
        let measured = backend.disk_probe();
        let probe = RecordedProbe {
            measured: measured.as_ref(),
            levels: recorded_levels(db.as_ref(), &game, &full_inv),
        };
        let est = estimate::estimate_game_cancellable(
            &game.install_dir,
            &full_inv,
            model.as_ref(),
            &est_opts,
            &probe,
            Some(cancel.as_ref()),
        );
        return out.emit(&EstimateOut::new(&game, &est, &est_opts), || {
            println!("{} (dry run, nothing written)", game.title);
            print_estimate(&est, &est_opts);
        });
    }

    crate::jobs::invalidate_for_cli(&game.install_dir)?;

    // The scan that spots a launch has to start before the sandbox goes up,
    // because Landlock then denies the reads it makes.
    let watch =
        busy::BackgroundScan::start(game.install_dir.clone(), std::time::Duration::from_secs(2));
    // Drop this process's access to everything except the game itself, before
    // any worker thread exists. Discovery is already finished, so nothing
    // further needs to read Steam's configuration.
    confine(&game.install_dir);

    // Sampled before the pass runs. Compressing changes what the probe sees,
    // so an estimate taken afterwards reports a saving that has already been
    // taken, which is why this cannot be deferred to the end. It runs after
    // the sandbox so the sampling threads are confined too.
    let pass_saving = {
        let model = backend.model(&opts);
        let est_opts =
            EstimateOpts::new(opts.btrfs_level(), &fs).with_floor(opts.attainable_floor());
        let measured = backend.disk_probe();
        let probe = RecordedProbe {
            measured: measured.as_ref(),
            levels: recorded_levels(db.as_ref(), &game, &full_inv),
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

    let progress = Progress::new();
    let guard = LaunchGuard {
        watch: &watch,
        stop: no_pause.then_some(cancel.as_ref()),
    };
    let ctx = JobCtx {
        events: &progress,
        cancel: cancel.as_ref(),
        busy: Some(&guard as &dyn backend::BusyCheck),
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
            println!(
                "Freed about {} (from free space, so approximate).",
                size(n.unsigned_abs())
            );
        }
        _ => println!(
            "Free space did not go up. On a drive already mounted with compression, \
             most of the gain was already there."
        ),
    }
    report_failures(&outcome);

    if let Some(open) = db.as_mut() {
        let mut record = GameRecord::new(
            game.id.clone(),
            game.title.clone(),
            install.clone(),
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
        // Only the share of the plan this pass got through counts, or a
        // cancelled pass followed by a rerun would count the saving twice.
        let planned: u64 = inv.to_compress().map(|f| f.size).sum();
        let pass_saving = estimate::scaled_saving(pass_saving, outcome.bytes, planned);
        record.est_saving = previous
            .as_ref()
            .map_or(0, |p| p.est_saving)
            .saturating_add(i64::try_from(pass_saving).unwrap_or(i64::MAX));
        record.level = level_to_record(
            opts.level_plan(),
            outcome.effective_level,
            previous.as_ref().map(|p| p.level),
            record.level,
        );
        let floor = opts.level_plan().floor();
        if outcome
            .effective_level
            .is_some_and(|applied| applied < floor)
        {
            println!(
                "Note: this kernel does not accept a compression level, so the files were \
                 compressed at the filesystem default rather than {floor}."
            );
        }
        // Fingerprints are stored for the whole install, not just the files
        // this pass touched, or the skipped ones would look new next time.
        if let Err(e) = open.record_outcome(&record, &full_inv, &outcome.completed) {
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
    ensure!(
        !outcome.cancelled && outcome.errors.is_empty(),
        "{} was not fully compressed: {}",
        game.title,
        pass_problem(&outcome)
    );
    Ok(())
}

/// Prints the first few per-file failures to stderr.
fn report_failures(outcome: &backend::Outcome) {
    if outcome.errors.is_empty() {
        return;
    }
    eprintln!("{} files failed; the first few:", outcome.errors.len());
    for e in outcome.errors.iter().take(5) {
        eprintln!("  {e}");
    }
}

/// Why a pass counts as unfinished, for the error that sets the exit status.
fn pass_problem(outcome: &backend::Outcome) -> String {
    match (outcome.cancelled, outcome.errors.len()) {
        (true, 0) => "it was stopped early".to_owned(),
        (true, n) => format!("it was stopped early and {n} files failed"),
        (false, n) => format!("{n} files failed"),
    }
}

/// The level to keep for a game after a pass.
///
/// `applied` is the lowest level any file got. Once it reaches the plan's
/// floor the plan is done, so the ceiling is kept; a per-file plan leaves
/// some files at its floor. A higher level from an earlier pass stays, since
/// files skipped as unchanged still sit there.
fn level_to_record(
    plan: backend::LevelPlan,
    applied: Option<i32>,
    previous: Option<i32>,
    requested: i32,
) -> i32 {
    let level = match applied {
        Some(level) if level >= plan.floor() => plan.ceiling(),
        Some(level) => level,
        None => requested,
    };
    previous.map_or(level, |p| p.max(level))
}

fn cmd_decompress(env: &Env, selector: &str, force: bool, cancel: &Arc<AtomicBool>) -> Result<()> {
    let game = find_game(env, selector)?;
    crate::jobs::validate_folder(&game.install_dir)?;
    if !force {
        return queued_job(
            game,
            crate::jobs::Operation::Decompress,
            CompressOpts::default(),
            cancel,
        );
    }
    let _operation = crate::jobs::operation_lock()?;
    let (_fs, backend) = backend_for(&game.install_dir)?;
    check_idle(&game, force)?;
    preflight(&game.install_dir, None, true)?;
    let mut db = open_db();
    let inv = walk(&game, backend.as_ref(), Some(cancel.as_ref()))?;
    println!("Decompressing {}", game.title);
    crate::jobs::invalidate_for_cli(&game.install_dir)?;
    if let Some(open) = db.as_mut() {
        open.invalidate_compression(&game.install_dir.canonicalize()?)?;
    }
    // Same restriction as a compress job: by this point every path the work
    // needs is known, so the process has no business reaching anything else.
    confine(&game.install_dir);
    let progress = Progress::new();
    // No pause on the way back out. Decompress is what someone runs to
    // undo, and it should not sit waiting on a game.
    let ctx = JobCtx {
        events: &progress,
        cancel: cancel.as_ref(),
        busy: None,
    };
    let outcome = backend
        .decompress(&game.install_dir, &inv, &ctx)
        .with_context(|| format!("decompressing {}", game.title))?;
    println!("Done: {} files rewritten", outcome.files);
    report_failures(&outcome);
    let freed = outcome.freed();
    if let Some(n) = freed
        && n < 0
    {
        println!("Uses about {} more space now.", size(n.unsigned_abs()));
    }

    let complete = !outcome.cancelled && outcome.errors.is_empty();
    if let Some(open) = db.as_mut() {
        // Forget first: the stored fingerprints describe a compressed install
        // that no longer exists, and `forget` also clears this game's log
        // entries, so the entry below has to come after it. An unfinished
        // pass leaves the record, since the game is still partly compressed.
        if complete && let Err(e) = open.forget(&game.id) {
            eprintln!(
                "warning: could not clear the record for {}: {e}",
                game.title
            );
        }
        let entry = Activity::new(
            if complete {
                ActivityLevel::Info
            } else {
                ActivityLevel::Warn
            },
            "decompress",
            if complete {
                format!(
                    "{} back to uncompressed ({} files)",
                    game.title, outcome.files
                )
            } else {
                format!(
                    "{} only partly uncompressed ({} files): {}",
                    game.title,
                    outcome.files,
                    pass_problem(&outcome)
                )
            },
        )
        .for_game(&game.id)
        // Zero when free space could not be read, so the log records "no
        // figure" instead of an invented one.
        .with_bytes(-freed.unwrap_or_default());
        if let Err(e) = open.log_activity(&entry) {
            tracing::warn!(error = %e, "could not write to the activity log");
        }
    }
    ensure!(
        complete,
        "{} was not fully decompressed: {}",
        game.title,
        pass_problem(&outcome)
    );
    Ok(())
}

/// Waits as a client. Disconnecting leaves ownership with the coordinator.
fn queued_job(
    game: Game,
    operation: crate::jobs::Operation,
    options: CompressOpts,
    cancel: &AtomicBool,
) -> Result<()> {
    use crate::jobs::{self, Command, Phase};
    let snapshot = jobs::request(Command::Enqueue {
        game: game.clone(),
        operation,
        options,
    })?;
    let id = snapshot
        .jobs
        .iter()
        .rev()
        .find(|j| j.game.install_dir == game.install_dir && j.operation == operation)
        .context("The queued job was not returned")?
        .id;
    println!(
        "Added {} as job {id}. It keeps running if you close this client.",
        game.title
    );
    let mut previous = String::new();
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            println!("Disconnected. Use `flummox jobs cancel {id}` to stop the background job.");
            return Ok(());
        }
        let snapshot = jobs::request(Command::Snapshot)?;
        let job = snapshot
            .jobs
            .iter()
            .find(|j| j.id == id)
            .context("Job history is unavailable")?;
        let status = format!(
            "{}: {} of {} files",
            job.phase.label(),
            job.files_done,
            job.files_total
        );
        if status != previous {
            eprintln!("{status}");
            previous = status;
        }
        if !job.phase.active() {
            anyhow::ensure!(
                job.phase == Phase::Completed,
                "{}: {} {}",
                job.phase.label(),
                job.message,
                job.errors.join("; ")
            );
            println!("{}", job.message);
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn cmd_status(env: &Env, out: Output, selector: &str, cancel: &Arc<AtomicBool>) -> Result<()> {
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
    println!(
        "{} on {} ({})",
        game.title,
        fs.fstype,
        backend.kind().label()
    );
    println!("  files      : {}", status.files);
    println!("  on disk    : {} of game data", size(status.total_bytes));
    println!(
        "  compressed : {} ({:.0}% of this game's data is stored compressed)",
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
    let entries = db
        .recent_activity(limit)
        .context("reading the activity log")?;

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
        .map(|sub| {
            if sub.is_empty() {
                steamapps.clone()
            } else {
                steamapps.join(sub)
            }
        })
        .filter(|p| p.is_dir())
        .collect()
}

/// Whether a tier can be worked on in place: a backend exists for it.
fn native_supported(tier: &Tier) -> bool {
    matches!(tier, Tier::Native(kind) if backend::for_kind(*kind).is_some())
}

/// Every Steam library on this machine, with duplicates removed.
fn steam_libraries(env: &Env) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in crate::launchers::steam::roots(env) {
        let Ok(libraries) = crate::launchers::steam::libraries(&root) else {
            continue;
        };
        for library in libraries {
            if !out.contains(&library) {
                out.push(library);
            }
        }
    }
    out
}

/// What to do with the background service.
#[derive(Debug, Subcommand)]
enum WatchAction {
    /// Start watching at login, and now.
    Enable,
    /// Stop watching, now and at login.
    Disable,
    /// Report whether the background service is on.
    Status,
}

/// The unit file name, under the user's own systemd directory.
const UNIT_NAME: &str = "flummox-watch.service";

/// First line of a unit this tool wrote.
///
/// `disable` removes a file only when it starts with this, so a unit someone
/// wrote by hand at the same path is left where it is.
const UNIT_MARKER: &str = "# Written by `flummox watch enable`.";

/// Where a systemd user unit belongs.
///
/// `$XDG_CONFIG_HOME/systemd/user`, falling back to `~/.config/...`. A
/// relative `XDG_CONFIG_HOME` is ignored, as the XDG specification requires.
fn unit_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("systemd").join("user"))
}

/// Runs `systemctl --user`, reporting whether it succeeded.
fn systemctl(args: &[&str]) -> Result<bool> {
    let status = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("running systemctl, which this machine may not have")?;
    Ok(status.success())
}

/// Quotes an executable path for a unit's `ExecStart=`.
///
/// systemd splits an unquoted value at spaces and expands `%` and `$`, so a
/// build unpacked under a path with a space would start a different program.
fn unit_exec(exe: &Path) -> Result<String> {
    let text = exe
        .to_str()
        .context("this executable's path is not valid UTF-8")?;
    ensure!(
        !text.chars().any(char::is_control),
        "this executable's path contains a control character"
    );
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '\\' | '"' => {
                quoted.push('\\');
                quoted.push(character);
            }
            '%' | '$' => {
                quoted.push(character);
                quoted.push(character);
            }
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

/// The text of the unit, running `exe watch` with the given settings.
///
/// The hardening directives match `packaging/flummox-watch.service`, so the
/// unit this tool writes in the user's own directory, which takes precedence
/// over the packaged one, is no weaker than it.
fn unit_text(exe: &Path, level: &LevelArgs, threads: usize, dry_run: bool) -> Result<String> {
    let mut args = String::from("watch");
    if let Some(name) = level.preset.to_possible_value() {
        args.push_str(&format!(" --preset {}", name.get_name()));
    }
    if let Some(value) = level.level {
        args.push_str(&format!(" --level {value}"));
    }
    args.push_str(&format!(" --threads {threads}"));
    if dry_run {
        args.push_str(" --dry-run");
    }
    // The running binary's own path is written in, so a build started from a
    // working tree runs that build rather than one installed elsewhere.
    Ok(format!(
        "{UNIT_MARKER}\n\
         [Unit]\n\
         Description=Compress Steam downloads as they finish\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} {args}\n\
         Restart=on-failure\n\
         RestartSec=30\n\
         NoNewPrivileges=true\n\
         PrivateTmp=true\n\
         RestrictSUIDSGID=true\n\
         RestrictNamespaces=true\n\
         MemoryDenyWriteExecute=true\n\
         Nice=10\n\
         IOSchedulingClass=idle\n\
         CPUSchedulingPolicy=idle\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        unit_exec(exe)?
    ))
}

/// Installs the user unit and starts it.
fn service_enable(level: &LevelArgs, threads: usize, dry_run: bool) -> Result<()> {
    let dir = unit_dir().context("neither XDG_CONFIG_HOME nor HOME is set")?;
    let exe = std::env::current_exe().context("finding this executable")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(UNIT_NAME);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        ensure!(
            existing.starts_with(UNIT_MARKER),
            "{} exists and this tool did not write it",
            path.display()
        );
    }
    let unit = unit_text(&exe, level, threads, dry_run)?;
    std::fs::write(&path, unit).with_context(|| format!("writing {}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    if !systemctl(&["enable", "--now", UNIT_NAME])? {
        bail!("systemctl could not enable {UNIT_NAME}");
    }
    println!("New downloads will be compressed on their own, starting now.");
    println!("Turn it off with `flummox watch disable`.");
    Ok(())
}

/// Stops the user unit and removes it.
fn service_disable() -> Result<()> {
    let dir = unit_dir().context("neither XDG_CONFIG_HOME nor HOME is set")?;
    let path = dir.join(UNIT_NAME);
    let _stopped = systemctl(&["disable", "--now", UNIT_NAME])?;
    match std::fs::read_to_string(&path) {
        Ok(text) if text.starts_with(UNIT_MARKER) => {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            systemctl(&["daemon-reload"])?;
        }
        Ok(_) => println!(
            "Left {} where it is: this tool did not write it.",
            path.display()
        ),
        Err(_) => {}
    }
    println!("Downloads are no longer compressed on their own.");
    println!("Anything already compressed stays as it is.");
    Ok(())
}

/// Reports whether the background service is installed and running.
fn service_status(out: Output) -> Result<()> {
    #[derive(serde::Serialize)]
    struct ServiceRow {
        starts_at_login: bool,
        running: bool,
    }
    let enabled = systemctl(&["is-enabled", "--quiet", UNIT_NAME])?;
    let active = systemctl(&["is-active", "--quiet", UNIT_NAME])?;
    let row = ServiceRow {
        starts_at_login: enabled,
        running: active,
    };
    out.emit(&row, || {
        println!("Starts at login : {}", if enabled { "yes" } else { "no" });
        println!("Running now     : {}", if active { "yes" } else { "no" });
        if !enabled {
            println!("\nTurn it on with `flummox watch enable`.");
        }
    })
}

/// Watches Steam and compresses each download once it finishes.
///
/// The property `flummox hook` sets compresses during the download and costs
/// nothing, but the filesystem judges each file as it is written and skips
/// ones it guesses will not pay. A pass afterwards recovers those.
fn cmd_watch(
    env: &Env,
    out: Output,
    action: Option<WatchAction>,
    level: &LevelArgs,
    threads: usize,
    dry_run: bool,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    match action {
        Some(WatchAction::Enable) => return service_enable(level, threads, dry_run),
        Some(WatchAction::Disable) => return service_disable(),
        Some(WatchAction::Status) => return service_status(out),
        None => {}
    }
    ensure!(
        !out.json,
        "watch has no JSON output; only `watch status` takes --json"
    );
    let libraries = steam_libraries(env);
    if libraries.is_empty() {
        bail!("no Steam libraries found; try `flummox doctor`");
    }
    for library in &libraries {
        println!("Watching {}", library.display());
    }
    println!("Waiting for downloads to finish. Press Ctrl-C to stop.");
    crate::watch::run(&libraries, cancel.as_ref(), |app| {
        if app.is_tool() {
            return;
        }
        println!("\n{} finished downloading.", app.name);
        if dry_run {
            println!("  (dry run, nothing written)");
            return;
        }
        // The full id, so a missing Steam folder cannot fall through to
        // another launcher's game with this number in its title.
        let selector = format!("steam:{}", app.appid);
        // The coordinator pauses a queued job while the game runs, so a
        // download that finishes as the player presses Play is still queued.
        let text = Output { json: false };
        if let Err(e) = cmd_compress(
            env, &selector, level, threads, false, false, false, true, text, cancel,
        ) {
            eprintln!("warning: could not compress {}: {e:#}", app.name);
        }
    })
    .context("watching Steam libraries")
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
    let mut applied = 0usize;
    for library in libraries {
        let fs = fsprobe::probe(&library).ok();
        // The property only means anything on a filesystem that compresses.
        let supported = fs
            .as_ref()
            .is_some_and(|f| native_supported(&fsprobe::tier_for(f)));
        let dirs = hook_dirs(&library);

        if supported {
            for dir in &dirs {
                let result = match action {
                    HookAction::On => backend::btrfs::set_dir_property(dir, true),
                    HookAction::Off => backend::btrfs::set_dir_property(dir, false),
                    HookAction::Status => Ok(()),
                };
                match result {
                    Ok(()) => applied += 1,
                    Err(e) => eprintln!("warning: {}: {e}", dir.display()),
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

    let changed = matches!(action, HookAction::On);
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
            HookAction::On if applied == 0 => {
                println!("\nNo library took the setting, so new downloads are not compressed.");
            }
            HookAction::On => println!(
                "\nNew downloads and patches in the libraries marked on will be compressed as \
                 they are written. Games already installed are untouched; run \
                 `flummox compress` for those."
            ),
            HookAction::Off => {
                println!(
                    "\nFuture downloads land uncompressed. Nothing already compressed changed."
                );
            }
            HookAction::Status => {}
        }
    })?;
    ensure!(
        !changed || applied > 0,
        "no library took the setting; `flummox doctor` shows why"
    );
    Ok(())
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

/// The one hidden game a selector names.
///
/// An id beats a title, a whole title beats part of one, and part of a title
/// must match exactly one entry, as in [`select_games`].
fn pick_hidden<'a>(
    hidden: &'a [(crate::model::GameId, String)],
    selector: &str,
) -> Result<&'a (crate::model::GameId, String)> {
    let wanted = selector.trim();
    ensure!(
        !wanted.is_empty(),
        "name an excluded game; try `flummox exclude list`"
    );
    let by_id = |entry: &&(crate::model::GameId, String)| {
        entry.0.to_string().eq_ignore_ascii_case(wanted) || entry.0.key == wanted
    };
    let by_title = |entry: &&(crate::model::GameId, String)| entry.1.eq_ignore_ascii_case(wanted);
    let lowered = wanted.to_lowercase();
    let by_part =
        |entry: &&(crate::model::GameId, String)| entry.1.to_lowercase().contains(&lowered);
    let mut found: Vec<_> = hidden.iter().filter(by_id).collect();
    if found.is_empty() {
        found = hidden.iter().filter(by_title).collect();
    }
    if found.is_empty() {
        found = hidden.iter().filter(by_part).collect();
    }
    match found.as_slice() {
        [] => bail!("{selector:?} is not excluded; try `flummox exclude list`"),
        [one] => Ok(one),
        many => {
            let titles: Vec<String> = many
                .iter()
                .map(|(id, title)| format!("{title} ({id})"))
                .collect();
            bail!(
                "{selector:?} matches several excluded games:\n  {}",
                titles.join("\n  ")
            )
        }
    }
}

fn cmd_exclude(env: &Env, out: Output, action: ExcludeAction) -> Result<()> {
    let Some(path) = Db::default_path() else {
        bail!("cannot work out where the state database lives; is HOME set?");
    };
    let db = Db::open(&path).with_context(|| format!("opening {}", path.display()))?;

    match action {
        ExcludeAction::Add { selector } => {
            let game = find_game(env, &selector)?;
            db.exclude(&game.id, &game.title)
                .context("recording the exclusion")?;
            crate::jobs::request(crate::jobs::Command::Exclude {
                id: game.id.to_string(),
                excluded: true,
            })?;
            println!("Excluded: {} ({})", game.title, game.id);
            println!("Jobs will skip it until you include it again.");
            Ok(())
        }
        ExcludeAction::Remove { selector } => {
            // Matched against the list rather than a scan, because an excluded
            // game no longer turns up in one.
            let hidden = db.excluded().context("reading the exclusion list")?;
            let (id, title) = pick_hidden(&hidden, &selector)?;
            db.unexclude(id).context("removing the exclusion")?;
            crate::jobs::request(crate::jobs::Command::Exclude {
                id: id.to_string(),
                excluded: false,
            })?;
            println!("Included again: {title} ({id})");
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
                .map(|(id, title)| ExcludedRow {
                    id: id.to_string(),
                    title: title.clone(),
                })
                .collect();
            out.emit(&rows, || {
                if rows.is_empty() {
                    println!("No games are excluded.");
                    return;
                }
                for row in &rows {
                    println!("{:<44}  {}", row.title, row.id);
                }
            })
        }
    }
}

/// One drive in `drives --json`.
#[derive(serde::Serialize)]
struct DriveRow {
    mountpoint: PathBuf,
    filesystem: String,
    games: usize,
    game_bytes: u64,
    free_bytes: u64,
    support: String,
    mounted_compression: Option<String>,
}

fn cmd_drives(env: &Env, out: Output) -> Result<()> {
    let scan = scan(env);
    let mut seen: Vec<(PathBuf, FsInfo, u64, usize)> = Vec::new();
    for game in &scan.games {
        let Ok(fs) = fsprobe::probe(&game.install_dir) else {
            continue;
        };
        match seen.iter_mut().find(|(mp, _, _, _)| *mp == fs.mountpoint) {
            Some(entry) => {
                entry.2 = entry.2.saturating_add(game.size_hint.unwrap_or(0));
                entry.3 += 1;
            }
            None => seen.push((fs.mountpoint.clone(), fs, game.size_hint.unwrap_or(0), 1)),
        }
    }
    if seen.is_empty() && !out.json {
        println!("No game drives found.");
        return Ok(());
    }
    let rows: Vec<DriveRow> = seen
        .into_iter()
        .map(|(mountpoint, fs, game_bytes, games)| {
            let support = match fsprobe::tier_for(&fs) {
                Tier::Native(kind) if native_supported(&Tier::Native(kind)) => {
                    format!("{} compression, in place", kind.label())
                }
                Tier::Native(kind) => format!("none ({} has no backend yet)", kind.label()),
                Tier::Pack if cfg!(feature = "pack-mount") && Path::new("/dev/fuse").exists() => {
                    "Maximum, with a store that updates can write to".to_owned()
                }
                Tier::Pack => {
                    "Maximum needs a build with the pack-mount feature and a working FUSE"
                        .to_owned()
                }
                Tier::Unsupported(why) => format!("none ({why})"),
            };
            DriveRow {
                free_bytes: backend::free_bytes(&mountpoint).unwrap_or(0),
                filesystem: fs.fstype.clone(),
                mounted_compression: fs.mount_compression().map(|(algo, level)| {
                    level.map_or_else(|| algo.clone(), |l| format!("{algo}:{l}"))
                }),
                mountpoint,
                games,
                game_bytes,
                support,
            }
        })
        .collect();
    out.emit(&rows, || {
        for row in &rows {
            println!("{} ({})", row.mountpoint.display(), row.filesystem);
            println!("  games      : {}, {}", row.games, size(row.game_bytes));
            println!("  free       : {}", size(row.free_bytes));
            println!("  support    : {}", row.support);
            if let Some(mounted) = &row.mounted_compression {
                println!("  mounted    : compress={mounted}");
            }
        }
    })
}

fn cmd_doctor(env: &Env) -> Result<()> {
    println!("flummox doctor\n");

    let roots = crate::launchers::steam::roots(env);
    if roots.is_empty() {
        println!(
            "[!] No Steam installation found under {}",
            env.home.display()
        );
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
            println!("[ok] kernel {release}");
            for note in kernel_notes(release) {
                println!("[!]  {note}");
            }
        }
        Err(e) => println!("[!] cannot read the kernel version: {e}"),
    }

    let fuse = Path::new("/dev/fuse").exists();
    println!(
        "{} /dev/fuse {}",
        if fuse { "[ok]" } else { "[!] " },
        if fuse {
            "present (ready for Maximum)"
        } else {
            "missing"
        }
    );

    for library in steam_libraries(env) {
        let on = backend::btrfs::dir_property(&library.join("steamapps"))
            .ok()
            .flatten();
        match on {
            Some(algo) => println!(
                "[ok] new downloads compress on arrival ({algo}): {}",
                library.display()
            ),
            None => println!(
                "[ ]  new downloads land uncompressed: {}. Turn it on with `flummox hook on`",
                library.display()
            ),
        }
    }

    let games = scan(env).games;
    let idle = games
        .iter()
        .filter(|g| g.state.is_idle() && !g.is_tool)
        .count();
    println!("[ok] {} games found, {idle} ready to compress", games.len());

    let mut checked: Vec<PathBuf> = Vec::new();
    for game in &games {
        let Ok(fs) = fsprobe::probe(&game.install_dir) else {
            continue;
        };
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

/// What this kernel cannot do for btrfs, as lines for `doctor`.
fn kernel_notes(release: &str) -> Vec<&'static str> {
    let mut notes = Vec::new();
    if parse_kernel_version(release) < (6, 15) {
        notes.push(
            "btrfs compression levels need kernel 6.15+; on this kernel the drive's \
             default level is used instead",
        );
    }
    notes.push(
        "decompress needs a kernel that honours the defrag NOCOMPRESS flag; one that \
         does not fails the pass or leaves files stored compressed, and says so",
    );
    notes
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
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    use super::*;

    #[test]
    fn doctor_names_both_kernel_requirements_on_an_old_kernel() -> TestResult {
        let old = kernel_notes("6.8.0-generic").join("\n");
        check(old.contains("6.15"), "the level requirement is named")?;
        check(
            old.contains("NOCOMPRESS"),
            "the decompress requirement is named",
        )?;
        let new = kernel_notes("7.2.3").join("\n");
        check(!new.contains("6.15"), "a new kernel has no level warning")?;
        check(new.contains("NOCOMPRESS"), "the decompress note stays")
    }

    #[test]
    fn parses_kernel_versions() -> TestResult {
        check_eq(
            parse_kernel_version("7.2.3-1-cachyos"),
            (7, 2),
            "a distro kernel release",
        )?;
        check_eq(parse_kernel_version("6.15.0"), (6, 15), "a plain version")?;
        check_eq(
            parse_kernel_version("nonsense"),
            (0, 0),
            "a release with no numbers",
        )
    }

    #[test]
    fn cli_parses_every_command() -> TestResult {
        use clap::CommandFactory;
        // clap's own consistency check over the derived command tree.
        Cli::command().debug_assert();
        Ok(())
    }

    #[test]
    fn a_selector_prefers_an_id_then_a_whole_title() -> TestResult {
        let game = |launcher, key: &str, title: &str| Game {
            id: crate::model::GameId::new(launcher, key),
            also: vec![],
            title: title.into(),
            install_dir: format!("/fixture/{key}").into(),
            build: None,
            size_hint: None,
            state: crate::model::InstallState::Idle,
            is_tool: false,
        };
        let games = || {
            vec![
                game(crate::model::Launcher::Steam, "400", "Portal"),
                game(crate::model::Launcher::Steam, "620", "Portal 2"),
                game(crate::model::Launcher::Manual, "elsewhere", "Area 620"),
            ]
        };
        let titles = |selector: &str| -> Vec<String> {
            select_games(games(), selector)
                .into_iter()
                .map(|g| g.title)
                .collect()
        };
        check_eq(titles("portal"), vec!["Portal".to_owned()], "whole title")?;
        check_eq(titles("620"), vec!["Portal 2".to_owned()], "id over title")?;
        check_eq(titles("steam:620"), vec!["Portal 2".to_owned()], "full id")?;
        check_eq(titles("port").len(), 2, "part of a title still matches")?;
        check(titles("  ").is_empty(), "an empty selector names nothing")
    }

    #[test]
    fn a_unit_path_is_quoted_so_systemd_reads_one_word() -> TestResult {
        check_eq(
            unit_exec(Path::new("/usr/bin/flummox")).ctx("plain")?,
            "\"/usr/bin/flummox\"".to_owned(),
            "a plain path",
        )?;
        check_eq(
            unit_exec(Path::new("/tmp/my build/50%/$HOME/a\"b\\c")).ctx("awkward")?,
            "\"/tmp/my build/50%%/$$HOME/a\\\"b\\\\c\"".to_owned(),
            "spaces stay inside the quotes and specifiers are doubled",
        )?;
        check(
            unit_exec(Path::new("/tmp/a\nExecStartPre=/bin/evil")).is_err(),
            "a newline cannot add a directive",
        )
    }

    #[test]
    fn recorded_levels_apply_only_to_unchanged_files() -> TestResult {
        let mut db = Db::open_in_memory().ctx("database")?;
        let game = Game {
            id: crate::model::GameId::new(crate::model::Launcher::Manual, "fixture"),
            also: vec![],
            title: "Fixture".into(),
            install_dir: "/fixture/game".into(),
            build: None,
            size_hint: None,
            state: crate::model::InstallState::Idle,
            is_tool: false,
        };
        let mut inv = Inventory {
            files: vec![inventory::FileEntry {
                rel: "assets.dat".into(),
                size: 100_000,
                ino: 42,
                mtime_ns: 1,
                ctime_ns: 1,
                action: inventory::Action::Compress,
            }],
            warnings: vec![],
        };
        let record = GameRecord::new(
            game.id.clone(),
            game.title.clone(),
            game.install_dir.clone(),
            crate::fsprobe::BackendKind::Btrfs,
            &CompressOpts::default(),
        );
        db.record_compression(&record, &inv).ctx("previous pass")?;
        check_eq(
            recorded_levels(Some(&db), &game, &inv).len(),
            1,
            "unchanged file reuses its result",
        )?;
        inv.files.first_mut().ctx("fixture entry")?.ctime_ns += 1;
        check_eq(
            recorded_levels(Some(&db), &game, &inv).len(),
            0,
            "patched file is sampled again",
        )
    }

    struct FakeProcs(Arc<std::sync::Mutex<Vec<busy::ProcInfo>>>);

    impl busy::ProcSource for FakeProcs {
        fn processes(&self) -> Vec<busy::ProcInfo> {
            self.0.lock().map(|list| list.clone()).unwrap_or_default()
        }
    }

    fn player(dir: &str) -> busy::ProcInfo {
        busy::ProcInfo {
            pid: 4242,
            name: "game".to_owned(),
            exe: Some(PathBuf::from(dir).join("game.bin")),
            ..busy::ProcInfo::default()
        }
    }

    fn fake_watch(
        procs: Vec<busy::ProcInfo>,
    ) -> (
        busy::BackgroundScan,
        Arc<std::sync::Mutex<Vec<busy::ProcInfo>>>,
    ) {
        let shared = Arc::new(std::sync::Mutex::new(procs));
        let watch = busy::BackgroundScan::start_with(
            PathBuf::from("/games/Portal"),
            std::time::Duration::from_millis(20),
            FakeProcs(Arc::clone(&shared)),
        );
        (watch, shared)
    }

    #[test]
    fn no_pause_stops_the_job_when_the_game_is_launched() -> TestResult {
        use backend::BusyCheck;
        let (watch, _procs) = fake_watch(vec![player("/games/Portal")]);
        let stop = AtomicBool::new(false);
        let guard = LaunchGuard {
            watch: &watch,
            stop: Some(&stop),
        };
        check_eq(
            guard.in_use_by(),
            None,
            "a stopping check never asks the backend to wait",
        )?;
        check(
            stop.load(std::sync::atomic::Ordering::Relaxed),
            "the launch should set the cancel flag",
        )
    }

    #[test]
    fn the_default_check_pauses_instead_of_stopping() -> TestResult {
        use backend::BusyCheck;
        let (watch, _procs) = fake_watch(vec![player("/games/Portal")]);
        let guard = LaunchGuard {
            watch: &watch,
            stop: None,
        };
        check_eq(
            guard.in_use_by(),
            Some("game (pid 4242)".to_owned()),
            "the pause check names who is playing",
        )?;
        let (idle, _procs) = fake_watch(vec![player("/games/Terraria")]);
        let guard = LaunchGuard {
            watch: &idle,
            stop: None,
        };
        check_eq(
            guard.in_use_by(),
            None,
            "another game's process is not a launch",
        )
    }

    #[test]
    fn the_watch_thread_notices_a_launch_after_it_started() -> TestResult {
        let (watch, procs) = fake_watch(Vec::new());
        check_eq(
            watch.latest(),
            busy::Usage::Free,
            "nothing is running at the start",
        )?;
        procs
            .lock()
            .ctx("lock the fake process list")?
            .push(player("/games/Portal"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while watch.latest() == busy::Usage::Free && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        check_eq(
            watch.latest(),
            busy::Usage::InUse("game (pid 4242)".to_owned()),
            "a launch after the first scan should show up in a later one",
        )
    }

    #[test]
    fn a_max_pass_that_met_its_floor_counts_as_finished() -> TestResult {
        let max = Preset::Max.level_plan();
        // The backend reports the lowest level any file got.
        check_eq(
            level_to_record(max, Some(9), None, 15),
            15,
            "Max records its ceiling once the floor was applied",
        )?;
        check(
            9 >= max.floor(),
            "the next run's reuse test compares against the floor",
        )?;
        check_eq(
            level_to_record(max, Some(3), None, 15),
            3,
            "a kernel that refused the level records what it applied",
        )?;
        check_eq(
            level_to_record(Preset::Balanced.level_plan(), Some(9), Some(15), 9),
            15,
            "a higher earlier level stays for the files skipped as unchanged",
        )?;
        check_eq(
            level_to_record(Preset::Fast.level_plan(), None, None, 3),
            3,
            "no level reported falls back to the requested one",
        )
    }

    #[test]
    fn a_cancelled_pass_counts_only_the_share_it_finished() -> TestResult {
        check_eq(
            estimate::scaled_saving(1000, 10, 100),
            100,
            "a tenth of the plan",
        )?;
        check_eq(
            estimate::scaled_saving(1000, 100, 100),
            1000,
            "the whole plan",
        )?;
        check_eq(
            estimate::scaled_saving(1000, 0, 0),
            1000,
            "an empty plan keeps the figure",
        )?;
        check_eq(
            estimate::scaled_saving(u64::MAX, u64::MAX - 1, u64::MAX),
            u64::MAX - 1,
            "large figures do not overflow",
        )
    }

    #[test]
    fn levels_and_threads_are_checked_at_the_parser() -> TestResult {
        use clap::Parser;
        let parse = |args: &[&str]| {
            let mut full = vec!["flummox", "compress", "game"];
            full.extend_from_slice(args);
            Cli::try_parse_from(full).is_ok()
        };
        check(parse(&["--level=15"]), "15 is the top of the range")?;
        check(parse(&["--level=-15"]), "-15 is the bottom of the range")?;
        check(!parse(&["--level=99"]), "99 is out of range")?;
        check(!parse(&["--level=-16"]), "-16 is out of range")?;
        check(
            !parse(&["--level=0"]),
            "0 reads as 'not compressed' in the database",
        )?;
        check(parse(&["--threads=32"]), "32 threads is the most allowed")?;
        check(!parse(&["--threads=0"]), "0 threads cannot work")?;
        check(!parse(&["--threads=33"]), "33 threads is over the limit")
    }

    #[test]
    fn direct_paths_refuse_what_the_coordinator_refuses() -> TestResult {
        let opts = |threads, level| CompressOpts {
            threads,
            level,
            ..CompressOpts::default()
        };
        check(
            check_job_opts(&opts(2, Some(9))).is_ok(),
            "ordinary settings",
        )?;
        check(check_job_opts(&opts(0, None)).is_err(), "no threads")?;
        check(check_job_opts(&opts(2, Some(99))).is_err(), "level 99")?;
        check(check_job_opts(&opts(2, Some(0))).is_err(), "level 0")
    }

    #[test]
    fn the_written_unit_is_as_hardened_as_the_packaged_one() -> TestResult {
        let packaged = include_str!("../../packaging/flummox-watch.service");
        let unit = unit_text(
            Path::new("/usr/bin/flummox"),
            &LevelArgs {
                preset: PresetArg::Max,
                level: None,
            },
            8,
            false,
        )
        .ctx("render the unit")?;
        let mut in_service = false;
        let mut directives = 0;
        for line in packaged.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_service = line == "[Service]";
                continue;
            }
            let hardening =
                line.contains('=') && !line.starts_with('#') && !line.starts_with("ExecStart=");
            if in_service && hardening {
                directives += 1;
                check(
                    unit.lines().any(|written| written == line),
                    format!("the written unit lacks {line}"),
                )?;
            }
        }
        check(directives >= 8, "the packaged unit was read")?;
        check(
            unit.lines()
                .any(|l| l == "ExecStart=\"/usr/bin/flummox\" watch --preset max --threads 8"),
            "the watch flags reach ExecStart",
        )
    }

    #[test]
    fn the_unit_carries_level_and_dry_run() -> TestResult {
        let unit = unit_text(
            Path::new("/usr/bin/flummox"),
            &LevelArgs {
                preset: PresetArg::Fast,
                level: Some(7),
            },
            2,
            true,
        )
        .ctx("render the unit")?;
        check(
            unit.lines().any(|l| {
                l == "ExecStart=\"/usr/bin/flummox\" watch --preset fast --level 7 --threads 2 --dry-run"
            }),
            "level and dry run are written in",
        )
    }

    #[test]
    fn exclude_remove_needs_one_clear_match() -> TestResult {
        use crate::model::{GameId, Launcher};
        let hidden = vec![
            (GameId::new(Launcher::Steam, "400"), "Portal".to_owned()),
            (GameId::new(Launcher::Steam, "620"), "Portal 2".to_owned()),
        ];
        let title = |selector: &str| pick_hidden(&hidden, selector).map(|(_, t)| t.clone());
        check_eq(
            title("portal").ctx("whole title")?,
            "Portal".to_owned(),
            "whole title",
        )?;
        check_eq(title("620").ctx("id")?, "Portal 2".to_owned(), "id")?;
        check_eq(
            title("steam:400").ctx("full id")?,
            "Portal".to_owned(),
            "full id",
        )?;
        check_eq(
            title("2").ctx("partial")?,
            "Portal 2".to_owned(),
            "one partial match",
        )?;
        check(
            title("port").is_err(),
            "a partial title with two matches is refused",
        )?;
        check(title("").is_err(), "an empty selector names nothing")?;
        check(title("  ").is_err(), "a blank selector names nothing")?;
        check(title("zelda").is_err(), "no match is an error")
    }

    #[test]
    fn bcachefs_is_not_offered_without_a_backend() -> TestResult {
        use crate::fsprobe::BackendKind;
        check(
            native_supported(&Tier::Native(BackendKind::Btrfs)),
            "btrfs has a backend",
        )?;
        check(
            !native_supported(&Tier::Native(BackendKind::Bcachefs)),
            "bcachefs has none",
        )?;
        check(
            !native_supported(&Tier::Pack),
            "pack is not an in-place tier",
        )
    }

    #[test]
    fn an_oversized_report_is_refused_before_it_is_parsed() -> TestResult {
        let tmp = tempfile::tempdir().ctx("temporary directory")?;
        let small = tmp.path().join("small.json");
        std::fs::write(&small, b"{}").ctx("write the small report")?;
        check_eq(
            read_report(&small).ctx("read the small report")?,
            b"{}".to_vec(),
            "a small file is read whole",
        )?;
        let big = tmp.path().join("big.json");
        std::fs::write(&big, vec![b' '; (MAX_REPORT_BYTES + 1) as usize])
            .ctx("write the big report")?;
        check(read_report(&big).is_err(), "over 1 MiB is refused")
    }

    #[test]
    fn typed_folders_are_made_absolute_for_the_coordinator() -> TestResult {
        let home = Path::new("/home/someone");
        let relative = client_path("games/Portal", home).ctx("relative")?;
        check(
            relative.is_absolute(),
            "a relative path gains its directory",
        )?;
        check(relative.ends_with("games/Portal"), "and keeps its tail")?;
        check_eq(
            client_path("~/games", home).ctx("home")?,
            PathBuf::from("/home/someone/games"),
            "a tilde still expands",
        )
    }
}
