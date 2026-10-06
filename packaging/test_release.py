#!/usr/bin/env python3
"""Check tag versions, checksums, and release architecture validation."""
import importlib.util
import copy
import json
from pathlib import Path
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
                    'corpus': {'sha256': 'a' * 64, 'files': 1, 'bytes': 1024},
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
                runs.append({'kind': kind, 'game': 'Fixture', 'tester': 'Fixture', 'date': '2026-10-05', 'report': f'{kind}.json', 'restart_verified': True, 'launcher_verification_passed': True})
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
            (root / 'Cargo.toml').write_text('[package]\nname = "flummox"\nversion = "0.1.0"\n\n[dependencies]\nexample = "1.2.3"\n')
            (root / 'Cargo.lock').write_text('version = 4\n\n[[package]]\nname = "flummox"\nversion = "0.1.0"\n\n[[package]]\nname = "example"\nversion = "1.2.3"\n')
            self.assertEqual(prepare.prepare(root, 'v0.0.1'), '0.0.1')
            self.assertEqual(tomllib.loads((root / 'Cargo.toml').read_text())['package']['version'], '0.0.1')
            packages = tomllib.loads((root / 'Cargo.lock').read_text())['package']
            self.assertEqual({entry['name']: entry['version'] for entry in packages}, {'flummox': '0.0.1', 'example': '1.2.3'})
            for tag in ['v1', '1.0.0', 'v01.0.0', 'v1.0.0/unsafe', 'v1.0.0\nextra']:
                with self.assertRaises(ValueError):
                    prepare.prepare(root, tag)

    def test_distribution_checksums_match_release_bytes(self):
        distributions = module('distributions.py')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for filename in ['linux-x86_64.tar.xz', 'linux-aarch64.tar.xz', 'macos-aarch64.zip']:
                (root / f'flummox-0.0.1-{filename}').write_bytes(b'release fixture')
            output = root / 'recipes'
            distributions.generate('0.0.1', root, output)
            cask = (output / 'homebrew/Casks/flummox.rb').read_text()
            import hashlib
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
        import shutil
        import tarfile
        if not shutil.which('cc') or not shutil.which('objcopy'):
            self.skipTest('Native ELF compiler and objcopy are needed')
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
        import shutil
        manifest = module('release-manifest.py')
        if not shutil.which('minisign'):
            self.skipTest('minisign is needed for signature tests')
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
