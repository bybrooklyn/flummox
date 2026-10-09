# How it works

The mechanism behind Standard compression, and the benchmark commands that
compare it with larger-window stores. State transitions and recovery are in
[the job and compression design](jobs-and-compression.md).

## Mechanism

The rest of this is for people who want the mechanism.

**Compressing.** btrfs can store any file compressed, and decompresses blocks
as they are read, which is why the game neither knows nor cares. Flummox drives
`BTRFS_IOC_DEFRAG_RANGE` once per file to rewrite it compressed at a chosen
zstd level, and sets a property on the folder so files Steam writes later
inherit it.

**Estimating.** Reading a 60 GB install to predict the result would cost as
much as doing it. Flummox samples evenly spaced blocks and models what the
filesystem will actually do: btrfs compresses each 128 KiB block separately and
keeps a block uncompressed when compressing it would not free a whole 4 KiB
sector. It then reads which extents are already compressed, via `FIEMAP`, so it
never promises a saving that a previous pass already took.

**Only redoing what changed.** Each pass records size, inode, mtime and ctime
per file in a small SQLite database. After a game update, only the files that
actually changed are recompressed. Compressing does not disturb any of those
four fields, which a regression test pins down.

**Finding games.** Flummox parses Steam's own `libraryfolders.vdf` and
`appmanifest_*.acf`. That means handling the awkward parts: several app IDs
sharing one install folder, and Steam's "running" flag still being set after a
crash. It cross-checks against live processes rather than trusting the flag.

**Safety machinery.** Files are opened relative to a held directory handle with
`openat2` and `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, so a symlink swapped in
after the scan cannot redirect a write. Each job restricts itself with Landlock
before any worker thread starts, so the process can reach the game folder and
little else. The manifest parser caps nesting, after a property test found that
about 20 KB of nested braces would overflow the stack and abort the process.

**Checks.** `just ci` runs clippy at deny-warnings, the full test suite, and
`cargo deny check` for advisories, licences, duplicate versions and sources.
The pre-push hook in `.githooks/`, installed with `just hooks`, runs the same
thing, so CI is a second opinion after the first. `unwrap`, `expect`, `panic` and indexing are banned throughout,
including in tests.

See [the job and compression design](jobs-and-compression.md) for state
transitions, recovery guarantees, sampling policy, and current boundaries.

## Compare compression approaches

```sh
flummox benchmark /path/to/game --budget-mib 32 --json
```

This read-only experiment compares zstd levels 3, 9, and 15 in independent
128 KiB blocks with levels 9, 15, and 19 in frames up to 4 MiB. Every candidate
uses the same source samples, and every frame is decoded and checked against
its input. The report includes input coverage, encoded size, and encode/decode
time. It does not change the game's storage or measure recovered disk space.

Larger frames can reuse repetition outside btrfs's compression window.
They also require more decompression work per random read. The frame-sampling
rows exclude store metadata and allocation costs. The complete store benchmark provides full-corpus measurements that include
metadata, content-defined chunk deduplication, verified decompressing, and an
optional read-only or writable FUSE mount. Content-derived boundaries recover
sharing after insertions and removals instead of losing every later match.
Directory stores also hard-link matching chunk objects through a same-drive
pool, sharing physical allocation across games while keeping every store
independently readable.
The current store groups similar small files only when the group beats their
separate encodings. See the [small-file comparison](benchmarks/2026-09-24-small-files.md)
for its measured space and read-time tradeoff.
The [full-game comparison](benchmarks/2026-09-24-lzx.md) measures four
complete installs against a verified 32 KiB WOF/LZX proxy. Flummox retained
59.70% of the 1.81 GB corpus versus 65.51% for the proxy, using 105.2 MB less
allocated space. A direct Game Compressor claim still needs the included Windows
`compact.exe` measurement. Related-game pool tests saved another 227.5 MB,
or 14.76% of already-compressed allocation, across two game families.

```sh
flummox pack benchmark /path/to/game --maximum --json
flummox pack benchmark /path/to/game --maximum \
  --wof-lzx-helper /path/to/wof-lzx-helper --json
flummox pack create /path/to/game /path/to/game.flumpack --maximum
flummox pack verify /path/to/game.flumpack
flummox pack restore /path/to/game.flumpack /path/to/new-folder
flummox pack activate /path/to/game.flumpack /launcher/game/path
```

Source folders stay. Builds with `pack-mount` can use a persistent
copy-on-write directory, so game writes and launcher file replacements survive
without changing the base store. A stopped layer can be committed into a new
checked store. Switching a game to Maximum from the window is
available when the app is built with `gui,pack-mount`. Switching keeps
the launcher's existing path, mounts the game again at login, and keeps the original until
you delete it. See [store commands and format](pack-store.md).
