#!/usr/bin/env python3
"""Run the filesystem-sensitive test binaries from a btrfs mount.

Each run must report `test result: ok. N passed` with N at or above a floor,
so a filter that matches nothing fails instead of passing with zero tests.
FLUMMOX_REQUIRE_BTRFS is set for the children, which turns a test's own
"not btrfs" skip into a failure.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent

# (target kind or name, extra arguments, minimum passed). The floors sit below
# the number of #[test] functions in each target on Linux, so they catch a
# filter that stops matching without breaking when one test is added.
RUNS = [
    ('lib', [], 200),
    ('lib', ['backend::btrfs'], 5),
    ('lib', ['allocation::'], 3),
    ('btrfs_backend', [], 5),
    ('jobs_lifecycle', [], 10),
]


def passed(output):
    """The passed count of the one `test result: ok.` line in `output`."""
    found = re.findall(r'^test result: ok\. (\d+) passed;', output, flags=re.M)
    if len(found) != 1:
        raise ValueError(f'Expected one passing test summary, found {len(found)}')
    return int(found[0])


def require(output, floor):
    count = passed(output)
    if count < floor:
        raise ValueError(f'Only {count} tests passed, expected at least {floor}')
    return count


def executables(build_output):
    """Map each test target to its executable from `cargo --message-format=json`."""
    found = {}
    for line in build_output.splitlines():
        if not line.startswith('{'):
            continue
        item = json.loads(line)
        if item.get('reason') != 'compiler-artifact' or not item.get('executable'):
            continue
        if not item.get('profile', {}).get('test'):
            continue
        target = item['target']
        key = 'lib' if target['kind'] == ['lib'] else target['name']
        found[key] = item['executable']
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mount', type=Path, required=True)
    args = parser.parse_args()
    build = subprocess.run(['cargo', 'test', '--locked', '--no-run', '--message-format=json'], cwd=ROOT, text=True, stdout=subprocess.PIPE)
    if build.returncode != 0:
        print(f'cargo test --no-run exited with {build.returncode}', file=sys.stderr)
        return 1
    built = executables(build.stdout)
    env = dict(os.environ, FLUMMOX_REQUIRE_BTRFS='1')
    # Cargo runs tests from the package root. These tests create their
    # temporary directories in the current directory, so run them from the mount.
    os.chdir(args.mount)
    failed = False
    for name, extra, floor in RUNS:
        executable = built.get(name)
        if executable is None:
            print(f'No test executable named {name}', file=sys.stderr)
            return 1
        result = subprocess.run([executable, *extra, '--nocapture'], env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        print(f'== {name} {" ".join(extra)}'.rstrip())
        print(result.stdout)
        if result.returncode != 0:
            print(f'{name} exited with {result.returncode}', file=sys.stderr)
            failed = True
            continue
        try:
            count = require(result.stdout, floor)
        except ValueError as problem:
            print(f'{name}: {problem}', file=sys.stderr)
            failed = True
            continue
        print(f'{name}: {count} passed (floor {floor})')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
