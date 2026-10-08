# Pack tier bug audit

Read-only audit. Nothing was built or run, so every finding rests on code reading. CONFIRMED means I traced the full path through the code (and through fuser 0.18 source where relevant); it does not mean reproduced.

All paths are under `/home/brook/data/gamecompressor/`.

## Findings

### 1. Deleted names keep their inode number, so two different files can share one: high, CONFIRMED
- **Where:** `src/pack/mount.rs:60-77` (`Nodes::inode`, `Nodes::alias`), `:405-422` (`remove`: "The inode table is left alone"), `:262-277` (`path`).
- **Defect:** unlink never removes a path from `Nodes`, so a new file created at a deleted name gets the old inode number back. If that inode has a second live name, the kernel treats two unrelated files as one.
- **Scenario (link through the mount):**
  1. Create `f.tmp` (inode N), `link(f.tmp, f)`, `unlink(f.tmp)`.
  2. Create `f.tmp` again: `attr_path` → `Nodes::inode("f.tmp")` returns N again.
  3. `path(N)` returns the first visible path, now `f.tmp`. Reads and getattr on `f` serve the new temp file's bytes.
  4. After `link(f.tmp, f2)` and `unlink(f.tmp)`, `path(N)` resolves to `f`. Reads of `f2` return `f`, and writes to `f2` land in `f`'s upper file.
- **Scenario (base hard links T and A):** delete A, create a new A. Every write to the new A is applied to T; A stays empty.
- **Persistence:** a compaction built from the live mount reads the wrong bytes and stores them.
- **Secondary effect:** with a single-name inode, an old fd held across unlink and recreate starts reading and writing the new file.
- **Fix:** on unlink, rmdir and rename-replace, drop the path from `paths` and from the inode's list. Give any later creation at that path a fresh inode number.

### 2. A failed recovery erases `Reclaiming`, and rollback then publishes a half-deleted original: high, CONFIRMED
- **Where:** `src/jobs/packs.rs:485-487`, `src/pack/install.rs:299-325` and `:532-539`.
- **Defect:** `recover_packs` overwrites any phase with `Attention` on error, and `recover` treats `Attention` as mountable. The journal state is lost while `backup_path` stays set.
- **Scenario:**
  1. Reclaim is interrupted partway through `remove_dir_all(backup)` (crash, or EACCES on a mode 0555 folder inside the original).
  2. On restart, `recover` fails once: the store's drive is not mounted yet, or the same EACCES repeats.
  3. Phase becomes `Attention`. Five seconds later the retry mounts and sets `Mounted` with the message "rollback copy retained".
  4. The user runs Restore. `current_backup` is the partial folder, `apply_to` succeeds when the layer is small, and `publish` renames it to the game path.
- **Outcome:** rollback reports success and removes the record, and the game folder is incomplete. The store and layer still exist, so the data is recoverable by hand.
- **Fix:** keep `Reclaiming`, `Pruning` and `Switching` in the record when recovery fails, and carry the failure in the message or a separate flag. `recover` must not mount past an unfinished reclaim.

### 3. The compaction switch unmounts under a running game: high, CONFIRMED
- **Where:** `src/jobs/packs.rs:359-381` (no `process_using` check, unlike rollback and reclaim), `src/pack/mount.rs:214-216`.
- **fuser behaviour:** for a non-root user, `umount_impl` in `fuse_pure.rs` falls back to `fusermount3 -u -q -z`. That is a lazy unmount with the result ignored. `join()` then waits for the session thread, which runs until the last open file or cwd is released.
- **Scenario:** the game is started during the long build and writes nothing in its folder, so the generation is unchanged and the switch proceeds. The same happens when `flummox pack compact` is run while the game is open.
- **Outcome:**
  - The game path detaches to the empty mount point, so every new open by path gets ENOENT.
  - `frozen` is still held, so every write on an already-open file gets EBUSY.
  - `mounted.stop()` blocks until the game exits. For the direct `PackCompact` command that is the coordinator's main loop.
- **Docs mismatch:** `docs/pack-store.md:124-125` says the switch "briefly rejects new mutations".
- **Fix:** after `freeze()`, check `busy::process_using(&canonical)`. If busy, discard the new store and ask for a retry.

### 4. Reads and writes on an unlinked or renamed-over open file fail with EIO: medium, CONFIRMED
- **Where:** `src/pack/mount.rs:433-436` (handles carry no state), `:105-109` (a replaced inode loses its path), `src/pack/overlay.rs:203`.
- **Scenario:** process A has `config.dat` open. Process B writes a temp file and renames it over `config.dat`, or unlinks it. A's next uncached read or page fault resolves the inode to no path ("Missing inode path" or "File was deleted") and returns EIO. For a mapped executable or library that is a SIGBUS.
- **Fix:** keep an open upper fd, or a store entry reference, per file handle. Defer the trash removal until release.

### 5. `RENAME_EXCHANGE` runs as a destructive plain rename: medium, CONFIRMED
- **Where:** `src/pack/mount.rs:941-946`.
- **Key line:** `!flags.contains(RENAME_EXCHANGE | RENAME_WHITEOUT)`. `RenameFlags` is a bitflags type, so `contains` is true only when both bits are set. Either flag alone passes.
- **Scenario:** `renameat2(a, b, RENAME_EXCHANGE)` (for example `mv --exchange`, or an atomic-swap saver) replaces `b` with `a`. `a` disappears and `b`'s old content is gone, while the kernel swaps its dentries.
- **Fix:** use `flags.intersects(...)` and return EINVAL.

### 6. A file created read-only cannot be written through its own descriptor: medium, CONFIRMED
- **Where:** `src/pack/overlay.rs:394-399` (upper file created with the requested mode), `src/pack/mount.rs:719-722` (write reopens by path with `write(true)`), `:806` (truncate does the same).
- **Scenario:** `cp -p` or `cp -a` of a 0444 file into the game folder, or `install -m 444`, creates the file 0400/0444 and then writes. The daemon's reopen gets EACCES and the copy fails. `fchmod` to read-only before the last write fails the same way.
- **Fix:** keep the fd from `create_file` or `open` in the file handle. Alternatively create upper files owner-writable and report the requested mode separately.

### 7. Extended attributes are set after the file is made read-only: medium, CONFIRMED by reading (kernel rule from `xattr_permission`)
- **Where:**
  - `src/pack/restore.rs:101-102`, `:112-113`, `:133-134`
  - `src/pack/overlay.rs:252-253`, `:289-290`, `:320-321`, `:658-660`, `:668-669`
- **Defect:** `set_permissions` runs before `apply_xattrs` or `copy_xattrs`. Setting a `user.*` attribute needs write permission on the inode, so a 0444 file or 0555 folder returns EACCES for a non-root owner.
- **Scenario:** a game holds a read-only file with `user.DOSATTRIB` (Wine or Proton) or any other user xattr. Create and verify succeed. `pack restore`, rollback after reclaim, `verify_restored`, and copy-up on rename or chmod all fail.
- **Outcome:** after reclaim the user cannot get ordinary files back without a code change.
- **Test gap:** the round-trip tests set xattrs only on writable files.
- **Fix:** apply xattrs before the mode in every one of those places.

### 8. A whiteout covering a subtree is dropped when a folder lands on it by an ancestor rename: medium, CONFIRMED
- **Where:** `src/pack/overlay.rs:691-694` (`reveal`'s `retain`). The same rule is at `:111` in `open`.
- **Scenario:** the store holds `data/sub/deep.bin`.
  1. `mv data/sub elsewhere` writes one whiteout, `data/sub`.
  2. Build `newdata/sub/new.bin`, then `mv newdata data`.
  3. `reveal("data")` sees an upper `data/sub` and drops the `data/sub` whiteout. It adds no whiteouts for the store children.
- **Outcome:** the stale `data/sub/deep.bin` reappears beside `new.bin`, and survives into compaction and restore.
- **Test gap:** `a_recreated_or_replaced_folder_shows_only_its_own_files` covers only whiteouts at the moved path itself.
- **Crash variant:** `open` does the same if a crash lands between `mkdir` and the journal write.
- **Fix:** when dropping a whiteout whose store entry is a directory, insert whiteouts for that entry's store children, as the `deleted.remove(path)` branch does.

### 9. An interrupted prune of a directory store can never be finished: medium, CONFIRMED
- **Where:** `src/pack/install.rs:433-440`.
- **Defect:** `finish_prune` requires `Reader::open(previous)` to succeed whenever the path exists. A crash during `remove_dir_all` of a version 7 store leaves a missing manifest or missing chunk objects, so the open fails.
- **Outcome:**
  - Recovery sets `Attention`, then `Mounted` with `previous_store_path` still set (same phase loss as finding 2).
  - Every later prune fails with "is not a store, so it was left alone".
  - `pack_compact_observed` refuses to run (`packs.rs:313-316`), so the update layer grows without bound.
- **Fix:** rename the previous store to a tombstone name recorded in the install before deleting, or accept a path whose name matches `compact_path`'s pattern for this game.

### 10. A deleted hard-link name still counts in `nlink`, and compaction then always fails: low, CONFIRMED
- **Where:** `src/pack/mount.rs:79-84` and `:372-376`, `src/pack/create.rs:225-229`.
- **Scenario:** the store has `T` and alias `A`. Delete `A` through the mount. `T` still reports `nlink` 2. `snapshot` walks one path and fails with "A hard-linked file also has links outside the game folder".
- **Fix:** falls out of the fix for finding 1; count only visible paths.

### 11. `setattr` on a symlink follows the upper link: low, CONFIRMED
- **Where:** `src/pack/mount.rs:816-819`.
- **Defect:** `utimensat(AT_SYMLINK_NOFOLLOW)` (`cp -a`, `rsync -a`, `touch -h`) reaches `OpenOptions::open(&upper)`, which follows the link relative to the layer folder.
- **Outcome:** the mtime lands on whatever the target resolves to there, including outside the layer for a `../..` target. If nothing resolves, the call returns ENOENT.
- **Fix:** use `utimensat` with `AT_SYMLINK_NOFOLLOW` on the upper path.

### 12. A crash around the compaction switch leaks a full store: low, CONFIRMED
- **Where:** `src/jobs/packs.rs:247-262` and `:329-381`.
- **Defect:** the new store's name contains the pid and is not in the record until line 389. A crash any time after publication orphans it. Nothing later looks for it, and its hard links pin pool objects against `pool-prune`. The `mounted.stop()` error path at `:381` also leaves it behind.
- **Fix:** record the candidate path in the install before building. Delete it in `recover` when the phase is `Compacting` or the candidate is unreferenced.

### 13. Copy-up temp files live inside the visible upper tree; trash is never emptied: low, CONFIRMED
- **Where:** `src/pack/overlay.rs:241`, `:285`, `:466-468`.
- **Scenario:** the coordinator is killed during copy-up of a large file. A partial `.tmpXXXXXX` stays in `files/`, shows up in the game folder, and is carried into compaction and restore. A `trash/removed-*` folder left the same way is never cleaned by `open`.
- **Fix:** stage in a folder outside `files/` on the same filesystem. Clear staging and trash in `Overlay::open`.

### 14. Rename of a store symlink or empty folder persists the whiteout before the upper entry is durable: low, PLAUSIBLE
- **Where:** `src/pack/overlay.rs:317-322` and `:554-558`.
- **Defect:** copy-up of a symlink or directory does no fsync, but the journal write that hides `from` is synced.
- **Scenario:** power loss after the rename can leave the whiteout on disk with neither `from` nor `to` in the upper tree.
- **Fix:** sync the upper parent before `persist()`.

### 15. `apply_to` is not re-runnable when a store folder was replaced by a file: low, CONFIRMED
- **Where:** `src/pack/overlay.rs:598-602`.
- **Scenario:** an interrupted rollback onto the retained original is retried. A child whiteout such as `D/a` now finds `D` is a file, and `parents_are_directories` fails the whole apply.
- **Fix:** treat a non-directory parent in the whiteout pass as "already gone".

### 16. `pack mount --writes` accepts a layer inside the mount target: low, PLAUSIBLE
- **Where:** `src/pack/mount.rs:1134-1140`.
- **Defect:** `Overlay::open` creates the layer after the emptiness check. A layer path under the target is then covered by the mount, and every overlay call re-enters the single FUSE thread and hangs.
- **Fix:** apply the `validate_paths` rules from `install.rs` in `mount`.

### 17. Tests that cannot fail for the stated reason: low
- **`src/pack/tests.rs:726-733`:** the proptest feeds up to 1024 random bytes. They never match the 8-byte magic, so index parsing and `validate` are never exercised. Fix: mutate a valid store with a recomputed index hash.
- **`src/pack/tests.rs:986`:** the fixture is a version 6 store with non-zero chunk offsets, so `index.validate(end, 7).is_err()` is true with or without the setuid bit. Only the version 6 half tests the rule.

## Read and found sound

- **Files read in full:** `src/pack/{format,create,mount,overlay,install,recovery,restore,cli,mod,tests}.rs`, `tests/pack_mount.rs`, `docs/pack-store.md`. Callers read: `src/jobs/packs.rs`, parts of `src/jobs/service.rs`, `src/fsprobe.rs`, and fuser 0.18 unmount and flags source.
- **Format parsing:**
  - Header and index bounds are checked before allocation.
  - Chunk tiling uses checked arithmetic.
  - zstd output is capped at `chunk.raw` (at most 4 MiB) with a window limit.
  - BLAKE3 is verified on every uncached decode, and object headers are rechecked on each read.
  - Version gates for xattrs, hard links and slices hold; slice coverage is checked.
  - `safe_path` covers entry paths and hard-link targets; symlink escape and cycle checks hold.
- **Creation and restore publication:** staged file or folder, fsync, full verify, second source snapshot, no-replace publish, parent fsync. Restore is staged and renamed with no-replace.
- **Pool:** objects are removed only at `nlink == 1` under the pool lock, and stores read through their own links. I found no path that deletes a chunk another store uses.
- **Activation and reclaim ordering:**
  - The record is saved before the rename and activation is re-runnable.
  - Reclaim verifies every chunk before deleting and only removes the expected sibling folder.
  - Compaction's generation counter and freeze catch every mutating handler.
  - The phase saves on both sides of the switch recover correctly, apart from finding 12.
- **Overlay journal:** atomic replace with file and directory fsync. Whiteout-before-remove ordering is right for regular files.
- **`apply_to`:** refuses to write through destination symlinks.
- **FUSE test guards:** they skip without `/dev/fuse`, but CI sets `FLUMMOX_REQUIRE_FUSE=1` (`.github/workflows/ci.yml:205`), so the assertions run there.
- **`as` casts and space arithmetic:** nothing exploitable found.
