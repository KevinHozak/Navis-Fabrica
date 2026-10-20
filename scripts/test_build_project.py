import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts' / 'dev-version'))
sys.path.insert(0, str(ROOT / 'src-tauri' / 'scripts'))
spec = importlib.util.spec_from_file_location('build_project', ROOT / 'src-tauri' / 'scripts' / 'build_project.py')
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class BuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='fabrica-script-build-')
        self.root = Path(self.temp.name).resolve()
        assert self.root.is_relative_to(Path(tempfile.gettempdir()).resolve())
        assert self.root.name.startswith('fabrica-script-build-')
        self.addCleanup(self.temp.cleanup)
        self.repo = self.root / 'project with spaces'
        (self.repo / 'src-tauri').mkdir(parents=True)
        (self.repo / 'src').mkdir()
        self.state = self.root / 'version-state'
        package = dict(name='fixture', version='0.1.0', packageManager='npm@11.11.1', scripts={'desktop:build': 'tauri build'})
        (self.repo / 'package.json').write_text(json.dumps(package), encoding='utf-8')
        (self.repo / 'package-lock.json').write_text(json.dumps(dict(version='0.1.0', packages={'':dict(version='0.1.0')})), encoding='utf-8')
        (self.repo / 'src-tauri' / 'Cargo.toml').write_text('[package]\nname = "fixture"\nversion = "0.1.0"\nedition = "2021"\n', encoding='utf-8')
        (self.repo / 'src-tauri' / 'Cargo.lock').write_text('version = 3\n[[package]]\nname = "fixture"\nversion = "0.1.0"\n', encoding='utf-8')
        (self.repo / 'src-tauri' / 'tauri.conf.json').write_text(json.dumps(dict(version='0.1.0', app=dict(windows=[dict(title='Fixture v0.1.0')]), bundle=dict(active=False))), encoding='utf-8')
        (self.repo / 'src' / 'main.ts').write_text('before\n', encoding='utf-8')
        self.git('init', '-b', 'main')
        self.git('config', 'user.name', 'Fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        self.git('add', '.')
        self.git('commit', '-m', 'Baseline')
        self.head = self.git('rev-parse', 'HEAD')
        (self.repo / 'src' / 'main.ts').write_text('local changes\n', encoding='utf-8')

    def git(self, *args):
        return subprocess.check_output(['git', '-C', str(self.repo), *args], stderr=subprocess.PIPE).decode().strip()

    def fake_build(self, command, repo):
        self.command = command
        profile = 'debug' if '--debug' in command else 'release'
        artifact = repo / 'src-tauri' / 'target' / profile / 'fixture.exe'
        artifact.parent.mkdir(parents=True, exist_ok=True)
        artifact.write_bytes(b'fixture executable')

    def version(self, path):
        version = json.loads((self.repo / 'src-tauri' / 'tauri.conf.json').read_text())['version']
        return dict(fileVersion=version, productVersion=version)

    def test_release_preserves_local_work_and_reuses_preparation(self):
        with patch.object(helper, 'run_build', self.fake_build), patch.object(helper, 'metadata', self.version), patch.object(helper, 'smoke') as smoke:
            artifact = helper.build(self.repo, 'release', self.state)
            self.assertNotIn('--debug', self.command)
            self.assertIn('release', artifact.parts)
            smoke.assert_called_once_with(artifact)
            helper.build(self.repo, 'release', self.state)
        self.assertEqual(self.version(artifact)['fileVersion'], '0.1.1')
        self.assertEqual(self.git('rev-parse', 'HEAD'), self.head)
        self.assertEqual((self.repo / 'src' / 'main.ts').read_text(), 'local changes\n')
        state = helper.versions.load(helper.versions.context(self.repo, self.state))
        self.assertEqual(state['last_build']['version'], '0.1.1')

    def test_failed_build_or_bad_metadata_cannot_record_success(self):
        with patch.object(helper, 'run_build', side_effect=RuntimeError('builder failed')):
            with self.assertRaises(RuntimeError):
                helper.build(self.repo, 'release', self.state)
        with patch.object(helper, 'run_build', self.fake_build), patch.object(helper, 'metadata', return_value=dict(fileVersion='wrong', productVersion='wrong')), patch.object(helper, 'smoke') as smoke:
            with self.assertRaisesRegex(ValueError, 'version mismatch'):
                helper.build(self.repo, 'release', self.state)
            smoke.assert_not_called()
        state = helper.versions.load(helper.versions.context(self.repo, self.state))
        self.assertIsNone(state['last_build'])
        self.assertEqual(state['prepared']['version'], '0.1.1')

    def test_debug_is_explicit_and_failed_startup_cannot_record_success(self):
        with patch.object(helper, 'run_build', self.fake_build), patch.object(helper, 'metadata', self.version), patch.object(helper, 'smoke', side_effect=ValueError('startup failed')):
            with self.assertRaisesRegex(ValueError, 'startup failed'):
                helper.build(self.repo, 'debug', self.state)
        self.assertIn('--debug', self.command)
        state = helper.versions.load(helper.versions.context(self.repo, self.state))
        self.assertIsNone(state['last_build'])
        with patch.object(helper, 'run_build', self.fake_build), patch.object(helper, 'metadata', self.version), patch.object(helper, 'smoke'):
            artifact = helper.build(self.repo, 'debug', self.state)
        self.assertIn('debug', artifact.parts)
        self.assertEqual(self.version(artifact)['fileVersion'], '0.1.1')


if __name__ == '__main__':
    unittest.main()
