# Experimental compressed stores

The `pack` commands store game payloads in independently readable,
content-defined chunks from 512 KiB to 4 MiB, targeting about 2 MiB. Similar
small files can share one frame when its encoded size beats separate files by
at least 4 KiB. Exact duplicate files keep their own shared chunk. These
chunks can reuse matches beyond btrfs's 128 KiB compression window. Their
content-derived boundaries converge again after inserted or removed bytes, so
unchanged regions of shifted archives and patched files can still share one
stored copy. This includes identical encrypted or already-compressed regions.
Unique high-entropy data still has little compression potential.

```sh
flummox pack benchmark /path/to/game --maximum --scratch-dir /path/to/scratch --json
flummox pack benchmark /path/to/game --maximum \
  --wof-lzx-helper /path/to/wof-lzx-helper --json
flummox pack create /path/to/game /path/to/game.flumpack \
  --maximum --pool /path/to/.flummox-pool
flummox pack create /path/to/other-game /path/to/other.flumpack \
  --maximum --pool /path/to/.flummox-pool
flummox pack verify /path/to/game.flumpack
flummox pack info /path/to/game.flumpack --json
flummox pack restore /path/to/game.flumpack /path/to/new-folder
flummox pack activate /path/to/game.flumpack /launcher/game/path
flummox pack compact /launcher/game/path
```

Creation keeps the source folder. The reported store size includes every
payload byte, the header, and index. Keeping both copies initially consumes
additional space. It is a serialized size comparison; filesystem allocation,
snapshots, and space already saved by native compression affect the actual
drive cost. Full-store benchmarks need temporary disk space and delete their
own temporary store when finished.

Store summaries split unique payloads into raw, zstd-compressed, and zero-filled
bytes, then report bytes removed by exact chunk sharing separately. This makes
a weak result diagnosable: a large raw count means the unique content did not
shrink, while a large metadata count points to many paths or chunk references.
Managed installs retain this verified summary and show it in the expanded game
row.

Passing `--pool` creates a version 7 directory store. Each encoded chunk is a
content-addressed file hard-linked from the pool into every game store that
uses it. Matching chunks across games therefore occupy physical storage once.
The pool and stores must use the same filesystem. Each store keeps its own hard
links and remains readable if the pool directory is removed or unavailable.
The GUI creates a hidden `.flummox-pool` beside the chosen store path, so stores
placed in the same folder share automatically.

Deleting a store releases its links. Unreferenced pool links can then be
removed safely:

```sh
flummox pack pool-prune /path/to/.flummox-pool
```

The command holds the pool lock while pruning and removes only objects whose
pool entry is their last hard link. Managed version pruning performs this pool
cleanup automatically. Store size reports describe the independently usable
store; `shared_bytes` reports the portion whose allocation is also used by
another game store.

Default compression is zstd level 9. `--maximum` compares levels 9, 15, 19,
and 22 on each unique chunk and keeps the smallest encoding. Any level from 1
through 22 is available for explicit experiments. Raw fallback prevents incompressible payloads from
expanding beyond their input length; metadata still has a cost. Zero chunks
have no stored payload. Whole-store size can exceed source size on small or
incompressible inputs.

## Reads and mounts

```sh
cargo build --release --features pack-mount
mkdir /path/to/empty-view
flummox pack mount /path/to/game.flumpack /path/to/empty-view \
  --writes /path/to/game-writes
```

The mount serves file reads and executable files directly from the store.
It requires Linux FUSE, `/dev/fuse`, and an unprivileged mount helper. It is
owner-only and mounted with device and setuid execution disabled. Without
`--writes` it is read-only. With `--writes`, unchanged files remain in the
compressed store and the first modification copies that file into an owner-only
update directory. New files, directories, relative symlinks, truncation,
deletion, mode and timestamp changes, and atomic file replacement persist there.
Whole base directories can be renamed after their visible children are copied
into the update layer. This includes changed, new, and deleted children.
Deletion and rename tombstones reach disk before upper data is moved or removed.

Keep the command running while using the view; Ctrl-C unmounts it. External
unmounts are supported. A crash may require `fusermount3 -u` to clear a stale
mount. Mounting over a populated folder is refused. Do not edit the update
directory directly. Managed activations recognize their own disconnected
`flummox-pack` mount and clear it with the installed FUSE helper before remounting.

`pack activate` is the managed form for installed games. It verifies that the
store still matches every source path and byte before changing the launcher
path. It records the transaction, renames the original folder to a hidden
sibling, and mounts the writable store at the original path. The background
coordinator owns the mount, restarts it after a coordinator restart or login,
and stays running while any managed install exists. The GUI exposes the same
create, activate, compact, reclaim, and restore actions in each expanded game
row.

The original folder remains as a rollback copy until this explicit command:

```sh
flummox pack reclaim /launcher/game/path
```

Reclaiming is the step that releases the original folder's disk allocation.
After reclaiming, `pack rollback` reconstructs ordinary files from the base
store and persistent update layer. Before reclaiming, rollback merges launcher
updates into the retained original and moves it back into place. Both paths
keep downloaded patches and newly created files.

Large launcher updates can be folded into a replacement store without taking
the game path offline for the full build:

```sh
flummox pack compact /launcher/game/path
```

Compaction reads the mounted merged view while normal reads and writes remain
available. The final switch briefly rejects new mutations with `EBUSY`. If any
write occurred during the build, Flummox discards the replacement and asks for
a retry, so a late launcher update cannot be lost. The current store and update
layer are recorded before unmounting, and the replacement is recorded before
remounting, allowing coordinator restart recovery on either side of the switch.

The previous store and update layer remain until the replacement has been
tested. Reclaim them explicitly:

```sh
flummox pack prune /launcher/game/path
```

Only one previous version is retained. Another compaction is refused until it
is pruned.

For an unmanaged or already stopped writable layer, merge changes into a new
store directly:

```sh
flummox pack commit /path/to/game.flumpack /path/to/game-writes \
  /path/to/game-v2.flumpack --maximum --scratch-dir /path/to/scratch
```

Commit refuses a live update layer. It restores and merges into scratch space,
then builds and verifies a new store. It needs enough scratch space for a full
uncompressed install. The old store and update layer remain for rollback.
Native GUI compression remains the default.

The reader verifies each newly decoded chunk against BLAKE3. Reads are limited
to 4 MiB per request and may cross several chunks. Its decoded chunk cache holds
eight chunks, up to 32 MiB. Kernel file caching also applies to mounted reads.
Verification rereads all unique payloads and bypasses that cache.

## Format versions

| Region | Layout |
|---|---|
| Header, 64 bytes | `FLUMPK01`, little-endian version u32, maximum chunk size u32, index offset u64, index length u64, index BLAKE3 digest |
| Payload | Contiguous unique raw or zstd chunks; zero chunks occupy no bytes |
| Index | UTF-8 JSON containing ordered entries and chunk descriptors |

Each chunk descriptor contains its offset, encoded length, decoded length,
codec, and decoded BLAKE3 digest. Ordinary files refer to chunk IDs in order.
Version 2 monolithic stores use content-defined chunks; non-final chunks are at least
512 KiB and every chunk is at most 4 MiB. The reader also accepts version 1
stores, where all non-final chunks are exactly 4 MiB. The index preserves Unix
filename bytes using the same lossless path encoding as job IPC.

Version 3 is the original directory store. Its `manifest` uses the same authenticated
header and index, while its `chunks` directory contains owner-controlled chunk
objects with authenticated descriptors. Those objects may be hard-linked to a
shared pool and other version 3 stores. Version 3 uses the same content-defined
boundaries as version 2. Readers use only the links inside the store.

Version 4 extends the monolithic store with internal hard-link identity,
extended attributes, ACL data exposed through those attributes, and special
permission bits. Version 5 applies the same metadata model to directory stores.
Version 6 adds small-file slices to monolithic stores; version 7 adds them to
directory stores. Each slice names one chunk, an offset, and a length. The
index requires every shared frame to be covered exactly, with no gaps or
overlaps, before any read. A request decodes one bounded frame and returns only
the named file's bytes. Readers continue to accept versions 1 through 5.
Hard links that also point
outside the game folder are refused because removing the original install
would otherwise change their meaning.

Limits are 32 MiB of encoded index, 100,000 entries, 1,000,000 chunks, and
4,096 bytes per path. Parents must precede children. Absolute entry paths,
escaping parents, duplicate names, children under files, invalid lengths,
unreferenced chunks, overlapping payloads, truncation, and trailing data are
rejected. Symlink chains are resolved within the index with a 40-link bound.
Absolute targets, cycles, and links escaping the root are refused.

Checksums detect corruption. They do not establish who created an archive;
the parser validates structure and paths even when index checksums match.

## Publication and recovery

Creation opens sources through an anchored directory, refuses symlink swaps,
and checks file identity before and after reads. It rescans the complete tree
after writing and verifying the temporary archive. Changes to paths, lengths,
timestamps, modes, or symlink targets abort publication. This detects observed
changes; it is not a filesystem snapshot. Pack games while the game and
launcher are idle.

The output is flushed before publication. Creation uses a no-replace publish;
restoration uses a no-replace directory rename. Both flush the destination
parent afterward. Existing files, directories, and symlink destinations are
refused. A cancellation or corruption error leaves the final destination
unpublished. Temporary files may remain after an abrupt process kill.

Restoration and update commit write regular files in a private directory, create symlinks only
after all payload writes, then applies directory permissions. File bytes,
file modes, special permission bits, modification times, extended attributes,
ACLs, internal hard-link identity, empty files, directories, and internal
symlink targets are preserved. Ownership belongs to the restoring user.
Whole zero chunks restore and copy up as sparse holes. The exact source extent
layout is not retained. Symlink attributes and links crossing the game-folder
boundary are outside the current store metadata model. Device nodes, sockets,
and FIFOs are refused during creation.

## Validation and comparisons

Tests cover random reads, cross-chunk boundaries, round trips, corruption,
truncation, path validation, overwrite refusal, cancellation, duplicate data,
and incompressible controls. A separate FUSE test reads files, follows a link,
executes a fixture, refuses writes, and unmounts. Its CI job requires FUSE;
other test environments announce a skip when the device is unavailable.

`pack benchmark` builds and verifies the complete corpus and includes metadata
and 4 KiB allocation in its size report. Its read probes exercise head, middle,
and tail offsets in up to 256 files. They are not game-load traces. Passing a
helper built from `tools/wof-lzx-helper.c` also measures a round-trip-verified
32 KiB LZX proxy over the same files, including WOF's offset table and per-file
allocation. The benchmark rejects mismatched file and byte counts and emits a
portable corpus fingerprint. `tools/measure-wof-lzx.ps1` obtains the final
native Windows value from `compact.exe` on a disposable copy. See the
[September 2026 comparison](benchmarks/2026-09-18-lzx.md) for method and
results.
