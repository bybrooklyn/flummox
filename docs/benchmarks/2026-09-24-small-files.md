# Small-file frame comparison

Regular files smaller than 512 KiB were copied from two game installs into
temporary corpora. Each corpus was packed twice at zstd level 9 using the same
source directory. The previous version 4 store and the new version 6 store
both passed full payload verification. Both version 6 stores were restored and
compared byte for byte with their source trees.

| Corpus and store | Serialized bytes | Metadata | Unique chunks | Build and verify | Random-read probe |
| --- | ---: | ---: | ---: | ---: | ---: |
| Celeste, 1,088 files, version 4 | 28,633,754 | 395,282 | 1,077 | 1.01 s | 17.7 ms |
| Celeste, version 6 | 17,130,612 | 246,015 | 182 | 1.46 s | 21.3 ms |
| RetroArch, 17,106 files, version 4 | 103,430,691 | 6,079,543 | 15,668 | 2.00 s | 1.38 ms |
| RetroArch, version 6 | 89,205,329 | 3,635,429 | 909 | 2.26 s | 2.01 ms |

Celeste held 63,748,528 logical bytes. Grouping removed 11,503,142 more
serialized bytes, or 40.2% of the previous store's size. RetroArch held
155,459,802 logical bytes and saved 14,225,362 more bytes, or 13.8% of the
previous store. Exact duplicate sharing was unchanged in each pair. The
random-read probe requested 27,980,163 bytes for Celeste and 715,540 bytes
for RetroArch from head, middle, and tail offsets in the first 256 files. It
is not a game-load trace. These numbers describe small-file subsets, not full
games or reclaimed drive space. The older WOF/LZX comparison measures different
complete corpora and should not be combined with this result.

Each pair used the repository's `pack benchmark` command on the same copied
directory. A fixture test covers shared-frame reads, hard links, copy-up,
restore, and invalid slice rejection. Files with identical contents remain on
the exact chunk-sharing path. The new store checks each candidate group against
separate encodings and keeps the group only when it clears a 4 KiB gain floor.
