# Linux workflow completion

The Linux desktop now connects matching local compatibility reports to automatic
Maximum Space recommendations, queues storage work with durable task parameters,
and offers native folder/report selection through KDialog or Zenity. It retains
the current sidebar, theme, and game-card structure.

## Validation

- `just ci`: clippy, tests, dependency policy, and prose pass.
- `just build`: Linux release CLI and GUI build with `gui,pack-mount`.
- `cargo check --no-default-features --offline`: CLI configuration builds cleanly.
- Fixture coordinator tests cover queued creation, rejection of a mismatched
  qualification, activation, compaction, previous-version pruning, original
  reclaim, and restoration with launcher updates preserved.
- Headless previews render the real Games, Drives, Settings, and Queue widgets
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
  dependencies, replaces the package while its coordinator runs, then restarts
  the coordinator through the public CLI. All steps pass.
- The installed GUI launches under Xvfb with a software renderer. Its screenshot
  was inspected. This test found and fixed missing libXcursor and xkbcommon-X11
  runtime dependencies. Headless Settings previews cover the new restart button.
- `actionlint` accepts both workflows. The release workflow runs the same Arch
  install/upgrade/GUI startup smoke script. Remote execution passed for v0.0.1.

The installed-binary upgrade fixture replaces the same version; the protocol
fixture separately verifies rejection of an older client. Coordinators from
before restart support require a logout/login after work is finished and mounted
installs are restored. Installation instructions are in [install.md](install.md).

## Remaining release work

- Run actual native and Proton games from Maximum Space, including launch,
  patch, launcher verification, login remount, and restoration. Produce matching
  qualification reports with measured load-time and gameplay results.
- Exercise the folder/report dialogs in KDE and another desktop session. The
  headless tests cover selection parsing and layout, not desktop interaction.
- The tag-triggered release workflow passed and published v0.0.1, including
  native Linux x64/ARM64, macOS ARM64, and Windows x64 builds, Windows installer
  checks, Homebrew installation, and Arch install/upgrade/GUI startup. AUR and
  Homebrew updates also passed. See release-automation.md for the pipeline.
- Windows launcher/maintenance parity, real NTFS game tests, and direct
  WOF/Game Compressor comparisons remain separate work. Native APFS compression is implemented for 0.0.2 and has native fixture evidence.
  Real-game acceptance is pending. Mac FUSE, Bottles, and community report sharing remain deferred.

Small-file grouping is a bounded sample reported separately from the overall
prediction. It is not a full-store allocation estimate. Restore and reclaim
transactions finish without pause/cancel once filesystem switching starts.

## Custom games locations

Drives & libraries supports both a collection of immediate game subfolders and
one individual game. Home shortcuts and escaped spaces are accepted. Tests cover
legacy registrations, collection discovery, hidden/symlink filtering, overlapping
locations, persistent registration, and removal without deleting game files.
The coordinator protocol is version 7; the version 6 restart command supports
upgrading the previous coordinator.

Prioritized acceptance work and proposed product improvements are tracked in
[next-steps.md](next-steps.md).
