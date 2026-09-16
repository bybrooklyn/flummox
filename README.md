# Game Compressor

Compress installed games with zstd on Linux, and keep playing them.

This is a Linux counterpart to the Windows tool of the same idea: instead of NTFS
LZX compression, it uses the filesystem's own transparent zstd support, so a
compressed game is still an ordinary directory of ordinary files. Steam does not
know anything happened, and there is no archive to unpack before playing.

**Status: early.** The btrfs backend works and is in daily use on the author's
library. Support for ext4, XFS and F2FS — which is what the Steam Deck needs —
is designed but not built yet.

## What it does

- **Finds your games.** Steam libraries (native, Flatpak and snap), including
  the awkward cases: several app IDs sharing one folder, and Steam's "running"
  flag going stale after a crash.
- **Estimates before it acts.** Files are sampled rather than read whole, and
  the estimate models what the *filesystem* will do — btrfs compresses each
  128 KiB block separately and stores a block uncompressed when compressing it
  would not free a whole 4 KiB sector.
- **Compresses in place.** Games stay playable, and files Steam writes later
  inherit compression.
- **Only redoes what changed.** Each pass records a fingerprint per file, so
  after a game update it recompresses the handful of files that actually
  changed rather than the whole install.
- **Reverses cleanly.** `decompress` puts a game back.

It refuses to touch a game that is running, updating or being validated, and it
warns before compressing on a snapshotted subvolume, where rewriting extents can
*increase* usage until the snapshots expire.

## Install

Requires a recent Rust toolchain and a btrfs filesystem.

```sh
git clone https://github.com/bybrooklyn/gamecompressor
cd gamecompressor
cargo build --release
```

The binaries are `target/release/gamecompressor` (command line) and
`target/release/gamecompressor-gui` (desktop window).

## Use

```sh
gamecompressor scan                     # what is installed, and where
gamecompressor estimate 105600          # what compressing it would save
gamecompressor compress 105600 --preset max
gamecompressor status 105600            # how much is stored compressed
gamecompressor decompress 105600        # put it back
gamecompressor log                      # what this tool has done
gamecompressor doctor                   # check this machine
```

A game can be named by app ID, by `steam:105600`, or by part of its title.
Presets are `fast`, `balanced` and `max` (zstd 3, 9 and 15); `--level` overrides
them. Every read-only command takes `--json`.

Compression is worth least on a drive already mounted with `compress=zstd:1`,
because most of the gain is already banked — the tool says so rather than
quietly reporting a large number.

## How it protects you

This runs on your own machine with write access to entire game libraries, and it
reaches the kernel through `unsafe` ioctls. That earns some care:

- **Every file is opened through a held directory handle**, using `openat2` with
  `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, so a symlink swapped in between
  choosing a file and rewriting it cannot redirect the job elsewhere.
- **Jobs are sandboxed with Landlock** before any worker thread starts: the
  process can reach the game folder and little else, whatever the code does.
- **The manifest parser is bounded.** It reads files Steam writes, in a
  directory anything running as you can write to. Nesting is capped, after a
  property test found that about 20 KB of nested braces would overflow the stack
  and abort the process outright.
- **Nothing is deleted.** Compression rewrites a file's storage; the contents are
  unchanged, which the test suite checks by comparing checksums before and after.
- **Ctrl-C stops between files**, so a cancelled job leaves a valid, partly
  compressed game that can be resumed.

Dependencies are checked with `cargo deny check` (advisories, licences,
duplicate versions and sources).

## Does it actually save anything?

That depends entirely on the game. Already-compressed data — video, audio,
textures, packed archives — gives back nothing, and the tool skips it rather
than burning CPU to prove it. Measured here on btrfs already mounted with
`compress=zstd:1`, so these are gains *on top of* what the mount had:

| Game    | Install | Freed at zstd 15 |
|---------|---------|------------------|
| Celeste | 1.20 GB | 80.35 MB         |
| Balatro | 66.7 MB | 3.04 MB          |

Estimates are sampled, so treat them as a guide.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
