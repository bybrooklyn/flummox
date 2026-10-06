# Desktop polish implementation, 2026-10-06

The [implementation plan](../plans/desktop-polish.md) remains the delivery contract.
The baseline Linux desktop phase is committed as `596b21e`. The continuation
adds native UI parity and the Windows coordinator/maintenance implementation.
Native CI and release acceptance are recorded separately below.

## Delivered changes

- Overview, Games, Settings navigation. Linux Settings combines jobs, locations,
  recovery, maintenance, appearance, compatibility reports and About in one
  scrollable page with jump links. Updates are a Games filter.
- Charcoal/green and equivalent light styling. Direction follows sidebar order;
  navigation and details translate within clipped bounds without moving layout.
- Wheel steps ease for 100 ms, clamp to content bounds, retarget from the current
  position and stop without momentum. Trackpad pixel input stays direct. Linux
  reduced motion disables interpolation. Pages restore their scroll offsets.
- Running, Waiting, Needs attention and History groups, persistent row keys,
  retained jobs during connection failures and ordered snapshots. A worker epoch
  and revision reject late responses. Running work stays visible on Overview.
- Linux discovery runs on a cooperative, cancellable background worker. Providers
  publish batches, one scan runs at a time, repeated refresh requests coalesce,
  and stale GUI probes are discarded. Requests no longer wait for discovery;
  process checks retain their separate cadence. Missing games stay remembered.
- Local Steam artwork is indexed once per GUI scan. Icons and covers have
  deterministic role selection and separate bounded decode sizes. Two image
  workers feed a 256-entry cache; visible rows request images, placeholders have
  fixed dimensions and corrupt images do not remove games. Clicking a row image
  opens a local-file override picker. Metadata and artwork require no network.

## Native continuation

- Native Settings now includes Jobs, Locations, Recovery, Maintenance, Appearance,
  compatibility reports and About, with shared scroll jumps. Theme/motion persist;
  collections preserve legacy folder identities; local artwork uses the shared
  bounded decode cache. Search, sorting and Updated/Needs attention filters stay
  selected through navigation and refresh.
- Windows discovery reads Steam registry/library manifests, Epic item manifests,
  GOG installed-game registry and Heroic local metadata. Individual malformed
  manifests produce warnings. Resolved directories merge launcher aliases;
  remembered missing games remain visible and cannot start work.
- Windows jobs run through one current-user coordinator. Its named pipe uses a
  current-user-only DACL, rejects remote clients, bounds frames, checks protocol
  versions and refuses a duplicate server instance. Client windows can close
  while jobs run. The durable queue preserves job IDs and drive identities;
  interrupted work requires review. Retry cannot duplicate a game's active job.
- Maintenance is opt-in by location. The first healthy scan establishes a whole
  batch baseline, including empty libraries. Paused, busy, offline or failed
  discovery does not consume pending updates. Exclusions and opt-out cancel
  automatic jobs; recovery blocks new automatic compression. Restoration remains
  available for excluded games. File metadata stamps detect custom-folder changes.
- Process checks defer work for running games, known updater tools and unknown
  activity. Original-drive checks pause work after disconnection or replacement.
  Existing WOF/LZX files are queried before opening a write handle, so unchanged
  compressed files are skipped. See Microsoft's
  [WOF query contract](https://learn.microsoft.com/en-us/windows/win32/api/wofapi/nf-wofapi-wofisexternalfile).
- Tray actions open Flummox, pause/resume and exit after the current file. The
  icon is restored after Explorer restarts. Login startup is separately opt-in.
  Upgrade/uninstall asks the owned coordinator to stop before replacing files;
  uninstall removes only this installation's startup entry.
- CI now exports native wide/narrow light/dark fixture previews and runs the
  Windows installer compilation/install/upgrade/uninstall smoke checks. Windows
  tests use isolated data roots, catalogs, pipe names and disposable payloads.
  They cover rejected protocol versions, pipe ACLs, client closure, persistence,
  actual WOF compression and unchanged-file skips. They have only been compiled
  in this session.

## Validation

- `just lint`: passed, all Linux features and targets, warnings denied.
- `just test`: 144 library tests passed, including offscreen previews and motion,
  artwork, discovery cancellation and snapshot-ordering regressions. With the
  session restrictions removed, all eight coordinator lifecycle tests pass. The
  btrfs fixture now closes its own directory handle before queueing work, so busy
  detection does not classify the fixture as a running game. `just ci` passes,
  including a fresh dependency-policy check and all packaging tests.
- The six remaining integration targets were run separately: 21 tests passed.
  The single doctest passed. Filesystem-dependent tests retain their existing
  capability guards. A separate required-FUSE run passed all ten pack-mount and
  coordinator tests without skips, including a real btrfs worker round trip.
- `python3 packaging/test_release.py`: eight tests passed, including the existing
  acceptance-check changes. The temporary test signer was used.
- Windows x64 GUI cross-compilation and Clippy over all targets passed with
  warnings denied. This is compilation evidence, not a Windows runtime test.
- `cargo deny --offline check`: passed using the cached advisory database;
  the continuation also passes the normal policy check with the updated advisory
  database. Prose and whitespace checks passed.
- Fixture screenshots were rendered without a display and inspected in wide,
  narrow, dark and light layouts. The preview exporter now converts Iced's BGRA
  desktop pixels to RGBA before saving PNGs.

Baseline logs and previews are under `/tmp/flummox-polish-*`. Continuation logs
are under `/tmp/flummox-next-*`. Workflow checks passed with `actionlint`; YAML
parsing passed. The selected
[dark Overview](desktop-polish/overview.png),
[light Overview](desktop-polish/overview-light.png),
[dark Games](desktop-polish/games.png),
[light Games](desktop-polish/games-light.png),
[Settings](desktop-polish/settings.png) and
[narrow Settings](desktop-polish/settings-narrow.png) fixtures accompany this
record. They are not screenshots of a running desktop session.

## Live Linux acceptance

An optimized build from `be09967` ran in an isolated headless Gamescope/Xwayland
session with a temporary home, 250 fake Steam games and a custom collection.
Ordinary game libraries were hidden. Mouse and keyboard input verified:

- Overview/Games/Settings navigation, Settings jumps and direct Home scrolling.
- Wheel movement reaching its endpoint without momentum and retained per-page
  offsets. Modifier notifications no longer cancel easing.
- Search across page changes. A needs-attention storage job stayed
  visible after returning to Settings, with no connection/loading placeholder.
- Light/dark, reduced motion, wide/narrow layouts and fixed missing-art slots.
- A collection containing a disposable game, file analysis, remembered missing
  folders after refresh and restored folders after reconnecting the fixture.
- Native folder picker open/cancel and normal GUI closure with exit code zero.

The [path-free check record](2026-10-06-gui-smoke.json) states the scope and
limitations. Selected live window captures are checked in:
[dark Overview](desktop-polish/live/overview-live.png),
[light Overview](desktop-polish/live/overview-light-live.png),
[dark Games](desktop-polish/live/games-live.png),
[narrow light Games](desktop-polish/live/games-narrow-live.png),
[dark Jobs](desktop-polish/live/jobs-dark-return-live.png),
[light Jobs](desktop-polish/live/jobs-retained-live.png),
[Locations](desktop-polish/live/locations-live.png), and
[disconnected fixture](desktop-polish/live/offline-game-live.png).
These are screenshots of the running app with fixtures.

A second isolated btrfs fixture with 60 fake games completed native decompression
and recompression of two files containing 262,357,952 logical bytes. The large
file's SHA-256 matched its generated bytes after both operations. Completed
[history](desktop-polish/live/history-return-btrfs-live.png) and game
[selection](desktop-polish/live/selection-return-btrfs-live.png) survived page
changes. These short jobs completed before a paused/running window capture;
those GUI states remain unverified. Normal GUI closure returned zero again.

## Required follow-up

Physical wheel/trackpad behavior, all job phases, real external-drive removal,
Windows/Mac dialogs, tray, startup and real-game qualification remain required.
The Linux disconnection check used a rename. Storage on the isolated fixture
mount is unsupported and produces an attention row; it does not establish
compression there. Real btrfs and FUSE behavior was verified separately.

The [native CI run](https://github.com/bybrooklyn/flummox/actions/runs/37531175295)
passes every job for `be09967`: Linux policy/tests, btrfs matrix, required FUSE,
Mac native tests/builds and Windows native tests/builds plus installer lifecycle.
Windows and Mac export native previews. Verify
tray/Explorer restart, startup ownership, dialogs, actual game/updater deferral,
recovery and upgrades during work on disposable native installations.

Git metadata is now writable. The continuation and its repairs are committed and
pushed through `be09967`. Native validation covers canonical location aliases,
acknowledged pipe replies, joined shutdown listeners and temporary empty pipe
buffers. Explicit animation redraws, final scroll frames and direct keyboard
scrolling cover the shared surface. Flat cards avoid the software-renderer blur
artifacts seen during live checks. The complete local CI gate passes.
No new tag or release was made.
Interactive Mac and Windows acceptance and all real-game qualification records
remain pending.
Version 0.0.2 must remain unpublished until its acceptance records pass.
