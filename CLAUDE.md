# Flummox

Compresses installed games and keeps them playable. One crate, two binaries:
`flummox` (command line) and `flummox-gui` (window, behind the `gui` feature).
AGPL-3.0-or-later.

## Before claiming anything works

```
just lint     # clippy -D warnings, all features, all targets
just test     # every test, all features
```

Both must be clean. `cargo check` passing proves neither.

## Before believing a measurement

Seven harnesses in one session produced clean tables that could not have
failed. Every one was a shell mistake rather than a wrong hypothesis, and each
returned numbers that looked like an answer:

- `du -b` implies `--apparent-size`, so an "on disk" column compared a file
  against itself and every ratio was 1.000. On btrfs neither `du` nor
  `stat %b` sees compression at all. `compsize` is what reads it, and it needs
  privileges this tool does not take. The sysfs counter
  `/sys/fs/btrfs/<uuid>/allocation/data/bytes_used` is readable and works.
- `cp` and GNU `cat` both reflink on btrfs through `copy_file_range`, so seven
  compression variants shared one set of extents and no variant ever wrote a
  byte. `filefrag -v` showing identical physical offsets is the tell. `dd`
  writes real bytes.
- `btrfs filesystem defragment` takes the algorithm as `-czstd` and the level
  as a separate `-L 15`. A `||` fallback turned the rejected `-czstd:15` into
  four runs of the same default command.
- `2>/dev/null` on a path that does not exist returns silence, which looks the
  same as a clean result.
- zsh does not word-split unquoted expansions, so `for f in $fields` ran once
  with every field joined into one string. Use `while IFS= read -r`.
- `timeout ... | tail` reports tail's exit status, so the pipeline claimed a
  process had survived when nothing about it had been checked.

So:

1. Include a control that must fail. Zeros must compress, random must not, a
   baseline must land on its known value. If the control is wrong, the numbers
   are noise whatever they say.
2. Never send stderr to `/dev/null` while establishing a result.
3. Never use `||` as a fallback in a measurement. It hides the first command
   failing and silently answers a different question.
4. Capture the exit status of the thing being measured, not of a pipe.
5. Check what the tool measures, not what the column is called.

## Code rules

1. No `unwrap`, `expect`, `panic!`, `assert!`, `unreachable!`, `todo!`, or
   indexing (`v[0]`, `&s[a..b]`). This holds in tests too; clippy.toml grants
   no test exemption. Use `.get(i)`, `ok_or_else`, `let ... else`,
   `unwrap_or_default`.
2. Tests return `testutil::TestResult` and report with `check`, `check_eq`,
   `check_ne` and `.ctx("what this was")?`.
3. `// SAFETY:` states the invariant the call relies on and where it is
   established. It does not claim an invariant the code does not need.
4. Never touch a real game library in a test. Use a temp directory, a fixture,
   or a stub. The machine this runs on has a 75-game Steam library.
5. The GUI runs jobs as child processes. Landlock is per process and
   irreversible, so a GUI that sandboxed itself to one game could never touch
   another.

## Prose rules

These come from an audit that counted the tics. The numbers are what they were
when the rule was written.

1. No em dashes. A sentence needing one is two sentences. (was 47)
2. No "X is not Y, it is Z" reversal. Say what the thing is, once. (was 14)
3. "rather than" only where it marks a real contrast. If cutting it loses no
   information, cut it. (was 56, now 44)
4. Never "on purpose", "deliberately", "by design". If a reader might think it
   an accident, say why the alternative is worse. (was 12)
5. Never "worth knowing", "the point is", "exactly the". Say the fact. (was 17)
6. A doc comment says what the item does and what a caller must honour. It does
   not argue for its own importance. Cap: 10 lines on a module header, 6 on an
   item. Longer belongs in the commit message.
7. Each measured anecdote appears once, in the commit that introduced it. Facts
   a caller needs (a limit, a kernel version) belong in the code; the story of
   how it was found does not.
8. Do not editorialise about failure. No "silently", "quietly", "lies",
   "nobody". Say what happens: "returns an empty list, so the caller cannot
   tell the directory was unreadable".
9. Commit subjects: imperative, under 72 characters, no full stop. Bodies say
   why, and do not repeat what a code comment already says.

## Things already learned, so they need not be rediscovered

- btrfs defrag does **not** change ctime, size or inode. The incremental pass
  depends on that, and `compressing_leaves_the_fingerprint_fields_alone` locks
  it in.
- A btrfs compression property on a directory is inherited by new files. That
  is how downloads can land compressed with no extra I/O. It carries no level:
  directories set to `zstd:1` and `zstd:15` produced byte-identical output,
  both at the mount's level. The write path also applies the kernel's
  heuristic and skips files a forced defrag does compress, measured as
  halflife.wad landing at 1.000 through the property and 0.692 after a pass.
  So the property is the cheap half and a pass still collects the rest.
- btrfs compresses each 128 KiB block on its own, with no matches carried
  between blocks. Against high-level large-window zstd on the same files:
  resources.assets 0.620 to 0.455, UnityPlayer.so 0.368 to 0.313, icudtl.dat
  0.399 to 0.352. No zstd level reaches past that, so the 5 to 16 percent it
  represents needs the pack tier.
- FIEMAP reports which extents hold compressed data, not how much they shrank.
  Measuring bytes saved needs `compsize`, which needs privileges this tool does
  not ask for, so savings are reported as estimates and labelled as such.
- lzo and zlib lose to zstd on real game files at every level measured, so
  per-file choice is worth making over the zstd level and not over the
  algorithm. Level 3 to 15 is worth 3 to 13 points depending on the file, at 5
  to 10 times the CPU, and level 15 is sometimes worse than 9.
- iced 0.14: `Space::new` takes no arguments, and the `theme` and `view`
  arguments of `iced::application` must be function items. A closure is
  inferred for one specific lifetime and will not satisfy `ViewFn`.
- The free-space delta is a whole-filesystem reading and includes other
  processes' writes. Never present it as a measurement of one job.
- Windows WOF decompresses any file opened for write, so a game update undoes
  compression and there is no directory inheritance to prevent it.
