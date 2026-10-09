Read-only audit of the estimate, classify, recommendation, benchmark, compatibility, qualification and db modules. Nothing was edited or built; every finding comes from reading code, none was reproduced by running it.

## Findings, most severe first

**1. GUI analysis samples only the first and last 512 KiB of every file over 1 MiB.** High. CONFIRMED.
- `src/jobs/worker.rs:286-287,326` sets `byte_cap = budget.min(1 MiB)`.
- `src/estimate.rs:484-494` then gives `window = 512 KiB`, `samples = 2`, and `sample_offset` (835-842) returns `0` and `size - window` for two samples.
- Scenario: a 20 GB `.pak` with a compressible header and trailing index but an already-compressed body. Two windows show about 50% saving, scaled by `size / sampled_in` to about 10 GB. The inverse also happens: an incompressible head and tail score zero.
- The same result decides what a Compress job rewrites: only `native.worthwhile()` files are pushed to `candidates` (worker.rs:372-374), so a misjudged file is never compressed.
- At most about 28-32 files are sampled per game (32 MiB budget, 1 MiB each); the rest are scaled from them.
- `paired_estimate_reads_once_and_scores_both_backends` uses uniform data, so it cannot see this.
- Fix: use more, smaller windows spread across the file (as `sample_block` does for the CLI path), and add a test with a compressible head and tail around a noise body.

**2. The 5% relative threshold is measured against only the files that shrink.** Medium. CONFIRMED.
- `src/recommendation.rs:88-93` compares `saving` to `current = estimate.disk_now`.
- `disk_now` only accumulates worthwhile files: `estimate.rs:742-749` (`if !worthwhile { continue }`) and `worker.rs:357-358`.
- Scenario: a 50 GB game with 49 GB of video and 1 GB of DLLs saving 30%. `disk_now` is about 1 GB, the ratio reads 30%, and `clears_threshold` passes. Against the game it is 0.6%.
- This feeds `State::prospect` (`src/gui/app.rs:601`), the Worth order, `maximum_advantage_bps`, and `saving_ratio()`.
- The recommendation tests set `disk_now` to 10 GiB by hand, a value the real estimator would not produce for such a game.
- Fix: compare against `install_bytes`, or the current usage of all eligible files.

**3. CLI Max preset never takes the incremental path.** Medium. CONFIRMED.
- Max is `LevelPlan::PerFile { low: 9, high: 15 }` (`src/backend/mod.rs:59`).
- The pass reports the minimum level applied (`src/backend/btrfs.rs:421,460`), which is 9 once any file keeps the cheaper level.
- `src/cli/mod.rs:1270` stores that as `record.level`; line 1095 then tests `prev.level >= opts.btrfs_level()`, which is `9 >= 15`.
- Scenario: `compress --preset max --force` (or `--no-pause`) twice in a row rewrites the whole install both times.
- The worker compares against `level_plan().floor()` (`worker.rs:128`) and is sound. `LevelPlan::ceiling()` is documented for this purpose and has no callers.
- Fix: compare `prev.level >= opts.level_plan().floor()`.

**4. CLI estimate keeps advertising savings after a Max pass, and prices compressed data at the wrong level.** Medium. CONFIRMED.
- `src/estimate.rs:677-680` skips a file only if `applied >= opts.level`. Files that `choose_level` kept at 9 are recorded as 9, and the Max estimate targets 15, so they are resampled.
- Line 431 prices the already-compressed share at `opts.mount_level.unwrap_or(3)`, ignoring the recorded level.
- Scenario: after a Max pass, `flummox estimate --preset max` reports the level-3 to level-15 gap for every level-9 file. A rerun says "Nothing to do".
- The same mispricing applies after a Balanced pass when estimating Max: the 9-to-15 gain is priced as 3-to-15.
- Fix: treat a file as attempted when its recorded level is at or above the plan's floor, and use the recorded level in `mount_cost` when one exists.

**5. CLI estimate depends on rayon scheduling once the 512 MiB budget binds.** Medium. CONFIRMED by reading, not reproduced.
- `src/estimate.rs:643-661` sorts largest first, then runs `par_iter` with a shared budget.
- rayon splits the slice into contiguous ranges, so only one worker starts at the largest files. The others spend budget on small files further down the list.
- Scenario: a game with thousands of eligible files gives different estimates run to run, and multi-GB files can end up unsampled and scaled from small ones. This contradicts the comment at 641-642.
- The existing test pins one thread to get a stable answer.
- Fix: choose the sampled set sequentially (accumulate `min(size, cap)` until the budget), then `par_iter` over that set only.

**6. Compatibility reports for custom folders contain the absolute install path.** Medium. CONFIRMED.
- `src/launchers/desktop.rs:253`: `let key = path.to_string_lossy().into_owned();` for `Launcher::Manual`.
- `src/qualification.rs:149` copies `game.id.key` into the report.
- Scenario: qualifying `~/Games/Foo` saves JSON with `"key": "~/Games/Foo"`. `docs/gui-architecture.md:38` says the fields cannot hold paths or user names.
- `GameBuild.key`, `build` and `flummox_version` are unconstrained strings, and `validate` (`src/compatibility.rs:161-196`) checks only non-emptiness, so an imported report can carry anything too.
- Fix: key Manual games by a hash, as `desktop.rs::manual_game` already does on other platforms, and have `validate` reject keys or builds containing `/`, `\`, or over a length cap.

**7. `est_saving` is double-counted when a pass is cancelled and resumed.** Medium-low. CONFIRMED.
- CLI: `src/cli/mod.rs:1261-1264` adds the full `pass_saving` with no check on `outcome.cancelled`.
- Worker: `src/jobs/worker.rs:470-476` passes the full `summary.saving()` as `this_pass`.
- Scenario: cancel at 1%, rerun. The record holds roughly twice the estimate, shown as "saved" via `gui/app.rs:564`.
- Fix: scale this pass's figure by `outcome.bytes / planned bytes`.

**8. Files compressed at a lower level in an incremental CLI pass are never upgraded.** Medium-low. CONFIRMED.
- `src/db.rs:653-656` reselects a file only if its fingerprint changed or its level is `NOT_ATTEMPTED`.
- `src/cli/mod.rs:1271-1273` keeps `record.level = max(previous, this)`.
- Scenario: level-15 pass, game update, `compress --level 3 --force` (changed files get 3, record stays 15), then `--level 15`. `reuse` is true, the level-3 files match, and the result is "Nothing to do".
- Fix: let `changed_since` take the requested floor and return files whose `level_applied` is below it.

**9. The last sampled file in GUI analysis gets whatever sliver of budget remains.** Low-medium. CONFIRMED arithmetic; the worst case is rare.
- `src/jobs/worker.rs:300,315,326`: the budget is `32 MiB - small_files.bytes`, large files take exactly 1 MiB each, and the next file gets the remainder as its `byte_cap`.
- `src/estimate.rs:488` then takes one window of that size from the middle.
- Scenario: a 2 KiB remainder on a 2 GB compressible file. Both models require a whole 4 KiB sector saved, which a 2 KiB sample cannot show, so the file is "not worthwhile", counted in `sampled_size` with zero saving, and dropped from the Compress list.
- Larger slivers under a few sectors distort the result through sector rounding scaled by `size / sliver`.
- Fix: treat a budget under about 128 KiB as exhausted and push the file as an unsampled candidate.

**10. Confidence label counts files, not bytes.** Low-medium. CONFIRMED.
- `src/recommendation.rs:97-108`.
- Scenario: 30 files of 3 GB each, 30 MiB sampled (0.03% of bytes, head and tail only). All files count as inspected, so the label is "Strong estimate".
- Fix: factor in sampled bytes over eligible bytes, or per-file coverage.

**11. The qualification wizard hashes the whole install with no cancel, no progress, and no guard against repeat clicks.** Low. CONFIRMED.
- `src/qualification.rs:89-91,178-182` passes a fresh `AtomicBool::new(false)` and `NoObserver`.
- `src/gui/app.rs:1270-1283`: `Message::Qualify` spawns a task on every click.
- Scenario: clicking "Qualify compatibility" three times on a 100 GB game starts three full hashes that cannot be stopped.
- Fix: keep a cancel flag in state and disable the button while one is pending.

**12. CLI forced decompress may fail to invalidate the record.** Low. PLAUSIBLE; needs a symlink in the install path.
- `src/cli/mod.rs:1250` records `game.install_dir` as scanned; line 1330 invalidates by `game.install_dir.canonicalize()`.
- `invalidate_compression` matches `path` by exact bytes (`db.rs:795-801`).
- Scenario: a Lutris or custom game reached through a symlink. The invalidation matches no row, and an interrupted decompress leaves fingerprints that make the next pass skip decompressed files.
- Fix: store the canonical path, as the worker does via `validate_folder`.

**13. Smaller issues.** All low.
- `src/db.rs:407` runs `PRAGMA journal_mode=WAL` before `busy_timeout` is set at 414. A worker and the GUI creating the database at once can get an immediate `SQLITE_BUSY`. Set the timeout first.
- `src/recommendation.rs:147-153`: on a drive with no native backend and an unqualified game, a pack saving that clears the thresholds still gets "The measured saving is below the automatic optimization threshold." The GUI currently shows only `confidence`, `native_saving` and `maximum_saving`, so this is latent.
- `src/jobs/worker.rs:93-96`: the comment says analysis on non-native drives uses the btrfs model, but `Tier::Pack.backend()` returns `Some(Pack)`. On ext4 or xfs the "Standard about X" line is a pack-model figure at the btrfs level. `UnitModel::level()` has no callers.
- `src/compatibility.rs:215-218`: both sides of the load check saturate. A report with `baseline_load_ms` around 2e15 passes any `candidate_load_ms`. Use `u128` or bound the values in `validate`.
- `validate` accepts uppercase hex and `qualifies` ignores case, but `worker.rs:261` and `service.rs:1235` compare `report.corpus == corpus` against lowercase. Such a report triggers a full hash on every Analyze and never matches. Normalise in `validate`.
- `src/cli/mod.rs:403`: `compatibility import` reads the file unbounded. The GUI import caps at 1 MiB.
- `src/benchmark.rs:360-364`: with a short final sample, offsets overlap and leave a gap (a 9 MiB file gives 0-4, 2.5-6.5 and 8-9 MiB) even when the whole file fits the budget.
- Sparse or preallocated files: the estimators read holes as zeros and price `disk_now` at full raw size; FIEMAP supplies only a compressed fraction. PLAUSIBLE; I found no handling but did not test it.

**14. Tests that cannot fail or miss the edge.** Low.
- `compatibility.rs:538` `stored_reports_have_no_titles_or_paths`: the report fixture is built by hand and never derives from the `Game` holding "Private title" and "/home/person". It would pass with finding 6 present. Build it through `Wizard::report` for a Manual game.
- `db.rs:1102` `migrations_run_twice_without_complaint`: the second and third calls return at `current >= SCHEMA_VERSION`. No test opens a v1 database without `hidden`.
- `tests/estimate_properties.rs` covers `BtrfsModel::disk_cost` only. Nothing property-tests `estimate_open_file`, `estimate_open_file_pair`, `PackModel`, or sample placement.

## Read and found sound
- `BtrfsModel::disk_cost` and its four properties; they are stated independently of the implementation.
- `classify.rs`: every header read is bounds-checked, the RIFF walk terminates, the DXGI ranges are right, and a misclassification only lowers the sample count from 32 to 8.
- Division-by-zero guards, `as` casts and saturating sums in `estimate.rs`. Empty files never reach `to_compress` because of `size <= min_size`.
- Report store: filenames are content hashes, so report strings cannot cause traversal. Files are created `0600` with `create_new`, and load caps each at 1 MiB and skips bad ones.
- Automatic Maximum Space activation rehashes the install twice and requires `report.corpus == corpus` plus `qualifies` (`service.rs:1233-1272`). Build and platform checks hold.
- `compatibility::corpus`: anchored opens, fingerprint checks before and after each file, a second walk, and cancellation.
- Database: all SQL is parameterised, multi-statement writes are in transactions that begin with a write, the upsert avoids the cascade, u64/i64 round trips are exact, non-UTF-8 paths survive, the launcher, backend and preset decode lists match their enums, and a newer schema is left alone.
- `testutil.rs`.
