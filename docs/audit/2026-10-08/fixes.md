# Fix status

Fixes are on the `audit-fixes` branch. Each row names the report and the
finding number in it. "Tested first" means a new test was run against the old
code and seen to fail before the fix went in. Where that was not done, the row
says how the fix was checked.

Three limits apply to everything below:

- Linux fixes were built, linted and tested on this machine, with the FUSE,
  btrfs and Landlock tests forced on (`FLUMMOX_REQUIRE_FUSE`,
  `FLUMMOX_REQUIRE_BTRFS`, `FLUMMOX_REQUIRE_LANDLOCK`).
- Windows fixes were cross-compiled and linted with `just win-lint`. None has
  run on Windows.
- Mac fixes were type-checked and linted with `just mac-lint`, which uses zig
  for the C dependencies. Nothing was linked or run on a Mac.

No fix was confirmed in the real window. The previews are rendered by the
software renderer from fixture state.

## Needs the repository owner

These cannot be done from a commit.

1. Move `RELEASE_SIGNING_KEY`, `AUR_SSH_PRIVATE_KEY` and
   `HOMEBREW_DEPLOY_KEY` into the `release` environment and delete the
   repository-level copies (13, 1.1).
2. Add a required reviewer and a `v*` tag ruleset (13, 1.1).
3. Pin the Arch container image by digest, Inno Setup by version and
   checksum, and the `cargo-about` version (13, 1.7). The values need network
   access to verify.
4. Run CI on Windows and on a Mac before trusting those fixes. Reports 11 and
   12 each end with a list of what to check on the real system.

## Needs a decision

| Report | Finding | Why it was left |
|---|---|---|
| 06 | 3 | A user with a mounted Maximum game cannot restart the worker after a protocol change. A fix lets Restart unmount a game that may be running. |
| 06 | batch enqueue | Needs a protocol version bump. |
| 10 | 2 | Heroic GOG games that were keyed by list position get a new id. Exclusions and artwork set under the old id stay there. No migration was written. |
| 11 | 9 | A pipe-owner check would lock the user out if the worker ever ran elevated. Confirm on Windows first. |
| 11 | 5c | Removable drives swapped at one letter. Needs a change to the saved settings format. |
| 11 | 15c | The tray icon is the generic one. Needs an `.ico` and a resource embed. |
| 09 | 10 | The confidence cut-offs (under 0.05 percent of bytes sampled is Low, under 0.5 percent is Medium) were chosen by the fixing agent. |
| 09 | 6 | Reports for custom-folder games now store a hash of the key. Reports saved before this no longer match and must be made again. |
| 01 | 20 | The first frame assumes a dark desktop. iced gives the desktop theme only after the window opens. |
| 01 | 18 | Escape closes the storage plan and leaves the compatibility form open, since closing it discards typed input. |
| 03 | 11 | The sidebar glyphs depend on the system font. Bundling an icon font is an asset decision. |
| 13 | 1.3 | Acceptance reports carry no commit hash, so the gate cannot tie one to the tagged commit. |

## Fixed

### 01 and 02, Linux window behaviour

All of 01 except 18 (partly) and 20. All of 02. Nineteen `app.rs` tests were
written before their fixes and all nineteen failed on the old code. The rest
test functions that did not exist before. The preview renderer now draws
widgets in their real state and fails on a blank page.

### 03, visual

Findings 1 to 15, except the icon font in 11. Shared builders and themed
controls are in `src/gui/theme.rs`. Contrast floors (3:1 for control edges and
bars, 4.5:1 for text) are unit tests there. Paused jobs are still counted
under Running.

### 06, job coordinator

Findings 1, 2, 4 to 13, 15 (two of three parts) and 16. Finding 14 has the
retry backoff and, from report 07, the phase is no longer overwritten. Eleven
tests were seen failing first. The receipt batching, the save throttle and
the buffered reply have no test.

### 07, Maximum stores

Findings 1 to 7 and 9 to 17, and the rename case of 8. The crash case of 8 is
open: `Overlay::open` would need a store reader, which changes its callers.
Fourteen tests were seen failing first, most against a mounted store. One
defect the audit missed was found and fixed: a renamed file's path kept a
trailing slash, so later requests by inode failed.

### 08, btrfs backend and sandbox

Findings 1, 2, 5 to 9, 11 to 13, 15 and 16, the CLI and worker halves of 3, 4
and 10, and most of 14. Open in 14: syscalls with the x32 bit set are not
matched. Nine tests were seen failing first on this machine's btrfs.
Finding 1 adds no early probe. A decompress that leaves a file compressed
fails that file.

### 09, estimates and database

Findings 1 to 12, 14 and the listed parts of 13. Sparse and preallocated
files are untouched. The audit's claim about a version 1 database without the
`hidden` table was wrong: version 1 already has it.

### 10, discovery

Findings 1 to 13, 15 (except case folding for offline folders on Windows)
and 16. Finding 14 is fixed on the service side: a scan past 120 seconds is
abandoned and games stop counting as idle. Eight of sixteen tests were seen
failing first.

### 11, Windows backend

Findings 1 to 4, 5a, 5b, 6 to 8, 10 to 14, 15a, 15b, 16, 18 and 19. Finding
17 has a test and no code change. Six were tested on Linux through shared
logic. The rest are compiled only.

### 12, Mac backend and command line

Part A: all twelve. Part B: all sixteen, with `doctor --json` returning an
error where the audit asked for output. Two of the new CLI tests were shown
to fail against the old code by mutation. Seven CLI fixes were made by
reading, since the sandbox cannot be undone inside a test.

### 13, CI, packaging and docs

Every item except the owner actions above. The release workflow and the new
CI jobs were checked as text and by unit tests of the scripts. None has run.
`just prose` now reads grep's status, scans tracked files and covers the
phrases from rules 5 and 8.

### Added along the way

- `just mac-lint`, with `tools/zig-cc-macos` and `tools/zig-ar`.
- `src/observer.rs`: the progress trait moved out of the Linux-only store
  module so every platform builds it.

## In progress when this was written

- 05, the Windows and Mac window.
- 04, wording and the glossary.
