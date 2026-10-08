## Bug audit: backend, safeio, sandbox, fsprobe, busy, watch, allocation, storage, inventory, path_serde

Read-only pass: nothing was executed, so every claim about kernel behaviour is from memory of the kernel source. "CONFIRMED" means I traced the code path in this repo. "PLAUSIBLE" means the code path is traced but the outcome rests on kernel behaviour I could not run. No critical or high findings; the data path (anchored open, fingerprint check before and after, in-place defrag) is sound.

### Findings

**1. Decompress has no fallback or kernel check for `BTRFS_DEFRAG_RANGE_NOCOMPRESS`. Medium. PLAUSIBLE (code CONFIRMED, kernel versions from memory).**
- Where: `src/backend/btrfs.rs:168-181`, flag defined at `:44`.
- Defect: `decompress_range` sends `DEFRAG_RANGE_NOCOMPRESS | DEFRAG_RANGE_START_IO` and maps every error straight through. The compress path has an `EOPNOTSUPP | EINVAL` retry; decompress has none. `doctor` (`src/cli/mod.rs:2017-2024`) only mentions 6.15 for levels.
- Failure: the flag is recent (6.16/6.17 era). On kernels that validate defrag flags but lack it, every file fails with `EOPNOTSUPP`. By then `set_dir_property(false)` (`btrfs.rs:547`) and `db.invalidate_compression` (`worker.rs:144`, `cli/mod.rs:1325`) have already run. On older kernels that ignore unknown flags, the ioctl does a plain defrag and returns 0. The job then reports success, writes receipts and calls `db.forget`, while the files are still compressed.
- Fix: probe the flag once on a scratch file before touching state and fail with one clear message; add the kernel requirement to `doctor`. After a decompress, check `compressed_bytes_fd` is 0 before issuing a receipt.

**2. `compress --force` never pauses for a running game once Landlock is active. Medium. PLAUSIBLE (path CONFIRMED).**
- Where: `src/cli/mod.rs:1183-1202` with `src/busy.rs:111-128`.
- Defect: `sandbox::restrict` runs first, then `GameInUse` is handed to the backend. It reads `/proc/<pid>/{exe,cwd,root,fd/*}` with `read_link`. Those links go through `ptrace_may_access`, and Landlock denies ptrace-level access to any process outside the caller's domain. Every `read_link(...).ok()` becomes `None` or an empty list, so `uses_dir` is false.
- Failure: user runs `flummox compress --force <game>` (pause enabled) and starts the game mid-pass. The pass keeps rewriting at full I/O. The coordinator path is unaffected: it scans `/proc` unsandboxed (`service.rs:1574`) and the worker reads a flag.
- Fix: do the `/proc` scan in an unsandboxed helper (parent before restricting, or a pipe-fed child). Treat "saw processes but could not read any link" as "unknown, stay paused", as `service.rs:1596` does.

**3. On kernels before 6.15, every pass rewrites the whole game. Medium. CONFIRMED (logic).**
- Where: `src/backend/btrfs.rs:137-146`, `src/jobs/worker.rs:128`, `src/cli/mod.rs:1093-1095`.
- Defect: the fallback returns `DEFAULT_LEVEL` (3). The worker discards receipts where `level < level_plan().floor()` (9 for Balanced and Max). The CLI reuses only when `prev.level >= opts.btrfs_level()`.
- Failure: Ubuntu 24.04 (6.8) or Debian (6.1/6.12), Balanced preset. Every compress job, including each one `watch` triggers after an update, re-reads and rewrites the full install. Defrag does not change the fingerprint, so nothing ever converges. The worker does emit "A newer kernel is needed" each time.
- Fix: record that the kernel refused a level (per job or in the receipt) and accept receipts at "best this kernel can do".

**4. CLI direct path with `--preset max` redoes everything and prints a false kernel warning. Medium-low. CONFIRMED.**
- Where: `src/cli/mod.rs:1095` and `:1274-1276`, against `src/backend/btrfs.rs:420`.
- Defect: the backend reports the minimum level over all files. For Max that is 9 unless every file sampled into 15. The CLI compares against `opts.btrfs_level()`, which is 15.
- Failure: `reuse` is false on every later run, so the incremental skip never applies. Line 1276 (`applied < opts.btrfs_level()`) prints "this kernel does not accept a compression level" on a 7.x kernel.
- Fix: compare against `opts.level_plan().floor()`, as `worker.rs:445` does.

**5. A full `/proc` scan runs for every file, bypassing the rate limit. Medium-low (performance). CONFIRMED.**
- Where: `src/backend/btrfs.rs:193-198`.
- Defect: `rewrite_ranges` builds a fresh gate per file, already 60 s in the past: `let gate = Mutex::new(Instant::now() - Duration::from_secs(60));`. `wait_while_busy` (`mod.rs:236`) therefore always calls `busy.in_use_by()` on the first range.
- Failure: with the CLI's `GameInUse` (2 s interval), a 200k-file install does 200k scans, each reading every fd link of every user process. The doc on `wait_while_busy` names this as the case the limit exists to prevent. The worker's `Pause` (interval zero) is unaffected.
- Fix: pass the pass-wide `last_busy_check` gate into the per-file closure.

**6. The compression property reaches only the top-level folder, and decompress leaves inherited copies. Low-medium. PLAUSIBLE.**
- Where: `src/backend/btrfs.rs:516` and `:547`.
- Defect: btrfs copies `btrfs.compression` from the immediate parent at inode creation only. Subdirectories that already exist under the install dir never get it. Decompress clears only the top-level xattr.
- Failure: a file created later in `Game/data/` is not compressed on write, though the warning text at `:519` implies it would be. After a decompress, directories and files created while the property was on still carry their own copy, so writes there keep landing compressed.
- Fix: set and clear on every directory during the walk (and clear on files in decompress), or reword the claim to "files created directly in this folder".

**7. `watch` loses an update that starts and finishes while another job is being awaited. Low-medium. CONFIRMED.**
- Where: `src/watch.rs:101-114`, caller `src/cli/mod.rs:1777-1790`.
- Defect: `on_ready` calls `cmd_compress`, which goes to `queued_job`, which loops until the job ends. `settled` is refreshed only after that returns.
- Failure: game B updates completely while game A compresses. The rescan sees `now == before == true` for B, so B's updated files get no pass until its next update.
- Related, same file: an `IN_Q_OVERFLOW` event has no name, so `drain` returns `false` and the rescan is skipped (`:127-134`). `IN_IGNORED` (library unmounted) leaves the loop polling a dead watch with no error.
- Fix: also compare each manifest's build id or mtime; treat overflow as "saw a manifest"; return an error when every watch is gone.

**8. Files without the owner write bit fail on every pass. Low-medium. PLAUSIBLE.**
- Where: `src/inventory.rs:197-233` (no mode check), surfaced at `src/backend/btrfs.rs:128`.
- Defect: for non-root callers the defrag ioctl requires `inode_permission(MAY_WRITE)` and returns `EPERM` otherwise. The same applies to `chattr +i` files.
- Failure: a GOG or Lutris install with 0444/0555 files gets an error for each one, the job is marked failed, the dir property is skipped (`btrfs.rs:514-515`), and no receipt is written, so they fail again next time.
- Fix: classify these at walk time as a skip with a stated reason, or map `EPERM` to a skip.

**9. `compress_range` reports the requested level, not the applied one. Low. CONFIRMED.**
- Where: `src/backend/btrfs.rs:122` and `:130`.
- Defect: sends `level.clamp(-15, 15)` and returns `Ok(level)`. `--level` has no range check (`cli/mod.rs:237-239`).
- Failure: `--level 19` records 19 in receipts and `games.level` while the kernel applied 15. `--level 0` records 0, which `invalidate_compression` uses to mean "not compressed", while the kernel applied its default.
- Fix: return the clamped value; reject 0 and out-of-range values at the argument parser.

**10. A compress job with nothing left to do sets `games.level` to 0. Low. CONFIRMED.**
- Where: `src/jobs/worker.rs:462`: `record.level = outcome.effective_level.unwrap_or(0);`.
- Failure: all files have valid receipts, the backend returns the default `Outcome` (`btrfs.rs:364-369`), and the upsert writes level 0. The next CLI run sees `reuse == false` and redoes everything.
- Fix: keep the previous record's level when `effective_level` is `None`.

**11. The directory property is set by path and does not follow a final symlink. Low. CONFIRMED for the crate, PLAUSIBLE for impact.**
- Where: `src/backend/btrfs.rs:214-245`; callers `jobs/service.rs:267-281`, `cli/mod.rs:1823`.
- Defect: xattr 1.6.1's `get`/`set`/`remove` are the non-dereferencing variants (`set_deref` is separate). The path is also re-resolved after the job instead of using the anchor's fd.
- Failure: if `steamapps` is a symlink to another drive, the property and `user.flummox.live-compression` go on the link inode or fail, and `dir_property` reads the link, so status can say "on" while the real directory is unset.
- Fix: `fsetxattr`/`fgetxattr` on an opened directory fd (the `Anchor`'s in `compress`/`decompress`).

**12. `fsprobe::probe` does not cross-check the magic, and misses superblock-level read-only. Low. CONFIRMED.**
- Where: `src/fsprobe.rs:261-280`; module header `:4-5` says the type is "cross-checked against the `statfs` magic".
- Defect: `magic` is stored but never compared with the mountinfo type. `read_only` looks only at per-mount options.
- Failure: with a shadowed longer mount point (`/mnt/a/b` mounted, then `/mnt/a` over-mounted), longest-prefix picks the hidden entry and the wrong tier. A btrfs that flipped read-only after an error shows `rw` per mount and `ro` in the super options, so it is offered as Native and every file fails with `EROFS`.
- Fix: when `fstype_from_magic(magic)` is `Some` and differs, prefer the magic; include `super_options` in `read_only`.

**13. Fallback volume identity splits one btrfs into several volumes. Low. PLAUSIBLE.**
- Where: `src/storage.rs:112-116`.
- Defect: when no `/dev/disk/by-uuid` link resolves to the mount source (multi-device btrfs, for example), identity becomes `source:f_fsid`. btrfs mixes the subvolume id into `f_fsid`.
- Failure: two subvolumes of one filesystem get different identities, `SpacePlan::add` keeps separate rows, and each is checked alone against the same free space.
- Fix: read the filesystem UUID from `/sys/fs/btrfs/<uuid>` or the `BTRFS_IOC_FS_INFO` ioctl.

**14. Sandbox gaps. Low, hardening. CONFIRMED in code unless noted.**
- CLI direct path (`cli/mod.rs:1183-1187`, `:1334-1338`) still grants the whole state folder writable and never calls `deny_sockets`. That is the situation the audit doc fixed for workers only.
- CLI direct path calls `estimate_game_cancellable` before `restrict` (`cli/mod.rs:1158`). If that reaches `estimate.rs:646`'s `par_iter`, the global rayon pool predates the restriction; Landlock is per thread, so those threads stay unrestricted for the life of the process. I did not trace the call into `estimate.rs`.
- `deny_sockets` (`sandbox.rs:170`) blocks only `socket` and `socketpair` by native number. `IORING_OP_SOCKET` (5.19+) and, where the kernel enables x32, syscall numbers with the x32 bit are not matched. Both need attacker-controlled syscalls. (PLAUSIBLE)
- `restrict` grants `AccessFs::from_all` on the game folder, which includes execute and make-device/socket/fifo rights that a defrag job never uses.
- Both `restrict` and `deny_sockets` fail open with a warning. That is documented, so not a defect.

**15. NOCOW files receive a success receipt though nothing is compressed. Low. PLAUSIBLE.**
- Where: `src/backend/btrfs.rs:411-427`.
- Defect: btrfs will not compress a `chattr +C` inode, but the ioctl still returns 0.
- Failure: the file is rewritten once, recorded at level N, and counted in the estimate's saving.
- Fix: read `FS_IOC_GETFLAGS` at open and skip with a reason.

**16. Tests that cannot fail, or pass without running. CONFIRMED.**
- `tests/inventory_properties.rs:210-213`, `every_decision_explains_itself`: all three `reason()` values are non-empty literals. The file header (`:14-16`) also describes an extension rule `decide` no longer applies.
- `src/safeio.rs:191-195`: the no-openat2 branch is `check(true, …)`.
- Every btrfs ioctl test returns `Ok` with "skipped" off btrfs (`btrfs.rs:622-626`), including `compressing_leaves_the_fingerprint_fields_alone`, which CLAUDE.md names as the lock on the ctime invariant. On an ext4 CI runner that invariant is untested.
- Both Landlock tests in `tests/sandbox_enforcement.rs:16-19` and `:67-70` do the same when Landlock is absent.
- Fix: make skips visible (an env var that turns a skip into a failure on the machine that is supposed to have btrfs and Landlock).

Smaller, same area: a user cancel between ranges goes through `fail()` (`btrfs.rs:199-202`, `:429`), so a cleanly cancelled job reports "Stopped between file ranges" as a per-file error.

### Audit doc `docs/security/2026-10-08-audit.md`: open items I can confirm are still open
- Open 7 (path-based opens outside the anchor): `compressed_bytes(path)` at `btrfs.rs:252`, used by `FiemapProbe` (`:326`), `allocation.rs:101` and `worker.rs:323-324`.
- Open 8 (huge sparse file): `rewrite_ranges` still issues one ioctl per 16 MiB of length (`btrfs.rs:197-207`).
- The other open items are outside the files I read.

### Read in full and found sound
- **ioctl layer (`btrfs.rs`)**: `DefragRangeArgs` is 48 bytes and matches the kernel union; ioctl numbers `_IOW(0x94,16)` and `_IOWR('f',11)` are right; `Fiemap` 32 and `FiemapExtent` 56 bytes. The FIEMAP loop is bounds-checked with `get`, uses unaligned reads, and terminates on `LAST` or no progress. All fds outlive their ioctls and the SAFETY comments state what the calls need.
- **`safeio.rs`**: `openat2` with `BENEATH | NO_SYMLINKS`, `O_NONBLOCK` against FIFOs, `is_contained` and its property tests. The pre-5.6 fallback is weaker and reported; the worker refuses to run without `openat2` (`worker.rs:223-226`).
- **Job loop (`btrfs.rs` `run`)**: identity is checked before and after each rewrite; progress is counted under one lock and only for successes, so it cannot exceed the `Started` totals or go backwards; sums saturate; empty and tiny files never reach compress (`size <= min_size`).
- **Worker ordering (`worker.rs`)**: database, receipts and report store are opened before `restrict`; `restrict` and `deny_sockets` run before the control thread and the rayon pool are created; a closed stdin cancels.
- **`sandbox.rs`**: the seccomp action order (mismatch Allow, match Errno) is right for seccompiler 0.5; it is a denylist, so no allowlist gaps can kill the worker on newer glibc; a foreign architecture is killed.
- **`fsprobe.rs`**: mountinfo field positions, optional fields, `\040` decoding, lossy UTF-8, and stacked mounts at the same point (last wins); the `statfs` unsafe block.
- **`mod.rs`**: `wait_while_busy` pause and cancel semantics, `free_bytes` arithmetic, `Outcome::freed`.
- **`inventory.rs`**: the walk does not follow links and skips special files; fingerprint is size, inode, mtime, ctime.
- **`allocation.rs`**: hard-link dedup by `(dev, ino)`, FUSE refusal, encoded-extent refusal.
- **`storage.rs`**: checked arithmetic throughout, `recheck` identity comparison.
- **`path_serde.rs`**: round-trips non-UTF-8 paths.
- **`watch.rs`**: the poll and stop-flag loop and `EINTR` handling.
- **`busy.rs`**: uid filter, pressure-vessel root stripping, non-path fd targets filtered; pid reuse only affects a display string.
