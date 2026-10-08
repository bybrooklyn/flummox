#!/usr/bin/env python3
"""Bundle native Linux, macOS, and Windows release binaries."""
import argparse
import hashlib
import plistlib
import subprocess
import zipfile
from pathlib import Path
import shutil
import tarfile
import tempfile
import tomllib
import json

ROOT = Path(__file__).resolve().parent.parent


def split_debug(executable, debug_dir):
    """Strip a staging copy, preserving matching symbols outside the download."""
    tool = shutil.which('objcopy') or shutil.which('llvm-objcopy')
    if not tool:
        raise RuntimeError('Linux packaging requires objcopy or llvm-objcopy')
    debug_dir.mkdir(parents=True, exist_ok=True)
    symbols = debug_dir / (executable.name + '.debug')
    before = executable.stat().st_size
    subprocess.run([tool, '--only-keep-debug', str(executable), str(symbols)], check=True)
    subprocess.run([tool, '--strip-debug', str(executable)], check=True)
    subprocess.run([tool, '--add-gnu-debuglink=' + str(symbols.resolve()), str(executable)], check=True)
    return {'file': executable.name, 'before': before, 'after': executable.stat().st_size}


NOTICES = 'THIRD-PARTY-LICENSES.txt'


def notices_input(path, required):
    """The generated third-party licence file, or None when it is absent and optional."""
    if path.is_file():
        return path
    if required:
        raise RuntimeError(f'Release build requires the third-party licence notices, missing: {path}')
    return None


def linux_inputs(binaries, notices):
    files = {
        'bin/flummox': binaries / 'flummox',
        'bin/flummox-gui': binaries / 'flummox-gui',
        'share/applications/flummox.desktop': ROOT / 'packaging/flummox.desktop',
        'lib/systemd/user/flummox-watch.service': ROOT / 'packaging/flummox-watch.service',
        'share/doc/flummox/README.md': ROOT / 'README.md',
        'share/doc/flummox/usage.md': ROOT / 'docs/usage.md',
        'share/doc/flummox/status.md': ROOT / 'docs/status.md',
        'share/doc/flummox/install.md': ROOT / 'docs/install.md',
        'share/licenses/flummox/LICENSE': ROOT / 'LICENSE',
    }
    if notices:
        files['share/licenses/flummox/' + NOTICES] = notices
    return files


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / 'dist')
    parser.add_argument('--binaries', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--platform', choices=['linux', 'macos', 'windows'], default='linux')
    parser.add_argument('--arch', choices=['x86_64', 'aarch64'], default='x86_64')
    parser.add_argument('--debug-output', type=Path, default=ROOT / 'release-debug')
    parser.add_argument('--notices', type=Path, default=ROOT / NOTICES)
    parser.add_argument('--require-notices', action='store_true', help='fail when the notices file is absent')
    args = parser.parse_args()
    notices = notices_input(args.notices, args.require_notices)
    version = tomllib.loads((ROOT / 'Cargo.toml').read_text())['package']['version']
    name = f'flummox-{version}-{args.platform}-{args.arch}'
    if args.platform != 'linux':
        package_desktop(args, version, name, notices)
        return
    files = linux_inputs(args.binaries, notices)
    if (ROOT / 'RELEASE-NOTES.md').is_file():
        files['share/doc/flummox/CHANGELOG.md'] = ROOT / 'RELEASE-NOTES.md'
    for destination, source in files.items():
        if not source.is_file():
            raise RuntimeError(f'Missing release input: {source}')
        if destination.startswith('bin/'):
            # Refuse a mislabeled ELF architecture.
            with source.open('rb') as executable:
                header = executable.read(20)
            if header[:6] != b'\x7fELF\x02\x01' or header[18:20] != {'x86_64': b'\x3e\x00', 'aarch64': b'\xb7\x00'}[args.arch]:
                raise RuntimeError(f'Expected a Linux {args.arch} ELF executable: {source}')
    args.output.mkdir(parents=True, exist_ok=True)
    archive = args.output / f'{name}.tar.xz'
    sizes = []
    with tempfile.TemporaryDirectory(prefix='flummox-release-') as temporary:
        stage = Path(temporary) / name
        for destination, source in files.items():
            target = stage / destination
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
            target.chmod(0o755 if destination.startswith('bin/') else 0o644)
            if destination.startswith('bin/'):
                sizes.append(split_debug(target, args.debug_output / name))
                result = subprocess.run([str(target), '--version'], check=True, capture_output=True, text=True)
                if result.stdout.strip() != f'flummox {version}':
                    raise RuntimeError(f'Unexpected packaged version: {target}')
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
    (args.output / f'{name}-sizes.json').write_text(json.dumps({'executables': sizes, 'archive': archive.stat().st_size}, indent=2) + '\n')
    with archive.open('rb') as content:
        digest = hashlib.file_digest(content, 'sha256').hexdigest()
    checksum = args.output / f'{archive.name}.sha256'
    checksum.write_text(f'{digest}  {archive.name}\n')
    print(archive)
    print(checksum)



def checksum(artifact):
    with artifact.open('rb') as content:
        digest = hashlib.file_digest(content, 'sha256').hexdigest()
    artifact.with_name(artifact.name + '.sha256').write_text(f'{digest}  {artifact.name}\n')


def package_desktop(args, version, name, notices):
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='flummox-release-') as temporary:
        stage = Path(temporary)
        if args.platform == 'macos':
            executable = args.binaries / 'flummox-gui'
            header = executable.read_bytes()[:8]
            if args.arch != 'aarch64' or header != b'\xcf\xfa\xed\xfe\x0c\x00\x00\x01':
                raise RuntimeError('Expected an Apple Silicon Mach-O executable')
            contents = stage / 'Flummox.app/Contents'
            (contents / 'MacOS').mkdir(parents=True)
            (contents / 'Resources').mkdir()
            shutil.copy2(executable, contents / 'MacOS/flummox-gui')
            (contents / 'MacOS/flummox-gui').chmod(0o755)
            cli = args.binaries / 'flummox'
            if cli.read_bytes()[:8] != b'\xcf\xfa\xed\xfe\x0c\x00\x00\x01':
                raise RuntimeError('Expected an Apple Silicon CLI executable')
            shutil.copy2(cli, contents / 'MacOS/flummox')
            (contents / 'MacOS/flummox').chmod(0o755)
            info = {'CFBundleName': 'Flummox', 'CFBundleDisplayName': 'Flummox',
                    'CFBundleIdentifier': 'dev.bybrooklyn.flummox',
                    'CFBundleExecutable': 'flummox-gui', 'CFBundlePackageType': 'APPL',
                    'CFBundleShortVersionString': version.split('-')[0],
                    'CFBundleVersion': version.split('-')[0],
                    'LSMinimumSystemVersion': '14.0', 'NSHighResolutionCapable': True}
            (contents / 'Info.plist').write_bytes(plistlib.dumps(info))
            for filepath in ['LICENSE', 'RELEASE-NOTES.md']:
                shutil.copy2(ROOT / filepath, contents / 'Resources' / filepath)
            if notices:
                shutil.copy2(notices, contents / 'Resources' / NOTICES)
            subprocess.run(['codesign', '--force', '--deep', '--sign', '-', str(contents.parent)], check=True)
            subprocess.run(['codesign', '--verify', '--deep', '--strict', str(contents.parent)], check=True)
            artifact = args.output / f'{name}.zip'
            subprocess.run(['ditto', '-c', '-k', '--sequesterRsrc', '--keepParent', str(contents.parent), str(artifact)], check=True)
        else:
            if args.arch != 'x86_64':
                raise ValueError('Windows release supports x86_64')
            artifact = args.output / f'{name}.zip'
            with zipfile.ZipFile(artifact, 'w', compression=zipfile.ZIP_DEFLATED) as bundle:
                for filename in ['flummox.exe', 'flummox-gui.exe']:
                    executable = args.binaries / filename
                    with executable.open('rb') as stream:
                        if stream.read(2) != b'MZ':
                            raise RuntimeError(f'Expected a Windows PE executable: {executable}')
                        stream.seek(0x3c)
                        offset = int.from_bytes(stream.read(4), 'little')
                        stream.seek(offset)
                        if stream.read(6) != b'PE\x00\x00\x64\x86':
                            raise RuntimeError(f'Expected an x64 Windows executable: {executable}')
                    bundle.write(executable, filename)
                for filename in ['LICENSE', 'RELEASE-NOTES.md']:
                    bundle.write(ROOT / filename, filename)
                if notices:
                    bundle.write(notices, NOTICES)
            installer = args.output / f'{name}-setup.exe'
            if not installer.is_file():
                raise RuntimeError(f'Missing Windows installer: {installer}')
            checksum(installer)
        checksum(artifact)
        print(artifact)


if __name__ == '__main__':
    main()
