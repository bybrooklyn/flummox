# Code cleanup backlog

From the comment pass of 2026-10-08. Four readers went through the modules
that had almost no comments and reported what they would simplify and what
looked wrong. Nine of the reported bugs were fixed on 2026-10-08 and are no longer listed.
None of the items below has been applied or tested. Line numbers are from before that pass.

## Possible bugs, unverified

Maximum Space:
- `pack/overlay.rs` `copy_up`: copied-up symlinks and directories are not
  synced, unlike files.

Window:
- `gui/app.rs` `saving_order` is computed only when a sort is chosen, so games
  analysed later keep their old position under the Saving sort.
- `gui/app.rs` `send_many` enqueues several games with no space plan review.
  Each worker still rechecks free space before it writes.

Windows and Mac, which could not be run:
- `windows/mod.rs`: a user stop leaves `windows-job.json`, which blocks other
  games until the same folder is run again.
- `macos.rs`: the command-line `Recover` repeats the recovery loop without
  taking `native.lock`.
- `gui/native.rs`: on Mac a failed timed scan leaves `refreshing` set.
- `desktop_discovery.rs`: the Steam busy mask omits the downloading, staging
  and committing bits that the Linux detector checks.
- `native.rs`: replacing `"\ "` with a space also rewrites a Windows path
  whose folder name starts with a space.

## Simplifications

Shared helpers that would remove repeated code:
- One Steam discoverer. Linux, the shared desktop catalog and `macos.rs` each
  have their own, with different limits and state masks.
- One typed-path resolver for `gui/native.rs` and `desktop.rs`.
- One atomic JSON write for `desktop.rs`, `desktop_jobs.rs`, `libraries.rs`
  and `macos.rs`, and one lock-file open for the five places that do it.
- `jobs/service.rs`: a `put_setting` helper with named row numbers, a
  `sync_autostart` helper, and one lookup for "job no longer exists".
- `jobs/packs.rs`: the four transactions open with the same
  canonicalize-then-find-install lines.
- `pack/overlay.rs` `copy_up` and `pack/format.rs` `validate` each handle
  `File` and `SlicedFile` in two near-identical arms.
- `pack/create.rs`: `create_observed` and `create_shared_observed` share their
  level check, space plan and verification tail.
- `pack/mount.rs`: the overlay-lock chain appears about fifteen times.
- `gui/view.rs`: search and filter controls are built twice, for compact and
  wide layouts, and the job kind is matched identically in two row builders.

Dead or redundant code:
- `native::add_folder`, `native::discover`, `windows::recover_folder` and
  `macos::discover_steam` have no callers.
- `gui/app.rs`: `State::polling` is only ever true on Linux, `motion_easing`
  returns the same value from both arms, and `reduced_motion` duplicates
  `motion == Reduced`.
- `compatibility.rs` `filename` hashes JSON that `identity` already hashed.

Long lines rustfmt leaves alone, mostly widget builders: `gui/view.rs`,
`gui/native.rs`, `qualification.rs`, `pack/cli.rs`.

Performance, if large stores feel slow: `Overlay::children` and
`Reader::hardlink_aliases` scan every entry per call, so listing a folder in
a 100,000-entry store is linear each time.
