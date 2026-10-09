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
  run on Windows. The second round ran the Windows library tests under Wine.
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
3. Run CI on Windows and on a Mac before trusting those fixes. Reports 11 and
   12 each end with a list of what to check on the real system.

## Decided and fixed in a second round

The first round left these as needing a decision. Each now has a default,
chosen by the session that ran the fixes, and is implemented. Change the
default if it is the wrong one.

| Report | Finding | What was done |
|---|---|---|
| 06 | 3 | Restart unmounts idle stores and the next worker remounts them. It refuses, naming the game, when a process runs from a store. A worker of another protocol version accepts Stop, Pause and status. |
| 06 | batch enqueue | `Command::EnqueueMany` and `jobs::request_many`. The protocol went from 8 to 9. The Linux window uses it. |
| 06 | 11, 13, 15 | The exclusion is written to history first and undone on failure. Mounts survive a failed start, with a FUSE test. A client that connects during start-up is told the worker is starting and retries. |
| 07 | 8 | The crash case: opening a layer with the store hides the store children of a folder whose whiteout it drops. |
| 08 | 1, 14, 15 | A decompress job probes the kernel with a scratch file before changing anything. The x32 numbers of the socket calls are denied. A no-copy-on-write file gets no predicted saving. |
| 09 | 6 | Old reports for custom folders are rewritten on load with the hashed key, so they still match and no path stays on disk. |
| 09 | 13 | A sparse file is priced by its allocated blocks, on Linux. |
| 10 | 2 | A Heroic game keeps its old position id as an alias, so exclusions and history still apply. |
| 10 | 15 | Offline folders are merged without regard to case on Windows. |
| 11 | 9 | The client accepts a pipe owned by the user or by the Administrators group and refuses any other. |
| 11 | 5c | A location's drive identity is saved. A location that moves to another drive gets a new baseline and queues nothing. |
| 11 | 15c | The tray draws a Flummox mark at run time. |
| 11 | 12, 17 | A custom game past the stamp limits gets a partial stamp and stays usable. The freed figure is cross-checked against the compressed file size. |
| 12 | B9, B11 | `watch` retries a failed job at 10, 60 and 300 seconds. `doctor --json` prints its checks. |
| 13 | 1.3 | A report carries the commit it was built from, and a stable release requires each report's commit to be an ancestor of the release. |
| 13 | 1.7 | The Arch image digest, Inno Setup 6.7.1 and cargo-about 0.9.2 are pinned. The values were looked up on 2026-10-08. |
| 01 | 20 | The desktop theme is read before the window opens, from `GTK_THEME` and then `gsettings` under a 300 ms limit. |
| 01 | 18 | Escape closes an untouched compatibility form and keeps a filled one, with a notice. |
| 03 | 11 | Sidebar icons and the small marks are drawn from rectangles in `src/gui/icon.rs`. No glyph depends on a system font. |
| 05 | unsupported page | Systems with no backend get a titled page with a card. `just other-lint` type-checks it for FreeBSD. |

## Cannot be done from this machine

| What | Why |
|---|---|
| Move the release secrets, add a reviewer and a tag rule | GitHub settings, owner only. |
| Run the Windows compress and decompress pass | Wine has no WOF and, without a display, no drive lookup. 108 of 122 Windows tests pass under Wine. The 14 that fail are listed in the Windows round's report, and one of them was a test defect that is fixed. |
| Run the Inno Setup script | Needs Windows. |
| Link or run anything on a Mac | No Mac toolchain. The Mac code is type-checked and linted only. |
| Run the new CI and release workflow steps | They run on GitHub. They were checked as text and through unit tests of the scripts. |
| Confirm any fix in the real window | The previews are software renders of fixture state. |

## Fixed in the first round

This section is the first round as it stood. Where it calls an item open, the
table above says how the second round closed it.

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

### 05, Windows and Mac window

All 25 findings, and the parity rows marked as drift. The two windows now
share `src/gui/shell.rs`: the sidebar, toast and its timing, the storage plan
panel, artwork tiles, keyboard shortcuts and the theme lookup. The decisions
the native window makes (which controls are enabled and why, which job a
folder owns, what a location change does) are pure functions in
`src/gui/native_rules.rs` with tests that run on Linux. Nothing was run or
drawn on Windows or a Mac.

Not added to the Windows and Mac window, because those backends report no
data for them: savings figures, progress bars with totals, bulk compression,
the drive and launcher filters, report import and diagnostics export. The
sidebar highlight there does not animate. `src/gui/unsupported.rs` still has
its own plain page, since no fourth target can be built here.

### 04, wording

The glossary is applied to the window, the command line output and the
current docs. `src/text.rs` holds the count and duration helpers. A paused
job is listed under Waiting. The toast sits above the job bar.

Left as they were: release notes, validation records and the changelog for
past versions, which describe the program as it was named then. The Linux
"Restart worker" button keeps its label because the install guide names it.
On a Mac the recovery action reads "Decompress to ordinary files" though it
replays a journal and puts a kept original back.

## State at the end

On the final commit of `audit-fixes`:

- `just lint`, `just win-lint`, `just mac-lint` and `just other-lint` pass.
- `cargo test --all-features` passes 444 tests with the three
  `FLUMMOX_REQUIRE_` variables set. Before the audit it passed 200.
- Under Wine, 108 of 122 Windows library tests pass.
- `python3 packaging/test_release.py` passes 19 tests and skips the one that
  needs minisign.
- `just prose` is clean.
