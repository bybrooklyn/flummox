#!/usr/bin/env python3
"""Create or verify a signed manifest of finalized release downloads."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parent.parent


def artifact_names(version):
    names = [f'flummox-{version}-{suffix}' for suffix in (
        'linux-x86_64.tar.xz', 'linux-aarch64.tar.xz', 'macos-aarch64.zip',
        'windows-x86_64.zip', 'windows-x86_64-setup.exe')]
    if '-' not in version:
        names.append(f'flummox-bin-{version}-1-x86_64.pkg.tar.zst')
    return sorted(names)


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def create(assets, version, commit):
    if not re.fullmatch(r'\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?', version):
        raise ValueError('Invalid release version')
    if not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('Invalid release commit')
    actual = sorted(path.name for path in assets.iterdir() if path.name.endswith(('.tar.xz', '.zip', '-setup.exe', '.pkg.tar.zst')))
    if actual != artifact_names(version):
        raise ValueError('Release contains missing or unlisted binary downloads')
    records = []
    for name in artifact_names(version):
        path = assets / name
        if not path.is_file() or path.is_symlink():
            raise ValueError(f'Missing release artifact: {name}')
        records.append({'file': name, 'bytes': path.stat().st_size, 'sha256': digest(path)})
    manifest = assets / f'flummox-{version}-manifest.json'
    manifest.write_text(json.dumps({'schema': 1, 'tag': f'v{version}', 'commit': commit, 'artifacts': records}, indent=2) + '\n')
    return manifest


def verify(assets, manifest, public_key, version, commit=None):
    subprocess.run(['minisign', '-V', '-H', '-m', str(manifest), '-p', str(public_key)], check=True)
    data = json.loads(manifest.read_text())
    if data['schema'] != 1 or data['tag'] != f'v{version}':
        raise ValueError('Manifest belongs to a different release')
    if commit is not None and data['commit'] != commit:
        raise ValueError('Manifest belongs to a different commit')
    if [record['file'] for record in data['artifacts']] != artifact_names(version):
        raise ValueError('Manifest does not contain the exact release artifact set')
    for record in data['artifacts']:
        path = assets / record['file']
        if path.is_symlink() or path.stat().st_size != record['bytes'] or digest(path) != record['sha256']:
            raise ValueError(f'Artifact differs from signed manifest: {path.name}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['sign', 'verify'])
    parser.add_argument('--assets', type=Path, default=Path('dist'))
    parser.add_argument('--version', required=True)
    parser.add_argument('--commit')
    parser.add_argument('--key', type=Path)
    parser.add_argument('--public-key', type=Path, default=ROOT / 'packaging/minisign.pub')
    args = parser.parse_args()
    manifest = args.assets / f'flummox-{args.version}-manifest.json'
    if args.action == 'sign':
        if args.key is None or args.commit is None:
            parser.error('Signing requires --key and --commit')
        manifest = create(args.assets, args.version, args.commit)
        subprocess.run(['minisign', '-S', '-W', '-s', str(args.key), '-m', str(manifest), '-t', f'Flummox v{args.version} commit {args.commit}'], check=True)
    verify(args.assets, manifest, args.public_key, args.version, args.commit)


if __name__ == '__main__':
    main()
