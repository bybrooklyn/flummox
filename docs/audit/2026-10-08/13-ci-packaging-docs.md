# Non-Rust audit of flummox (workflows, packaging, docs, test gating)

Nothing in the repo was edited and no cargo, publishing, signing or network command was run. Two side effects of my own to know about:
- I ran `python3 -m py_compile packaging/*.py` and then cleared `packaging/__pycache__/*.pyc`. That directory is gitignored and already existed, so any earlier cached bytecode in it is gone too (Python regenerates it).
- My first prose grep ran under zsh with an unquoted `$F` list and returned zeros for everything. The stderr warning exposed it, and I reran under bash with a control that had to match (461 hits). The prose counts below are from the rerun.

Things I could not verify offline: that the pinned action SHAs match their commented tags, the GitHub repository and environment settings, and whether `RUSTSEC-2026-0192` exists.

## 1. Release and supply chain

**1.1 The signing and channel secrets are probably still repo-level, while the workflow comment says they are protected.** High, PLAUSIBLE (depends on repo settings).
- Where: `.github/workflows/release.yml:305-307` and `:403`.
- Evidence: the comment reads "This environment accepts only v* tags. Secrets stored in it cannot be read by a workflow edited on a branch." Commit 09fddeb, which added it, says "The three secrets still sit at repository level until they are moved, so this commit prepares the protection without yet providing it." `docs/security/2026-10-08-audit.md:54-60` still lists this as open item 1. The key is used with `minisign -S -W`, so it has no password.
- Consequence: anyone with push access can edit a workflow on a branch and read `RELEASE_SIGNING_KEY`, `AUR_SSH_PRIVATE_KEY` and `HOMEBREW_DEPLOY_KEY`.
- Fix: move the three secrets into the `release` environment, delete the repo-level copies, add a required reviewer and a `v*` tag ruleset. Until then, reword the comment to say what is true.

**1.2 The acceptance gate applies to exactly one version string.** Medium, CONFIRMED.
- Where: `release.yml:46`: `if: startsWith(github.ref, 'refs/tags/') && steps.version.outputs.version == '0.0.2'`.
- Consequence: `v0.0.3`, `v0.1.0` or `v0.0.2-1` publish, sign and push to AUR and Homebrew with no acceptance check. `docs/release-automation.md:108-110` documents this as intended, but `docs/status.md:46` ("The next tag requires real-game acceptance"), the README link text and the `check-acceptance.py` docstring ("before publishing a new tag") read as a standing gate.
- Fix: run the gate for every non-prerelease tag (`prerelease == 'false'`), or make those three texts say it is a one-off.

**1.3 The gate cannot tie evidence to the released build, and one report satisfies two kinds.** Medium, CONFIRMED.
- Where: `packaging/check-acceptance.py:19-24` and `:53-64`.
- `native-linux` and `proton` both map to `platform=linux, mode=maximum-space`, and nothing stops two runs naming the same report file or checks the runtime. One Linux report passes both.
- `flummox_version` must equal `"0.0.2"`. Reports record `env!("CARGO_PKG_VERSION")` (`src/qualification.rs:162`), and every commit since the bump reports `0.0.2`, including all three rc tag trees. So a report from any old dev build passes.
- A report from the published candidates says `0.0.2-rc.N` and is rejected, although `docs/releases/0.0.2-rc.*.md` say the candidates exist to be tried on real games before 0.0.2.
- `allocated_after` is not compared with `allocated_before`, and `run.game` is not compared with the report's game.
- Fix: require distinct report paths and corpus hashes per run, record and check a candidate commit that is an ancestor of the tag, and accept `0.0.2-rc.*` as the report version.

**1.4 A tag on any commit publishes and signs; nothing checks it is on main.** Medium, CONFIRMED.
- Evidence: `git merge-base --is-ancestor v0.0.2-rc.1 main` returns 1. rc.1 was published from 75a32f6, which is not in main's history. Tags are unsigned (`git tag -v`: "no signature found").
- Consequence: a release can be cut from an unreviewed or since-rewritten commit. The manifest binds the commit SHA, but that commit may not be reachable from any branch.
- Fix: in `prepare`, fail unless `git merge-base --is-ancestor "$GITHUB_SHA" origin/main`.

**1.5 Re-running on an existing release replaces published assets.** Medium, CONFIRMED.
- Where: `release.yml:391-395`.
- `gh release view "$TAG" >/dev/null 2>&1` treats any failure (auth, network) as "no release". `--verify-tag` applies only to create. Then `gh release upload --clobber` overwrites assets of a release that may already be public.
- Consequence: a moved tag or a re-run swaps signed downloads in place under the same version. The docs say "Never move a published tag" but nothing enforces it. This is also the `2>/dev/null` pattern CLAUDE.md warns about.
- Fix: read `isDraft` with `gh release view --json isDraft` without discarding stderr, and refuse to upload when the release exists and is not a draft.

**1.6 There is no tag-versus-source version check.** Low-medium, CONFIRMED.
- Where: `packaging/prepare-release.py:16-21` overwrites `Cargo.toml` with whatever the tag says.
- Consequence: the tag's tree always carries a different version than its binaries (all rc tags say `version = "0.0.2"`). `packaging/PKGBUILD` `pkgver`, `CHANGELOG.md` and `docs/releases/<version>.md` are never checked against the tag. A typo such as `v0.2.0` ships as 0.2.0 with auto-generated notes.
- Fix: require the tag's base version to equal the `Cargo.toml` version, require `docs/releases/<version>.md` for stable tags, and add a `test_release.py` check that PKGBUILD `pkgver` matches `Cargo.toml`.

**1.7 Signed artifacts are built with mutable inputs.** Low-medium, CONFIRMED. The first two are already open items 2 and 9 in the audit doc and are still present.
- `Swatinem/rust-cache` restores `target/` in the release build (`release.yml:102-104`).
- `choco install innosetup` is unpinned and unverified, and it produces the signed `setup.exe` (`:143`).
- `container: archlinux:base` has no digest and runs `pacman -Syu` (`:207-210`).
- `rustup toolchain install stable` and `taiki-e/install-action` tools `just,cargo-deny` are unversioned.
- Fix: drop the cache from the release build job, pin Inno Setup by version and checksum, pin the image by digest, pin the toolchain.

**1.8 Release archives carry no third-party licence notices.** Low-medium, CONFIRMED.
- Where: `packaging/package-release.py:45-57` and `:135`, `:157` ship only the AGPL `LICENSE`.
- The binaries statically link about 480 crates under MIT, Apache-2.0, BSD-2/3 and ISC, plus bundled zstd (BSD-3) and SQLite. Those licences require their notices in binary redistributions.
- Fix: generate a `THIRD-PARTY-LICENSES` file with cargo-about or cargo-license at release and include it in every archive, the installer and the app bundle.

**1.9 Linux debug symbols expire after 14 days.** Low, CONFIRMED.
- Where: `release.yml:197`, `retention-days: 14`. Documented in `release-automation.md:105`.
- Consequence: a crash report on a release older than two weeks cannot be symbolised.
- Fix: attach the `.debug` files to the release, or keep them for the release's lifetime.

## 2. CI coverage

**2.1 One btrfs test never runs on btrfs anywhere.** Medium, CONFIRMED.
- Where: `src/allocation.rs:214-221`, `btrfs_compressed_extents_are_refused`, which prints "skipped: the extent refusal requires btrfs" off btrfs.
- The fs-matrix job runs the lib test binary with the filter `backend::btrfs` (`ci.yml:179`), which does not match `allocation::tests::...`. Every other job runs from ext4.
- Consequence: the refusal the acceptance procedure depends on (`validation/README.md:26-28`) is untested in CI.
- Fix: run the whole lib binary from `/tmp/btrfs`, or add `allocation::` to the filter.

**2.2 The fs-matrix job passes if the filter matches nothing.** Medium, CONFIRMED.
- Where: `ci.yml:176-185`. The only checks are the exit code and the absence of `skipped:`.
- Consequence: renaming `backend::btrfs` yields "running 0 tests" and a green job.
- Fix: parse `test result: ok. N passed` and require N at or above a known floor.

**2.3 The Landlock and seccomp tests can pass with no sandbox, and the skip message is swallowed.** Medium, CONFIRMED mechanism (runners do have Landlock today).
- Where: `tests/sandbox_enforcement.rs:16-19` and `:67-70`. The child prints "skipped: Landlock is unavailable" and returns Ok. The parent captures it with `.output()` and checks only `status.success()` (`:35-51`).
- There is no `FLUMMOX_REQUIRE_LANDLOCK` equivalent of the FUSE gate.
- Consequence: on a runner image or container without Landlock, the security-boundary tests go green and the log shows nothing.
- Fix: add a require variable set in CI that turns the skip into a failure, and have the parent re-emit the child's stderr.

**2.4 One FUSE unit test is never required.** Low-medium, CONFIRMED.
- Where: `src/pack/install.rs:653-661`, `activation_preserves_updates_across_both_rollback_paths`.
- The pack-mount job sets `FLUMMOX_REQUIRE_FUSE=1` but runs only `--test pack_mount --test jobs_lifecycle` (`ci.yml:206`). The lib test runs only in `just test`, where a missing `/dev/fuse` skips it.
- Fix: add `--lib pack::` to the pack-mount job.

**2.5 Releases do not depend on the required btrfs or FUSE jobs.** Low-medium, CONFIRMED.
- `release.yml` runs `cargo test --all-features` per target with no btrfs mount and no `FLUMMOX_REQUIRE_FUSE`, and does not require CI to have passed on the tagged commit. Combined with 1.4, a tag can ship where those tests never ran.
- Fix: make the fs-matrix and pack-mount jobs reusable and call them from the release workflow, or check the commit's CI status in `prepare`.

**2.6 `just prose` repeats two of the CLAUDE.md shell mistakes and misses rule patterns.** Low-medium, CONFIRMED.
- Where: `justfile:49`: `grep -rn "$pattern" "${targets[@]}" 2>/dev/null | ... || true`. A renamed or missing target reports "prose: clean".
- The pattern list has rule 5's first phrase with its words in a different order from the rule, and omits "exactly the", "silently", "quietly", "lies" and "nobody". `CHANGELOG.md`, `Cargo.toml`, `clippy.toml` and `tools/` are not scanned.
- It recurses into gitignored `packaging/pkg/` and `packaging/src/` (makepkg output), so local results can differ from CI.
- Violations it misses today:
  - `deny.toml:32` "rather than quietly ship"
  - `deny.toml:39` "nobody is willing"
  - `.github/workflows/ci.yml:14` "a warning nobody reads"
  - `docs/jobs-and-compression.md:91` "does not silently lower"
  - `README.md:50` "you won't believe it-- the GUI" (an em dash spelt with hyphens)
- Fix: drop `2>/dev/null` and `|| true` (test grep's status: 0 hit, 1 clean, 2 error), use `git ls-files` for the file set, and add the missing patterns.

**2.7 The Arch smoke test's screenshot is discarded.** Low, CONFIRMED.
- `packaging/smoke-arch.sh:39` writes `/validation/arch-gui.png`; the upload at `release.yml:241-251` lists only `dist/...` paths.
- The only GUI assertion is that the process is alive after 10 s (`:34`), although `release-readiness.md:42` says the screenshot "was inspected".
- Fix: upload `/validation`.

**2.8 `rust-version = "1.98"` is never tested.** Low, CONFIRMED. Every job installs `stable`. Add an MSRV check job or drop the claim.

**2.9 The pre-push hook is not in the repository.** Low, CONFIRMED.
- `docs/how-it-works.md:43` and `ci.yml:114` describe a hook that "runs the same thing". It exists only as local `.git/hooks/pre-push`, with no `core.hooksPath`.
- Fix: track it under `.githooks/` with a `just` recipe to install it, or reword.

## 3. Packaging

**3.1 `just release` packages a stale changelog.** Low, CONFIRMED.
- `packaging/package-release.py:56-57` includes whatever `RELEASE-NOTES.md` is on disk. The gitignored one in the tree is dated Oct 3, and `dist/` still holds `flummox-0.1.0-*` artifacts.
- Fix: have `just release` run `prepare-release.py` first.

**3.2 Two different watch units exist under the same name.** Low, CONFIRMED.
- `packaging/flummox-watch.service` has `PrivateTmp`, `RestrictSUIDSGID`, `RestrictNamespaces`, `MemoryDenyWriteExecute` and `CPUSchedulingPolicy=idle`.
- `flummox watch enable` (`src/cli/mod.rs:1688-1705`) writes `~/.config/systemd/user/flummox-watch.service` with none of those, and it shadows the packaged unit.
- `docs/usage.md:159-166` recommends `watch enable`; `docs/install.md:33` calls the packaged unit an "optional legacy watch service".
- Consequence: the hardened unit is never the one that runs for a user who follows the docs.
- Fix: have `watch enable` enable the packaged unit when it exists, or generate the same directives; settle the wording.

**3.3 Installed docs have dead links.** Low, CONFIRMED.
- `PKGBUILD:66-69` and the tarball put `README.md`, `install.md`, `next-steps.md` and `release-readiness.md` flat in `/usr/share/doc/flummox`.
- Their relative links (`docs/usage.md`, `LICENSE`, `release-automation.md`, `validation/...`, `plans/...`) resolve to nothing there. `usage.md`, the doc a user needs, is not shipped.
- Fix: ship `usage.md` and `status.md` in place of the release-engineering notes.

**3.4 Smaller packaging points.** Low.
- `PKGBUILD:19` `depends` omits `glibc`, and nothing declares a Vulkan/GL loader although `smoke-arch.sh:8` installs `vulkan-swrast` to get the GUI up (PLAUSIBLE; iced may fall back to software rendering).
- The generated `flummox-bin` recipe drops the `btrfs-progs` optdepend that the source PKGBUILD has (`distributions.py:31`).
- `test_release.py:189` and `:213` skip without failing when `cc`, `objcopy` or `minisign` is missing. CI installs them, so only local runs are affected.

## 4. Docs that are wrong or stale

**4.1 The curated notes that will publish for v0.0.2 contradict the app.** Medium, CONFIRMED.
- `docs/releases/0.0.2.md:10-12` says "Overview, Games and one continuous Settings page replace the separate queue, drive and recovery pages".
- The Linux sidebar is `PAGES = [Overview, Games, Queue]` (`src/gui/app.rs:32`), and `CHANGELOG.md:42` says "Jobs have their own page in the sidebar".
- The file also omits everything rc.3 added: Standard/Maximum choice, guided Maximum, automatic store upkeep, sorted Games page.
- `docs/status.md:18` has the same stale "Overview, Games and Settings navigation".

**4.2 `CHANGELOG.md:8-9` says 0.0.2 "has no download yet".** Low, CONFIRMED. Three prereleases are tagged and their notes describe downloads. Say "no stable download".

**4.3 `docs/release-readiness.md` describes an older UI and layout.** Low, CONFIRMED.
- `:16` "Games, Drives, Settings, and Queue widgets" and `:77` "Drives & libraries"; the rest of the docs use Settings > Locations.
- The file is shipped to users in every package.

**4.4 README statements against CLAUDE.md's learned facts.** Low, CONFIRMED.
- `README.md:7` gives "one 240 MB Firewatch asset file went down to 162 MB on my drive" as a measurement with no estimate label. CLAUDE.md says per-file savings on btrfs need `compsize`.
- The same paragraph says "nothing ever gets *bigger*", and `:62-63` says usage can go up with snapshots.
- `:5` says "The files are not moved", while Maximum Space moves the original aside (`:57`).
- `:37` asks for "FUSE dev files", but `fuser` is built with `default-features = false`. It does not list the X/Wayland/fontconfig headers the `gui` feature needs (`ci.yml:110-112` has the real list).

**4.5 `docs/validation/2026-10-08-rc1-linux-storage.md:22-27` derives a per-job saving from the whole-drive counter.** Low, CONFIRMED. "the saving is best read as 1.5 to 1.8 GB". It is hedged with idle-window controls, but CLAUDE.md says never present that reading as a measurement of one job.

**4.6 `docs/install.md:122-124` overstates prerelease checks.** Low, CONFIRMED. It says all installation checks must pass before publishing, but the `arch` and `homebrew-check` jobs are skipped for prereleases (`release.yml:204`, `:257`).

**4.7 `docs/usage.md:134` and `:60-62` disagree.** Low. One says Ctrl-C "stops between files", the other says workers stop at 16 MiB boundaries inside large files.

## 5. Licensing and policy

Nothing beyond 1.8. All 479 registry packages in `Cargo.lock` resolve to an expression satisfiable by the `deny.toml` allow list (checked against the local registry's `Cargo.toml` files, with zero missing). Every allowed licence is GPL-3 compatible. `tools/wof-lzx-helper.c` carries an SPDX AGPL header and links wimlib dynamically for benchmarks only; it is not shipped.

## Checked and found sound

- **Workflow triggers and permissions:** `ci.yml` uses `pull_request`, not `pull_request_target`, with `contents: read`. `release.yml` defaults to read, and only `publish` has `contents: write`.
- **Script injection:** every `${{ }}` value reaches `run:` through `env:`.
- **Action pinning:** all third-party actions are pinned to 40-character SHAs.
- **Masking:** no `continue-on-error`, and no `|| true` outside a cleanup trap and the prose recipe.
- **Dispatch safety:** a manual run cannot sign, publish or push channels. `sign` and `publish` need a tag ref; `channels` needs `publish`.
- **Signing flow:** the manifest binds tag, commit, exact file set, sizes and SHA-256. It is verified before publish and again before channel pushes. The key file is temporary and removed from the child environment.
- **Key and docs:** `packaging/minisign.pub` is the only key source the docs name (`install.md:166-170`). No doc carries a divergent key string.
- **Channel pushes:** `publish-channel.py` uses argument-list subprocesses and `StrictHostKeyChecking=yes`. The AUR host key is checked against a pinned fingerprint and GitHub's keys come over HTTPS. No `shell=True` in any script.
- **Packaging script:** the tarball is built from an explicit file list, so no untracked files get in, with uid, gid and mtime zeroed. Architecture checks cover ELF, Mach-O and PE.
- **Version checks:** `prepare-release.py` rejects non-SemVer tags, and the built binary's `--version` is checked on every platform.
- **Acceptance gate, missing data:** a missing or empty record fails closed. `docs/validation/0.0.2.json` is `pending` with no runs, so v0.0.2 cannot publish today.
- **Feature coverage:** `--all-features` clippy and tests run on Linux, Windows and macOS. Windows and macOS tests are executed, not only compiled. `cargo deny` and `just prose` run in CI via `just ci`.
- **FUSE and btrfs gating:** the FUSE job fails on a skip, and the fs-matrix job runs on a real loop-mounted btrfs and fails on a `skipped:` line.
- **Lockfile:** all sources are crates.io with checksums.
- **Desktop entry:** `Exec` is valid, and the missing `Icon` is explained in the file. It is validated in the release job.
- **Systemd unit:** the `ExecStart` path matches the install path.
- **Docs against the CLI:** every `flummox ...` command and flag in README, `usage.md`, `install.md`, `pack-store.md`, `how-it-works.md` and `status.md` exists in `src/cli/mod.rs` or `src/pack/cli.rs`. Protocol version 8 matches `src/jobs/mod.rs:23`.
- **Links and references:** no broken relative links or anchors across 29 Markdown files. Every commit hash cited in `docs/validation` and `docs/plans` exists and is on main. The one missing hash, in `docs/benchmarks/2026-09-18-lzx.md:56`, is a wimlib commit.
- **Scripts compile:** all six packaging scripts and `test_release.py` pass `py_compile`.
- **Licence file:** `LICENSE` is the full AGPL-3.0 text.
