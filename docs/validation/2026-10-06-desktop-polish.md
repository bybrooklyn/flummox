# Desktop polish implementation, 2026-10-06

The [implementation plan](../plans/desktop-polish.md) remains the delivery contract.
This change implements the Linux desktop phase and brings the native front end
onto the shared three-page navigation and scrolling wrapper. It is not full
Windows parity or release acceptance.

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

## Validation

- `just lint`: passed, all Linux features and targets, warnings denied.
- `just test`: 136 library tests passed, including offscreen previews and motion,
  artwork, discovery cancellation and snapshot-ordering regressions. Six
  coordinator lifecycle tests failed because this sandbox rejects Unix socket
  creation with `Operation not permitted`. The complete test gate is not green.
- The six remaining integration targets were run separately: 21 tests passed.
  The single doctest passed. Filesystem-dependent tests retain their existing
  capability guards; passing does not establish FUSE or sandbox enforcement here.
- `python3 packaging/test_release.py`: eight tests passed, including the existing
  acceptance-check changes. The temporary test signer was used.
- Windows x64 GUI cross-compilation and Clippy over all targets passed with
  warnings denied. This is compilation evidence, not a Windows runtime test.
- `cargo deny --offline check`: passed using the cached advisory database;
  no claim of fresh advisory coverage. Prose and whitespace checks passed.
- Fixture screenshots were rendered without a display and inspected in wide,
  narrow, dark and light layouts. The preview exporter now converts Iced's BGRA
  desktop pixels to RGBA before saving PNGs.

Logs and previews from this run are under `/tmp/flummox-polish-*`. The selected
[dark Overview](desktop-polish/overview.png),
[light Overview](desktop-polish/overview-light.png),
[dark Games](desktop-polish/games.png),
[light Games](desktop-polish/games-light.png),
[Settings](desktop-polish/settings.png) and
[narrow Settings](desktop-polish/settings-narrow.png) fixtures accompany this
record. They are not screenshots of a running desktop session.

## Required follow-up

Run `just ci` in an environment allowing coordinator sockets. Reproduce the
reported queue disappearance in a real window and verify wheel/trackpad input,
rapid navigation, scroll restoration, jump links, keyboard input, native dialogs,
large libraries, offline locations and reduced motion. The code addresses stale
responses and widget identity; live reproduction has not been established here.

The native Mac/Windows front end still needs the full Settings sections, stored
appearance/motion controls and artwork integration. Windows Epic/GOG/Heroic
providers, collections, persistent secured coordinator, tray, maintenance,
login startup and installer lifecycle remain the later Windows phase.

No commit, push, tag or release was made: this workspace exposes `.git` read-only.
Native Mac runtime and all real-game qualification records remain pending.
Version 0.0.2 must remain unpublished until its acceptance records pass.
