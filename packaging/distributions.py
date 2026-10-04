#!/usr/bin/env python3
"""Generate checksum-pinned AUR and Homebrew recipes from built releases."""
import argparse
import hashlib
from pathlib import Path
import re


def generate(version, assets, output):
    if not re.fullmatch(r'\d+\.\d+\.\d+', version):
        raise ValueError('Package channels accept stable releases only')
    url = f'https://github.com/bybrooklyn/flummox/releases/download/v{version}'

    def digest(filename):
        with (assets / filename).open('rb') as stream:
            return hashlib.file_digest(stream, 'sha256').hexdigest()

    aur = output / 'aur/flummox-bin'
    aur.mkdir(parents=True, exist_ok=True)
    archives = {arch: f'flummox-{version}-linux-{arch}.tar.xz' for arch in ['x86_64', 'aarch64']}
    hashes = {arch: digest(filename) for arch, filename in archives.items()}
    pkg = f'''# Maintainer: Brooklyn <brooklyn.halmstad@proton.me>
pkgname=flummox-bin
pkgver={version}
pkgrel=1
pkgdesc="Compress installed games and keep playing them"
arch=('x86_64' 'aarch64')
url="https://github.com/bybrooklyn/flummox"
license=('AGPL-3.0-or-later')
depends=('glibc>=2.39' 'gcc-libs' 'libxkbcommon' 'libxkbcommon-x11' 'wayland' 'libx11' 'libxcursor' 'libxi' 'libxrandr' 'fontconfig')
optdepends=('fuse3: Maximum Space mounts' 'zenity: native folder picker' 'kdialog: KDE folder picker')
provides=('flummox')
conflicts=('flummox')
options=('!strip' '!debug')
source_x86_64=("{url}/{archives['x86_64']}")
source_aarch64=("{url}/{archives['aarch64']}")
sha256sums_x86_64=('{hashes['x86_64']}')
sha256sums_aarch64=('{hashes['aarch64']}')

package() {{
    cd "$srcdir/flummox-$pkgver-linux-$CARCH"
    install -Dm755 bin/flummox "$pkgdir/usr/bin/flummox"
    install -Dm755 bin/flummox-gui "$pkgdir/usr/bin/flummox-gui"
    install -Dm644 share/applications/flummox.desktop "$pkgdir/usr/share/applications/flummox.desktop"
    install -Dm644 lib/systemd/user/flummox-watch.service "$pkgdir/usr/lib/systemd/user/flummox-watch.service"
    install -Dm644 share/licenses/flummox/LICENSE "$pkgdir/usr/share/licenses/flummox/LICENSE"
    install -d "$pkgdir/usr/share/doc/flummox"
    cp -r share/doc/flummox/. "$pkgdir/usr/share/doc/flummox/"
}}
'''
    (aur / 'PKGBUILD').write_text(pkg)
    # Generated without sourcing the PKGBUILD, so the same generator runs on macOS.
    srcinfo = f'pkgbase = flummox-bin\n\tpkgdesc = Compress installed games and keep playing them\n\tpkgver = {version}\n\tpkgrel = 1\n\turl = https://github.com/bybrooklyn/flummox\n'
    for value in ['x86_64', 'aarch64']:
        srcinfo += f'\tarch = {value}\n'
    srcinfo += '\tlicense = AGPL-3.0-or-later\n'
    for value in ['glibc>=2.39', 'gcc-libs', 'libxkbcommon', 'libxkbcommon-x11', 'wayland', 'libx11', 'libxcursor', 'libxi', 'libxrandr', 'fontconfig']:
        srcinfo += f'\tdepends = {value}\n'
    for value in ['fuse3: Maximum Space mounts', 'zenity: native folder picker', 'kdialog: KDE folder picker']:
        srcinfo += f'\toptdepends = {value}\n'
    srcinfo += '\tprovides = flummox\n\tconflicts = flummox\n\toptions = !strip\n\toptions = !debug\n'
    for arch, filename in archives.items():
        srcinfo += f'\tsource_{arch} = {url}/{filename}\n\tsha256sums_{arch} = {hashes[arch]}\n'
    (aur / '.SRCINFO').write_text(srcinfo + '\npkgname = flummox-bin\n')
    casks = output / 'homebrew/Casks'
    casks.mkdir(parents=True, exist_ok=True)
    mac = f'flummox-{version}-macos-aarch64.zip'
    (casks / 'flummox.rb').write_text(f'''cask "flummox" do
  version "{version}"
  sha256 "{digest(mac)}"

  url "{url}/{mac}"
  name "Flummox"
  desc "Compress installed games using native APFS storage"
  homepage "https://github.com/bybrooklyn/flummox"

  depends_on arch: :arm64
  depends_on macos: ">= :sonoma"
  app "Flummox.app"
end
''')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', required=True)
    parser.add_argument('--assets', type=Path, default=Path('dist'))
    parser.add_argument('--output', type=Path, default=Path('dist/recipes'))
    args = parser.parse_args()
    generate(args.version, args.assets, args.output)


if __name__ == '__main__':
    main()
