# Status

What the current source does on each platform, what it does not do yet, and
what the next release adds.

## Development build (unreleased 0.0.2)

These capabilities describe the current source, not the published v0.0.1
download. Real-game release acceptance is
[still pending](validation/README.md).

| | |
|---|---|
| **Linux games** | Steam (native, Flatpak and snap), Heroic installed manifests, Lutris, and custom game folders |
| **Linux storage** | Native btrfs compression; verified writable Maximum Space stores on ext4, XFS, F2FS, ZFS, and btrfs with FUSE |
| **Windows** | Local Steam, Epic, GOG and Heroic discovery, plus custom folders on NTFS; WOF/LZX compression, durable background jobs and opt-in maintenance |
| **macOS** | Local Steam and Heroic discovery, custom folders, native APFS compression and replacement recovery; jobs run in the app, without a durable Mac coordinator |
| **Desktop** | Wayland and X11 on Linux; native windows on Windows and macOS; Overview, Games and Settings navigation |

## What does not work yet

| | |
|---|---|
| **Linux native compression outside btrfs** | Maximum Space works through a writable FUSE store, but these filesystems have no in-place native backend |
| **Windows Maximum Space and Xbox discovery** | Windows currently offers native WOF/LZX compression; protected Xbox installs need separate feasibility and safety work |
| **macOS Maximum Space and durable background jobs** | Native APFS compression is implemented; FUSE stores and a persistent Mac coordinator are not |
| **Bottles** | Discovery is planned |
| **Flatpak build** | Not available. The sandbox hides other processes, so Flummox could not tell whether a game was running, which is the check that keeps it from touching a game you are playing |

## Next release: storage planning and recovery

The development version shows per-volume storage plans before individual jobs,
combines simultaneous allocations, and rechecks free space and volume identity
in workers. Linux CLI users can run `flummox plan FOLDER --store STORE` or
`flummox plan FOLDER --restore` without writing game files.

Recovery groups interrupted jobs and retained Maximum Space storage. Its
verification action reconstructs a temporary expected installation before
closing an interrupted restoration record. Diagnostics are local and include
folder paths; exporting them does not upload them.

Disconnected libraries retain their last discovered games. A different volume
mounted at the same location stays unavailable. In a game's details, choose
**Qualify compatibility** to record measurements and save a local report.

The next tag requires [real-game acceptance](validation/README.md). A manual
Release workflow produces candidates without publishing. Linux debug symbols
stay in internal CI artifacts, and public downloads are covered by one
[Minisign manifest](release-automation.md#release-signatures).

## Downloads and release tags

[GitHub Releases](https://github.com/bybrooklyn/flummox/releases) provides Linux
x86_64/ARM64 archives, an Apple Silicon macOS app, and a Windows x64 installer
and portable ZIP. Each release shows its version and changelog, with SHA-256
digests displayed beside each download. The published v0.0.1 Mac app is a shell;
the development version adds native APFS storage.

On Arch, install `flummox-bin` from the AUR. On Apple Silicon, run
`brew tap bybrooklyn/flummox` and `brew install --cask flummox`.
Windows users can run the setup executable for Start menu integration, upgrades,
and an uninstaller in Installed apps.

Commit the completed acceptance records, tag that commit with `git tag v0.0.2`,
and push the tag with `git push origin v0.0.2`. CI builds the tag version and publishes after native
build and installation checks pass. See [installation](install.md) and
[release automation](release-automation.md) for details.
