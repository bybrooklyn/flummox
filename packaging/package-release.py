#!/usr/bin/env python3
"""Bundle already-built Linux binaries and their desktop integration files."""
import argparse
import hashlib
from pathlib import Path
import shutil
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / 'dist')
    parser.add_argument('--binaries', type=Path, default=ROOT / 'target/release')
    args = parser.parse_args()
    version = tomllib.loads((ROOT / 'Cargo.toml').read_text())['package']['version']
    name = f'flummox-{version}-linux-x86_64'
    files = {
        'bin/flummox': args.binaries / 'flummox',
        'bin/flummox-gui': args.binaries / 'flummox-gui',
        'share/applications/flummox.desktop': ROOT / 'packaging/flummox.desktop',
        'lib/systemd/user/flummox-watch.service': ROOT / 'packaging/flummox-watch.service',
        'share/doc/flummox/README.md': ROOT / 'README.md',
        'share/doc/flummox/release-readiness.md': ROOT / 'docs/release-readiness.md',
        'share/doc/flummox/install.md': ROOT / 'docs/install.md',
        'share/doc/flummox/next-steps.md': ROOT / 'docs/next-steps.md',
        'share/licenses/flummox/LICENSE': ROOT / 'LICENSE',
    }
    for destination, source in files.items():
        if not source.is_file():
            raise RuntimeError(f'Missing release input: {source}')
        if destination.startswith('bin/'):
            # ELF e_machine = EM_X86_64. Refuse a mislabeled cross-build.
            with source.open('rb') as executable:
                header = executable.read(20)
            if header[:6] != b'\x7fELF\x02\x01' or header[18:20] != b'\x3e\x00':
                raise RuntimeError(f'Expected a Linux x86_64 ELF executable: {source}')
    args.output.mkdir(parents=True, exist_ok=True)
    archive = args.output / f'{name}.tar.xz'
    with tempfile.TemporaryDirectory(prefix='flummox-release-') as temporary:
        stage = Path(temporary) / name
        for destination, source in files.items():
            target = stage / destination
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
            target.chmod(0o755 if destination.startswith('bin/') else 0o644)
        with tarfile.open(archive, 'w:xz') as bundle:
            for path in sorted(stage.rglob('*')):
                info = bundle.gettarinfo(path, arcname=str(path.relative_to(stage.parent)))
                info.uid = info.gid = 0
                info.uname = info.gname = ''
                info.mtime = 0
                if path.is_file():
                    with path.open('rb') as content:
                        bundle.addfile(info, content)
                else:
                    bundle.addfile(info)
    with archive.open('rb') as content:
        digest = hashlib.file_digest(content, 'sha256').hexdigest()
    checksum = args.output / f'{archive.name}.sha256'
    checksum.write_text(f'{digest}  {archive.name}\n')
    print(archive)
    print(checksum)


if __name__ == '__main__':
    main()
