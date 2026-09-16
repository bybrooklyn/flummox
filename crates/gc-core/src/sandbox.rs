//! Locking the process down to the game it is working on, using Landlock.
//!
//! A compression job needs exactly two things: the install directory it is
//! rewriting, and the handful of system paths any Rust program reads. It has
//! no business touching the rest of the user's home directory — so before the
//! job starts we ask the kernel to make that impossible, for this process,
//! for the rest of its life.
//!
//! [Landlock](https://landlock.io) is the right tool because it needs no
//! privileges and no setup: an ordinary user process can drop its own ambient
//! filesystem rights. If a bug in this tool, or in a crate it depends on,
//! ever tried to read `~/.ssh` or write outside a game, the kernel refuses it
//! rather than trusting our own path checks (which [`crate::safeio`] still
//! applies as the first line of defence).
//!
//! Two properties worth knowing:
//! - **Regular-file ioctls stay allowed.** The btrfs backend drives
//!   `BTRFS_IOC_DEFRAG_RANGE` on ordinary files, and Landlock's ioctl control
//!   (ABI v5) governs *device* files only, so sandboxing does not interfere.
//! - **Restriction is inherited, not retroactive.** It applies to the calling
//!   thread and anything it starts afterwards, so it must be applied before
//!   the worker pool is built — never in the middle of a job.
//!
//! Enforcement is best-effort by design: on a kernel with an older Landlock
//! ABI the kernel grants what it can and reports [`SandboxStatus::Partial`],
//! and where the feature is missing entirely the job still runs, unsandboxed
//! but honest about it. Compression is not a security boundary the user chose
//! to rely on, so failing to sandbox must never fail the job.

use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, path_beneath_rules,
};

/// The Landlock ABI whose access rights we ask for.
///
/// v5 covers file truncation (v3) and device-ioctl control (v5). Asking for a
/// newer ABI than the kernel has is safe: best-effort compatibility drops
/// what it cannot honour and downgrades the reported status.
const TARGET_ABI: ABI = ABI::V5;

/// How much of the sandbox the kernel actually applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    /// Every requested restriction is in force.
    Enforced,
    /// The kernel applied what it supports, which is less than we asked for.
    Partial,
    /// No sandbox: the kernel lacks Landlock, or building the ruleset failed.
    Unavailable(String),
}

impl SandboxStatus {
    /// Whether the kernel is enforcing anything at all.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Enforced | Self::Partial)
    }

    /// A line suitable for the activity log or `doctor`.
    pub fn describe(&self) -> String {
        match self {
            Self::Enforced => "sandboxed: this process can only reach the game folder".to_owned(),
            Self::Partial => {
                "partly sandboxed: this kernel's Landlock is older than we asked for".to_owned()
            }
            Self::Unavailable(why) => format!("not sandboxed ({why})"),
        }
    }
}

/// Which paths a job is allowed to reach.
#[derive(Debug, Clone, Default)]
pub struct SandboxPlan {
    /// Directories the job may read and write: the games, and the state
    /// database's directory.
    pub writable: Vec<PathBuf>,
    /// Directories the job may only read.
    pub readable: Vec<PathBuf>,
}

impl SandboxPlan {
    /// Builds a plan from explicit path lists, dropping any that do not exist.
    ///
    /// Landlock rejects a rule naming a missing path, and a Steam library on
    /// an unplugged drive is an ordinary situation rather than an error.
    pub fn for_paths(writable: Vec<PathBuf>, readable: Vec<PathBuf>) -> Self {
        let keep = |paths: Vec<PathBuf>| -> Vec<PathBuf> {
            paths.into_iter().filter(|p| p.exists()).collect()
        };
        Self { writable: keep(writable), readable: keep(readable) }
    }

    /// The plan for a compression job on one game.
    ///
    /// `state_dir` is where the database lives, if the caller will write to
    /// it while sandboxed. The read-only set covers the shared libraries,
    /// locale data and `/proc` entries that a running Rust program touches;
    /// without them the process would trip over its own runtime rather than
    /// over anything it was trying to protect.
    pub fn for_job(install_dir: &Path, state_dir: Option<&Path>) -> Self {
        let mut writable = vec![install_dir.to_path_buf()];
        writable.extend(state_dir.map(Path::to_path_buf));
        let readable = ["/usr", "/etc", "/proc", "/sys", "/lib", "/lib64", "/dev/urandom"]
            .iter()
            .map(PathBuf::from)
            .collect();
        Self::for_paths(writable, readable)
    }
}

/// Restricts this process to the paths in `plan`, permanently.
///
/// Never fails: a kernel without Landlock returns
/// [`SandboxStatus::Unavailable`] and the caller carries on. Call this before
/// starting worker threads, and only once the tool has finished discovering
/// games — scanning reads Steam's configuration, which lives outside every
/// game folder.
pub fn restrict(plan: &SandboxPlan) -> SandboxStatus {
    if plan.writable.is_empty() {
        return SandboxStatus::Unavailable("no writable paths given".to_owned());
    }
    let result = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(TARGET_ABI))
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(&plan.readable, AccessFs::from_read(TARGET_ABI))))
        .and_then(|r| r.add_rules(path_beneath_rules(&plan.writable, AccessFs::from_all(TARGET_ABI))))
        .and_then(|r| r.restrict_self());

    match result {
        Ok(status) => match status.ruleset {
            RulesetStatus::FullyEnforced => SandboxStatus::Enforced,
            RulesetStatus::PartiallyEnforced => SandboxStatus::Partial,
            RulesetStatus::NotEnforced => {
                SandboxStatus::Unavailable("this kernel does not support Landlock".to_owned())
            }
        },
        Err(e) => SandboxStatus::Unavailable(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use gc_testutil::{Ctx, TestResult, check};

    use super::*;

    /// Runs `body` in a forked child and reports whether it succeeded.
    ///
    /// Landlock is irreversible and applies to the whole process, so
    /// enforcing it directly in a test would silently restrict every test
    /// that ran afterwards in the same binary. A child process is the only
    /// honest way to test the real thing.
    fn in_forked_child(body: impl FnOnce() -> bool) -> Result<bool, String> {
        // SAFETY: fork() takes no arguments and returns a pid; the child
        // below only opens files and calls _exit, and never returns into the
        // test harness.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err("fork failed".to_owned());
        }
        if pid == 0 {
            let code = if body() { 0 } else { 1 };
            // SAFETY: _exit() ends the child immediately without running the
            // parent's atexit handlers, which is what a forked child must do.
            unsafe { libc::_exit(code) };
        }
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a live, correctly typed local for the call.
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited < 0 {
            return Err("waitpid failed".to_owned());
        }
        Ok(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0)
    }

    #[test]
    fn a_plan_drops_paths_that_do_not_exist() -> TestResult {
        let tmp = tempfile::tempdir().ctx("temporary directory")?;
        let plan = SandboxPlan::for_paths(
            vec![tmp.path().to_path_buf(), PathBuf::from("/nonexistent-game-dir")],
            vec![PathBuf::from("/nonexistent-system-dir")],
        );
        check(plan.writable.len() == 1, "the missing writable path should be dropped")?;
        check(plan.readable.is_empty(), "the missing readable path should be dropped")
    }

    #[test]
    fn a_job_plan_covers_the_game_and_the_state_directory() -> TestResult {
        let tmp = tempfile::tempdir().ctx("temporary directory")?;
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).ctx("create the state directory")?;
        let plan = SandboxPlan::for_job(tmp.path(), Some(&state));
        check(plan.writable.contains(&tmp.path().to_path_buf()), "the game must be writable")?;
        check(plan.writable.contains(&state), "the state directory must be writable")?;
        // /usr exists on any Linux system this runs on; its presence shows the
        // read-only set is actually populated.
        check(plan.readable.contains(&PathBuf::from("/usr")), "system paths must be readable")
    }

    #[test]
    fn refuses_to_sandbox_with_nothing_writable() -> TestResult {
        let status = restrict(&SandboxPlan::default());
        check(!status.is_active(), "an empty plan must not be treated as enforced")
    }

    #[test]
    fn enforcement_blocks_paths_outside_the_game() -> TestResult {
        let tmp = tempfile::tempdir().ctx("temporary directory")?;
        let game = tmp.path().join("game");
        std::fs::create_dir(&game).ctx("create the game directory")?;
        std::fs::write(game.join("inside.dat"), b"game data").ctx("write a game file")?;
        let outside = tmp.path().join("secret.dat");
        std::fs::write(&outside, b"not the game's business").ctx("write the outside file")?;

        let inside = game.join("inside.dat");
        let succeeded = in_forked_child(move || {
            let plan = SandboxPlan::for_paths(vec![game], Vec::new());
            let status = restrict(&plan);
            if !status.is_active() {
                // No Landlock on this kernel: there is nothing to prove, and
                // failing here would just punish older systems.
                return true;
            }
            let inside_readable = std::fs::File::open(&inside).is_ok();
            let outside_refused = std::fs::File::open(&outside).is_err();
            inside_readable && outside_refused
        })?;
        check(
            succeeded,
            "inside the sandbox the game must stay readable while everything else is refused",
        )
    }
}
