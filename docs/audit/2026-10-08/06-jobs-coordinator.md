Read-only audit of the Linux job coordinator. Nothing was edited or run; every claim below comes from reading the code. Files are under /home/brook/data/gamecompressor.

I found no path that corrupts or loses game bytes. The serious findings are jobs that fail when they should wait, and a coordinator that exits on any single error and takes every mounted game with it.

## Findings

**1. A job that cannot take `operation.lock` fails instead of waiting. HIGH.**
- Where: src/jobs/mod.rs:441-444, src/jobs/worker.rs:85, src/jobs/service.rs:1117, 1849-1855, 1889-1941.
- Defect: `operation_lock` is a `try_lock`, and its failure becomes a terminal `Failed` job. The coordinator starts the next job in the same pass it sees `Done`, without waiting for the old child to exit.
- Scenario A (CONFIRMED): `flummox compress --force` or `--no-pause` holds the lock for its whole run (src/cli/mod.rs:1075, 1322). Every queued coordinator job is started and failed in turn, one per 50 ms pass, with "Another Flummox process is working".
- Scenario B (PLAUSIBLE): the worker declares `_operation` first, so it drops last, after the inventories, receipt map and `Db`. On a large game that teardown can outlast the next spawn, so a back-to-back job fails. A pack job after a worker job is more exposed, because a thread starts faster than a process. The preempted-analysis path hits the same window.
- Fix: block on the lock with a bounded wait, or do not schedule until the previous child is reaped. Treat "lock busy" as a requeue.

**2. Any `?` in the main loop ends the coordinator. HIGH. CONFIRMED.**
- Where: src/jobs/service.rs:1354, 1645, 1655, 1666-1667, 1799, 1818, 1833, 1845, 1877, 1913, 1970, and `autostart::configure(...)?` at 1395-1398.
- Defect: `run()` returns on any SQLite error, including the per-file receipt insert at 922 reached through 1833, and on any accept error other than `WouldBlock`.
- Scenario: ENOSPC or an I/O error on the state filesystem during a decompress. The process exits, the worker is cancelled, a storage thread dies mid-task, and every Maximum Space FUSE session dies with it. Mounted games return ENOTCONN until some client next starts a coordinator.
- Fix: log per-pass errors, mark the affected job, and keep serving. Exit only on errors that cannot be survived.

**3. After a protocol bump, a user with mounted games or active jobs cannot follow the error's advice. MEDIUM. CONFIRMED.**
- Where: src/jobs/service.rs:1676-1692, src/jobs/client.rs:128-142.
- Defect: the old coordinator rejects every non-`Restart` command from a newer client, and refuses `Restart` while `snapshot.packs` is non-empty or any job is active.
- Scenario: upgrade with one Maximum Space game mounted. Every GUI and CLI call fails with "Restore mounted Maximum Space games before restarting the background worker". Restoring or cancelling is itself a command the old coordinator rejects. A user-paused queued job has the same effect.
- Fix: let `Restart` unmount cleanly when no game process uses a mount, and let the new coordinator remount through `recover_packs`. Or accept Cancel and Restore across versions.

**4. Pack job dedupe ignores the task. MEDIUM. CONFIRMED.**
- Where: src/jobs/service.rs:428-434, 541-546.
- Defect: the test is `j.phase.active() && j.game.install_dir == path && j.operation == operation`, so any active Pack job for the folder absorbs a different `PackTask` and returns `Ok`.
- Scenario: upkeep has queued `Compact`, held behind a running game. The user requests `Restore`. The reply is success, nothing is queued, and Compact runs. `EnqueuePlanned` then attaches the Restore plan to the Compact job.
- Fix: for Pack, compare `job.pack` too, and return an error when a different task is already active.

**5. Coordinator-thread I/O that scales badly. MEDIUM. CONFIRMED.**
- src/jobs/service.rs:922 with `PRAGMA synchronous=FULL` at 315: one autocommit, so one fsync, per completed file, up to 256 per pass. A game with 100k small files costs 100k fsyncs.
- src/jobs/service.rs:1354: `poll_pack` saves the job every 50 ms pass for the whole pack job, about 20 fsyncs a second. The worker path is throttled to once a second at 1844.
- src/jobs/service.rs:1790-1792: the reply is serialised straight into the socket with no `BufWriter`, so each JSON token is a write syscall, on every one-second GUI poll.
- src/jobs/service.rs:179: `layer_bytes` walks every install's update layer on each finished scan, before `upkeep_due`'s cheap early returns.
- Fix: batch receipts in one transaction per pass, throttle the pack save, buffer the reply, and compute `layer_bytes` only after the cheap checks pass.

**6. Upkeep requeues what the user cancelled, and the "played" gate is weak. MEDIUM.**
- Where: src/jobs/service.rs:191-199, 1603-1618; src/busy.rs:36.
- Requeue (CONFIRMED): `stuck` covers only `Failed | Partial | Interrupted`. A cancelled automatic `Compact` on a large layer, or a cancelled `Prune`, is queued again on the next scan, 3 to 30 seconds later.
- Gate (PLAUSIBLE): `played` is set when `gaming` names the game, and `uses_dir` matches any process with an exe, cwd or open file there. Steam verifying or updating the game, or a shell in that folder, lets `Prune` delete the previous store before the game has run from the new one.
- Fix: count `Cancelled` as stuck until the build or layer changes. Require something stronger than any open fd for `played`.

**7. Enabling maintenance on a location discovery has not scanned queues every game in it. MEDIUM-LOW. PLAUSIBLE.**
- Where: src/jobs/service.rs:1766-1771, 66-77.
- Defect: `known.baseline(&games, ...)` covers only the last finished scan. Games first seen afterwards have no observation, and `old.is_none_or(...)` returns true.
- Scenario: add a custom collection, then tick "Maintain new installs and updates" before the next scan finishes (up to 30 seconds). Every existing game there is queued, against the `enabling does not compress existing installs` test.
- Fix: mark the library as needing a baseline and apply it on the next finished scan.

**8. Maintenance queues native compression for mounted Maximum Space games, and uses up the observation when the enqueue fails. LOW-MEDIUM.**
- Where: src/jobs/service.rs:66-77, 1627-1640; src/fsprobe.rs:180.
- FUSE (PLAUSIBLE): `observe` does not skip installs in `snapshot.packs`. `fuse` maps to `Tier::Unsupported`, so each update of such a game yields a Failed job, "Compression is not supported on this drive yet."
- Observation (CONFIRMED): the stamp is recorded before `enqueue`. If the enqueue fails, for example on a full queue, that update is never queued again.
- Fix: skip pack installs, and record the stamp only after a successful enqueue.

**9. An active job that can never start keeps the coordinator busy indefinitely. LOW-MEDIUM. CONFIRMED.**
- Where: src/jobs/service.rs:1497-1501, 1894-1898, 1444-1448, 1975-1981.
- Three ways in: a queued job the user paused, a queued job whose game left discovery, and a queued job whose exclusion was imported from history at startup (the import cancels nothing).
- Effect: all three stay `active()`, so discovery, the /proc scan and the layer walks run every 3 seconds, and idle exit never happens.
- Fix: base the scan cadence on runnable or running jobs, and cancel queued jobs for imported exclusions and vanished games.

**10. `PackControl::transaction` holds `transition` across a blocking channel send. LOW. Mechanism CONFIRMED, narrow window.**
- Where: src/jobs/service.rs:998-1008, 1012-1017, 1097.
- Defect: `self.started(0, 0, message)` sends twice on `sync_channel(128)` with the mutex held. Only `poll_pack` drains the channel, on the coordinator thread.
- Scenario: the channel is full when the transaction begins, and a Pause or Cancel for that job arrives before the next drain. `request_control` blocks on `transition` and the coordinator hangs for good.
- Fix: emit after dropping the guard, or use `try_send` for progress.
- Related: per-file progress (src/pack/create.rs:663, src/pack/format.rs:1039) caps the storage thread at 128 events per 50 ms pass.

**11. Exclude during a must-finish storage step mislabels the result. LOW. CONFIRMED.**
- Where: src/jobs/service.rs:733-737, 1339-1341, 1376.
- Defect: Exclude sets `Cancelling` without `request_control`, and `poll_pack` then sets `cancel` unconditionally. A step that fails for a real reason is recorded `Cancelled` ("Stopped") instead of `Failed`.
- Also: `snapshot.excluded.retain` at 723 runs before the fallible `send_control`, so an error leaves memory and the database disagreeing. `history.exclude` at 1761 can fail after `apply` has already committed.
- Fix: derive `Cancelled` only from a cancel accepted through `request_control`.

**12. Worker events overwrite `Cancelling`. LOW. CONFIRMED.**
- Where: src/jobs/service.rs:929-935.
- Defect: `Paused` and `Resumed` set the phase without checking for `Cancelling`, which re-enables the pause control at 1812. `Resumed` also sets `Running` on an Analyze job, which should be `Analyzing`.
- Fix: ignore both events while `Cancelling`, and restore the phase by operation.

**13. Resource leaks on start failures. LOW. CONFIRMED.**
- src/jobs/service.rs:843-861: if writing `Work` or taking stdout fails, the `Child` is dropped unreaped and stays a zombie.
- src/jobs/service.rs:1917-1921 with 1095: `std::mem::take(&mut mounts)` is passed by value before `start_pack` validates `job.pack`. An early `Err` drops every live mount. This is reachable only with a Pack row that has no task.
- Worker stderr goes to `Stdio::null()` at 847, so a worker panic shows only as "Worker disconnected: Incomplete or oversized worker message".
- Fix: kill and wait on error, validate before taking the mounts, and send worker stderr to `service.log`.

**14. Periodic pack recovery retries forever and erases the interrupted-restore guidance. LOW. CONFIRMED.**
- Where: src/jobs/service.rs:1966-1972; src/jobs/packs.rs:469-494; src/pack/install.rs:319-326.
- Defect: a `Restoring` record becomes `Attention` with its guidance message. Five seconds later the same loop calls `pack::recover`, which accepts `Attention`.
- Effect: it either remounts the install as `Mounted`, or replaces the message with "Could not mount automatically: ...". It then repeats every 5 seconds, opening every store and saving each time.
- Fix: give an interrupted restore its own sticky state, and back off on repeated failures.

**15. Startup fragility. LOW. PLAUSIBLE.**
- src/jobs/service.rs:323: one queue row that no longer deserialises stops the coordinator starting at all. Theme and motion rows tolerate this at 355 and 364.
- src/jobs/service.rs:1423-1429: the socket is bound before `recover_packs`. A long recovery, such as finishing an interrupted reclaim delete, makes clients connect and then time out at 8 seconds.
- src/jobs/service.rs:1461-1465 with src/gui/app.rs:1577-1581: `worker_epoch` is wall-clock time. If the clock steps back across a restart, the GUI discards every snapshot from the new coordinator.

**16. Tests that cannot fail for the reason they name. LOW. CONFIRMED.**
- src/jobs/service.rs:2196-2200: the "bounded request" input has no newline, so it is rejected by `ends_with('\n')` even with `LIMIT` removed. It needs a payload over the limit that ends in a newline.
- tests/jobs_lifecycle.rs:276-283, 403-410, 521-528, 586-593: four worker tests return `Ok` on a non-btrfs filesystem with only an `eprintln`. The FUSE tests have `FLUMMOX_REQUIRE_FUSE`; btrfs has no such switch.
- No test queues two real worker jobs back to back, which is why finding 1 scenario B is uncovered.

## Read and found sound

- **Socket access:** the state directory is 0700 and owner-checked, the message size is bounded, and `Within` enforces an overall request deadline.
- **Stale replies:** `revision` rises on every reply, which is all the GUI needs to drop them.
- **Receipts:** other-policy receipts are invalidated before the first rewrite, and file names keep their non-UTF-8 bytes.
- **Worker ordering:** whole-filesystem checks and database opens come before the sandbox, and the walk and `Anchor::open` come after. Closing stdin cancels the worker.
- **Space plans:** every route is rechecked, at `EnqueuePlanned`, at worker start, and twice on the storage thread. Unplanned enqueue, Retry and maintenance all reach the worker's own `native_plan` recheck. Byte arithmetic in `space_plan`, `layer_bytes` and `carried_saving` saturates.
- **Pack transactions:** the record is saved before each path change in activate, reclaim, compact and prune. No checkpoint follows a `transaction()` call. Direct `Pack*` commands are refused while anything runs.
- **Mount hand-off:** the storage thread's records and mounts come back intact, because the coordinator does not touch `snapshot.packs` while it runs.
- **Queue logic:** Retry repeats every enqueue check, the queue caps hold (200 active, 300 finished, 500 loaded), and a never-started paused job survives a restart.
- **Autostart:** the entry is written atomically and a foreign file is refused.
