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
  is how downloads can land compressed with no extra I/O.
- iced 0.14: `Space::new` takes no arguments, and the `theme` and `view`
  arguments of `iced::application` must be function items. A closure is
  inferred for one specific lifetime and will not satisfy `ViewFn`.
- The free-space delta is a whole-filesystem reading and includes other
  processes' writes. Never present it as a measurement of one job.
- Windows WOF decompresses any file opened for write, so a game update undoes
  compression and there is no directory inheritance to prevent it.
