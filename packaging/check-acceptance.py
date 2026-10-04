#!/usr/bin/env python3
"""Require recorded real-game acceptance before publishing a new tag."""
import argparse
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REQUIRED = {'native-linux', 'proton', 'windows', 'macos'}


def check(root, version):
    path = root / 'docs/validation' / f'{version}.json'
    data = json.loads(path.read_text())
    if data.get('version') != version or data.get('status') != 'passed':
        raise ValueError('Real-game acceptance is pending; use a workflow dispatch for build validation')
    runs = data.get('runs', [])
    if {run.get('kind') for run in runs} != REQUIRED or len(runs) != len(REQUIRED):
        raise ValueError('Acceptance requires Linux native, Proton, Windows, and Mac runs')
    for run in runs:
        if not run.get('tester') or not run.get('date') or not run.get('game'):
            raise ValueError('Acceptance run lacks tester, date, or game')
        evidence = (root / 'docs/validation' / run['report']).resolve()
        if not evidence.is_relative_to((root / 'docs/validation').resolve()):
            raise ValueError('Report path escaped the validation directory')
        report = json.loads(evidence.read_text())
        checks = report['checks']
        if not all(checks.get(name) is True for name in ['bytes_verified', 'metadata_verified', 'writable_update_verified', 'rollback_verified', 'launched']):
            raise ValueError(f'Incomplete checks: {run["kind"]}')
        if checks.get('anti_cheat_issue') is not False or checks.get('gameplay_issue') is not False:
            raise ValueError(f'Compatibility issue remains: {run["kind"]}')
        if report.get('flummox_version') != version or not report.get('game', {}).get('build'):
            raise ValueError('Acceptance report is for a different version or missing its game build')
        if not all(isinstance(checks.get(name), int) and checks[name] > 0 for name in ['baseline_load_ms', 'candidate_load_ms']):
            raise ValueError('Load-time measurements are missing')
        platform = 'linux' if run['kind'] in {'native-linux', 'proton'} else run['kind']
        if report.get('platform') != platform:
            raise ValueError('Acceptance platform disagrees with its report')
        if not run.get('restart_verified') or not run.get('launcher_verification_passed'):
            raise ValueError('Restart and launcher verification evidence is missing')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', required=True)
    args = parser.parse_args()
    check(ROOT, args.version)


if __name__ == '__main__':
    main()
