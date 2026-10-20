"""Real local protected Git remote plus a compiled GitHub CLI fixture."""
import json
import os
import shutil
import sys
from pathlib import Path
import subprocess
import tempfile
import unittest
from test_commit_reviewed import ReviewedGitTests


STUB = r'''
using System;
using System.IO;
using System.Diagnostics;
class FakeGh {
  static string Run(string args) {
    var info = new ProcessStartInfo("git.exe", args) { UseShellExecute=false, RedirectStandardOutput=true, RedirectStandardError=true, CreateNoWindow=true };
    var p = Process.Start(info); string text = p.StandardOutput.ReadToEnd(); string error = p.StandardError.ReadToEnd(); p.WaitForExit();
    if (p.ExitCode != 0) throw new Exception(error); return text.Trim();
  }
  static void FinishMerge(string root, string head, string mode) {
    string remote = Path.Combine(root, "remote.git");
    string baseHead = Run("\"--git-dir=" + remote + "\" rev-parse refs/heads/main");
    string tree = Run("rev-parse " + head + "^{tree}");
    if (mode == "sync-ignored") {
      File.AppendAllText(Path.Combine(Environment.CurrentDirectory, ".git", "info", "exclude"), "\nignored.cache\n");
      File.WriteAllText("ignored.cache", "upstream content");
      Run("add -f ignored.cache"); tree = Run("write-tree"); Run("reset HEAD -- ignored.cache");
      File.WriteAllText("ignored.cache", "preserve ignored user cache");
    }
    string parents = " -p " + baseHead + (mode == "sync-merge" ? " -p " + head : "");
    string merge = Run("commit-tree " + tree + parents + " -m \"Fixture merged PR\"");
    Run("push origin " + merge + ":refs/heads/fixture-merge");
    Run("\"--git-dir=" + remote + "\" update-ref refs/heads/main " + merge + " " + baseHead);
    Run("\"--git-dir=" + remote + "\" update-ref -d refs/heads/fixture-merge");
    File.WriteAllText(Path.Combine(root, "merged-commit"), merge);
    File.WriteAllText(Path.Combine(root, "merged"), "yes");
  }
  static int Main(string[] args) {
    string root = Environment.GetEnvironmentVariable("FABRICA_GH_FIXTURE");
    File.AppendAllText(Path.Combine(root, "calls.txt"), String.Join("\t", args) + "\n");
    if (args[0] == "auth") return 0;
    string mode = Environment.GetEnvironmentVariable("FABRICA_MERGE_FIXTURE") ?? "";
    if (args[0] == "api") { Console.WriteLine("{\"allow_merge_commit\":true,\"allow_squash_merge\":true,\"allow_auto_merge\":" + (mode.StartsWith("auto") ? "true" : "false") + "}"); return 0; }
    var p = Process.Start(new ProcessStartInfo("git.exe", "rev-parse HEAD") { UseShellExecute=false, RedirectStandardOutput=true, CreateNoWindow=true });
    string head = p.StandardOutput.ReadToEnd().Trim(); p.WaitForExit();
    if (File.Exists(Path.Combine(root, "pr-head"))) head = File.ReadAllText(Path.Combine(root, "pr-head"));
    string url = "https://github.com/fixture/repo/pull/7";
    string details = "{\"url\":\"" + url + "\",\"headRefOid\":\"" + head + "\",\"baseRefName\":\"main\",\"headRefName\":\"codex/fabrica-push/" + head + "\",\"state\":\"OPEN\",\"statusCheckRollup\":[{\"name\":\"secret-scan\",\"status\":\"QUEUED\"}]}";
    if (args[1] == "list") { Console.WriteLine(File.Exists(Path.Combine(root, "created")) ? "[" + details + "]" : "[]"); return 0; }
    if (args[1] == "view") {
      string counter = Path.Combine(root, "views.txt");
      int views = File.Exists(counter) ? Int32.Parse(File.ReadAllText(counter)) + 1 : 1;
      File.WriteAllText(counter, views.ToString());
      if (mode == "auto-sync" && File.Exists(Path.Combine(root, "queued")) && views >= 4 && !File.Exists(Path.Combine(root, "merged"))) FinishMerge(root, head, "sync-merge");
      bool merged = File.Exists(Path.Combine(root, "merged"));
      bool queued = File.Exists(Path.Combine(root, "queued"));
      bool ready = mode.StartsWith("sync-") || mode == "ready" || mode == "merge-ambiguous" || (mode == "wait" && views >= 4);
      if (mode == "changed" && views >= 2) details = details.Replace(head + "\",\"baseRefName", new String('f', 40) + "\",\"baseRefName");
      if (mode == "failed") details = details.Replace("\"status\":\"QUEUED\"", "\"status\":\"COMPLETED\",\"conclusion\":\"FAILURE\"");
      if (ready) details = details.Replace("\"status\":\"QUEUED\"", "\"status\":\"COMPLETED\",\"conclusion\":\"SUCCESS\"");
      details = details.TrimEnd('}') + ",\"mergeStateStatus\":\"" + (ready ? "CLEAN" : "BLOCKED") + "\",\"reviewDecision\":\"\",\"autoMergeRequest\":" + (queued ? "{\"enabledAt\":\"2026-10-20\"}" : "null") + ",\"mergedAt\":" + (merged ? "\"2026-10-20T00:00:00Z\"" : "null") + ",\"mergeCommit\":" + (merged ? "{\"oid\":\"" + (File.Exists(Path.Combine(root, "merged-commit")) ? File.ReadAllText(Path.Combine(root, "merged-commit")) : new String('7',40)) + "\"}" : "null") + "}";
      if (merged) details = details.Replace("\"state\":\"OPEN\"", "\"state\":\"MERGED\"");
      Console.WriteLine(details); return 0;
    }
    if (args[1] == "merge") {
      if (mode.StartsWith("sync-")) FinishMerge(root, head, mode);
      else if (mode == "auto-pending" || mode == "auto-sync") File.WriteAllText(Path.Combine(root, "queued"), "yes");
      else File.WriteAllText(Path.Combine(root, "merged"), "yes");
      if (mode == "sync-dirty") File.WriteAllText("unrelated-edit.txt", "preserve this user edit");
      if (mode == "sync-moved") { File.WriteAllText("new-user-commit.txt", "preserve new user work"); Run("add new-user-commit.txt"); Run("commit -m \"Concurrent user commit\""); }
      if (mode == "merge-ambiguous") return 1;
      return 0;
    }
    if (args[1] == "create") {
      for (int i=0; i<args.Length-1; i++) if (args[i] == "--body-file") File.WriteAllText(Path.Combine(root, "body.txt"), File.ReadAllText(args[i+1]));
      if (Environment.GetEnvironmentVariable("FABRICA_GH_FAIL") == "before") return 1;
      File.WriteAllText(Path.Combine(root, "created"), "yes");
      File.WriteAllText(Path.Combine(root, "pr-head"), head);
      if (Environment.GetEnvironmentVariable("FABRICA_GH_FAIL") == "after") return 1;
      Console.WriteLine(url); return 0;
    }
    return 1;
  }
}
'''


class ProtectedPushTests(unittest.TestCase):
    setUp = ReviewedGitTests.setUp
    git = ReviewedGitTests.git
    request = ReviewedGitTests.request
    run_script = ReviewedGitTests.run_script

    @classmethod
    def setUpClass(cls):
        cls.stub_temp = tempfile.TemporaryDirectory(prefix='fabrica-gh-stub-')
        cls.stub_root = Path(cls.stub_temp.name)
        cls.stub = cls.stub_root / 'gh-fixture.exe'
        source = cls.stub_root / 'fake.cs'
        source.write_text(STUB, encoding='utf-8')
        command = f"Add-Type -Path '{source}' -OutputAssembly '{cls.stub}' -OutputType ConsoleApplication"
        env = os.environ.copy()
        env.pop('LIB', None)
        result = subprocess.run(['powershell.exe', '-NoProfile', '-Command', command], capture_output=True, text=True, env=env)
        if result.returncode: raise RuntimeError(result.stdout + result.stderr)

    @classmethod
    def tearDownClass(cls):
        cls.stub_temp.cleanup()

    def protected(self, message='GH006: Changes must be made through a pull request.'):
        remote = self.root / 'remote.git'
        subprocess.run(['git', 'init', '--bare', str(remote)], check=True, capture_output=True)
        self.git('remote', 'add', 'origin', str(remote))
        self.git('push', '-u', 'origin', 'main')
        self.base = self.git('rev-parse', 'HEAD')
        (remote / 'hooks' / 'pre-receive').write_text(
            '#!/bin/sh\nwhile read old new ref; do\nif [ "$ref" = "refs/heads/main" ]; then\n'
            f'echo "{message}" >&2\nexit 1\nfi\ndone\n', encoding='utf-8')
        self.extra_env = {'FABRICA_GH_FIXTURE': str(self.root)}
        (self.repo / 'file.txt').write_text('reviewed change\n')
        request = self.request(['file.txt'], push=True)
        request.update(allowPullRequest=True, githubRepository='fixture/repo', ghExecutable=str(self.stub))
        return request

    def test_protected_push_opens_pr_preserves_upstream_and_reuses_it(self):
        result = self.run_script(self.protected())
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        head = self.git('rev-parse', 'HEAD')
        self.assertEqual(self.git('ls-remote', 'origin', 'refs/heads/main').split()[0], self.base)
        self.assertEqual(self.git('ls-remote', 'origin', 'refs/heads/codex/fabrica-push/' + head).split()[0], head)
        self.assertEqual(self.git('rev-parse', '--abbrev-ref', '@{upstream}'), 'origin/main')
        self.assertIn('https://github.com/fixture/repo/pull/7', result.stdout)
        self.assertIn('Check secret-scan: QUEUED', result.stdout)
        self.assertNotIn('\x1b', result.stdout)
        self.assertEqual((self.root / 'body.txt').read_text().strip(), 'Approved description\nSecond line')
        retry = self.request([], commit=False, push=True)
        retry.update(allowPullRequest=True, githubRepository='fixture/repo', ghExecutable=str(self.stub))
        result = self.run_script(retry)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((self.root / 'calls.txt').read_text().count('pr\tcreate\t'), 1)

    def test_ambiguous_creation_is_reconciled_without_duplicate_pr(self):
        request = self.protected()
        self.extra_env['FABRICA_GH_FAIL'] = 'after'
        result = self.run_script(request)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((self.root / 'calls.txt').read_text().count('pr\tcreate\t'), 1)

    def test_pr_failure_preserves_published_branch_receipt(self):
        request = self.protected()
        self.extra_env['FABRICA_GH_FAIL'] = 'before'
        result = self.run_script(request)
        self.assertNotEqual(result.returncode, 0)
        effects = [json.loads(line.split(':', 1)[1]) for line in result.stdout.splitlines() if line.startswith('NAVIS_SIDE_EFFECTS:')]
        self.assertTrue(effects[-1]['remoteBranch'].startswith('codex/fabrica-push/'))
        self.assertEqual(effects[-1]['pullRequestUrl'], '')

    def test_other_push_rejections_do_not_open_pr_or_publish_branch(self):
        result = self.run_script(self.protected('GH013: Push blocked because secrets were detected.'))
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / 'calls.txt').exists())
        self.assertEqual(self.git('ls-remote', 'origin', 'refs/heads/codex/*'), '')

    def merge_request(self, mode):
        request = self.protected()
        request['autoMergePullRequest'] = True
        self.extra_env['FABRICA_MERGE_FIXTURE'] = mode
        if mode == 'auto-pending': request['mergeWaitSeconds'] = 1
        return request

    def merge_calls(self):
        return [line for line in (self.root / 'calls.txt').read_text().splitlines() if line.startswith('pr\tmerge\t')]

    def test_auto_merge_is_queued_after_readback(self):
        result = self.run_script(self.merge_request('auto-pending'))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('"state":"queued"', result.stdout)
        self.assertNotIn('"state":"merged"', result.stdout)
        calls = self.merge_calls()
        self.assertEqual(len(calls), 1)
        self.assertIn('--auto', calls[0])
        self.assertIn('--match-head-commit\t' + self.git('rev-parse', 'HEAD'), calls[0])
        self.assertNotIn('--admin', calls[0])

    def test_disabled_auto_merge_waits_and_merges_normally(self):
        result = self.run_script(self.merge_request('wait'))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('"state":"waiting"', result.stdout)
        self.assertIn('"state":"merged"', result.stdout)
        self.assertIn('"mergeCommit":"' + '7' * 40 + '"', result.stdout)
        self.assertEqual(len(self.merge_calls()), 1)
        self.assertNotIn('--auto', self.merge_calls()[0])
        self.assertNotIn('--admin', self.merge_calls()[0])

    def test_failed_checks_and_changed_heads_stop_before_merging(self):
        result = self.run_script(self.merge_request('failed'))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('"state":"stopped"', result.stdout)
        self.assertEqual(self.merge_calls(), [])

    def test_changed_head_is_never_merged(self):
        result = self.run_script(self.merge_request('changed'))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('commit changed', result.stdout)
        self.assertEqual(self.merge_calls(), [])

    def test_ambiguous_merge_is_confirmed_without_repeating_mutation(self):
        result = self.run_script(self.merge_request('merge-ambiguous'))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('"state":"merged"', result.stdout)
        self.assertEqual(len(self.merge_calls()), 1)

    def test_github_auto_merge_can_complete_immediately(self):
        result = self.run_script(self.merge_request('auto-ready'))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('"state":"merged"', result.stdout)
        self.assertIn('--auto', self.merge_calls()[0])

    def assert_synced(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('"state":"synced"', result.stdout)
        self.assertIn('local branch synced and verified', result.stdout)
        head = self.git('rev-parse', 'HEAD')
        self.assertEqual(head, self.git('ls-remote', 'origin', 'refs/heads/main').split()[0])
        self.assertEqual(self.git('rev-list', '--left-right', '--count', 'HEAD...@{upstream}'), '0\t0')
        self.assertEqual(self.git('status', '--porcelain'), '')
        self.assertNotEqual(self.git('for-each-ref', '--format=%(objectname)', 'refs/fabrica/after-merge/'), '')

    def test_merge_commit_fast_forwards_local_main(self):
        result = self.run_script(self.merge_request('sync-merge'))
        self.assert_synced(result)
        self.assertIn('Fast-forwarding local main', result.stdout)

    def test_squash_and_rebase_merges_align_local_main_without_reset(self):
        result = self.run_script(self.merge_request('sync-squash'))
        self.assert_synced(result)
        self.assertIn('Aligning local main after squash/rebase merge', result.stdout)
        calls = (self.root / 'calls.txt').read_text()
        self.assertNotIn('--admin', calls)

    def test_queued_auto_merge_is_watched_until_local_sync(self):
        result = self.run_script(self.merge_request('auto-sync'))
        self.assert_synced(result)
        self.assertIn('"state":"queued"', result.stdout)
        self.assertIn('"state":"merged"', result.stdout)

    def test_new_local_files_are_preserved_and_sync_is_skipped(self):
        result = self.run_script(self.merge_request('sync-dirty'))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('"state":"merged"', result.stdout)
        self.assertIn('"state":"skipped"', result.stdout)
        self.assertNotIn('"state":"synced"', result.stdout)
        self.assertEqual((self.repo / 'unrelated-edit.txt').read_text(), 'preserve this user edit')
        self.assertNotEqual(self.git('rev-parse', 'HEAD'), self.git('ls-remote', 'origin', 'refs/heads/main').split()[0])

    def test_new_local_commits_are_preserved_and_sync_is_skipped(self):
        result = self.run_script(self.merge_request('sync-moved'))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('"state":"skipped"', result.stdout)
        self.assertEqual(self.git('log', '-1', '--format=%s'), 'Concurrent user commit')
        self.assertEqual((self.repo / 'new-user-commit.txt').read_text(), 'preserve new user work')

    def test_tauri_push_receipt_is_recorded_before_local_sync_changes_head(self):
        import test_build_project as builds
        import version_workflow as versions
        builds.BuildTests.setUp(self)
        self.protected()
        versions.prepare(self.repo, self.state)
        paths = self.git('diff', '--name-only').splitlines() + ['file.txt']
        request = self.request(paths, push=True)
        folder = self.root / 'version-helpers'
        folder.mkdir()
        for source, name in [(Path(versions.__file__), 'version_workflow.py'),
                             (Path(builds.helper.versions.__file__), 'prepare_version.py'),
                             (Path(builds.helper.versions.setter.__file__), 'set_version.py')]:
            shutil.copy2(source, folder / name)
        request.update(allowPullRequest=True, autoMergePullRequest=True, githubRepository='fixture/repo',
                       ghExecutable=str(self.stub), python=sys.executable,
                       versionHelper=str(folder / 'version_workflow.py'), versionStateDir=str(self.state))
        self.extra_env['FABRICA_MERGE_FIXTURE'] = 'sync-merge'
        result = self.run_script(request)
        self.assert_synced(result)
        state = builds.helper.versions.load(builds.helper.versions.context(self.repo, self.state))
        self.assertEqual(state['last_push']['version'], '0.1.1')
        self.assertEqual(state['last_push']['commit'], (self.root / 'pr-head').read_text())
        self.assertNotEqual(state['last_push']['commit'], self.git('rev-parse', 'HEAD'))
        self.assertIn('push receipt saved', result.stdout)

    def test_ignored_files_that_become_tracked_upstream_are_preserved(self):
        result = self.run_script(self.merge_request('sync-ignored'))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('"state":"skipped"', result.stdout)
        self.assertIn('existing local path would be replaced', result.stdout)
        self.assertEqual((self.repo / 'ignored.cache').read_text(), 'preserve ignored user cache')


if __name__ == '__main__':
    unittest.main()
