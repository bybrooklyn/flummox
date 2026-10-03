# Linux installation and upgrades

## Arch and CachyOS

Install a downloaded Arch package with `sudo pacman -U flummox-*.pkg.tar.zst`.
From a source checkout, run `cd packaging && makepkg -si`. This builds both
binaries with Maximum Space support. Install `fuse3` for Maximum Space and
`kdialog` or `zenity` for the desktop folder picker.

## Linux x86_64 archive

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

## Build release files

Run `just release` to build the Linux binaries and place a versioned archive and
SHA-256 checksum in `dist/`. The Release workflow can also be run manually to
produce downloadable workflow artifacts. A matching `v` tag runs the checks and
creates Linux archive and Arch package downloads in a draft GitHub release; desktop acceptance remains required before
publishing the draft.
