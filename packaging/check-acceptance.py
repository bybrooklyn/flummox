#!/usr/bin/env python3
"""Require recorded real-game acceptance before publishing a stable tag.

Reads docs/validation/<version>.json for the version it is given and fails
when that record or any report it names is missing or incomplete.
"""
import argparse
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REQUIRED = {'native-linux', 'proton', 'windows', 'macos'}


def positive_u64(value):
    return type(value) is int and 0 < value <= 2**64 - 1


def version_matches(recorded, version):
    """A report names the release itself or one of its release candidates."""
    return isinstance(recorded, str) and (
        recorded == version or re.fullmatch(re.escape(version) + r'-rc\.\d+', recorded) is not None)


def validate_report(report, kind, version):
    if not isinstance(report, dict) or type(report.get('version')) is not int or report['version'] != 1:
        raise ValueError('Unsupported compatibility report schema')
    platform = 'linux' if kind in {'native-linux', 'proton'} else kind
    mode = 'maximum-space' if platform == 'linux' else 'native'
    if report.get('platform') != platform or report.get('mode') != mode:
        raise ValueError('Acceptance platform or storage mode disagrees with its report')
    if not version_matches(report.get('flummox_version'), version):
        raise ValueError('Acceptance report is for a different Flummox version')
    game = report.get('game', {})
    if not isinstance(game, dict) or not all(isinstance(game.get(name), str) and game[name].strip() for name in ['launcher', 'key', 'build']):
        raise ValueError('Acceptance report game identity is incomplete')
    corpus = report.get('corpus', {})
    if not isinstance(corpus, dict) or not isinstance(corpus.get('sha256'), str) or not re.fullmatch(r'[0-9a-fA-F]{64}', corpus['sha256']):
        raise ValueError('Acceptance report corpus hash is invalid')
    if not all(positive_u64(corpus.get(name)) for name in ['files', 'bytes']):
        raise ValueError('Acceptance report corpus is empty or invalid')
    storage = report.get('storage', {})
    if not isinstance(storage, dict) or not all(positive_u64(storage.get(name)) for name in ['logical_bytes', 'allocated_before', 'allocated_after']):
        raise ValueError('Allocated-byte measurements are missing or invalid')
    if storage['logical_bytes'] != corpus['bytes']:
        raise ValueError('Acceptance report corpus sizes disagree')
    if storage['allocated_after'] > storage['allocated_before']:
        raise ValueError('Acceptance report allocated more bytes after compression than before')
    checks = report.get('checks', {})
    if not isinstance(checks, dict) or not all(checks.get(name) is True for name in ['bytes_verified', 'metadata_verified', 'writable_update_verified', 'rollback_verified', 'launched']):
        raise ValueError(f'Incomplete checks: {kind}')
    if checks.get('anti_cheat_issue') is not False or checks.get('gameplay_issue') is not False:
        raise ValueError(f'Compatibility issue remains: {kind}')
    if not all(positive_u64(checks.get(name)) for name in ['baseline_load_ms', 'candidate_load_ms']):
        raise ValueError('Load-time measurements are missing or invalid')


def check(root, version):
    path = root / 'docs/validation' / f'{version}.json'
    data = json.loads(path.read_text())
    if data.get('version') != version or data.get('status') != 'passed':
        raise ValueError('Real-game acceptance is pending; use a workflow dispatch for build validation')
    runs = data.get('runs', [])
    if {run.get('kind') for run in runs} != REQUIRED or len(runs) != len(REQUIRED):
        raise ValueError('Acceptance requires Linux native, Proton, Windows, and Mac runs')
    seen_reports = set()
    seen_corpora = set()
    for run in runs:
        if not run.get('tester') or not run.get('date') or not isinstance(run.get('game'), str) or not run['game'].strip():
            raise ValueError('Acceptance run lacks tester, date, or game')
        if not isinstance(run.get('report'), str) or not run['report']:
            raise ValueError('Acceptance run names no report file')
        evidence = (root / 'docs/validation' / run['report']).resolve()
        if not evidence.is_relative_to((root / 'docs/validation').resolve()):
            raise ValueError('Report path escaped the validation directory')
        if evidence in seen_reports:
            raise ValueError('Two acceptance runs name the same report file')
        seen_reports.add(evidence)
        report = json.loads(evidence.read_text())
        validate_report(report, run['kind'], version)
        corpus_hash = report['corpus']['sha256'].lower()
        if corpus_hash in seen_corpora:
            raise ValueError('Two acceptance runs share one corpus hash')
        seen_corpora.add(corpus_hash)
        # The report holds a launcher key and build, not a title, so the run
        # records the key it claims and it must be the report's.
        if run.get('game_key') != report['game']['key'] or run.get('game_build') != report['game']['build']:
            raise ValueError('Acceptance run game disagrees with its report')
        if run.get('restart_verified') is not True or run.get('launcher_verification_passed') is not True:
            raise ValueError('Restart and launcher verification evidence is missing')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', required=True)
    args = parser.parse_args()
    check(ROOT, args.version)


if __name__ == '__main__':
    main()
