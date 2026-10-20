import json
from pathlib import Path
import subprocess
import shutil
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_build_project as build_tests
import test_commit_reviewed as git_tests
import version_workflow as workflow
helper = build_tests.helper


class VersionWorkflowTests(unittest.TestCase):
    setUp = build_tests.BuildTests.setUp
    git = build_tests.BuildTests.git
    fake_build = build_tests.BuildTests.fake_build
    version = build_tests.BuildTests.version

    def push(self):
        remote = self.root / 'remote.git'
        subprocess.run(['git', 'init', '--bare', str(remote)], capture_output=True, check=True)
        self.git('remote', 'add', 'origin', str(remote))
        self.git('add', '.')
        self.git('commit', '-m', 'Reviewed change with prepared version')
        self.git('push', 'origin', 'HEAD:refs/heads/main')
        head = self.git('rev-parse', 'HEAD')
        observed = self.git('ls-remote', 'origin', 'refs/heads/main').split()[0]
        return head, observed

    def build(self):
        with patch.object(helper, 'run_build', self.fake_build), patch.object(helper, 'metadata', self.version), patch.object(helper, 'smoke'):
            return helper.build(self.repo, 'release', self.state)

    def test_push_then_build_reuses_the_prepared_version(self):
        prepared = workflow.prepare(self.repo, self.state)
        self.assertEqual(prepared['version'], '0.1.1')
        self.assertIn('package.json', self.git('diff', '--name-only'))
        head, observed = self.push()
        workflow.record_push(self.repo, self.state, head, observed)
        artifact = self.build()
        self.assertEqual(self.version(artifact)['fileVersion'], '0.1.1')

    def test_build_then_push_reuses_version_and_new_source_bumps_once(self):
        self.build()
        self.assertEqual(workflow.prepare(self.repo, self.state)['version'], '0.1.1')
        head, observed = self.push()
        workflow.record_push(self.repo, self.state, head, observed)
        (self.repo / 'src' / 'main.ts').write_text('next change\n', encoding='utf-8')
        self.assertEqual(workflow.prepare(self.repo, self.state)['version'], '0.1.2')
        self.assertEqual(workflow.prepare(self.repo, self.state)['version'], '0.1.2')
        with self.assertRaises(ValueError):
            workflow.record_push(self.repo, self.state, head, observed)

    def test_canonical_path_spellings_share_identity_and_push_only_is_readonly(self):
        prepared = workflow.prepare(self.repo, self.state)
        normal = workflow.inspect(self.repo, self.state)
        canonical = workflow.inspect('\\\\?\\' + str(self.repo), '\\\\?\\' + str(self.state))
        self.assertEqual(normal['token'], canonical['token'])
        before = (self.repo / 'package.json').read_bytes()
        (self.repo / 'src' / 'main.ts').write_text('unprepared content\n', encoding='utf-8')
        self.assertFalse(workflow.inspect(self.repo, self.state, push_only=True)['ready'])
        self.assertEqual((self.repo / 'package.json').read_bytes(), before)
        with self.assertRaisesRegex(ValueError, 'stale'):
            workflow.inspect(self.repo, self.state)
        self.assertEqual(prepared['version'], '0.1.1')

    def test_unborn_tauri_repo_keeps_declared_initial_version(self):
        self.git('checkout', '--orphan', 'initial')
        self.git('read-tree', '--empty')
        before = (self.repo / 'package.json').read_bytes()
        self.assertEqual(workflow.prepare(self.repo, self.state)['version'], '0.1.0')
        self.assertFalse(workflow.inspect(self.repo, self.state)['ready'])
        self.assertEqual((self.repo / 'package.json').read_bytes(), before)
        self.assertFalse(self.state.exists())

    def test_non_tauri_review_does_not_create_version_state(self):
        self.assertEqual(workflow.prepare(self.root, self.state), {'supported': False})
        self.assertFalse(self.state.exists())

    def test_push_script_verifies_prepared_version_and_records_remote_receipt(self):
        self.run_version_push()

    def test_rebased_app_content_does_not_record_a_stale_version_receipt(self):
        self.run_version_push(upstream=True)

    def run_version_push(self, upstream=False):
        remote = self.root / 'remote.git'
        subprocess.run(['git', 'init', '--bare', str(remote)], capture_output=True, check=True)
        self.git('remote', 'add', 'origin', str(remote))
        if upstream:
            self.git('checkout', '--', 'src/main.ts')
            self.git('push', 'origin', 'HEAD:refs/heads/main')
            (self.repo / 'src' / 'upstream.ts').write_text('upstream app change\n', encoding='utf-8')
            self.git('add', 'src/upstream.ts')
            self.git('commit', '-m', 'Upstream app change')
            self.git('push', 'origin', 'HEAD:refs/heads/main')
            self.git('reset', '--hard', self.head) # Only this disposable fixture.
            (self.repo / 'src' / 'main.ts').write_text('local changes\n', encoding='utf-8')
        workflow.prepare(self.repo, self.state)
        paths = self.git('diff', '--name-only').splitlines()
        request = git_tests.ReviewedGitTests.request(self, paths, push=True)
        request['status'] = subprocess.check_output(['git', '-C', str(self.repo), 'status', '--porcelain=v1', '--untracked-files=all']).decode().rstrip('\r\n')
        folder = self.root / 'version-helpers'
        folder.mkdir()
        shutil.copy2(Path(workflow.__file__), folder / 'version_workflow.py')
        shutil.copy2(Path(helper.versions.__file__), folder / 'prepare_version.py')
        shutil.copy2(Path(helper.versions.setter.__file__), folder / 'set_version.py')
        request.update(python='\\\\?\\' + sys.executable,
            versionHelper=str(folder / 'version_workflow.py'), versionStateDir=str(self.state))
        result = git_tests.ReviewedGitTests.run_script(self, request)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        state = helper.versions.load(helper.versions.context(self.repo, self.state))
        if upstream:
            self.assertIn('no stale version receipt', result.stdout)
            self.assertIsNone(state['last_push'])
            self.assertEqual(self.git('ls-remote', 'origin', 'refs/heads/main').split()[0], self.git('rev-parse', 'HEAD'))
            return
        self.assertIn('push receipt saved', result.stdout)
        self.assertEqual(state['last_push']['version'], '0.1.1')
        self.assertEqual(state['last_push']['commit'], self.git('rev-parse', 'HEAD'))


if __name__ == '__main__':
    unittest.main()
