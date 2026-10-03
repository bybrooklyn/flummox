# Installation and upgrades

## Arch and CachyOS

Install a downloaded Arch package with `sudo pacman -U flummox-*.pkg.tar.zst`.
From a source checkout, run `cd packaging && makepkg -si`. This builds both
binaries with Maximum Space support. Install `fuse3` for Maximum Space and
`kdialog` or `zenity` for the desktop folder picker.

## Linux x86_64 and ARM64 archives

Choose `linux-x86_64` for Intel/AMD or `linux-aarch64` for ARM64.
The archive contains both binaries, desktop integration, and documentation.
It is built on Ubuntu 24.04 and requires glibc 2.39 or newer plus the windowing
libraries listed in the Arch package. It is not a static portable executable.

Verify the archive before extracting it:

```sh
sha256sum -c flummox-0.1.0-linux-x86_64.tar.xz.sha256
tar -xJf flummox-0.1.0-linux-x86_64.tar.xz
cd flummox-0.1.0-linux-x86_64
sudo install -Dm755 bin/flummox /usr/bin/flummox
sudo install -Dm755 bin/flummox-gui /usr/bin/flummox-gui
sudo install -Dm644 share/applications/flummox.desktop /usr/share/applications/flummox.desktop
sudo install -Dm644 share/licenses/flummox/LICENSE /usr/share/licenses/flummox/LICENSE
```

Adjust the archive version to the downloaded release. Source builds are available
for distributions with an older glibc. Launch `flummox-gui` from the desktop menu
or terminal. The coordinator starts as the current user; no root daemon is needed.
The optional legacy watch service is included separately and is not enabled by
installation. The desktop manages its own login startup when maintenance or
mounted installs require it.

## Upgrade a running installation

Finish or cancel queued jobs and restore mounted Maximum Space games before
replacing the binaries. After installation, use Settings > Restart worker or:

```sh
flummox jobs restart
```

Restart preserves queue history, library settings, exclusions, and reports. It
refuses while work is queued/running or compressed games remain mounted. Workers
from versions without the restart command require logging out and back in after
the upgrade. Close Flummox windows before logging out. Do not kill a coordinator
while storage work is running.

## macOS (Apple Silicon)

Install from the Homebrew tap:

```sh
brew tap bybrooklyn/flummox
brew install --cask flummox
brew upgrade --cask flummox
```

Alternatively, download `flummox-VERSION-macos-aarch64.zip`, verify its SHA-256,
and drag `Flummox.app` into Applications. macOS 14 or newer is required. The app
is ad hoc signed; Apple Developer ID signing and notarization are not configured.
macOS can require approval in System Settings > Privacy & Security when opening
the downloaded app. Compression is not implemented on macOS yet; this download
provides the desktop shell and displays its version.

## Windows (64-bit)

Download `flummox-VERSION-windows-x86_64-setup.exe` from GitHub Releases.
The installer installs into your user account, creates a Start menu shortcut,
and registers an uninstaller in Settings > Apps > Installed apps. No administrator
password is needed. Run a newer installer to upgrade the existing installation;
close Flummox and finish compression jobs first. Uninstalling removes the app
and shortcuts; it does not remove game files or application data.

A portable ZIP containing both executables is also available. The CLI is in the
installation directory; the installer does not change your PATH. Windows builds
are currently unsigned, so SmartScreen can display an unknown-publisher prompt.

## Arch User Repository

The stable binary package is `flummox-bin`, maintained by `bybrooklyn`:

```sh
yay -S flummox-bin
```

It supports x86_64 and ARM64, verifies each release archive with SHA-256, and
installs the GUI, CLI, desktop entry, and optional user service. Publication
requires the release automation's SSH key to be registered on the AUR account.

## Publish a release

Commit and push the changes first, then tag that commit and push the tag:

```sh
git add .
git commit -m "Prepare release"
git push origin main
git tag -a v0.0.1 -m "Flummox 0.0.1"
git push origin v0.0.1
```

Tags refer to the commit that exists when you create them. Creating a tag before
committing your changes would release the earlier commit. `git push` alone does
not normally push tags; push the tag explicitly as above.

The tag supplies the version in the CI build's Cargo manifest and lockfile;
there is no manual version bump required before later tags. The release includes
a changelog of commits since the previous reachable version tag, versioned
Linux x86_64/ARM64 archives, an Apple Silicon app bundle, Windows x64 installer
and portable ZIP, an Arch package, and SHA-256 checksums. All native builds,
tests, dependency checks, and installation checks must pass before the release
is published. Stable releases then update the Homebrew tap and AUR recipes.
`v0.0.2-rc.1` publishes a prerelease and leaves the stable package channels alone.
Do not reuse published tags.

The Release workflow can be run manually on a branch to build and test downloadable
artifacts without publishing a release. `just release` builds a local Linux
x86_64 archive. See [release automation](release-automation.md) for credentials,
retries, and signing.

## Games in other locations

Open **Drives & libraries**, choose **Games library**, and browse or enter a
location such as `~/My Games` or `/mnt/other-drive/Games`. Each immediate
subfolder appears as a separate game. Choose **Single game** when the folder
itself contains one game's files. Spaces, quoted paths, and `~/My\ Games` are
accepted; Flummox expands your home folder without executing shell commands.
Hidden folders, files directly in the library root, and symlinked subfolders
are excluded from collection discovery. Add a symlinked game directly if needed.
Refresh to discover newly installed subfolders. Overlapping locations are merged
with detected launcher games. Locations persist across app and coordinator
restarts. **Remove location** forgets the registration and preserves every file;
finish jobs and restore Maximum Space games in that location before removal.
Automatic maintenance remains off until you enable it for the location.

The CLI uses the same configuration:

```sh
flummox jobs add-folder '~/My Games'
flummox jobs add-folder '/mnt/games/One Game' --single-game
flummox jobs remove-folder '~/My Games'
```

The custom-location protocol is version 6. When upgrading a running version 5
coordinator, use `flummox jobs restart` before adding locations.
