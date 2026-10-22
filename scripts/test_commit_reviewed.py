"""Real Git fixtures for the bundled, provider-independent commit script."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'src-tauri' / 'scripts' / 'commit-reviewed.ps1'

class ReviewedGitTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='fabrica-reviewed-')
        self.root = Path(self.temp.name).resolve()
        assert self.root.is_relative_to(Path(tempfile.gettempdir()).resolve())
        assert self.root.name.startswith('fabrica-reviewed-')
        self.addCleanup(self.temp.cleanup)
        self.repo = self.root / 'repo with spaces'
        self.repo.mkdir()
        self.git('init', '-b', 'main')
        self.git('config', 'user.name', 'Fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        (self.repo / 'file.txt').write_text('before\n')
        self.git('add', 'file.txt')
        self.git('commit', '-m', 'Initial')

    def git(self, *args):
        result = subprocess.run(['git', '-C', str(self.repo), *args], capture_output=True, check=True)
        return result.stdout.decode('utf-8').rstrip('\r\n')

    def request(self, paths, commit=True, push=False):
        return dict(cwd=str(self.repo), head=self.git('rev-parse', 'HEAD'), branch='main',
            status=self.git('status', '--porcelain=v1', '--untracked-files=all'),
            index=self.git('ls-files', '--stage'), files=[dict(path=p,
                hash=self.git('hash-object', '--no-filters', '--', p) if (self.repo / p).is_file() else '') for p in paths],
            remote='origin' if push else None, remoteUrl=str(self.root / 'remote.git') if push else None,
            remoteBranch='main' if push else None, commit=commit, push=push,
            subject='Keep literal $variables and `backticks`', body='Approved description\nSecond line')

    def run_script(self, request):
        folder = self.root / ('inputs-' + str(len(list(self.root.glob('inputs-*')))))
        folder.mkdir()
        path = folder / 'request.json'
        path.write_text(json.dumps(request), encoding='utf-8')
        env = os.environ.copy()
        for name in ('GIT_INDEX_FILE', 'GIT_DIR', 'GIT_WORK_TREE'):
            env.pop(name, None)
        env.update(getattr(self, 'extra_env', {}))
        return subprocess.run(['powershell.exe', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', ('\\\\?\\' + str(SCRIPT)) if getattr(self, 'extended_paths', False) else str(SCRIPT), '-RequestPath', ('\\\\?\\' + str(path)) if getattr(self, 'extended_paths', False) else str(path)],
            capture_output=True, text=True, encoding='utf-8', env=env, timeout=120)

    def test_status_drift_reports_the_changed_path_before_commit(self):
        (self.repo / 'file.txt').write_text('after\n')
        request = self.request(['file.txt'])
        (self.repo / 'new-after-review.txt').write_text('late edit\n')
        result = self.run_script(request)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('new-after-review.txt', result.stdout + result.stderr)
        self.assertIn('click Push again', result.stdout + result.stderr)
        self.assertEqual(self.git('rev-parse', 'HEAD'), request['head'])
        self.assertTrue((self.repo / 'new-after-review.txt').exists())

    def test_exact_message_and_large_path_scope(self):
        paths = ['file.txt']
        (self.repo / 'file.txt').write_text('after\n')
        for i in range(289):
            name = f'new file {i}.txt'
            (self.repo / name).write_text('new\n')
            paths.append(name)
        request = self.request(paths)
        result = self.run_script(request)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.git('log', '-1', '--format=%B'), request['subject'] + '\n\n' + request['body'])
        self.assertEqual(self.git('status', '--porcelain'), '')

    def test_stale_content_and_index_are_rejected_before_commit(self):
        (self.repo / 'file.txt').write_text('after\n')
        request = self.request(['file.txt'])
        (self.repo / 'file.txt').write_text('changed after approval\n')
        result = self.run_script(request)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.git('rev-parse', 'HEAD'), request['head'])

        request = self.request(['file.txt'])
        self.git('add', 'file.txt')
        result = self.run_script(request)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.git('rev-parse', 'HEAD'), request['head'])

    def test_canonical_windows_working_directory(self):
        (self.repo / 'file.txt').write_text('after\n')
        request = self.request(['file.txt'])
        request['cwd'] = '\\\\?\\' + str(self.repo)
        self.extended_paths = True
        result = self.run_script(request)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.git('status', '--porcelain'), '')

    def test_commit_push_and_push_only_verify_remote(self):
        remote = self.root / 'remote.git'
        subprocess.run(['git', 'init', '--bare', str(remote)], capture_output=True, check=True)
        self.git('remote', 'add', 'origin', str(remote))
        (self.repo / 'file.txt').write_text('after\n')
        result = self.run_script(self.request(['file.txt'], push=True))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.git('ls-remote', 'origin', 'refs/heads/main').split()[0], self.git('rev-parse', 'HEAD'))
        self.assertIn('"remoteBranch":"main"', result.stdout)
        (self.repo / 'file.txt').write_text('another\n')
        self.git('commit', '-am', 'Another')
        request = self.request([], commit=False, push=True)
        result = self.run_script(request)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.git('rev-parse', 'HEAD'), request['head'])
        self.assertEqual(self.git('ls-remote', 'origin', 'refs/heads/main').split()[0], request['head'])

    def test_conflicting_rebase_stops_preserves_original_and_does_not_push(self):
        remote = self.root / 'remote.git'
        subprocess.run(['git', 'init', '--bare', str(remote)], capture_output=True, check=True)
        self.git('remote', 'add', 'origin', str(remote))
        self.git('push', 'origin', 'HEAD:refs/heads/main')
        old_head = self.git('rev-parse', 'HEAD')
        (self.repo / 'file.txt').write_text('remote branch advances\n')
        self.git('commit', '-am', 'Remote advance')
        remote_head = self.git('rev-parse', 'HEAD')
        self.git('push', 'origin', 'HEAD:refs/heads/main')
        # This reset is confined to the freshly created fixture repository.
        self.git('reset', '--hard', old_head)
        (self.repo / 'file.txt').write_text('different local change\n')
        result = self.run_script(self.request(['file.txt'], push=True))
        self.assertNotEqual(result.returncode, 0)
        original = self.git('rev-parse', 'ORIG_HEAD')
        self.assertNotEqual(original, old_head)
        self.assertIn(original, result.stdout)
        self.assertIn('GIT_STEP:conflict:', result.stdout)
        self.assertIn('file.txt', self.git('diff', '--name-only', '--diff-filter=U'))
        self.assertIn(original, self.git('for-each-ref', '--format=%(objectname)', 'refs/fabrica/before-push/'))
        self.assertNotIn('GIT_STEP:push:', result.stdout)
        self.git('rebase', '--abort')
        self.assertEqual(self.git('rev-parse', 'HEAD'), original)
        self.assertEqual(self.git('ls-remote', 'origin', 'refs/heads/main').split()[0], remote_head)

    def diverged(self):
        remote = self.root / 'remote.git'
        subprocess.run(['git', 'init', '--bare', str(remote)], capture_output=True, check=True)
        self.git('remote', 'add', 'origin', str(remote))
        self.git('push', '-u', 'origin', 'main')
        base = self.git('rev-parse', 'HEAD')
        (self.repo / 'upstream.txt').write_text('remote change\n')
        self.git('add', 'upstream.txt')
        self.git('commit', '-m', 'Upstream change')
        upstream = self.git('rev-parse', 'HEAD')
        self.git('push', 'origin', 'main')
        self.git('reset', '--hard', base) # Only this disposable fixture.
        return upstream

    def test_diverged_commit_push_rebases_and_preserves_message(self):
        upstream = self.diverged()
        (self.repo / 'file.txt').write_text('local change\n')
        request = self.request(['file.txt'], push=True)
        result = self.run_script(request)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.git('rev-parse', 'HEAD^'), upstream)
        self.assertEqual(self.git('ls-remote', 'origin', 'refs/heads/main').split()[0], self.git('rev-parse', 'HEAD'))
        self.assertEqual(self.git('log', '-1', '--format=%B'), request['subject'] + '\n\n' + request['body'])
        self.assertEqual(self.git('status', '--porcelain'), '')
        for phase in ('review', 'commit', 'fetch', 'rebase', 'push', 'verify', 'complete'):
            self.assertIn('GIT_STEP:' + phase + ':', result.stdout)

    def test_diverged_push_only_rebases_existing_commit(self):
        upstream = self.diverged()
        (self.repo / 'file.txt').write_text('local change\n')
        self.git('commit', '-am', 'Existing approved message')
        original = self.git('rev-parse', 'HEAD')
        result = self.run_script(self.request([], commit=False, push=True))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.git('rev-parse', 'HEAD^'), upstream)
        self.assertNotEqual(original, self.git('rev-parse', 'HEAD'))
        self.assertEqual(self.git('log', '-1', '--format=%s'), 'Existing approved message')
        self.assertEqual(self.git('ls-remote', 'origin', 'refs/heads/main').split()[0], self.git('rev-parse', 'HEAD'))

    def test_existing_git_operation_and_dirty_hook_stop_before_push(self):
        (self.repo / '.git' / 'rebase-merge').mkdir()
        request = self.request([], commit=False)
        result = self.run_script(request)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('already in progress', result.stdout)
        (self.repo / '.git' / 'rebase-merge').rmdir()
        self.diverged()
        hooks = self.repo / '.git' / 'hooks'
        (hooks / 'post-commit').write_text('#!/bin/sh\necho hook-change > unreviewed.txt\n')
        (self.repo / 'file.txt').write_text('local change\n')
        result = self.run_script(self.request(['file.txt'], push=True))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Uncommitted changes remain', result.stdout)
        self.assertNotIn('GIT_STEP:rebase:', result.stdout)

    def test_hook_stderr_streams_without_powershell_exception_wrappers(self):
        (self.repo / '.git' / 'hooks' / 'pre-commit').write_text('#!/bin/sh\necho "Hook warning on stderr" >&2\n')
        (self.repo / 'file.txt').write_text('local change\n')
        result = self.run_script(self.request(['file.txt']))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('Hook warning on stderr', result.stdout)
        self.assertNotIn('RemoteException', result.stdout)

if __name__ == '__main__':
    unittest.main()
