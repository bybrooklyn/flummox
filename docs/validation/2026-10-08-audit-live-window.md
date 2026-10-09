# The audit fixes in a running window, 2026-10-08

Status: covers the Linux window on the `audit-fixes` branch. It does not
qualify real games and says nothing about Windows or a Mac.

## Setup

A debug build with `gui,pack-mount` ran under headless Gamescope and
Xwayland with the software renderer (`ICED_BACKEND=tiny-skia`). Every
process was started with `env -i` and a home, state, data, config, cache and
runtime directory inside one scratch folder, so nothing pointed at the real
home. The control for that isolation: `flummox scan` in this environment
printed "No games found" on a machine with a 75-game Steam library.

Two fixture games on btrfs, 101 MB and 17 MB, were added as a Games library
with `flummox jobs add-folder`. Each file repeats 18 KiB of text and 4 KiB of
random bytes. Input was `xdotool`. The window was captured with ImageMagick.

## Results

- **The window starts and lists the games.**
  [Overview](audit-live/overview.png) shows two games and 117.55 MB
  installed. The sidebar icons are the drawn ones.
- **The estimate agrees with the command line.** Both games read "Little to
  save". The drive is mounted with `compress=zstd:1`, and
  `flummox estimate` reported an extra saving of 4 percent of the whole game,
  which is under the 5 percent threshold now measured against the install.
- **A row opens when its title is clicked.** Decompress drew disabled on the
  uncompressed game. The mode buttons, Analyze and Advanced were present.
- **Compress runs from the window.** One click queued a Standard compression
  and the window stayed on Games. [Jobs](audit-live/jobs-after-compress.png)
  then listed it under History as Completed, 2 files. SHA-256 of all eight
  fixture files matched before and after.
- **A toast leaves the page where it was scrolled.** Settings was scrolled
  180 pixels with three wheel steps, then "Export local diagnostics" was
  clicked. The top 300 pixels of the page were identical, with 0 differing
  pixels, before the toast, with it showing and after it left. The toast sat
  above the bottom edge and did not cover the page controls.
- **Wheel scrolling finishes without further input.** After one step and
  after three, two captures two seconds apart were identical, and a one-pixel
  mouse move afterwards changed nothing. Six repeats of navigate, scroll and
  compare gave the same result.

## Observed and left unchanged

- One capture taken 0.8 seconds after three wheel steps showed the page 60
  pixels down. Captures at 2 seconds showed 180. This was a debug build with
  software rendering on a loaded machine, so slow frames are the likely
  cause. The cause was not established.
- The closed disclosure chevron and the toast's close mark are blocky at
  their size.

## Limits

Input was synthetic. The renderer was tiny-skia in a debug build, so nothing
here speaks to frame rate. No job was paused, cancelled or failed, and
Maximum was not used. The harness ended the window with SIGTERM.

## Other checks made the same day

- `actionlint` 1.7.12, without shellcheck, reports nothing for the three
  workflow files.
- The Mac command line binary links. `cargo build --target
  aarch64-apple-darwin --bin flummox` with zig as the linker produced a
  Mach-O arm64 executable with no undefined symbols, after `-liconv` was
  dropped from the link line because zig does not ship that library. The
  binary was not run. The window binary was not linked: it needs Apple's
  frameworks.
