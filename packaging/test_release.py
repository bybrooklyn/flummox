#!/usr/bin/env python3
"""Check tag versions, checksums, and release architecture validation."""
import importlib.util
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib
import unittest

HERE = Path(__file__).resolve().parent


def module(filename):
    spec = importlib.util.spec_from_file_location(filename, HERE / filename)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


class ReleaseTests(unittest.TestCase):
    def require_tools(self, *names):
        """Skip locally when a tool is missing, but fail under CI, which installs them."""
        missing = [name for name in names if not shutil.which(name)]
        if missing and os.environ.get('CI'):
            self.fail(f'CI must provide: {", ".join(missing)}')
        if missing:
            self.skipTest(f'{", ".join(missing)} needed')

    def test_acceptance_requires_complete_evidence_for_each_backend(self):
        acceptance = module('check-acceptance.py')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / 'docs/validation'
            directory.mkdir(parents=True)
            runs = []
            reports = {}
            for kind in sorted(acceptance.REQUIRED):
                linux = kind in {'native-linux', 'proton'}
                reports[kind] = {
                    'version': 1,
                    'game': {'launcher': 'manual', 'key': 'fixture', 'build': '1'},
                    'corpus': {'sha256': hashlib.sha256(kind.encode()).hexdigest(), 'files': 1, 'bytes': 1024},
                    'platform': 'linux' if linux else kind,
                    'mode': 'maximum-space' if linux else 'native',
                    'flummox_version': '0.0.2',
                    'storage': {'logical_bytes': 1024, 'allocated_before': 4096, 'allocated_after': 2048, 'random_read_p95_ns': None},
                    'checks': {
                        'bytes_verified': True, 'metadata_verified': True,
                        'writable_update_verified': True, 'rollback_verified': True,
                        'launched': True, 'anti_cheat_issue': False, 'gameplay_issue': False,
                        'baseline_load_ms': 1000, 'candidate_load_ms': 1050,
                    },
                }
                runs.append({'kind': kind, 'game': 'Fixture', 'tester': 'Fixture', 'date': '2026-10-05', 'report': f'{kind}.json', 'game_key': 'fixture', 'game_build': '1', 'restart_verified': True, 'launcher_verification_passed': True})
            manifest = {'version': '0.0.2', 'status': 'passed', 'runs': runs}

            def write_evidence(data, evidence):
                (directory / '0.0.2.json').write_text(json.dumps(data))
                for kind, report in evidence.items():
                    (directory / f'{kind}.json').write_text(json.dumps(report))

            write_evidence(manifest, reports)
            acceptance.check(root, '0.0.2')
            invalid_reports = [
                (['version'], True), (['version'], 2),
                (['mode'], 'native'), (['mode'], None), (['platform'], 'windows'),
                (['flummox_version'], '0.0.1'), (['game', 'build'], ''),
                (['game', 'key'], ''), (['corpus', 'sha256'], 'invalid'),
                (['corpus', 'files'], 0), (['corpus', 'bytes'], True),
                (['storage'], None), (['storage'], {}), (['storage', 'logical_bytes'], 2048),
                (['storage', 'allocated_before'], 0), (['storage', 'allocated_after'], True),
                (['checks', 'baseline_load_ms'], True), (['checks', 'baseline_load_ms'], 0),
                (['checks', 'candidate_load_ms'], 2**64),
                (['checks', 'metadata_verified'], False), (['checks', 'gameplay_issue'], True),
            ]
            for path, value in invalid_reports:
                with self.subTest(path=path, value=value):
                    changed = copy.deepcopy(reports)
                    target = changed['proton']
                    for key in path[:-1]:
                        target = target[key]
                    target[path[-1]] = value
                    write_evidence(manifest, changed)
                    with self.assertRaises(ValueError):
                        acceptance.check(root, '0.0.2')
            for field, value in [('restart_verified', 'yes'), ('launcher_verification_passed', 1)]:
                with self.subTest(field=field):
                    changed = copy.deepcopy(manifest)
                    changed['runs'][0][field] = value
                    write_evidence(changed, reports)
                    with self.assertRaises(ValueError):
                        acceptance.check(root, '0.0.2')
            for status in ['pending', 'failed']:
                changed = copy.deepcopy(manifest)
                changed['status'] = status
                write_evidence(changed, reports)
                with self.assertRaises(ValueError):
                    acceptance.check(root, '0.0.2')
            for kind in ['windows', 'macos']:
                changed = copy.deepcopy(reports)
                changed[kind]['mode'] = 'maximum-space'
                write_evidence(manifest, changed)
                with self.assertRaises(ValueError):
                    acceptance.check(root, '0.0.2')
            for changed_runs in [runs[:-1], runs[:-1] + [runs[0]]]:
                changed = copy.deepcopy(manifest)
                changed['runs'] = changed_runs
                write_evidence(changed, reports)
                with self.assertRaises(ValueError):
                    acceptance.check(root, '0.0.2')
            changed = copy.deepcopy(manifest)
            changed['runs'][0]['report'] = '../outside.json'
            (root / 'docs/outside.json').write_text(json.dumps(reports['macos']))
            write_evidence(changed, reports)
            with self.assertRaisesRegex(ValueError, 'escaped'):
                acceptance.check(root, '0.0.2')

    def acceptance_fixture(self, root, version, report_version):
        acceptance = module('check-acceptance.py')
        directory = root / 'docs/validation'
        directory.mkdir(parents=True, exist_ok=True)
        runs = []
        reports = {}
        for kind in sorted(acceptance.REQUIRED):
            linux = kind in {'native-linux', 'proton'}
            reports[kind] = {
                'version': 1,
                'game': {'launcher': 'manual', 'key': f'fixture-{kind}', 'build': '1'},
                'corpus': {'sha256': hashlib.sha256(kind.encode()).hexdigest(), 'files': 1, 'bytes': 1024},
                'platform': 'linux' if linux else kind,
                'mode': 'maximum-space' if linux else 'native',
                'flummox_version': report_version,
                'storage': {'logical_bytes': 1024, 'allocated_before': 4096, 'allocated_after': 2048, 'random_read_p95_ns': None},
                'checks': {
                    'bytes_verified': True, 'metadata_verified': True,
                    'writable_update_verified': True, 'rollback_verified': True,
                    'launched': True, 'anti_cheat_issue': False, 'gameplay_issue': False,
                    'baseline_load_ms': 1000, 'candidate_load_ms': 1050,
                },
            }
            runs.append({'kind': kind, 'game': 'Fixture', 'game_key': f'fixture-{kind}', 'game_build': '1', 'tester': 'Fixture', 'date': '2026-10-05', 'report': f'{kind}.json', 'restart_verified': True, 'launcher_verification_passed': True})
        manifest = {'version': version, 'status': 'passed', 'runs': runs}

        def write(data, evidence):
            (directory / f'{version}.json').write_text(json.dumps(data))
            for kind, report in evidence.items():
                (directory / f'{kind}.json').write_text(json.dumps(report))

        write(manifest, reports)
        return acceptance, manifest, reports, write

    def test_acceptance_ties_each_run_to_its_own_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            acceptance, manifest, reports, write = self.acceptance_fixture(root, '0.0.2', '0.0.2')
            acceptance.check(root, '0.0.2')
            # Two kinds naming one report file.
            changed = copy.deepcopy(manifest)
            changed['runs'][1]['report'] = changed['runs'][0]['report']
            changed['runs'][1]['game_key'] = changed['runs'][0]['game_key']
            write(changed, reports)
            with self.assertRaisesRegex(ValueError, 'same report'):
                acceptance.check(root, '0.0.2')
            # Two reports with one corpus hash.
            changed = copy.deepcopy(reports)
            first, second = sorted(changed)[:2]
            changed[second]['corpus']['sha256'] = changed[first]['corpus']['sha256'].upper()
            write(manifest, changed)
            with self.assertRaisesRegex(ValueError, 'corpus hash'):
                acceptance.check(root, '0.0.2')
            # Compression that allocated more than the original.
            changed = copy.deepcopy(reports)
            changed['proton']['storage']['allocated_after'] = 8192
            write(manifest, changed)
            with self.assertRaisesRegex(ValueError, 'allocated more'):
                acceptance.check(root, '0.0.2')
            # A run that names a different game than its report.
            for field, value in [('game_key', 'another-game'), ('game_build', '2'), ('game', ' ')]:
                changed = copy.deepcopy(manifest)
                changed['runs'][0][field] = value
                write(changed, reports)
                with self.subTest(field=field), self.assertRaises(ValueError):
                    acceptance.check(root, '0.0.2')

    def test_acceptance_takes_the_version_it_is_given(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            # A candidate report counts for its release.
            for recorded in ['0.0.3', '0.0.3-rc.1', '0.0.3-rc.12']:
                acceptance, manifest, reports, write = self.acceptance_fixture(root, '0.0.3', recorded)
                with self.subTest(recorded=recorded):
                    acceptance.check(root, '0.0.3')
            for recorded in ['0.0.2', '0.0.30', '0.0.3-rc', '0.0.3-beta.1', '0.0.3-rc.1.2', None]:
                acceptance, manifest, reports, write = self.acceptance_fixture(root, '0.0.3', recorded)
                with self.subTest(recorded=recorded), self.assertRaises(ValueError):
                    acceptance.check(root, '0.0.3')
            # No record for the version fails closed.
            with self.assertRaises(FileNotFoundError):
                acceptance.check(root, '0.0.4')
            record = root / 'docs/validation/0.0.4.json'
            record.write_text('')
            with self.assertRaises(ValueError):
                acceptance.check(root, '0.0.4')

    def test_acceptance_gate_applies_to_every_stable_tag(self):
        workflow = (HERE.parent / '.github/workflows/release.yml').read_text()
        self.assertNotIn("== '0.0.2'", workflow)
        self.assertRegex(workflow, r"check-acceptance\.py")
        gate = workflow.split('check-acceptance.py')[0].rsplit('- name:', 1)[1]
        self.assertIn("prerelease == 'false'", gate)
        self.assertIn("startsWith(github.ref, 'refs/tags/')", gate)

    def test_btrfs_runs_must_report_enough_passing_tests(self):
        runner = module('btrfs-tests.py')
        ok = 'running 12 tests\n\ntest result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.1s\n'
        self.assertEqual(runner.passed(ok), 12)
        self.assertEqual(runner.require(ok, 12), 12)
        # Controls: a filter matching nothing, a floor above the count,
        # a failure, and no summary at all must each be refused.
        empty = 'running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 300 filtered out; finished in 0.0s\n'
        with self.assertRaisesRegex(ValueError, 'Only 0'):
            runner.require(empty, 1)
        with self.assertRaisesRegex(ValueError, 'Only 12'):
            runner.require(ok, 13)
        with self.assertRaises(ValueError):
            runner.passed('test result: FAILED. 11 passed; 1 failed; 0 ignored;\n')
        with self.assertRaises(ValueError):
            runner.passed('')
        with self.assertRaises(ValueError):
            runner.passed(ok + ok)
        build = '\n'.join([
            json.dumps({'reason': 'compiler-artifact', 'profile': {'test': True}, 'executable': '/t/lib-abc', 'target': {'kind': ['lib'], 'name': 'flummox'}}),
            json.dumps({'reason': 'compiler-artifact', 'profile': {'test': True}, 'executable': '/t/jobs-abc', 'target': {'kind': ['test'], 'name': 'jobs_lifecycle'}}),
            json.dumps({'reason': 'compiler-artifact', 'profile': {'test': False}, 'executable': '/t/other', 'target': {'kind': ['bin'], 'name': 'flummox'}}),
            'not json',
        ])
        self.assertEqual(runner.executables(build), {'lib': '/t/lib-abc', 'jobs_lifecycle': '/t/jobs-abc'})

    def test_release_archives_carry_the_third_party_notices(self):
        bundle = module('package-release.py')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            missing = root / bundle.NOTICES
            self.assertIsNone(bundle.notices_input(missing, False))
            with self.assertRaisesRegex(RuntimeError, 'third-party licence notices'):
                bundle.notices_input(missing, True)
            missing.write_text('MIT: example 1.0\n')
            self.assertEqual(bundle.notices_input(missing, True), missing)
            self.assertIn('share/licenses/flummox/' + bundle.NOTICES, bundle.linux_inputs(root, missing))
            self.assertNotIn('share/licenses/flummox/' + bundle.NOTICES, bundle.linux_inputs(root, None))
            # A release build with no notices stops before it reads any binary.
            result = subprocess.run(['python3', str(HERE / 'package-release.py'), '--require-notices', '--notices', str(root / 'absent.txt'), '--binaries', str(root), '--output', str(root / 'dist')], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('third-party licence notices', result.stderr)

    def test_installed_docs_are_the_ones_users_need(self):
        bundle = module('package-release.py')
        inputs = bundle.linux_inputs(HERE, None)
        for name in ['usage.md', 'status.md', 'install.md']:
            self.assertIn('share/doc/flummox/' + name, inputs)
        for name in ['release-readiness.md', 'next-steps.md']:
            self.assertNotIn('share/doc/flummox/' + name, inputs)
        for source in inputs.values():
            if source.suffix == '.md':
                self.assertTrue(source.is_file(), source)

    def test_installed_docs_have_no_dead_relative_links(self):
        bundle = module('package-release.py')
        inputs = bundle.linux_inputs(HERE, None)
        installed = {destination.rsplit('/', 1)[1] for destination in inputs if destination.startswith('share/doc/flummox/')}
        for destination, source in inputs.items():
            if not destination.startswith('share/doc/flummox/') or source.suffix != '.md':
                continue
            for target in re.findall(r'\]\(([^)\s]+)\)', source.read_text()):
                if target.startswith(('http://', 'https://', '#', 'mailto:')):
                    continue
                with self.subTest(document=destination, link=target):
                    self.assertIn(target.split('#')[0], installed)

    def test_release_workflow_guards(self):
        workflow = (HERE.parent / '.github/workflows/release.yml').read_text()
        self.assertNotIn('rust-cache', workflow)
        self.assertNotIn('2>/dev/null', workflow)
        self.assertNotIn('|| true', workflow)
        self.assertIn('merge-base --is-ancestor', workflow)
        self.assertIn('--json isDraft', workflow)
        self.assertIn('--require-notices', workflow)
        self.assertIn('fs-tests.yml', workflow)
        self.assertNotIn('toolchain install stable', workflow)

    def test_curated_notes_override_the_commit_changelog(self):
        prepare = module('prepare-release.py')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            notes = root / 'docs/releases/0.0.1.md'
            notes.parent.mkdir(parents=True)
            notes.write_text('# Flummox 0.0.1\n\nFirst public release and installation guide.\n')
            self.assertEqual(prepare.release_notes(root, '0.0.1', '', 'v0.0.1'), notes.read_text())

    def test_later_release_notes_include_only_new_commits(self):
        prepare = module('prepare-release.py')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            commands = [
                ['git', 'init'],
                ['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.com', 'commit', '--allow-empty', '-m', 'Initial implementation'],
                ['git', 'tag', 'v0.0.1'],
                ['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.com', 'commit', '--allow-empty', '-m', 'Add a useful feature'],
            ]
            for command in commands:
                subprocess.run(command, cwd=root, check=True, capture_output=True)
            notes = prepare.release_notes(root, '0.0.2', 'v0.0.1', 'v0.0.2')
            self.assertIn('Add a useful feature', notes)
            self.assertNotIn('Initial implementation', notes)
            self.assertIn('/compare/v0.0.1...v0.0.2', notes)

    def test_tag_sets_manifest_and_lock_without_touching_dependencies(self):
        prepare = module('prepare-release.py')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'Cargo.toml').write_text('[package]\nname = "flummox"\nversion = "0.0.1"\n\n[dependencies]\nexample = "1.2.3"\n')
            (root / 'Cargo.lock').write_text('version = 4\n\n[[package]]\nname = "flummox"\nversion = "0.0.1"\n\n[[package]]\nname = "example"\nversion = "1.2.3"\n')
            self.assertEqual(prepare.prepare(root, 'v0.0.1-rc.2'), '0.0.1-rc.2')
            self.assertEqual(tomllib.loads((root / 'Cargo.toml').read_text())['package']['version'], '0.0.1-rc.2')
            packages = tomllib.loads((root / 'Cargo.lock').read_text())['package']
            self.assertEqual({entry['name']: entry['version'] for entry in packages}, {'flummox': '0.0.1-rc.2', 'example': '1.2.3'})
            for tag in ['v1', '1.0.0', 'v01.0.0', 'v1.0.0/unsafe', 'v1.0.0\nextra']:
                with self.assertRaises(ValueError):
                    prepare.prepare(root, tag)

    def test_tag_must_match_the_source_version_and_stable_tags_need_notes(self):
        prepare = module('prepare-release.py')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = '[package]\nname = "flummox"\nversion = "0.0.1"\n'
            lock = 'version = 4\n\n[[package]]\nname = "flummox"\nversion = "0.0.1"\n'
            (root / 'Cargo.toml').write_text(manifest)
            (root / 'Cargo.lock').write_text(lock)
            for tag in ['v0.2.0', 'v0.0.2', 'v0.2.0-rc.1']:
                with self.subTest(tag=tag), self.assertRaisesRegex(ValueError, 'Cargo.toml'):
                    prepare.prepare(root, tag)
                self.assertEqual((root / 'Cargo.toml').read_text(), manifest)
                self.assertEqual((root / 'Cargo.lock').read_text(), lock)
            with self.assertRaisesRegex(ValueError, 'docs/releases/0.0.1.md'):
                prepare.prepare(root, 'v0.0.1')
            self.assertEqual((root / 'Cargo.toml').read_text(), manifest)
            (root / 'docs/releases').mkdir(parents=True)
            (root / 'docs/releases/0.0.1.md').write_text('# Flummox 0.0.1\n')
            self.assertEqual(prepare.prepare(root, 'v0.0.1'), '0.0.1')

    def test_packaged_recipe_version_matches_cargo(self):
        cargo = tomllib.loads((HERE.parent / 'Cargo.toml').read_text())['package']['version']
        pkgbuild = (HERE / 'PKGBUILD').read_text()
        match = re.search(r'^pkgver=(\S+)$', pkgbuild, flags=re.M)
        self.assertIsNotNone(match)
        self.assertEqual(match[1], cargo)

    def test_distribution_checksums_match_release_bytes(self):
        distributions = module('distributions.py')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for filename in ['linux-x86_64.tar.xz', 'linux-aarch64.tar.xz', 'macos-aarch64.zip']:
                (root / f'flummox-0.0.1-{filename}').write_bytes(b'release fixture')
            output = root / 'recipes'
            distributions.generate('0.0.1', root, output)
            cask = (output / 'homebrew/Casks/flummox.rb').read_text()
            digest = hashlib.sha256(b'release fixture').hexdigest()
            self.assertIn(digest, cask)
            self.assertIn(digest, (output / 'aur/flummox-bin/PKGBUILD').read_text())
            self.assertIn(digest, (output / 'aur/flummox-bin/.SRCINFO').read_text())
            self.assertIn('/download/v0.0.1/', cask)
            with self.assertRaises(ValueError):
                distributions.generate('0.0.1-rc.1', root, output)

    def test_linux_bundle_rejects_wrong_architecture(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for filename in ['flummox', 'flummox-gui']:
                header = bytearray(20)
                header[:6] = b'\x7fELF\x02\x01'
                header[18:20] = b'\xb7\x00'
                (root / filename).write_bytes(header)
            result = subprocess.run(['python3', str(HERE / 'package-release.py'), '--arch', 'x86_64', '--binaries', str(root), '--output', str(root / 'dist')], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('Expected a Linux x86_64 ELF', result.stderr)
            result = subprocess.run(['python3', str(HERE / 'package-release.py'), '--arch', 'aarch64', '--binaries', str(root), '--output', str(root / 'dist')], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            version = tomllib.loads((HERE.parent / 'Cargo.toml').read_text())['package']['version']
            self.assertFalse((root / f'dist/flummox-{version}-linux-aarch64.tar.xz').is_file())

    def test_linux_download_strips_debug_without_changing_build_outputs(self):
        import tarfile
        self.require_tools('cc', 'objcopy', 'readelf')
        bundle = module('package-release.py')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'fixture.c'
            source.write_text('int main(void) { return 0; }\n')
            executable = root / 'fixture'
            subprocess.run(['cc', '-g', str(source), '-o', str(executable)], check=True)
            original = executable.read_bytes()
            staged = root / 'staged'
            shutil.copy2(executable, staged)
            sizes = bundle.split_debug(staged, root / 'debug')
            self.assertLess(sizes['after'], sizes['before'])
            self.assertEqual(executable.read_bytes(), original)
            self.assertTrue((root / 'debug/staged.debug').is_file())
            subprocess.run([str(staged)], check=True)
            sections = subprocess.run(['readelf', '-S', str(staged)], check=True, capture_output=True, text=True).stdout
            self.assertNotIn('.debug_info', sections)
            self.assertIn('.gnu_debuglink', sections)

    def test_signed_manifest_rejects_modified_artifacts_and_wrong_keys(self):
        manifest = module('release-manifest.py')
        self.require_tools('minisign')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in manifest.artifact_names('0.0.2'):
                (root / name).write_bytes(b'artifact fixture')
            pub = root / 'key.pub'
            key = root / 'key.secret'
            subprocess.run(['minisign', '-G', '-W', '-s', str(key), '-p', str(pub)], check=True, capture_output=True)
            path = manifest.create(root, '0.0.2', 'a' * 40)
            subprocess.run(['minisign', '-S', '-W', '-s', str(key), '-m', str(path)], check=True, capture_output=True)
            manifest.verify(root, path, pub, '0.0.2', 'a' * 40)
            with self.assertRaises(ValueError):
                manifest.verify(root, path, pub, '0.0.3')
            (root / manifest.artifact_names('0.0.2')[0]).write_bytes(b'modified')
            with self.assertRaises(ValueError):
                manifest.verify(root, path, pub, '0.0.2')
            wrong_pub = root / 'wrong.pub'
            subprocess.run(['minisign', '-G', '-W', '-s', str(root / 'wrong.secret'), '-p', str(wrong_pub)], check=True, capture_output=True)
            with self.assertRaises(subprocess.CalledProcessError):
                manifest.verify(root, path, wrong_pub, '0.0.2')


if __name__ == '__main__':
    unittest.main()
