# Next steps

## Desktop polish priority

The [desktop implementation plan](plans/desktop-polish.md) defines navigation,
motion, stable jobs, asynchronous discovery and local artwork. UI acceptance
precedes Windows expansion. The [desktop delivery record](validation/2026-10-06-desktop-polish.md)
separates implemented changes from graphical and platform acceptance still needed.

## Release acceptance

Native Factorio simulation and Super Meat Boy Proton startup passed disposable
storage-cycle smoke checks. See [the evidence](validation/2026-10-04-linux-smoke.md)
for measured results and the remaining limits. The typed-path GUI flow also
passed an isolated graphical check with a spaced collection path and an invalid
path control. Native desktop picker integration remains pending.

These checks remain necessary before calling the release fully validated:

- Qualify real native Linux and Proton games for Maximum Space. Check launch,
  gameplay, patching, launcher verification, login remount, and restoration.
  Record load times and gameplay results against an ordinary-directory baseline.
- Exercise folder and report dialogs in KDE and another desktop session,
  including selection, cancellation, spaces in paths, and disconnected drives.
- Test the Windows WOF backend on real NTFS game installs, including launch,
  gameplay, update, restore, and interrupted operations. Native CI and installer
  tests pass, but they do not establish real-game compression compatibility.

## Completed distribution work

Version 0.0.1 is published. Tag-triggered native builds, embedded versions,
release notes, installation tests, and automatic Homebrew/AUR updates passed in
[the release workflow](https://github.com/bybrooklyn/flummox/actions/runs/37156256792).
Linux x86_64/ARM64, Apple Silicon macOS, and Windows x64 downloads are available.
macOS is currently a shell without a compression backend. This replaces the
previous draft-release and cross-build-only acceptance items.

## Version 0.0.2 implementation

The main branch implements per-volume space planning, worker rechecks,
Recovery with explicit restoration verification, remembered offline games,
guided local compatibility reports, native APFS compression, and signed release
manifests. Linux downloads omit embedded debug data and retain internal symbols.

Real-game acceptance remains pending in `validation/0.0.2.json`. Native builds,
APFS fixtures, Windows installer checks, Arch and Homebrew installation,
signed-manifest verification, and offscreen GUI checks have passed.
User-run Windows and Mac launch/gameplay/update/restore evidence is required;
Linux native and Proton evidence is required too. See the
[acceptance procedure](validation/README.md).

## Adoption backlog

After desktop polish, simplify first use equally on Windows, Linux and Mac.
Compare discovery, savings, batch jobs, update handling and restoration against
[Game Compressor on Steam](https://store.steampowered.com/app/4339880/Game_Compressor/).
Test existing WOF/LZX recognition and migration on disposable copies. A new user
should install, compress a supported game, and find Restore without documentation.

## Later work

Mac FUSE support, Windows launcher and maintenance parity, scheduling, Bottles
integration, and community report sharing remain deferred. Apple Developer ID,
notarization, and Windows Authenticode require publisher credentials; current
release authentication uses a separate stable Minisign key.

## Dependency update (2026-10-03)

The manifest now requires current stable direct releases and the lockfile updates
compatible transitive dependencies. FUSE moves to 0.18, SHA-256 to sha2 0.11,
and signal handling to signal-hook 0.4. The mount call and SHA-256 formatting
were adapted without changing stored corpus hashes. CI actions also move to their
current releases.

The preview renderer keeps tiny-skia 0.11.4 because Iced 0.14's renderer exposes
0.11 types. Its latest renderer adapter, iced_tiny_skia 0.14.1, still requires
that line; adopting standalone tiny-skia 0.12 would make the preview types
incompatible. This dependency must move with Iced's renderer.

Validation passes: `just ci`, `just build`, CLI-only compilation, required FUSE
mount/coordinator fixtures, Windows CLI/GUI cross-build (`just win`), and workflow
linting (`actionlint`). A final Cargo update dry run finds no remaining compatible
lockfile updates. Native builds, portable-module tests, installation checks, and GitHub release
execution now pass. Real-game storage acceptance remains outstanding.
