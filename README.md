# Flummox

Your games take up less space and still work. Named for the reaction to how
much comes back.

By default, Flummox tells the filesystem to store installed games compressed. The
files stay where they are, the game still launches, and nothing is packed into
an archive you have to unpack first. One command puts it all back.

Savings depend on the actual bytes and the drive's starting state. Encoded
video and audio often have little left to gain, but raw textures and stored
archive entries can still compress well. Flummox samples content instead of
rejecting a file because its name ends in `.dds`, `.zip`, or `.pak`.

A drive already mounted with compression has already collected part of the
saving. Flummox labels its sampled predictions as estimates of **additional**
space. Whole-drive free-space changes include other applications' writes and
are shown separately.

## Get it

No prebuilt downloads yet. Build it from source, which takes a couple of
minutes.

Arch and CachyOS:

```sh
git clone https://github.com/bybrooklyn/flummox
cd flummox/packaging && makepkg -si
```

Linux, with a Rust toolchain and FUSE development files installed:

```sh
git clone https://github.com/bybrooklyn/flummox
cd flummox
cargo build --release --features gui,pack-mount
```

That gives you `flummox` for the terminal and `flummox-gui` for the window.
Leave off both features if you only want native btrfs commands, and you skip
compiling the entire window stack with it.

On Windows with the Rust MSVC toolchain:

```powershell
cargo build --release --features gui
```

The same `flummox-gui` application builds on Linux and Windows. Rust selects
the storage implementation for the target at compile time. Windows uses the
operating system's WOF/LZX storage and does not need a filesystem driver.

## What works today

| | |
|---|---|
| **Linux games** | Steam (native, Flatpak and snap), Heroic installed manifests, Lutris, and custom game folders |
| **Linux storage** | Native btrfs compression; verified writable Maximum Space stores on ext4, XFS, F2FS, ZFS, and btrfs with FUSE |
| **Windows** | Local Steam discovery and custom folders on NTFS using transparent WOF/LZX compression |
| **Desktop** | Wayland and X11 on Linux; native window on Windows |

## What does not work yet

| | |
|---|---|
| **Linux native compression outside btrfs** | Maximum Space works through a writable FUSE store, but these filesystems have no in-place native backend |
| **Windows launcher parity** | Steam and manual folders work. Heroic, Xbox, GOG Galaxy, background maintenance, and Maximum Space are still Linux-only |
| **macOS** | Deferred until there is user demand |
| **Bottles** | Discovery is planned |
| **Flatpak build** | Not possible. The sandbox hides other processes, so Flummox could not tell whether a game was running, which is the check that keeps it from touching a game you are playing |

## Use the window

Open `flummox-gui` on Linux, choose Games, select your games, and press **Free
up space**. Analysis starts in the background. Search, drive and launcher
filters, and sorting help with larger libraries. Expanding a game shows its
path, compression preset, analysis, and recovery actions. Maximum compares
levels 9, 15, 19, and 22 per unique chunk and keeps the smallest result;
ties use the cheaper level. Balanced remains the quicker native default.

On Windows, choose a detected Steam game or paste another installed-game
folder, then press **Optimize**. Flummox reports files processed and actual
allocated bytes freed while Windows works. **Stop** finishes the current file
and keeps completed work valid. **Restore** removes WOF backing and leaves the
same files at the same paths.

The window theme and responsive layout are shared across targets. Linux-only
storage controls appear only when their backend and FUSE support are available.
Other desktop targets build the shared shell with compression disabled until a
safe storage backend exists.

Queue supports pause, resume, cancel, and retry. Closing the window leaves jobs
with the background coordinator; reopening reconnects. Analysis and compression
pause when a detected game is running. Current filesystem operations finish
before workers stop at 16 MiB range boundaries, including inside large files.

Drives lets you add a game folder and opt each library into maintenance. An
opt-in starts observing from that point, so existing installs are not all
compressed immediately. Subsequent installations and completed updates queue
work. The coordinator stays running after the window closes and starts at
desktop login while any library has maintenance enabled. Disabling the last
library removes Flummox's startup entry.

`Ctrl+F` focuses game search, `Ctrl+R` refreshes, and `Escape` clears selection
and closes details. Tab and Shift+Tab move focus. Reduce motion is saved in
Drives. Artwork comes from Steam's local cache; no artwork or telemetry is sent
to a server.

The command line shares ordinary compression and decompression jobs with the
window. Interrupting the CLI disconnects the client and leaves its job running:

```sh
flummox jobs
flummox jobs pause 12
flummox jobs resume 12
flummox jobs cancel 12
flummox jobs retry 12
```

The advanced `--force` and `--no-pause` paths retain their direct execution
behavior and share an operation lock with coordinator workers.

## Compare compression approaches

```sh
flummox benchmark /path/to/game --budget-mib 32 --json
```

This read-only experiment compares zstd levels 3, 9, and 15 in independent
128 KiB blocks with levels 9, 15, and 19 in frames up to 4 MiB. Every candidate
uses the same source samples, and every frame is decoded and checked against
its input. The report includes input coverage, encoded size, and encode/decode
time. It does not change the game's storage or measure recovered disk space.

Larger frames can reuse repetition outside btrfs's native compression window.
They also require more decompression work per random read. The frame-sampling
rows exclude store metadata and allocation costs. The complete store benchmark provides full-corpus measurements that include
metadata, content-defined chunk deduplication, verified restoration, and an
optional read-only or writable FUSE mount. Content-derived boundaries recover
sharing after insertions and removals instead of losing every later match.
Directory stores also hard-link matching chunk objects through a same-drive
pool, sharing physical allocation across games while keeping every store
independently readable.
The current store groups similar small files only when the group beats their
separate encodings. See the [small-file comparison](docs/benchmarks/2026-09-24-small-files.md)
for its measured space and read-time tradeoff.
The [full-game comparison](docs/benchmarks/2026-09-24-lzx.md) measures four
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

Source folders are retained. Builds with `pack-mount` can use a persistent
copy-on-write directory, so game writes and launcher file replacements survive
without changing the base store. A stopped layer can be committed into a new
verified store. Automatic installation replacement and GUI activation are
available when the app is built with `gui,pack-mount`. Activation keeps
the launcher's existing path, remounts at login, and retains the original until
an explicit reclaim action. See [store commands and format](docs/pack-store.md).

## Compress new downloads automatically

Turn this on once:

```sh
flummox hook on
```

Every game you install or patch after that arrives compressed, because the
filesystem compresses the bytes the first and only time they are written. It
costs nothing: there is no second pass, no rewriting, and no time added to the
download. Games already installed are untouched, so run `flummox compress` for
those.

The hook does not catch everything. The filesystem judges each file as it is
written, using whatever compression level the drive was mounted with, and it
skips files it guesses will not pay. It guesses wrong often enough to matter.
Measured on this machine: a 37 MB Half-Life texture archive arrived
uncompressed, and a later `flummox compress` brought it to 26 MB. A 27 MB
Unity asset file arrived at 20 MB, and a pass took it to 17 MB. Running
`flummox compress` after a large download is still worth doing.

So turn on both halves:

```sh
flummox hook on          # compress during the download, at no cost
flummox watch enable     # collect the rest once it finishes
```

`flummox watch enable` starts a small background service that runs at login.
It reads Steam's own manifests and compresses a game once its download has
settled, which picks up the files the filesystem skipped on the way past. It
waits behind anything else using the disk, and it will not touch a game you
are playing.

`flummox watch status` says whether it is on, and `flummox watch disable`
turns it off. Running `flummox watch` with no argument does the same work in
the foreground, if you would rather watch it happen.

`flummox hook off` stops it applying to future downloads and leaves everything
already compressed exactly as it is.

## Use it

```sh
flummox scan                        # every game, and what drive it is on
flummox estimate 105600             # what compressing it would save
flummox compress 105600 --preset max
flummox decompress 105600           # put it back
flummox status 105600               # how much is stored compressed
flummox log                         # what Flummox has done
flummox doctor                      # check this machine
flummox compatibility list --json  # export path-free qualification records
```

Import a locally produced compatibility qualification with `flummox
compatibility import report.json`. Reports identify a launcher key, build and
corpus hash. They contain no game title, install path, user name or machine
identifier. Maximum Space automation accepts only a matching verified build
whose measured load-time change stays within policy.

Steam reports some folders that are not games, like shared redistributables
and runtimes. Flummox filters the obvious ones, and you can correct the rest:

```sh
flummox exclude add "Steamworks Shared"   # stop seeing it
flummox exclude list
flummox exclude remove 105600
```

A hidden entry stays out of scans and will not be compressed even if you name
it directly.

Name a game by its Steam app ID, by `steam:105600`, or by part of its title.
Presets are `fast`, `balanced` and `max`. Every read-only command takes
`--json`.

Flummox will not start on a game that is running, updating or being verified.
If you launch the game while it is working, it pauses and waits for you, then
picks up where it left off. Pass `--no-pause` if you would rather it stop.

Ctrl-C stops between files, so a cancelled job leaves a game that is partly
compressed, still playable, and safe to resume.

## Is this safe?

Compression changes how the filesystem stores a file, not the file. Every byte
reads back identically, which the tests check by comparing checksums before and
after. Nothing is deleted and nothing is moved.

The one case to know about: if your drive has snapshots, rewriting a file
unshares it from its snapshots, so usage can go **up** until those snapshots
expire. Flummox checks and warns before it starts.

---

## How it works

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
A pre-push hook runs the same thing, so CI is a second opinion rather than the
first one. `unwrap`, `expect`, `panic` and indexing are banned throughout,
including in tests.

See [the job and compression design](docs/jobs-and-compression.md) for state
transitions, recovery guarantees, sampling policy, and current boundaries.

## Licence

AGPL-3.0-or-later. See [LICENSE](LICENSE).
