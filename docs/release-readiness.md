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
  install/upgrade/GUI startup smoke script. Remote execution remains untested.

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
- Run the release workflow on GitHub after pushing the changes. The workflow
  builds Linux archives and Arch packages, checks installation and upgrade, and
  creates a draft for a matching version tag. Publish after desktop acceptance.
  There is no published release from this change.
- Windows parity, native Windows tests and direct WOF/Game Compressor comparisons
  remain separate work. macOS, Bottles, and community report sharing are deferred.

Small-file grouping is a bounded sample reported separately from the overall
prediction. It is not a full-store allocation estimate. Restore and reclaim
transactions finish without pause/cancel once filesystem switching starts.
