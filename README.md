# Flummox

Your games take up less space and still work. Named for the reaction to how
much comes back.

Flummox tells the filesystem to store your installed games compressed. The
files stay where they are, the game still launches, and nothing is packed into
an archive you have to unpack first. One command puts it all back.

How much you get back depends entirely on the game. Video, audio, textures and
packed archives give back nothing, and Flummox skips them instead of burning
CPU to prove it. A few real examples from one library, estimated at the
strongest setting:

| Game                  | Installed | Estimated saving |
|-----------------------|-----------|------------------|
| Detroit: Become Human | 65.8 GB   | 4.34 GB          |
| Warframe              | 55.3 GB   | 958 MB           |
| Celeste               | 1.20 GB   | 45 MB            |
| Just Cause 3          | 88.3 GB   | 56 MB            |

Just Cause 3 is the honest counterexample: 88 GB that is already packed tight,
so there is almost nothing to win. Flummox tells you that before it does any
work, which is the whole reason it estimates first.

Those figures come from a drive **already** compressing everything at the
weakest setting, so they are gains on top of that. On an uncompressed drive,
expect more.

## Get it

No prebuilt downloads yet. Build it from source, which takes a couple of
minutes.

Arch and CachyOS:

```sh
git clone https://github.com/bybrooklyn/flummox
cd flummox/packaging && makepkg -si
```

Anything else, with a Rust toolchain installed:

```sh
git clone https://github.com/bybrooklyn/flummox
cd flummox
cargo build --release --features gui
```

That gives you `flummox` for the terminal and `flummox-gui` for the window.
Leave off `--features gui` if you only want the command line tool, and you skip
compiling the entire window stack with it.

## What works today

| | |
|---|---|
| **Games** | Steam, including native, Flatpak and snap installs |
| **Drives** | btrfs |
| **Desktop** | Any Linux desktop. The window runs on Wayland and X11 |

## What does not work yet

| | |
|---|---|
| **ext4, XFS, F2FS** | Designed, not built. This is what the Steam Deck's internal drive uses, so the Deck is not supported yet |
| **Windows and macOS** | Planned. Windows will use its own compression, not zstd |
| **Heroic, Lutris, Bottles** | Planned. Steam only for now |
| **Flatpak build** | Not possible. The sandbox hides other processes, so Flummox could not tell whether a game was running, which is the check that keeps it from touching a game you are playing |

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
```

Name a game by its Steam app ID, by `steam:105600`, or by part of its title.
Presets are `fast`, `balanced` and `max`. Every read-only command takes
`--json`.

Flummox refuses to touch a game that is running, updating or being verified.
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

## Licence

AGPL-3.0-or-later. See [LICENSE](LICENSE).
