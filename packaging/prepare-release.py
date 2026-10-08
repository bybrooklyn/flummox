#!/usr/bin/env python3
"""Apply a release tag to the build checkout and generate its changelog."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parent.parent


def check_tag_against_source(root, version, current):
    """The tag must name the version the source declares, and a stable tag needs curated notes."""
    base = version.split('-')[0]
    if base != current.split('-')[0]:
        raise ValueError(f'Tag version {version} does not match Cargo.toml version {current}; bump Cargo.toml in the tagged commit')
    if '-' not in version and not (root / 'docs/releases' / f'{version}.md').is_file():
        raise ValueError(f'A stable release needs docs/releases/{version}.md')


def prepare(root, tag):
    manifest = root / 'Cargo.toml'
    current = tomllib.loads(manifest.read_text())['package']['version']
    version = tag.removeprefix('v') if tag else current
    if not re.fullmatch(r'(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?', version):
        raise ValueError('Use a SemVer tag such as v0.0.1 or v0.0.1-rc.1')
    if tag and tag != f'v{version}':
        raise ValueError('Release tags must start with v')
    if tag:
        check_tag_against_source(root, version, current)
    manifest.write_text(re.sub(r'^version = "[^"]+"$', f'version = "{version}"', manifest.read_text(), count=1, flags=re.M))
    lock = root / 'Cargo.lock'
    pattern = r'(\[\[package\]\]\nname = "flummox"\nversion = ")[^"]+("\n)'
    updated, count = re.subn(pattern, lambda match: match[1] + version + match[2], lock.read_text())
    if count != 1:
        raise ValueError('Expected one Flummox package in Cargo.lock')
    lock.write_text(updated)
    return version


def release_notes(root, version, start, tag):
    curated = root / 'docs/releases' / f'{version}.md'
    if curated.is_file():
        return curated.read_text()
    commits = subprocess.check_output(['git', 'log', '--format=- %s (%h)', f'{start}..HEAD' if start else 'HEAD'], cwd=root, text=True)
    notes = f'# Flummox {version}\n\n## Changes\n\n{commits}\n'
    notes += '## Downloads\n\nLinux: x86_64 and ARM64 archives. macOS: Apple Silicon app bundle. Windows: x64 installer and portable ZIP. GitHub displays a SHA-256 digest beside each download.\n\n'
    notes += 'macOS supports native APFS compression. Linux archives require glibc 2.39 or newer. Unsigned macOS and Windows downloads can trigger operating-system security prompts.\n'
    if start:
        notes += f'\n[Full changelog](https://github.com/bybrooklyn/flummox/compare/{start}...{tag or "HEAD"})\n'
    return notes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--tag', default='')
    args = parser.parse_args()
    version = prepare(ROOT, args.tag)
    previous = subprocess.run(['git', 'describe', '--tags', '--abbrev=0', '--match', 'v[0-9]*', 'HEAD^'], cwd=ROOT, text=True, capture_output=True)
    start = previous.stdout.strip() if previous.returncode == 0 else ''
    (ROOT / 'RELEASE-NOTES.md').write_text(release_notes(ROOT, version, start, args.tag))
    if output := os.environ.get('GITHUB_OUTPUT'):
        with open(output, 'a') as stream:
            stream.write(f'version={version}\nprerelease={str("-" in version).lower()}\n')
    print(version)


if __name__ == '__main__':
    main()
