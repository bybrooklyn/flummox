Read-only audit of discovery in /home/brook/data/gamecompressor. Nothing was edited, cargo was not run, and the real Steam library was not touched. "CONFIRMED" means traced through the code. Where a finding also depends on an external file format or location I recalled from memory, I say so.

## Findings

**1. A custom location can swallow other launchers' games as one "game". Severity: high. CONFIRMED.**
- Where: `src/launchers/desktop.rs:215-258` (`add_custom`), `:263-288` (`merge`), `src/jobs/service.rs:680-686` (`Command::Library`), `src/jobs/mod.rs:489` (`validate_folder`).
- Defect: `merge` only collapses games whose canonical paths are equal (`games.iter_mut().find(|g| g.install_dir == path)`). Nothing detects a Manual game that contains another discovered game or a Steam library. `enqueue_job` only dedups on `j.game.install_dir == path`.
- Scenario: the user adds `/mnt/games` as a "Games library", and it holds `SteamLibrary/` and `Heroic/`. `SteamLibrary` becomes `manual:/mnt/games/SteamLibrary`, Idle, `is_tool: false`. A job on it rewrites every Steam game, Proton, the runtimes, `compatdata`, `shadercache` and `steamapps/downloading`.
- Bypassed: per-game `StateFlags` busy state, `steam:<appid>` exclusions (different id), and the tool filter. With `automatic` on, `Observations::observe` queues it unprompted. A Maximum Space activation would move a whole Steam library into a store.
- Fix: at library add and at scan, refuse or drop a Manual candidate that is a strict ancestor of another game's `install_dir` or contains `steamapps/`. In `enqueue_job`, refuse a path that is an ancestor or descendant of another discovered game.

**2. Heroic array entries get their array index as GameId. Severity: high if the format recollection holds, otherwise medium. Code CONFIRMED, real-world trigger PLAUSIBLE.**
- Where: `src/launchers/desktop.rs:63-80`, `src/desktop_discovery.rs:224-230` and `:319-323`.
- Defect: `Value::Array(entries) => entries.iter().enumerate().map(|(i, v)| (i.to_string(), v))`, then `item.get("app_name")...unwrap_or(&key)`.
- From memory, Heroic's `gog_store/installed.json` is `{"installed":[{"appName":"1423049311","install_path":...,"buildId":...}]}`, camelCase with no `app_name`. I could not check that here.
- Scenario: GOG games get `heroic-gog:0`, `heroic-gog:1`. Uninstalling the first shifts every id down. Exclusions, maintenance observations (`known.0` keyed by `game.id.to_string()`), history rows and artwork overrides then belong to a different game. An excluded game can become eligible for automatic compression.
- Fix: read `appName` as well as `app_name` (and `buildId`). When neither exists, derive the key from the install path, never the position.
- Related, PLAUSIBLE: Nile's file is, as I recall, `nile_config/nile/installed.json` with `id`/`path` fields, so Amazon games are never found by either implementation.

**3. `remember` never forgets a launcher game and can lock a path out permanently. Severity: medium. CONFIRMED.**
- Where: `src/libraries.rs:84-119`, with `keep` from `src/launchers/mod.rs:126-131` and `src/desktop.rs:254`.
- (a) Ghost entries. `keep` is always true for non-Manual games, and loop 2 re-adds every cached game not seen this scan. An uninstalled Steam game, or one moved to another library with Steam's "Move install folder", stays listed forever as `Library unavailable: game was not rediscovered...`. `src/gui/app.rs:103` renders that as "Offline / Reconnect the original drive and refresh." A moved game yields two entries with the same `steam:<appid>`, while `service.rs:1529` matches batches by id. Deleted collection subfolders behave the same.
- (b) Lockout. Lines 87-96 skip a fresh sighting when the cached volume identity differs (`if !online { continue; }`), then loop 2 re-saves the old record with the old volume. Reformat a drive (new UUID), mount it at the same path, reinstall: every game there is Broken "reconnect its original drive" on every scan, with no way out except deleting `libraries.json`.
- (b) also fires when identity falls back to `"{source}:{fsid}"` (`src/storage.rs:112-118`) and the device name changes across boots. That is PLAUSIBLE for multi-device btrfs whose `/dev/disk/by-uuid` link points at a different member.
- Fix: drop a cached launcher game when its volume is online and its directory is absent, or the same id was found at another path. When a fresh sighting exists as a directory on a mounted volume, accept it and replace the cached volume.

**4. `RunningAppID` is never found on a standard install. Severity: medium. Code CONFIRMED, file location from memory.**
- Where: `src/launchers/steam.rs:161` and `:299-304`.
- Defect: `roots()` canonicalizes (`let canonical = c.canonicalize().unwrap_or(c);`), so `~/.steam/root` becomes `~/.local/share/Steam`. `running_app_id` then tries `<root>/registry.vdf` and `root.parent()/registry.vdf`, which is `~/.local/share/registry.vdf`. Steam writes `~/.steam/registry.vdf`; Flatpak writes `~/.var/app/com.valvesoftware.Steam/.steam/registry.vdf`.
- Outcome: `BusyReason::Running` is never produced. A running game with bit 64 set is reported as `StaleRunningFlag` ("possibly stale"), which the model documents as a confirmation prompt in manual mode. Only the `/proc` check in the service still catches it.
- The read is also unbounded and strict UTF-8 (`read_to_string`), unlike `read_manifest_text`.
- Fix: pass `env.home` and probe `home/.steam/registry.vdf` plus the Flatpak and snap equivalents, read through `read_manifest_text`. Add a fixture test with the real layout.

**5. The Windows/macOS Steam reader treats incomplete installs as Idle and has no tool filter. Severity: medium. CONFIRMED.**
- Where: `src/desktop_discovery.rs:297-306`, `:32-59`.
- Defect: `let flags = manifest.get_u32("StateFlags").unwrap_or(4);` and the busy mask is only `256 | 512 | 1024 | 2048 | 32768 | 65536 | 131072`.
- Not checked: `FULLY_INSTALLED` (flags 0 or 1 give Idle), `FILES_MISSING` 32, `FILES_CORRUPT` 128, `BACKUP_RUNNING` 4096, adding/preallocating/downloading/staging/committing/stopping, `BytesToDownload`/`BytesToStage`, `steamapps/downloading/<appid>`, and `RunningAppID`. The Linux `group_state` handles all of these.
- A manifest with a missing or unparsable `StateFlags` is Broken on Linux and Idle here.
- `installed()` hardcodes `is_tool: false`, so Steamworks Common Redistributables is a compressible game that maintenance will queue.
- Line 263 uses strict `from_utf8`, so a Latin-1 game name drops the game. Linux decodes lossily.
- Fix: share `state_flags`, `WORKING_FLAGS`, `group_state` and `is_tool` between both readers.

**6. Install-directory validation is thin for launcher-supplied paths and absent on Windows. Severity: medium. CONFIRMED.**
- `src/launchers/desktop.rs:37`: Heroic `install_path` and Lutris `directory` only need `is_absolute()`. `/`, `$HOME`, `/mnt/games` and paths with `..` all become games. The only later gate on Linux is `validate_folder`.
- `src/jobs/mod.rs:495-527` gaps: `home.as_ref() != Some(&path)` compares a canonical path with raw `$HOME`, so a symlinked home bypasses the home, `.ssh` and `.config` checks. It accepts `~/.local`, `~/.local/share`, `~/.var`, `~/.steam`, `~/Documents`, `/var/home`, `/mnt`, `/media`, `/run/media/<user>`, `/opt`, `/srv`, `/root`, `/tmp`.
- Windows: `src/desktop.rs:168-171`, `src/desktop_discovery.rs:39-42`, `src/windows/launchers.rs:173-176` and `src/windows/coordinator.rs:511-515` only refuse a drive root (`path.parent().is_some()`). `C:\Users\<me>`, `C:\Windows`, `C:\Program Files` are accepted from the picker, the registry `path` value, or Epic `InstallLocation`.
- Fix: one shared validator. Canonicalize home before comparing. Refuse any ancestor of home, the profile and system roots per platform, and mount points. Apply it at discovery so the game is listed Broken, as well as at enqueue.

**7. An unreadable `libraries.json` is never repaired. Severity: medium on native, low on Linux. CONFIRMED.**
- Where: `src/libraries.rs:78-82`, `src/native.rs:46` and `:51`, `src/launchers/mod.rs:132-140`.
- Defect: `serde_json::from_slice(&bytes).context("Reading remembered libraries")?` returns before the write, so a cache that fails to deserialize fails every scan. A schema change in `Game`/`InstallState` or a downgrade is enough.
- macOS/Windows: `remember(...)?` makes `discover_catalog` fail, so there is no game list at all. A malformed `desktop.json` does the same via `Preferences::load(&root)?`.
- Linux: it becomes a permanent warning. `service.rs:1548-1554` then keeps every previously seen game ("A scan with warnings may have failed to read a source") until the coordinator restarts.
- The read is also unbounded (`std::fs::read`).
- Fix: on a parse error, rename the file aside, continue with `Cache::default()`, and warn once. On native, degrade to the fresh catalog.

**8. Every scan rewrites and double-fsyncs the cache. Severity: low to medium. CONFIRMED.**
- Where: `src/libraries.rs:122-133`, cadence at `src/jobs/service.rs:1498-1502`.
- Defect: `remember` always writes a temp file, `sync_all`, renames, and fsyncs the directory. Scans run every 3 s while a job is active and every 30 s otherwise, so there are two fsyncs every 3 s during any compression run.
- Each scan also calls `storage::volume` one to three times per game, each listing and canonicalizing `/dev/disk/by-uuid`.
- Fix: compare the serialized bytes with the existing file and skip the write when equal. Cache volume lookups per mount point within one call.

**9. The Linux Steam reader accepts an install directory that resolves outside the library. Severity: low to medium. CONFIRMED.**
- Where: `src/launchers/steam.rs:237-248`, `:411-418`.
- Defect: only the `installdir` string is checked for `Normal` components. Discovery then uses `app.install_dir.canonicalize()`, so a symlink at `steamapps/common/<name>` makes its target the game directory. `desktop_discovery.rs:280-285` refuses this with `install.canonicalize()?.starts_with(common.canonicalize()?)`.
- A multi-component `installdir` such as `A/B` is accepted by both readers, giving nested game directories next to a game installed at `A`.
- Fix: require exactly one component. Either apply the containment check or mark out-of-library targets so the UI asks for confirmation.

**10. The VDF lexer drops the character after a lone `/`. Severity: low. CONFIRMED.**
- Where: `src/launchers/vdf.rs:256-269`.
- Defect: `self.bump(); if self.peek() == Some('/') {...} else { self.peeked = Some('/'); return; }`. The `peek()` already loaded the next character into `peeked`, and the assignment overwrites it.
- Scenario: an unquoted `/mnt/games` parses as `/nt/games`, and `"k" /}` loses its closing brace. Steam quotes everything, so this needs a hand-edited file. A swallowed newline is also not counted.
- No test covers it: the round-trip property only writes quoted strings.
- Fix: keep a two-character lookahead, or handle `/` in `next_token` by cloning `rest` to inspect the following character.

**11. `is_tool` matches real games by name prefix. Severity: low. CONFIRMED in code; titles from memory.**
- Where: `src/launchers/steam.rs:130-135`.
- Defect: `self.name.starts_with("Proton")` flags games such as "Protonwar" or "Proton Pulse" as tools.
- Outcome: maintenance skips them (`service.rs:74`) and they are left out of the running-process check (`.filter(|g| !g.is_tool)`, `service.rs:1585`).
- Fix: use the appid list plus exact patterns (`Proton <digit>`, `Proton Experimental`, `Proton Hotfix`, `Proton - `, `Proton EasyAntiCheat`/`BattlEye Runtime`).

**12. `remember` drops games it cannot place on a volume. Severity: low. CONFIRMED.**
- Where: `src/libraries.rs:99-102`.
- Defect: `Ok(volume) if game.install_dir.is_dir() => volume, _ => continue`.
- A Heroic/Lutris/Manual game first seen while its drive is unplugged is removed from the result. That contradicts the doc on `desktop.rs:26-28` ("kept and marked `Broken`, so a game on a disconnected drive stays listed").
- `storage::volume` errors on a read-only mount (`ensure!(!fs.read_only, ...)`). Games on a filesystem that went read-only vanish with no warning, or show "reconnect its original drive" if cached.
- Fix: push the game with its Broken state without caching it, and separate "read-only" from "absent".

**13. A failed settings read forgets remembered Manual games. Severity: low. CONFIRMED.**
- Where: `src/launchers/mod.rs:125`.
- Defect: `configured_libraries().unwrap_or_default()` turns a transient SQLite error into an empty library list. `keep` is then false for every Manual game, and `remember` deletes offline custom games from the cache. `configured_libraries` sets no busy timeout.
- Fix: on error, keep everything (`keep = |_| true`) and push a warning.

**14. A scan has no timeout and blocks all later scans. Severity: low. PLAUSIBLE.**
- Where: `src/launchers/scan_job.rs:30-35`, `src/jobs/service.rs:1503`.
- Defect: a new scan starts only when `discovery.is_none()`, and cancel is checked only between sources. An `is_dir`/`canonicalize` hung on a dead NFS or sshfs mount stops discovery for good. Job gating then uses the last completed `games` states, so a game that starts updating is still seen as Idle.
- Fix: track scan start time. After a deadline, abandon the worker, raise a warning, and treat states as unknown.

**15. Duplicate handling. Severity: low. CONFIRMED in code.**
- `src/launchers/steam.rs:388-420`: libraries are deduplicated per root only. A library listed by both the native and Flatpak roots is read twice, the group holds each app twice, and `also` contains the game's own primary id.
- `src/windows/launchers.rs:143`: `roots.contains(&path)` is case-sensitive. From memory, the registry `SteamPath` is `c:/program files (x86)/steam`, so each manifest is read two or three times. Existing folders are deduplicated by canonical path in `merge`. Offline ones are not, since canonicalize fails.
- `src/launchers/desktop.rs:133`: `let (id, title, path) = row?;` means one Lutris row with a NULL `name` discards every Lutris game.

**16. Property tests with weak or vacuous assertions. Severity: low.**
- `tests/vdf_properties.rs:267-271`: `check(!outcome.is_empty(), outcome)` cannot fail, because `outcome` always starts with `"case {i}: "`. The test only detects a crash or hang. A regression that accepted the truncated `"AppState" {` as an empty object would pass. Fix: assert the expected `Ok`/`ErrorKind` per case. CONFIRMED.
- `arbitrary_input_never_takes_the_parser_down`: the `Ok` branch needs a string, then `{`, then a balanced body from random pieces, which is rare. The `count_entries <= chars` bound is therefore almost never evaluated, and the determinism check is trivially true for a pure function. Fix: add a generator of structurally valid documents with noise. PLAUSIBLE, not run.
- No property covers unquoted tokens, which is why finding 10 passes.

## Read and found sound
- **VDF parser** (`vdf.rs`): depth cap of 128 (also bounds recursive drop), BOM strip, CRLF, unterminated string and conditional errors, first-wins case-insensitive duplicate keys, trailing-backslash handling, 16 MiB bounded lossy reads in `read_manifest_text`. `#include`/`#base` fail as `ExpectedRoot`, with no file inclusion.
- **`installdir` string check**: rejects empty, absolute, `.` and `..` in both readers.
- **Linux `group_state`**: `WORKING_FLAGS` covers update, validate, download, stage, commit and uninstall. Pending byte counters and `downloading/`/`temp/` leftovers also count as busy. A missing `StateFlags` gives Broken. `UpdatePending` and Broken both block jobs through `is_idle()`.
- **`libraryfolders.vdf`**: old flat and new nested formats both read. A malformed file no longer hides other roots. A bad manifest is isolated per file.
- **Exclusion matching**: `game.ids()` includes aliases at `service.rs:39`, `:423` and `coordinator.rs:517`. `parse_game_id` splits on the first `:`, so Manual path keys survive.
- **Collection scanning**: skips files, hidden names and symlinks on both platforms.
- **`RemoveLibrary`**: refuses while active jobs or stores exist under the path.
- **Unmounted drives**: an unmounted Steam or custom library keeps its games as Broken through the volume identity check. An empty mount point is not read as "uninstalled".
- **Cache write**: atomic (same-directory temp file, fsync, rename, directory fsync) under an inter-process lock, in a 0700 directory.
- **`data_dir`**: ignores a relative `XDG_STATE_HOME`. An unset `HOME` is an error, not a guess.
- **`scan_job::Worker`**: cancel on drop, `Finished` always last, a dead thread yields `Finished(None)`. Events from a cancelled scan are discarded by the service.
- **Fixture isolation**: fixture homes never read real custom libraries or the real cache (`Env::current().home == env.home` guard).
- **Windows registry**: sizes are in bytes, the type is restricted to `REG_SZ`, bounds are checked before slicing, and handles close once. `startup` refuses to overwrite a foreign Run entry.

Files read in full: `src/launchers/{mod,steam,vdf,desktop,scan_job}.rs`, `src/libraries.rs`, `src/model.rs`, `src/desktop_discovery.rs`, `src/windows/launchers.rs`, `src/native.rs`, `tests/vdf_properties.rs`, `src/desktop.rs`. Callers read in part: `src/jobs/service.rs`, `src/jobs/mod.rs`, `src/jobs/client.rs`, `src/storage.rs`, `src/cli/mod.rs`, `src/watch.rs`, `src/windows/coordinator.rs`, `src/gui/app.rs`, `src/busy.rs`, `src/desktop_jobs.rs`.
