# Read-only bug audit: macOS backend and command line

Nothing was edited, built or run. All findings come from reading the code. macOS findings that depend on how `codesign`, `ditto` or the kernel behave are marked PLAUSIBLE because they cannot be exercised on this machine.

## Part A: macOS (`src/macos.rs`)

1. **`macos.rs:573-577` (with 472-475). Post-swap signature check runs while the staging directory is still inside the bundle.**
   - Defect: `verify_bundle` runs `codesign --verify --deep --strict` before `clear_record` removes `.flummox-work-*/candidate`, which sits beside the file and therefore inside the `.app`.
   - Scenario: `flummox compress <game>` on any signed `.app`. The first eligible file is swapped, then codesign reports an unsealed added file and `stage` returns "Application signature verification failed". The journal is retained, the pass aborts, and every later pass refuses with "Review Recovery". Recovery swaps the file back, so a signed bundle can never be compressed.
   - Fix: stage outside the bundle (a sibling of the `.app` on the same volume), or move the retained original out of the bundle before verifying.
   - Severity: high. PLAUSIBLE (ordering confirmed; codesign's rejection of added files not run here).

2. **`macos.rs:469` and `574-576`. Whole-bundle signature verification runs twice per file.**
   - Defect: `signed_bundle(source)` and `verify_bundle` each hash the entire bundle, and both are called for every file staged.
   - Scenario: a 30 GB signed `.app` with 20,000 eligible files triggers about 40,000 full-bundle verifications, so the pass effectively never finishes. A bundle whose signature is already invalid (common after launcher patching) aborts the whole pass at its first file.
   - Fix: verify each bundle once before its first file and once after its last. Treat an already-invalid signature as "unsigned, skip verification".
   - Severity: high. CONFIRMED (call sites); cost is arithmetic.

3. **`macos.rs:407-408` and `426-443`. A crash between deleting the work directory and deleting the journal leaves an unrecoverable journal.**
   - Defect: `clear_record` removes the staging directory first and the journal second, with no fsync of the recovery directory. `recover_original` has no branch for "source holds the candidate, staged is gone".
   - Scenario: power loss or kill after `remove_dir_all`, before the journal unlink is durable. On the next run `identity(&record.staged)?` fails with ENOENT on every attempt, so `recover` always errors and `visit` refuses the game ("Review Recovery before processing this game") permanently. Neither the CLI nor `gui/native.rs:956` can discard a journal.
   - Fix: in `recover_original`, when source matches `candidate`, its hash matches `record.hash` and `staged` is absent, delete the journal and return Ok.
   - Severity: medium. CONFIRMED.

4. **`macos.rs:560-561`, `611-620`, `781-788`. Orphaned staging copies are never cleaned up.**
   - Defect: `temporary.keep()` runs before `save(&record)?`, so a failed journal write leaves the work directory with no journal. The macOS CLI passes `AtomicBool::new(false)` and installs no signal handler, so Ctrl-C kills the process mid-`ditto`. `visit` skips `.flummox-work-*` directories and nothing sweeps them.
   - Scenario: Ctrl-C during `flummox compress` on a 20 GB file leaves a partial `candidate` inside the game folder indefinitely. If it is inside a signed bundle, `signed_bundle` then fails on every later pass.
   - Fix: call `keep()` only after `save` succeeds. At pass start, remove work directories under the root that have no journal. Install a SIGINT flag for the CLI.
   - Severity: medium. CONFIRMED.

5. **`macos.rs:300-312` and `327-330`. Exact xattr equality includes kernel-managed attributes.**
   - Defect: `attributes()` excludes only `com.apple.decmpfs` and `com.apple.ResourceFork`, then requires byte-equal maps.
   - Scenario: on macOS 13 and later, files written by a provenance-tracked app (a quarantined cask install) get a kernel-set `com.apple.provenance` value that differs from the one on launcher-written files. The first file fails "File attributes changed during staging" and aborts the pass. The existing test creates both files from one process, so it would not see this.
   - Fix: exclude kernel-managed names (`com.apple.provenance` at least) from the comparison.
   - Severity: medium. PLAUSIBLE.

6. **`macos.rs:608` and `634`. Busy check runs once per pass, and any per-file error aborts the pass.**
   - Defect: `idle(&root)` is called only at pass start. `stage(...)?` propagates every per-file failure.
   - Scenario A: a launcher update starts ten minutes into a pass. A file being patched in place can be swapped between the pre-swap identity check (538) and the swap (562). Later writes through the updater's descriptor land in the retained original, which `clear_record` deletes.
   - Scenario B: one file that vanished, is unreadable, or that `ditto` rejects ends the whole game's pass, with `skipped` unused for it.
   - Fix: re-run a cheap busy check periodically, or open the source and compare fstat after the swap. Count per-file failures as skipped with a reason and continue.
   - Severity: medium. CONFIRMED (absence of recheck); the data-loss window is PLAUSIBLE.

7. **`macos.rs:790-796`. `flummox recover` does not take `native.lock`.**
   - Defect: the code comment states it: `// Same loop as recover_folder, without taking native.lock.`
   - Scenario: `flummox recover <game>` while the app is compressing that game picks up the in-flight journal. If `lsof` sees no open file at that instant, it swaps back and deletes the work directory underneath `stage`. Both sides then fail with identity or ENOENT errors. Bytes are identical in both copies, so no data is lost.
   - Fix: call `recover_folder(&folder)`.
   - Severity: low to medium. CONFIRMED.

8. **`macos.rs:233-257`. `validate` accepts volume roots and folders that contain Flummox's state.**
   - Defect: `root.parent().is_some()` rejects only `/`. The protected list blocks roots inside a protected path, not roots containing one.
   - Scenario: `flummox compress /Volumes/Games` passes "Choose a game folder, not a drive" and rewrites the whole external volume. `~/Library` or `~/Library/Application Support` is accepted and the walk includes `flummox/` state and the launcher's own install. The Linux `validate_folder` has the `state.starts_with(&path)` check that this lacks.
   - Fix: refuse `root == volume.path` and any root that is an ancestor of `data_dir()`.
   - Severity: low to medium. CONFIRMED.

9. **`macos.rs:437-443`. Recovery verifies the original's hash only after swapping it back.**
   - Defect: on a hash mismatch the files are left in the "never swapped" layout with the journal intact.
   - Scenario: a second `recover` takes the first branch (426-428) and `clear_record` deletes the verified candidate, keeping the original that just failed verification.
   - Fix: hash `record.staged` before swapping back.
   - Severity: low. CONFIRMED (needs content change without an mtime or size change to trigger).

10. **`macos.rs:609` (`storage.rs:347-364`). Compressing requires free space equal to the whole install.**
    - Defect: `native_plan` demands install bytes plus largest file plus 5 percent, although staging holds one file at a time and line 463 already checks 2x per file.
    - Scenario: a 100 GB game on a drive with 60 GB free is refused.
    - Fix: for macOS, plan on largest file times two plus headroom, or say why snapshots justify the larger bound.
    - Severity: low. CONFIRMED.

11. **`macos.rs:757-765`. The macOS CLI does not match `docs/usage.md`.**
    - Defect: commands take a folder only. `flummox compress 105600 --preset max`, `status`, `log` and `doctor` from the docs do not exist. A selector fails as a bare canonicalize error. `compress` and `decompress` pass `|_| {}` as the reporter, so a long pass prints nothing until the end.
    - Fix: mark the docs section as Linux, or add selector lookup through `discover()`. Print progress to stderr.
    - Severity: low. CONFIRMED.

12. **`macos.rs:508`. SAFETY comment claims an invariant the call does not need.**
    - `// SAFETY: stage_c names our private staging file; compression metadata was removed.` Memory safety of `chflags` needs only the terminated string (CLAUDE.md code rule 3).
    - Severity: low. CONFIRMED.

## Part B: command line

1. **`src/cli/mod.rs:1338`. `decompress --force` runs unsandboxed at default verbosity.**
   - Defect: `tracing::info!(status = %sandbox::restrict(&plan).describe(), "sandbox");` puts the Landlock call inside an event field. tracing evaluates fields only when the callsite is enabled (`tracing-0.1.44/src/macros.rs:680-697`), and the default filter is `warn` (line 440).
   - Scenario: `flummox decompress X --force` never calls `restrict`. With `-v` it does.
   - Fix: `let status = sandbox::restrict(&plan);` on its own line, as `cmd_compress` does at 1187, and warn when inactive.
   - Severity: high. CONFIRMED.

2. **`src/cli/mod.rs:1202` (`src/backend/mod.rs:194,229-231`). `--no-pause` neither pauses nor stops.**
   - Defect: `busy: (!no_pause).then_some(...)` passes `None`, which the backend documents as "`None` means never pause". Nothing else watches for the game.
   - Scenario: `flummox compress X --no-pause`, then launch the game. Help says "Stop if the game is launched" and docs say "Pass `--no-pause` if you would rather it stop", but the job keeps rewriting files under the running game.
   - Fix: pass a `BusyCheck` that sets `cancel` when the game is in use.
   - Severity: high. CONFIRMED.

3. **`src/cli/mod.rs:1347-1366`. `decompress --force` ignores `outcome.cancelled` and `outcome.errors`.**
   - Scenario: Ctrl-C a third of the way through, or files failing with EIO. Output is "Done: N files rewritten", exit 0, and `open.forget(&game.id)` erases the record of a game that is still mostly compressed. The worker forgets only when `!outcome.cancelled && outcome.errors.is_empty()` (`src/jobs/worker.rs:485`).
   - Fix: print errors and the cancellation, skip `forget`, and return Err.
   - Severity: medium. CONFIRMED.

4. **`src/cli/mod.rs:1209-1244,1308`. Direct `compress` exits 0 after partial failure or cancellation.**
   - Scenario: `flummox compress X --force` where 200 files fail prints "200 files failed" on stdout and returns `Ok(())`. The queued path fails on `Phase::Partial` (1434-1440), so the same job gives different exit codes depending on flags.
   - Fix: after recording, `bail!` when `!outcome.errors.is_empty()` or `outcome.cancelled`. Send the failure list to stderr.
   - Severity: medium. CONFIRMED.

5. **`src/cli/mod.rs:1069-1076,1322`. Direct paths skip checks the coordinator and worker enforce.**
   - Missing on `compress --force`, `compress --no-pause` and `decompress --force`:
     - the space plan (`native_plan(...).recheck()`, `worker.rs:101-105`);
     - thread and level range checks (`service.rs:411-420`);
     - the `anchor.fully_resolved()` refusal (`worker.rs:222-226`);
     - `deny_sockets` (`worker.rs:159`).
   - Scenario: `flummox decompress X --force` on a nearly full drive starts expanding with no free-space check and fails partway with ENOSPC. Finding 3 then reports "Done".
   - Fix: share one preflight function between the worker and the direct paths.
   - Severity: medium. CONFIRMED.

6. **`src/pack/cli.rs:316-320,341,352,366,388` and `src/cli/mod.rs:349,360`. Relative paths are sent to the coordinator unresolved.**
   - Defect: the coordinator canonicalizes them against its own working directory (`src/jobs/packs.rs:70,146,206`, `src/jobs/service.rs:681,690`), which is whatever the first client had. `folder_path` (`src/jobs/mod.rs:532`) returns relative input unchanged.
   - Scenario: `cd /mnt/games && flummox pack reclaim Portal` or `flummox jobs add-folder .` fails with "Finding the launcher path", or acts on a same-named folder under the coordinator's directory. `reclaim` and `prune` delete data.
   - Fix: apply `std::path::absolute` in the client before sending, and have the coordinator refuse non-absolute paths.
   - Severity: medium. CONFIRMED.

7. **`src/cli/mod.rs:1270-1283`. `--preset max` on the direct path prints a false kernel note and defeats incremental passes.**
   - Defect: `effective_level` is the minimum level over all files (`src/backend/btrfs.rs:420`). The per-file plan for `max` uses 9 or 15, but the CLI compares against `opts.btrfs_level()` (15). The worker compares against `level_plan().floor()` (`worker.rs:445`).
   - Scenario: `flummox compress X --preset max --force` prints "this kernel does not accept a compression level" on a current kernel and records level 9. The next `--preset max` run sees `prev.level >= 15` false (1095) and rewrites the whole install.
   - Fix: compare against `opts.level_plan().floor()` and record the plan's ceiling when the floor was met.
   - Severity: medium. CONFIRMED.

8. **`src/cli/mod.rs:464-471`. Ctrl-C does nothing in commands that never read the flag.**
   - Defect: the handler only sets a flag, for every subcommand. `plan`, `compatibility measure`, `scan`, `doctor`, `drives`, the FIEMAP half of `status` (`btrfs.rs:560`), and the pack coordinator requests do not poll it. Those requests block on a 7,200 second read timeout (`src/jobs/client.rs:111`).
   - Scenario: `flummox pack compact <game>` cannot be interrupted from the terminal for up to two hours.
   - Fix: use `signal_hook::flag::register_conditional_shutdown` so a second Ctrl-C exits, or install the flag only for commands that poll it.
   - Related docs conflict: `docs/usage.md:134` says "Ctrl-C stops between files", but on the default queued path Ctrl-C only disconnects and the job continues (`mod.rs:1413-1415`, and `usage.md:78`).
   - Severity: medium. CONFIRMED.

9. **`src/cli/mod.rs:1789` (`src/watch.rs` `settled.insert`). A failed watch attempt is never retried.**
   - Defect: `settled` is set to true before `on_ready` runs. `cmd_compress` bails in `check_idle` when any process has a file open in the game.
   - Scenario: the user presses Play as the download finishes. The watcher prints "warning: could not compress" and does not try again until the manifest next changes.
   - Fix: enqueue regardless of busy state (the coordinator already pauses during play), or clear the settled entry on failure.
   - Severity: medium. CONFIRMED.

10. **`src/cli/mod.rs:1763-1766,1696`. `watch enable` accepts and discards its flags.**
    - Scenario: `flummox watch --preset max --threads 8 enable` writes a unit with `ExecStart=... watch`, so the service runs balanced with 2 threads. `--dry-run enable` installs a service that really compresses.
    - Fix: write the flags into `ExecStart`, or reject them alongside a subcommand.
    - Severity: low to medium. CONFIRMED.

11. **`src/cli/mod.rs:40-41,326-327,1145-1147`. `--json` is global but several read-only commands ignore it.**
    - Scenario: `flummox drives --json`, `doctor --json`, `watch status --json` and `compress --dry-run --json` print text and exit 0. `docs/usage.md:127` says "Every read-only command takes `--json`".
    - Fix: route them through `Output::emit`, or reject the flag there.
    - Severity: low to medium. CONFIRMED.

12. **`src/cli/mod.rs:1822-1859` and `766`. `hook on` reports success regardless, and bcachefs is listed as supported.**
    - Defect: the "New downloads and patches in these libraries will be compressed" paragraph prints unconditionally and the command exits 0, even when every library is unsupported or every `set_dir_property` failed.
    - Defect: `Tier::Native(Bcachefs)` makes `scan` report `supported: true` and `drives` say "bcachefs compression, in place", while `backend::for_kind` returns `None` (`src/backend/mod.rs:421`). `compress` then fails, and `hook on` attempts the btrfs xattr on bcachefs.
    - Fix: fail when no library took the property. Mark support from `for_kind(kind).is_some()`.
    - Severity: low to medium. CONFIRMED.

13. **`src/cli/mod.rs:237-239`. `--level` is unvalidated on `estimate`, `--dry-run` and the direct paths.**
    - Scenario: `flummox compress X --level 99 --force` runs at 15 (`btrfs.rs:122` clamps) but `compress_range` returns the unclamped value (`btrfs.rs:130`), so the database records 99. `estimate --level 99` models a zstd level btrfs cannot apply.
    - Fix: add `value_parser = clap::value_parser!(i32).range(-15..=15)`.
    - Severity: low. CONFIRMED.

14. **`src/cli/mod.rs:1905-1911`. `exclude remove` takes the first substring match with no ambiguity check.**
    - Scenario: `flummox exclude remove port` with "Portal" and "Portal 2" hidden unhides whichever is listed first. `exclude remove ""` unhides the first entry.
    - Fix: reuse the id, whole-title, then unique-partial rule from `select_games`, and reject empty input.
    - Severity: low. CONFIRMED.

15. **`src/jobs/mod.rs:467`. `entrypoint` uses `std::env::args()`, which panics on non-Unicode arguments.**
    - Scenario: a non-UTF-8 first argument or install path panics before clap can report it. Both binaries call this.
    - Fix: use `args_os().nth(1)` and compare as `OsStr`.
    - Severity: low. CONFIRMED.

16. **`src/lib.rs:58-61`. The crate comment misstates where `unsafe` is allowed.**
    - It says "Three modules need `unsafe`: the btrfs ioctls, the anchored opens, and the Landlock call." Eleven files carry `#![allow(unsafe_code)]`, including `storage.rs`, `busy.rs`, `fsprobe.rs`, `macos.rs` and five under `windows/`. `sandbox.rs` is not among them.
    - Severity: low. CONFIRMED.

## Read and found sound

- **`macos.rs`**
  - Journal write is durable and precedes the swap.
  - `renameatx_np(RENAME_SWAP)` is anchored on `O_NOFOLLOW` directory descriptors.
  - The FFI SAFETY comments at 154, 167, 186 and 493 are accurate.
  - Symlinks and hard links are skipped.
  - `recover_original` checks root, volume and identity.
  - The restore path cannot clone (the `std::io::copy` read/write loop decompresses), and it strips decmpfs attributes and the flag.
  - Casts in `storage.rs` `volume_existing` are safe.
- **`src/bin/flummox.rs`, `src/bin/flummox-gui.rs`**: exit codes come from `main` returning `Result`; there is no `process::exit`.
- **`cli/mod.rs`**
  - `select_games` precedence is correct.
  - `unit_exec` quoting is correct.
  - `--json` output of `scan`, `estimate`, `status`, `log` and `hook` stays on stdout, with progress on stderr.
  - No clap `env` attributes are used.
  - Queued-job id matching works, because scan and coordinator both canonicalize.
  - The direct path takes `operation_lock`, and `compress` applies the sandbox before worker threads start.
- **`jobs/client.rs`**: timeouts and error propagation are sound.
- **`benchmark::run`**: the budget is range-checked before multiplication.
- **`compatibility::Report`**: carries no path fields.
- **`pack/cli.rs`**: `--maximum` and `--level` conflict handling is correct.
