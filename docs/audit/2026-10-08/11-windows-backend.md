Read-only audit of the Windows coordinator path. Nothing was compiled or run, so every finding is from reading. CONFIRMED means I traced the full path in the source. PLAUSIBLE means the mechanism is in the source but the trigger depends on Windows behaviour or timing I could not observe here.

All paths are under /home/brook/data/gamecompressor.

## Findings

**1. One unopenable file fails the job, and the journal it leaves fails every other game. High, CONFIRMED.**
- src/windows/mod.rs:342 `if operation(item.path())? {`, with :102-108 (`.write(true).share_mode(0)`), :327, :333, :341, :349.
- Any per-file error ends the pass with the journal in place. Triggers: a read-only file, a file another process holds, access denied on a game under `C:\Program Files` (the worker is unelevated), an EFS file, or a file deleted between the walk and the open.
- The next manual job for another game then starts (coordinator.rs:767 gates only automatic jobs on `recovery_pending`) and fails at mod.rs:306-310 with "Review the interrupted job before processing another game".
- Scenario: the user queues 20 games and game 1 has one read-only file. Within seconds all 20 show Failed.
- Automatic jobs wait instead, indefinitely.
- The only GUI resolution is Recover, which queues a full restore of that folder (src/gui/native.rs:950). The restore opens the same file for write and fails the same way.
- Fix: record per-file failures in `Progress` and continue, clearing the read-only attribute or skipping the file. Do not block other folders on a journal whose pass ended by error or stop; WOF changes are per-file, so the tree is consistent.

**2. Compression requires free space equal to the whole game plus its largest file. High for the product's purpose, CONFIRMED.**
- src/windows/mod.rs:312 `crate::storage::native_plan(&root, restore)?.recheck()?;`, with src/storage.rs `native_plan` (`footprint.bytes + footprint.largest`, plus headroom of 5 percent or 64 MiB).
- The reason string describes btrfs snapshots and shared extents. WOF needs roughly the compressed size of one file at a time.
- Scenario: a 120 GB game on a drive with 60 GB free is refused before any file is touched.
- Fix: on Windows, plan for the largest file plus headroom when compressing. Keep the full-size plan for restore.

**3. A cancelled, failed or shutdown-stopped automatic job is never retried. Medium, CONFIRMED.**
- src/windows/coordinator.rs:428-437 calls `queue.acknowledge(&observed)` when the job is enqueued.
- If that job ends Cancelled (tray Exit at :488-492, `Shutdown` at :575-579 as sent by the installer, or the exclusion sweep), Failed (finding 1) or Interrupted, `observe` no longer sees the build as changed.
- Scenario: Steam updates a game, the job is queued, the user upgrades Flummox. The waiting job becomes Cancelled and the update stays uncompressed until the next game update.
- Fix: acknowledge when the job reaches Completed. Leave waiting jobs Waiting across a shutdown, since `Queue::load` already preserves them.

**4. A worker started within 60 seconds of boot can start a job before any activity check or discovery. Medium, CONFIRMED given the `Instant` behaviour the code's own comment states.**
- src/windows/coordinator.rs:278-282 `overdue()` falls back to `Instant::now()`.
- Then `last_activity` and `last_scan` are "now", `snapshot.busy` is the default `None`, `snapshot.discovering` is false and `snapshot.games` is empty.
- The start gate at :760-779 passes on the first loop pass for any Waiting job persisted from the last session.
- The activity check follows 1 second later. Discovery, which carries launcher update state, follows 30 seconds later.
- Scenario: login autostart, Steam starts updating game X at login, Flummox takes exclusive handles on X's files for up to 30 seconds.
- The comment says the fallback "only delays the first scan by one interval".
- Fix: use `Option<Instant>` for never-run timers. Require one completed discovery and one activity check before any start.

**5. Maintenance baseline: three ways existing games are treated as newly installed. Medium.**
- 5a, CONFIRMED mechanism, timing-dependent. coordinator.rs:425 passes the worker's current `preferences` to `observe`, but the catalog was built by a discovery thread that loaded preferences itself (src/native.rs `discover_catalog`).
  - If a scan is in flight while the user adds location B and ticks its automatic box, that scan's result has none of B's games.
  - desktop_jobs.rs:308-313 still records B in `initialized_locations`. The next scan sees B's games with no baseline and queues all of them (:285-292).
  - A scan can run for several seconds (`content_stamp` allows 5 seconds per custom game) and runs every 3 seconds during a job.
  - Fix: carry the location set used by the scan in `Catalog` and initialise only those.
- 5b, PLAUSIBLE. The baseline is keyed by `game.id` (desktop_jobs.rs:277). `desktop::merge` keeps the first id seen for a folder, with launcher ids ahead of manual ones.
  - If a folder under an automatic Collection loses its launcher record without a warning, it reappears under a manual id with no baseline and is queued.
  - Silent paths: `libraryfolders.vdf` absent at scan time (desktop_discovery.rs `if !manifest.exists() { continue; }`), an appmanifest removed while leftover files remain, Steam uninstalled.
  - Fix: key the baseline by canonical install directory, or look up all of `game.ids()`.
- 5c, PLAUSIBLE. Locations carry no volume identity. A second removable drive mounted at the same letter with the same folder names yields changed stamps or unknown ids under an initialised location, so its games are queued. Jobs are volume-bound (desktop_jobs.rs:190). Locations and `initialized_locations` are not.
- Related, low, CONFIRMED: baseline entries are never removed. A game reinstalled or moved at the same build id is neither "changed" nor "new", so maintenance does not compress the new files.

**6. Clean stops leave the journal, which stalls all maintenance. Medium, CONFIRMED.**
- mod.rs:326 and :440-443 return "Operation stopped" before `remove_file` at :353.
- This covers user Cancel, tray Exit, installer `Shutdown`, the exclusion and opt-out sweep (coordinator.rs:608-611), and logoff or power-off during a job (nothing handles WM_QUERYENDSESSION).
- Afterwards coordinator.rs:742 holds every automatic job with "Review interrupted storage before maintenance", and manual jobs for other games fail as in finding 1.
- What a crash leaves at each point:
  - Between `persist` (:323) and the first file: journal present, nothing changed.
  - Mid-pass: a consistent mix of compressed and uncompressed files.
  - After the last file and before :353: fully done, yet still flagged.
- In no case is file data at risk. The cost is the block.
- Fix: remove the journal on a cancel that completes between files. On worker start, clear a journal whose job is not mid-file, or resume it.

**7. Files Windows reports as not beneficial are recompressed in full on every pass. Medium, CONFIRMED.**
- mod.rs:189-191 returns `Ok(false)` for `ERROR_COMPRESSION_NOT_BENEFICIAL` and records nothing.
- On the next pass `WofIsExternalFile` reports the file as not external, so it is opened for write and run through LZX again.
- Scenario: a 100 GB game with 80 GB of already-packed archives. Each update's maintenance job re-reads and recompresses those 80 GB to change nothing.
- The existing test covers only a compressible file.
- Fix: remember size and mtime of rejected files in a per-game sidecar in the data directory and skip them while unchanged.

**8. The activity check does processes times games path resolutions every second on the main loop. Medium, CONFIRMED.**
- src/windows/activity.rs:93-98 calls `path.canonicalize()` per process, then `game.install_dir.canonicalize()` for every game inside the per-process `find`.
- About 150 user processes and 75 games is roughly 11,000 directory opens per second, for as long as the worker lives, including during gameplay and with an empty queue.
- It runs on the coordinator loop thread (coordinator.rs:461). A game folder on a sleeping disk stalls the loop past the listener's 3 second reply window (:318), so clients time out.
- Related: discovery walks every custom game's whole tree (`content_stamp`) every 30 seconds while idle and every 3 seconds during a job (coordinator.rs:394).
- Fix: canonicalise game roots once per check, or once per discovery. Compare process paths without reopening them. Skip the check when no job is active or waiting.

**9. The client never verifies who owns the pipe. Medium on shared machines, CONFIRMED mechanism.**
- src/windows/ipc.rs:159-166 `connect_named` opens `\\.\pipe\flummox-<SID>-v1` and trusts it. The SID is not secret.
- Another local user can create that name first. The victim's worker then fails at `listener()?` (coordinator.rs:302, FIRST_PIPE_INSTANCE) and exits.
- The victim's GUI connects to the other user's pipe, sends its `Settings` payload (location paths) and renders whatever `Snapshot` comes back.
- `SECURITY_IDENTIFICATION` blocks impersonation. The result is a persistent denial of service plus a spoofed UI.
- Fix: after connecting, call `GetSecurityInfo(OWNER_SECURITY_INFORMATION)` or `GetNamedPipeServerProcessId` and compare the token user with `user_sid()`. Have the worker report the failure in a way the GUI can show.

**10. A startup entry from another install stops the worker and blocks uninstall. Medium, CONFIRMED.**
- coordinator.rs:349-351 `if preferences.start_at_login { crate::windows::launchers::startup(true)?; }`.
- `startup` fails with "belongs to another installation" when the Run value names a different path (launchers.rs:256-261).
- Scenario: a portable or dev copy enabled start at login, or the install folder was moved. The other copy's worker exits at start. The GUI cannot change the setting, because Settings goes through the worker.
- The same error in `cleanup_startup` (:179), or a malformed `desktop.json` at :180, makes `--native-worker-exit --remove-owned-startup` exit nonzero.
- packaging/windows/flummox.iss:86-88 then refuses to uninstall, every time, with the unrelated message "Flummox is finishing a storage operation".
- Fix: warn and continue at worker start. In `cleanup_startup`, treat a foreign entry and unreadable preferences as nothing to remove.

**11. Any I/O error in the loop exits the worker; corrupt state files stop it from starting. Medium to low, CONFIRMED.**
- `queue.save(&root)?` at coordinator.rs:442, :493, :708, :785, :799, `preferences.save(&root)?` at :477 and :481, and `crate::windows::recovery()?` at :718 all return out of `run()`.
- Scenario: the system drive is full, which is likely for this tool's users. The temp file cannot be created, the worker cancels its job and exits, the tray icon disappears and the job loads as Interrupted.
- A malformed `windows-job.json` is worse: the worker dies at :718 on every start that has a Waiting job.
- `Queue::load(&root)?` (:300) and `Preferences::load(&root)?` (:347) on a malformed or newer-format file stop the worker from ever starting.
- No user data is discarded, but there is no UI way out. Each GUI request respawns a worker and waits 5 seconds.
- Fix: surface these as `snapshot.busy` or warnings and keep serving. On a parse failure at start, rename the file aside and start empty with a warning.

**12. Custom-game stamping can halt all maintenance. Medium to low; limits CONFIRMED, impact PLAUSIBLE.**
- src/desktop.rs:335-339 allows 250,000 entries and 5 seconds. :356-358 has `duration_since(UNIX_EPOCH)?`.
- A custom game with more entries, a cold HDD walk over 5 seconds, or one file with a pre-1970 or zero timestamp makes the game Broken and adds a warning.
- A warning makes the scan unhealthy (coordinator.rs:425), so no game anywhere is queued and running automatic jobs pause with "Discovery needs attention".
- The Broken game cannot be queued manually either (desktop_jobs.rs:153-156).
- Fix: fall back to a raw FILETIME or zero for odd timestamps. Scope the unhealthy flag to the affected location.

**13. The single pipe instance makes concurrent GUI calls fail, and `request` spawns a process on any connect error. Low, CONFIRMED.**
- ipc.rs:139 sets max instances to 1. coordinator.rs:112-123 treats every connect failure as "no worker".
- The GUI runs a 1 Hz `poll()`, `worker_send` and `scan()` on separate threads (src/gui/native.rs:1803-1820).
- A collision gives ERROR_PIPE_BUSY. `poll` then sets `worker_error` for a tick, and `request` launches a redundant `--native-coordinator` process that exits on the lock.
- `--native-worker-exit` uses `call(Command::Shutdown)?` (:161) with no retry, so a collision with a GUI poll makes the installer report "finishing a storage operation".
- Fix: on ERROR_PIPE_BUSY, call `WaitNamedPipeW` or retry briefly. Spawn only on ERROR_FILE_NOT_FOUND.

**14. A command whose reply timed out still executes. Low, CONFIRMED.**
- coordinator.rs:317-318: the listener gives up after 3 seconds and disconnects, but the request stays in the channel. The loop applies it later at :500 and drops the reply.
- The client sees an error for an Enqueue, Cancel or Shutdown that then takes effect.
- Fix: have the loop skip a request whose reply receiver is gone. That needs a liveness flag, since `mpsc::Sender` cannot detect it before sending.

**15. Tray problems. Low.**
- 15a, PLAUSIBLE. src/windows/tray.rs:285-286 `ensure!(added != 0, ...)` ends the tray thread if `Shell_NotifyIconW(NIM_ADD)` fails once. That can happen at login while Explorer is busy. The session then has no icon, and the TaskbarCreated re-add at :87 never runs because the thread is gone. Fix: keep the window and message loop, and retry on a timer or on TaskbarCreated.
- 15b, CONFIRMED. WM_CLOSE on the hidden window (:114), for example from `taskkill` without `/F`, destroys the tray. The worker keeps running with no icon and no Exit. Fix: publish `Action::Exit` on an external WM_CLOSE or WM_ENDSESSION.
- 15c, CONFIRMED. :267 loads `IDI_APPLICATION`, the generic system icon, so the tray entry cannot be recognised as Flummox.

**16. No guard against system folders. Low to medium, PLAUSIBLE.**
- coordinator.rs:511-515 and desktop.rs:168-171 refuse only a drive root.
- A user who adds `C:\Windows` or a folder holding boot-start drivers gets LZX applied to files the boot loader reads. As far as I know it handles XPRESS and not LZX.
- Fix: refuse `%SystemRoot%` and its descendants.

**17. Reported savings rest on an unchecked measurement. Low, PLAUSIBLE.**
- mod.rs:113-131 reads `FILE_STANDARD_INFO.AllocationSize`. tools/measure-wof-lzx.ps1 uses `GetCompressedFileSizeW`.
- No test asserts that `AllocationSize` moves when WOF compresses a file. The coordinator test checks `changed > 0` only.
- If the two APIs differ for WOF-backed files, "Freed X" is wrong in either direction.
- Fix, per CLAUDE.md's control rule: add a Windows test where a zeros file must show `allocation_after < allocation_before` and a random file must not, or switch to `GetCompressedFileSizeW`.

**18. The measurement script can report a clean no-op. Low, CONFIRMED.**
- tools/measure-wof-lzx.ps1:93 passes `/i` to compact.exe, which continues past per-file errors and can still exit 0.
- If every file fails, `allocated_bytes_after == allocated_bytes_before` is printed as a result. There is no control file.
- :17-22 passes paths without a `\\?\` prefix, so a path over MAX_PATH throws partway through.
- Fix: drop `/i`. Add a zeros file that must shrink and a random file that must not.

**19. `WofIsExternalFile` on a WIM-backed file aborts the pass. Low, PLAUSIBLE.**
- mod.rs:141-160 supplies an 8-byte buffer. A WIM provider reply is larger, so the call likely fails and `ensure!(query >= 0)` ends the pass.
- This is rare outside system images, and it becomes harmless once finding 1 is fixed.

## Read and found sound

- **FFI in src/windows/mod.rs**: NUL-terminated wide path, byte sizes for `WOF_FILE_COMPRESSION_INFO_V1` and `FILE_STANDARD_INFO`, handles owned by `File` on every path, `GetLastError` read straight after `DeviceIoControl`, HRESULT comparison, saturating totals, guarded subtraction in `describe`.
- **Paths**: the root is canonicalised to a verbatim path, so long paths work and journal and root comparisons use on-disk case. `walkdir` with `follow_links(false)` plus the `is_file()` check keeps symlinks and junctions from leading out of the tree.
- **Compression safety**: an exclusive open means a file in use is never compressed under a reader. Compressing a hardlinked file is transparent to its other links. The journal is fsynced and renamed into place before the first file.
- **src/windows/ipc.rs**: protected DACL with one ACE for the user, `PIPE_REJECT_REMOTE_CLIENTS`, `FILE_FLAG_FIRST_PIPE_INSTANCE`, default medium label blocking low-integrity writers, `INVALID_HANDLE_VALUE` check, `LocalFree` guards, aligned token buffer, 16 MiB bound checked before allocation, partial read and write loops with 3 second deadlines, ERROR_NO_DATA and ERROR_PIPE_LISTENING handling, ack before disconnect, version checked on both sides.
- **src/windows/activity.rs**: the WTS array is freed on every path, the process handle is closed before any early return, the SID string is freed, and an unopenable or unnameable process holds work.
- **src/windows/tray.rs**: thread-local state is only borrowed immutably after setup, so the nested menu loop cannot hit a `RefCell` conflict. Explorer restart is handled. `NIM_DELETE` runs on drop.
- **Coordinator**: one worker by file lock. Running is saved before the thread starts. `Active::drop` drains the bounded channel. Cancel is honoured while paused. Shutdown waits for the current file without blocking the loop. Exclusion is enforced on Enqueue, in `observe`, and by the per-pass sweep, which also covers Retry. Volume identity is the volume GUID path, which is stable across drive-letter changes.
- **Persistence**: `Queue::save` and `Preferences::save` use temp file, fsync and rename. An unhealthy scan records nothing, and the first healthy scan only sets baselines (apart from finding 5).
- **Startup entry and installer**: the Run value is quoted and its byte length includes the terminator. flummox.iss stops the worker before copying files, waits up to 30 seconds for the current file, and removes the entry on uninstall when this install owns it.
- **tools/wof-lzx-helper.c**: lengths are bounded by `CHUNK_BYTES`, the output limit is `length - 1`, every chunk is round-trip verified, and buffers are freed on all paths. It is built on Linux only, so stdio text mode does not apply.
- **Arithmetic**: I found no `as` cast or overflow defect in scope.
