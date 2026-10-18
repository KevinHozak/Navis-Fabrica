"""Disposable repository tests; no live GitHub mutations or provider calls."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parent
if os.environ.get("WORKFLOW_TEST_TMP"):
    fixture_root = Path(os.environ["WORKFLOW_TEST_TMP"]).resolve()
    fixture_root.mkdir(parents=True, exist_ok=True)
    tempfile.tempdir = str(fixture_root)
sys.path.insert(0, str(ROOT))
import workflow_evidence as w
VERSION_ROOT = ROOT if (ROOT / "set_version.py").exists() else ROOT / "dev-version"
sys.path.insert(0, str(VERSION_ROOT))
import set_version as v
import bump_version as b


def run_git(repo, *args):
    return subprocess.check_output(["git", "-c", "maintenance.auto=false", "-c", "gc.auto=0", "-c", "core.fsmonitor=false",
                                    "-C", str(repo), *args], stderr=subprocess.STDOUT).decode().strip()


def cleanup_fixture(fixture):
    # Windows scanners/process teardown can briefly retain a directory handle.
    for attempt in range(10):
        try:
            fixture.cleanup()
            return
        except PermissionError:
            if attempt == 9:
                raise
            time.sleep(0.2)


def make_app(root, *, root_version="1.0.0", title="Example · v1.0.0"):
    (root / "src-tauri").mkdir(parents=True)
    (root / "src-tauri/tauri.conf.json").write_text(json.dumps({"version": "1.0.0", "app": {"windows": [
        {"title": title}, {"title": "Tools"}]}, "other": {"version": "9.9.9"}}, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    (root / "src-tauri/Cargo.toml").write_text('[package]\nname = "example"\nversion = "1.0.0"\n\n[dependencies]\nserde = "1.0"\n', encoding="utf-8")
    (root / "src-tauri/Cargo.lock").write_text('version = 4\n\n[[package]]\nname = "example"\nversion = "1.0.0"\n\n[[package]]\nname = "third-party"\nversion = "1.0.0"\n', encoding="utf-8")
    (root / "package.json").write_text(json.dumps({"version": "1.0.0", "scripts": {"typecheck": "tsc --noEmit", "build": "tsc && vite build"}}, indent=2), encoding="utf-8")
    (root / "package-lock.json").write_text(json.dumps({"version": "1.0.0", "lockfileVersion": 3,
        "packages": {"": {"version": root_version}, "node_modules/dependency": {"version": "1.0.0"}}}, indent=2), encoding="utf-8")


class Fixture(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="workflow-evidence-")
        self.addCleanup(cleanup_fixture, self.tmp)
        self.root = Path(self.tmp.name)
        self.repo = self.root / "repo with spaces"
        self.repo.mkdir()
        run_git(self.repo, "init", "-b", "main")
        run_git(self.repo, "config", "user.name", "Fixture")
        run_git(self.repo, "config", "user.email", "fixture@example.invalid")
        (self.repo / "source.txt").write_text("baseline\n", encoding="utf-8")
        run_git(self.repo, "add", "source.txt")
        run_git(self.repo, "commit", "-m", "fixture")

    def record(self):
        return w.new_run(self.repo, {"required_checks": ["build"], "criteria": [{"id": "behavior", "status": "pending"}]})

    def check(self, data):
        w.checkpoint(data, self.repo, "begin-check", {"id": "build"})
        evidence = self.root / "check.txt"
        evidence.write_text("observed build passed\n", encoding="utf-8")
        w.record_check(data, self.repo, {"id": "build", "argv": ["fixture-check"], "status": "passed",
                       "exit_code": 0, "evidence": str(evidence), "environment": {"runtime": "fixture-1"}, "duration_seconds": 1})
        return evidence

    def ready(self):
        data = self.record()
        self.check(data)
        head = run_git(self.repo, "rev-parse", "HEAD")
        w.checkpoint(data, self.repo, "acceptance", {"id": "behavior", "status": "passed", "evidence": "observed fixture"})
        w.checkpoint(data, self.repo, "review", {"status": "complete", "head": head, "actionable": 0})
        w.checkpoint(data, self.repo, "remote", {"url": "https://example.invalid/pr/1", "head": head, "state": "open"})
        return data

    def test_preflight_preserves_dirty_index_and_source(self):
        make_app(self.repo)
        (self.repo / "source.txt").write_text("unrelated dirty work\n", encoding="utf-8")
        before = {str(p): p.read_bytes() for p in self.repo.rglob("*") if p.is_file()}
        result = w.preflight(self.repo, "12", "main")
        after = {str(p): p.read_bytes() for p in self.repo.rglob("*") if p.is_file()}
        self.assertEqual(before, after)
        ids = [x["id"] for x in result["suggested_checks"]]
        self.assertIn("src-tauri/Cargo.toml:test", ids)
        self.assertIn("package.json:typecheck", ids)
        self.assertIn("package.json:build", ids)
        self.assertFalse(result["packages"][0]["node_modules_present"])

    def test_shared_worktree_metadata_and_dirty_primary_preserved(self):
        checkout = self.root / "isolated"
        run_git(self.repo, "worktree", "add", "-b", "codex/12-example", str(checkout))
        (self.repo / "source.txt").write_text("primary unrelated work\n", encoding="utf-8")
        result = w.preflight(checkout)
        self.assertNotEqual(result["git_dir"], result["common_git_dir"])
        self.assertIn(str(self.repo).replace("\\", "/"), result["common_git_dir"].replace("\\", "/"))
        self.assertEqual((self.repo / "source.txt").read_text(), "primary unrelated work\n")
        run_git(self.repo, "worktree", "remove", str(checkout))

    def test_permission_recovery_and_install_recovery_bounded(self):
        data = self.record()
        w.checkpoint(data, self.repo, "recovery", {"error": ".git/FETCH_HEAD: Permission denied", "remedy": "narrow sandbox request"})
        with self.assertRaisesRegex(ValueError, "already attempted"):
            w.checkpoint(data, self.repo, "recovery", {"error": ".git/index.lock denied", "remedy": "repeat"})
        w.checkpoint(data, self.repo, "recovery", {"error": "npm Exit handler never called", "remedy": "locked install in approved network context"})
        with self.assertRaises(ValueError):
            w.checkpoint(data, self.repo, "recovery", {"error": "tsc missing", "remedy": "borrow node_modules"})
        self.assertFalse(w.readiness(data, self.repo)["ready"])

    def test_missing_and_interrupted_install_stays_draft(self):
        data = self.record()
        w.checkpoint(data, self.repo, "begin-check", {"id": "build"})
        w.record_check(data, self.repo, {"id": "build", "status": "blocked", "reason": "npm install interrupted; tsc unavailable"})
        self.assertEqual(w.readiness(data, self.repo)["status"], "draft")

    def test_source_changes_and_tampered_evidence_invalidate_checks(self):
        data = self.ready()
        self.assertTrue(w.readiness(data, self.repo)["ready"])
        (self.root / "check.txt").write_text("changed evidence", encoding="utf-8")
        self.assertFalse(w.readiness(data, self.repo)["ready"])
        self.check(data)
        (self.repo / "source.txt").write_text("updated source", encoding="utf-8")
        result = w.readiness(data, self.repo)
        self.assertIn("check build stale", result["reasons"])

    def test_check_rejects_source_change_during_execution(self):
        data = self.record()
        w.checkpoint(data, self.repo, "begin-check", {"id": "build"})
        (self.repo / "source.txt").write_text("changed", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "Source changed"):
            w.record_check(data, self.repo, {"id": "build", "status": "blocked"})

    def test_commit_of_identical_checked_content_keeps_evidence(self):
        (self.repo / "source.txt").write_text("feature", encoding="utf-8")
        data = self.record()
        self.check(data)
        before = w.fingerprint(self.repo)
        run_git(self.repo, "add", "source.txt")
        run_git(self.repo, "commit", "-m", "feat: fixture")
        self.assertEqual(w.fingerprint(self.repo), before)
        self.assertEqual(data["checks"]["build"]["fingerprint"], before)

    def test_duplicate_publication_requires_reconciliation(self):
        data = self.record()
        effect = {"id": "pr:fixture:head-1", "kind": "pr", "url": "https://example.invalid/1"}
        with self.assertRaises(ValueError):
            w.checkpoint(data, self.repo, "effect", effect)
        w.checkpoint(data, self.repo, "lookup", {"state": "absent", "head": "head-1"})
        w.checkpoint(data, self.repo, "effect", effect)
        with self.assertRaisesRegex(ValueError, "already recorded"):
            w.checkpoint(data, self.repo, "effect", effect)
        path = self.root / "record.json"
        w.save(path, data)
        self.assertIn(effect["id"], w.load(path)["effects"])

    def test_two_round_limit_and_pending_review(self):
        data = self.ready()
        w.checkpoint(data, self.repo, "repair", {})
        w.checkpoint(data, self.repo, "repair", {})
        with self.assertRaisesRegex(ValueError, "exhausted"):
            w.checkpoint(data, self.repo, "repair", {})
        w.checkpoint(data, self.repo, "review", {"status": "pending", "head": None, "actionable": None})
        self.assertEqual(w.readiness(data, self.repo)["status"], "draft")

    def test_deferral_cannot_be_claimed_as_pass(self):
        data = self.ready()
        with self.assertRaisesRegex(ValueError, "authorization"):
            w.checkpoint(data, self.repo, "acceptance", {"id": "behavior", "status": "deferred"})
        w.checkpoint(data, self.repo, "acceptance", {"id": "behavior", "status": "deferred", "user_authorization": "user deferred desktop review"})
        self.assertFalse(w.readiness(data, self.repo)["ready"])

    def test_cli_atomic_record_and_cross_repository_guard(self):
        spec = self.root / "spec.json"
        spec.write_text('{"required_checks":["build"]}', encoding="utf-8")
        record = self.root / "run.json"
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = w.main(["checkpoint", "--repo", str(self.repo), "--record", str(record), "--action", "init", "--input", str(spec)])
        self.assertEqual(result, 0)
        self.assertEqual(json.loads(output.getvalue())["status"], "draft")
        with self.assertRaisesRegex(ValueError, "another repository"):
            w.validate_run(w.load(record), self.root)

    def test_redaction(self):
        self.assertNotIn("fake-secret", json.dumps(w.redact({"token": "fake-secret", "error": "api_key=fake-secret"})))


class CycleTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="improve-cycle-")
        self.addCleanup(cleanup_fixture, self.tmp)
        self.root = Path(self.tmp.name)
        self.target = self.root / "workflow.md"
        self.target.write_text("pre-existing user edits\n", encoding="utf-8")
        self.other = self.root / "unrelated.md"
        self.other.write_text("untouched\n", encoding="utf-8")
        self.record = self.root / "cycle.json"
        w.cycle_operation(self.record, "snapshot", {"paths": [str(self.target)]})

    def evaluation(self):
        return {"evidence_sufficient": True, "targets": ["missing-version"], "regressions": ["preserve-user-file"],
                "held_out": ["other-title"], "baseline": {"missing-version": False, "preserve-user-file": True, "other-title": False},
                "candidate": {"missing-version": True, "preserve-user-file": True, "other-title": True}}

    def test_beneficial_change_retained(self):
        self.target.write_text("improved candidate", encoding="utf-8")
        result = w.cycle_operation(self.record, "candidate", self.evaluation())
        self.assertEqual(result["status"], "retain")
        self.assertEqual(self.target.read_text(), "improved candidate")

    def test_ineffective_change_reverted_preserves_unrelated(self):
        self.target.write_text("ineffective prose", encoding="utf-8")
        data = self.evaluation()
        data["candidate"]["missing-version"] = False
        self.assertEqual(w.cycle_operation(self.record, "candidate", data)["status"], "revert")
        w.cycle_operation(self.record, "rollback", {})
        self.assertEqual(self.target.read_text(), "pre-existing user edits\n")
        self.assertEqual(self.other.read_text(), "untouched\n")

    def test_regression_rolls_back(self):
        data = self.evaluation()
        data["candidate"]["preserve-user-file"] = False
        self.target.write_text("regression", encoding="utf-8")
        self.assertEqual(w.cycle_operation(self.record, "candidate", data)["status"], "revert")
        w.cycle_operation(self.record, "rollback", {})
        self.assertEqual(self.target.read_text(), "pre-existing user edits\n")

    def test_insufficient_evidence_and_missing_holdout(self):
        data = self.evaluation()
        data["held_out"] = []
        self.assertEqual(w.cycle_decision(data), "insufficient-evidence")
        data = self.evaluation()
        data["evidence_sufficient"] = False
        self.assertEqual(w.cycle_decision(data), "insufficient-evidence")

    def test_concurrent_edit_blocks_rollback(self):
        self.target.write_text("candidate", encoding="utf-8")
        w.cycle_operation(self.record, "candidate", self.evaluation())
        self.target.write_text("another writer", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "Concurrent edit"):
            w.cycle_operation(self.record, "rollback", {})
        self.assertEqual(self.target.read_text(), "another writer")

    def test_candidate_limit(self):
        w.cycle_operation(self.record, "candidate", self.evaluation())
        w.cycle_operation(self.record, "candidate", self.evaluation())
        with self.assertRaisesRegex(ValueError, "exhausted"):
            w.cycle_operation(self.record, "candidate", self.evaluation())


class VersionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="version-fixture-")
        self.addCleanup(cleanup_fixture, self.tmp)
        self.repo = Path(self.tmp.name)
        make_app(self.repo)

    def test_all_fields_and_dependency_preservation(self):
        paths = v.discover(self.repo)
        v.set_versions(paths, "1.2.3")
        self.assertEqual(set(v.read_versions(paths).values()), {"1.2.3"})
        lock = json.loads((self.repo / "package-lock.json").read_text())
        self.assertEqual(lock["packages"][""]["version"], "1.2.3")
        self.assertEqual(lock["packages"]["node_modules/dependency"]["version"], "1.0.0")
        config = json.loads(paths["tauri"].read_text(encoding="utf-8"))
        self.assertEqual(config["app"]["windows"][0]["title"], "Example · v1.2.3")
        self.assertEqual(config["app"]["windows"][1]["title"], "Tools")
        self.assertEqual(config["other"]["version"], "9.9.9")
        self.assertIn('name = "third-party"\nversion = "1.0.0"', paths["cargo_lock"].read_text())

    def test_repair_stale_root_and_title_without_reformat(self):
        root = self.repo / "package-lock.json"
        root.write_text('{"version":"1.0.0","packages":{"":{"version":"0.9.0"},"node_modules/x":{"version":"7.7.7"}}}', encoding="utf-8")
        config = self.repo / "src-tauri/tauri.conf.json"
        config.write_text('{"version":"1.0.0","app":{"windows":[{"title":"Hello v0.9.0"}]}}', encoding="utf-8")
        v.set_versions(v.discover(self.repo), "1.0.0")
        self.assertEqual(root.read_text(), '{"version":"1.0.0","packages":{"":{"version":"1.0.0"},"node_modules/x":{"version":"7.7.7"}}}')
        self.assertEqual(config.read_text(), '{"version":"1.0.0","app":{"windows":[{"title":"Hello v1.0.0"}]}}')

    def test_holdout_nested_app_crlf_legacy_lock_and_no_npm(self):
        nested = self.repo / "frontend"
        nested.mkdir()
        for p in list(self.repo.iterdir()):
            if p != nested:
                p.rename(nested / p.name)
        (nested / "package.json").unlink()
        (nested / "package-lock.json").unlink()
        config = nested / "src-tauri/tauri.conf.json"
        config.write_bytes(b'{\r\n  "version": "1.0.0",\r\n  "app": {"windows": [{"title": "Window 1.0.0"}]}\r\n}\r\n')
        v.set_versions(v.discover(self.repo), "2.3.4")
        self.assertIn(b'Window 2.3.4', config.read_bytes())
        self.assertEqual(config.read_bytes().count(b'\r\n'), 4)

    def test_ambiguous_project_or_title_causes_no_partial_writes(self):
        config = self.repo / "src-tauri/tauri.conf.json"
        config.write_text('{"version":"1.0.0","app":{"windows":[{"title":"App 1.0.0 vs 2.0.0"}]}}', encoding="utf-8")
        before = config.read_bytes()
        with self.assertRaisesRegex(ValueError, "Ambiguous"):
            v.set_versions(v.discover(self.repo), "3.0.0")
        self.assertEqual(config.read_bytes(), before)
        make_app(self.repo / "second")
        with self.assertRaisesRegex(ValueError, "found 2"):
            v.discover(self.repo)

    def test_pr_bump_once_and_scoped_breaking_classification(self):
        run_git(self.repo, "init", "-b", "main")
        run_git(self.repo, "config", "user.name", "Fixture")
        run_git(self.repo, "config", "user.email", "fixture@example.invalid")
        run_git(self.repo, "add", ".")
        run_git(self.repo, "commit", "-m", "baseline")
        run_git(self.repo, "switch", "-c", "codex/feature")
        (self.repo / "feature.txt").write_text("feature", encoding="utf-8")
        run_git(self.repo, "add", "feature.txt")
        run_git(self.repo, "commit", "-m", "feat(ui): add feature")
        command = [sys.executable, str(VERSION_ROOT / "bump_version.py"), "--repo", str(self.repo), "--base", "main"]
        subprocess.check_output(command, stderr=subprocess.STDOUT)
        self.assertEqual(set(v.read_versions(v.discover(self.repo)).values()), {"1.1.0"})
        before = {str(p): p.read_bytes() for p in v.discover(self.repo).values()}
        subprocess.check_output(command, stderr=subprocess.STDOUT)
        self.assertEqual(before, {str(p): p.read_bytes() for p in v.discover(self.repo).values()})
        self.assertEqual(b.classify("feat(api)!: incompatible"), "major")


if __name__ == "__main__":
    unittest.main(verbosity=2)
