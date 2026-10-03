#!/usr/bin/env python3
"""Push generated package recipes with a channel-specific SSH key."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('channel', choices=['aur', 'homebrew'])
    parser.add_argument('--version', required=True)
    args = parser.parse_args()
    secret = os.environ.get('DEPLOY_KEY', '')
    if not secret:
        name = 'AUR_SSH_PRIVATE_KEY' if args.channel == 'aur' else 'HOMEBREW_DEPLOY_KEY'
        raise SystemExit(f'Configure the {name} repository secret before updating this channel')
    with tempfile.TemporaryDirectory(prefix='flummox-channel-') as temporary:
        work = Path(temporary)
        key = work / 'key'
        key.write_text(secret + '\n')
        key.chmod(0o600)
        hosts = work / 'known_hosts'
        if args.channel == 'homebrew':
            with urllib.request.urlopen('https://api.github.com/meta', timeout=30) as response:
                keys = json.load(response)['ssh_keys']
            hosts.write_text(''.join(f'github.com {value}\n' for value in keys))
            remote = 'git@github.com:bybrooklyn/homebrew-flummox.git'
            source = Path('dist/recipes/homebrew')
            branch = 'main'
        else:
            scanned = subprocess.check_output(['ssh-keyscan', '-T', '20', '-t', 'ed25519', 'aur.archlinux.org'], text=True)
            valid = []
            # Published by https://aur.archlinux.org/ under SSH fingerprints.
            expected = 'RFzBCUItH9LZS0cKB5UE6ceAYhBD5C8GeOBip8Z11+4'
            for line in scanned.splitlines():
                parts = line.split()
                if len(parts) == 3:
                    digest = base64.b64encode(hashlib.sha256(base64.b64decode(parts[2])).digest()).decode().rstrip('=')
                    if digest == expected:
                        valid.append(line)
            if not valid:
                raise SystemExit('AUR SSH host key does not match its published fingerprint')
            hosts.write_text('\n'.join(valid) + '\n')
            remote = 'ssh://aur@aur.archlinux.org/flummox-bin.git'
            source = Path('dist/recipes/aur/flummox-bin')
            branch = 'master'
        env = dict(os.environ)
        env.pop('DEPLOY_KEY', None)
        env['GIT_SSH_COMMAND'] = shlex.join(['ssh', '-i', str(key), '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes', '-o', f'UserKnownHostsFile={hosts}'])
        repository = work / 'repository'
        subprocess.run(['git', 'clone', remote, str(repository)], check=True, env=env)
        for entry in source.rglob('*'):
            if entry.is_file():
                destination = repository / entry.relative_to(source)
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(entry, destination)
        for command in [
            ['git', 'checkout', '-B', branch],
            ['git', 'config', 'user.name', 'Brooklyn'],
            ['git', 'config', 'user.email', 'brooklyn.halmstad@proton.me'],
            ['git', 'add', 'Casks'] if args.channel == 'homebrew' else ['git', 'add', 'PKGBUILD', '.SRCINFO'],
        ]:
            subprocess.run(command, cwd=repository, check=True, env=env)
        changed = subprocess.run(['git', 'diff', '--cached', '--quiet'], cwd=repository, env=env).returncode
        if changed == 0:
            print('Channel already has this release')
            return
        if changed != 1:
            raise SystemExit('Could not inspect staged package update')
        subprocess.run(['git', 'commit', '-m', f'Update Flummox to {args.version}'], cwd=repository, check=True, env=env)
        subprocess.run(['git', 'push', 'origin', branch], cwd=repository, check=True, env=env)


if __name__ == '__main__':
    main()
