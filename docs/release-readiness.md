# Linux workflow completion

The Linux desktop now connects matching local compatibility reports to automatic
Maximum recommendations, queues Maximum jobs with durable task parameters,
and offers a native folder and report picker through KDialog or Zenity. It keeps
the current sidebar, theme, and game-card structure.

## Validation

- `just ci`: clippy, tests, dependency policy, and prose pass.
- `just build`: Linux release CLI and GUI build with `gui,pack-mount`.
- `cargo check --no-default-features --offline`: CLI configuration builds cleanly.
- Fixture background worker tests cover store creation, rejection of a mismatched
  compatibility report, switching to Maximum, folding in updates, deleting the
  previous version, deleting the original, and decompressing with launcher
  updates preserved.
- Headless previews render the real Overview, Games, Jobs and Settings pages
  at wide and narrow sizes. They use synthetic state and never scan a library.

To retain preview PNGs:

```sh
FLUMMOX_PREVIEW_DIR=/tmp/flummox-review cargo test --all-features gui::preview
```

The Landlock enforcement test uses a fresh executable. Forking the shared unit
suite inherited open FUSE update-layer locks and could make a concurrent restore
fail. The isolated test preserves enforcement coverage without retaining those
handles.

## Release preparation validation (2026-10-03)

- `just ci` passes, including graceful idle restart, persisted settings, and
  rejection of older clients before they can mutate state.
- Mounted integration tests run with `FLUMMOX_REQUIRE_FUSE=1`: seven coordinator
  tests and two mount tests pass, including refusal to restart mounted games.
- `just release` produces a Linux x86_64 archive and SHA-256 checksum. The
  extracted CLI launches successfully. `makepkg --force --nocheck --nodeps`
  produces the Arch package; the full test gate runs separately through `just ci`.
- A disposable clean Arch container installs the package and its declared
  dependencies, replaces the package while its background worker runs, then restarts
  the worker through the public CLI. All steps pass.
- The installed GUI launches under Xvfb with a software renderer. Its screenshot
  was inspected. This test found and fixed missing libXcursor and xkbcommon-X11
  runtime dependencies. Headless Settings previews cover the new restart button.
- `actionlint` accepts both workflows. The release workflow runs the same Arch
  install/upgrade/GUI startup smoke script. Remote execution passed for v0.0.1.

The installed-binary upgrade fixture replaces the same version; the protocol
fixture separately verifies rejection of an older client. Background workers from
before restart support require a logout/login after jobs are finished and games
using Maximum are decompressed. Installation instructions are in [install.md](install.md).

## Remaining release work

- Run actual native and Proton games from Maximum, including launch,
  patch, launcher verification, login remount, and decompressing. Produce matching
  compatibility reports with measured load-time and gameplay results.
- Exercise the folder/report dialogs in KDE and another desktop session. The
  headless tests cover selection parsing and layout, not desktop interaction.
- The tag-triggered release workflow passed and published v0.0.1, including
  native Linux x64/ARM64, macOS ARM64, and Windows x64 builds, Windows installer
  checks, Homebrew installation, and Arch install/upgrade/GUI startup. AUR and
  Homebrew updates also passed. See release-automation.md for the pipeline.
- Windows Steam, Epic, GOG and Heroic discovery, opt-in maintenance and the
  background worker are implemented for 0.0.2; manual lifecycle and real NTFS game
  tests remain. APFS compression has fixture evidence, but Mac
  interactive and real-game acceptance is pending. Direct WOF/Game Compressor
  comparisons, Mac FUSE, Bottles and community report sharing remain separate
  work.

Small-file grouping is a bounded sample reported separately from the overall
prediction. It is not a full-store allocation estimate. Decompressing and
deleting the original finish without pause or stop once filesystem switching
starts.

## Custom games locations

Settings > Locations supports both a collection of immediate game subfolders and
one individual game. Home shortcuts and escaped spaces are accepted. Tests cover
legacy registrations, collection discovery, hidden/symlink filtering, overlapping
locations, persistent registration, and removal without deleting game files.
The background worker protocol is version 8. A version 8 client asks an older idle
worker to restart and takes over; the restart command exists from
version 6, which is what 0.0.1 shipped.

Prioritized acceptance work and proposed product improvements are tracked in
[next-steps.md](next-steps.md).
