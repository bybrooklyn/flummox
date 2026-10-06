# Real-game release acceptance

Version 0.0.2 stays pending until these checks have evidence. A manual Release
workflow builds downloadable candidates without publishing a tag. Automated
fixtures must never operate on an existing game library.

The [Linux real-engine smoke record](2026-10-04-linux-smoke.md) establishes
Factorio deterministic simulation and Super Meat Boy Proton startup across
mounted reads, writable state, remount, and restoration on disposable copies.
The record lists the remaining checks. Follow the interactive procedure below
before qualifying the release.

Use a disposable installation or copy with its own launcher metadata. Keep your
ordinary installation and save files outside the test location. Do not point
fixture tests at your Steam library.

For each native Linux, Proton, Windows, and Mac run:

1. Record the game, build, OS, filesystem, Flummox version, and tester. Start the
   app's compatibility qualification on the ordinary disposable copy.
2. Measure baseline loading with the same save, route, and settings. Record real
   allocated bytes. Btrfs estimates and whole-drive free-space deltas are not
   allocated-byte measurements for a game.
3. Apply compression. On Linux, qualify Maximum Space; on Windows use WOF/LZX;
   on Mac use APFS. Check ordinary file reads, metadata, permissions, and signed
   executable bundles. Record allocated storage and compressed loading.
4. Launch and play the same route. Check anti-cheat and any content streaming
   issues. Record regressions, even if the job itself completed.
5. Apply a launcher update and run its verification. Confirm modifications stay
   writable and the updated game launches. Restart the app and OS; verify login
   remount for Maximum Space.
6. Restore ordinary files. Compare bytes and metadata with the expected updated
   copy and launch again. Exercise interruption and recovery on another
   disposable copy. Keep all retained copies until verification completes.
7. Save the local compatibility report through the app. Copy its path-free JSON
   into this directory. Add a run to `0.0.2.json` containing `kind`, `game`,
   `tester`, `date`, `report`, `restart_verified`, and
   `launcher_verification_passed`. Kinds are `native-linux`, `proton`, `windows`,
   and `macos`. Set `status` to `passed` only when all four have passed.

A tag build checks these records before publication. Build/install CI and native
APFS fixtures establish those particular behaviors; they do not establish
real-game compatibility. Windows and Mac interactive acceptance is performed
on the user's machines. Linux native and Proton evidence also remains required.

The publication check requires schema-version-1 reports with a game build and
corpus hash, matching logical-byte counts, and positive allocated-byte and
load-time measurements. Linux native and Proton runs must exercise Maximum
Space; Windows and Mac runs must exercise native compression. Measurements must
be integers, and restart and launcher verification must be JSON `true`.

## Desktop checks before game qualification

Use candidate artifacts from the Actions release workflow without creating a
tag. Keep location maintenance off until the disposable locations are added.

On Windows, install the candidate, add a temporary collection, and check the
initial baseline before creating a new child game directory. Enable maintenance
for that collection, create a disposable payload, and verify one automatic job.
Check exclusions, pause/resume, a running executable inside the fixture,
reopening the window, retained jobs after worker restart and unavailable drives.
Opt into login startup separately; reboot and check the tray. Restart Explorer
and check that its icon returns. Upgrade and uninstall while a disposable job
runs, then verify bytes and any retained recovery files. Confirm that another
installation's startup entry is preserved.

On Mac, use a temporary APFS game folder. Check discovery, dialogs, native
compression/restore, recovery, theme/motion persistence and closing during work.
On both platforms, inspect wide/narrow and light/dark layouts, wheel/trackpad
movement, rapid navigation, scroll restoration, missing/corrupt local artwork
and Running/Waiting/Needs attention/History rows. These checks supplement native
CI; they do not replace the real-game reports required for publication.
