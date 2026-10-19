"""Behavioral version preparation tests against temporary real Git repositories."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HELPERS = Path(__file__).resolve().parents[1] / "dev-version"
sys.path.insert(0, str(HELPERS))
spec = importlib.util.spec_from_file_location("version_preparation", HELPERS / "prepare_version.py")
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class VersionPreparation(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name) / "app with spaces"
        self.root.mkdir()
        self.store = Path(self.tmp.name) / "private state"
        self.git("init", "-q")
        for key, value in (("user.name", "Fixture"), ("user.email", "fixture@example.invalid"),
                           ("core.autocrlf", "false"), ("commit.gpgsign", "false")):
            self.git("config", key, value)
        self.write(".gitignore", "target/\nnode_modules/\ndist/\n")
        self.write("src/app.ts", "export const answer = 1;\n")
        self.write("src-tauri/tauri.conf.json", json.dumps({"version": "1.0.0", "app": {
            "windows": [{"title": "Fixture v1.0.0"}]}}) + "\n")
        self.write("src-tauri/Cargo.toml", '[package]\nname = "fixture"\nversion = "1.0.0"\n\n[dependencies]\nother = "1.0.0"\n')
        self.write("src-tauri/Cargo.lock", 'version = 4\n\n[[package]]\nname = "fixture"\nversion = "1.0.0"\n\n[[package]]\nname = "other"\nversion = "1.0.0"\n')
        self.write("package.json", json.dumps({"name": "fixture", "version": "1.0.0", "dependencies": {"other": "1.0.0"}}) + "\n")
        self.write("package-lock.json", json.dumps({"version": "1.0.0", "packages": {
            "": {"version": "1.0.0", "dependencies": {"other": "1.0.0"}},
            "node_modules/other": {"version": "1.0.0"}}}) + "\n")
        self.write("index.html", "<main>Fixture</main>\n")
        self.base = self.commit()
        self.ctx = helper.context(self.root, self.store)
        self.artifact = Path(self.tmp.name) / "verified-release.exe"
        self.artifact.write_bytes(b"synthetic verified artifact")

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args], stderr=subprocess.PIPE).decode().strip()

    def write(self, relative, content):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8", newline="")

    def commit(self):
        self.git("add", "--all")
        self.git("commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def change(self, value=2):
        self.write("src/app.ts", f"export const answer = {value};\n")

    def prepare(self, **kwargs):
        return helper.prepare(self.ctx, **kwargs)

    def state(self):
        return json.loads(self.ctx["state_path"].read_text())

    def build(self, prepared):
        return helper.record(self.ctx, token=prepared["token"], artifacts=[self.artifact])

    def push(self, prepared):
        sha = self.commit()
        return helper.record(self.ctx, token=prepared["token"], commit=sha, remote_head=sha)

    def test_build_first_then_push_and_retries_allocate_once(self):
        self.change()
        first = self.prepare(base=self.base)
        self.assertEqual(first["version"], "1.0.1")
        self.assertEqual(self.prepare()["token"], first["token"])
        self.build(first)
        self.assertEqual(self.prepare()["token"], first["token"])
        self.push(first)
        self.assertEqual(self.prepare()["token"], first["token"])
        self.assertEqual(self.state()["last_build"]["version"], "1.0.1")
        self.assertEqual(self.state()["last_push"]["version"], "1.0.1")

    def test_push_first_then_build_allocate_once(self):
        self.change()
        first = self.prepare(base=self.base)
        self.push(first)
        second = self.prepare()
        self.assertEqual(second["token"], first["token"])
        self.build(second)
        self.assertEqual(self.prepare()["version"], "1.0.1")

    def test_content_after_each_success_allocates_next_version(self):
        self.change()
        first = self.prepare(base=self.base)
        self.build(first)
        self.change(3)
        second = self.prepare()
        self.assertEqual(second["version"], "1.0.2")
        self.push(second)
        self.change(4)
        third = self.prepare()
        self.assertEqual(third["version"], "1.0.3")
        # A historical successful build must not seal this new, unbuilt allocation.
        self.change(5)
        self.assertEqual(self.prepare()["version"], "1.0.3")

    def test_baseline_only_docs_and_commit_do_not_consume_bump(self):
        baseline = self.prepare(base=self.base)
        self.assertEqual(baseline["action"], "baseline")
        self.assertFalse(baseline["allocated"])
        self.write("docs/notes.md", "Updated workflow\n")
        self.commit()
        self.assertEqual(self.prepare()["token"], baseline["token"])
        self.change()
        self.assertEqual(self.prepare()["version"], "1.0.1")

    def test_existing_bump_is_adopted_and_not_bumped_again(self):
        helper.setter.set_versions(self.ctx["paths"], "1.0.7")
        first = self.prepare(base=self.base)
        self.assertEqual((first["action"], first["version"]), ("adopted", "1.0.7"))
        self.change()
        self.assertEqual(self.prepare()["version"], "1.0.7")

    def test_failed_attempt_and_fix_reuse_unsealed_allocation(self):
        self.change()
        first = self.prepare(base=self.base)
        with self.assertRaises(FileNotFoundError):
            helper.record(self.ctx, token=first["token"], artifacts=[self.root / "missing.exe"])
        self.assertFalse(self.state()["prepared"]["sealed"])
        self.change(3)
        fixed = self.prepare()
        self.assertEqual(fixed["version"], "1.0.1")
        self.assertNotEqual(fixed["token"], first["token"])

    def test_dependency_versions_and_literal_code_are_not_masked(self):
        baseline = self.prepare(base=self.base)
        self.write("src/app.ts", 'export const answer = "1.0.0";\n')
        first = self.prepare()
        self.assertNotEqual(first["fingerprint"], baseline["fingerprint"])
        self.build(first)
        package = json.loads((self.root / "package.json").read_text())
        package["dependencies"]["other"] = "1.0.1"
        self.write("package.json", json.dumps(package) + "\n")
        second = self.prepare()
        self.assertEqual(second["version"], "1.0.2")
        self.assertIn('other = "1.0.0"', (self.root / "src-tauri/Cargo.toml").read_text())

    def test_new_untracked_file_deletion_and_ignored_output(self):
        first = self.prepare(base=self.base)
        self.write("src/new.ts", "new source\n")
        second = self.prepare()
        self.assertNotEqual(first["fingerprint"], second["fingerprint"])
        self.build(second)
        (self.root / "src/app.ts").unlink()
        third = self.prepare()
        self.assertEqual(third["version"], "1.0.2")
        self.write("src-tauri/target/release/generated.exe", "ignored\n")
        self.assertEqual(self.prepare()["fingerprint"], third["fingerprint"])

    def test_exact_target_reused_and_downgrade_rejected(self):
        self.change()
        first = self.prepare(base=self.base, version="2.0.0")
        self.assertEqual(self.prepare()["version"], "2.0.0")
        with self.assertRaises(ValueError):
            self.prepare(version="1.0.1")
        self.assertEqual(self.state()["prepared"]["token"], first["token"])

    def test_missing_base_and_corrupt_state_fail_without_version_write(self):
        with self.assertRaisesRegex(ValueError, "First use"):
            self.prepare()
        self.assertEqual(helper.synchronized(helper.setter.read_versions(self.ctx["paths"])), "1.0.0")
        self.prepare(base=self.base)
        self.ctx["state_path"].write_text('{"schema_version":1}')
        with self.assertRaises(ValueError):
            self.prepare()
        self.assertEqual(helper.synchronized(helper.setter.read_versions(self.ctx["paths"])), "1.0.0")

    def test_mismatched_fields_or_unexplained_manual_version_rejected(self):
        self.prepare(base=self.base)
        self.write("package.json", '{"version":"8.0.0"}')
        with self.assertRaisesRegex(ValueError, "disagree"):
            self.prepare()
        helper.setter.set_versions(self.ctx["paths"], "8.0.0")
        with self.assertRaisesRegex(ValueError, "differs"):
            self.prepare()
        self.assertEqual(self.prepare(version="8.0.0")["version"], "8.0.0")

    def test_receipts_reject_source_drift_stale_tokens_and_wrong_commit(self):
        self.change()
        first = self.prepare(base=self.base)
        with self.assertRaisesRegex(ValueError, "does not match"):
            helper.record(self.ctx, token=first["token"], commit=self.base, remote_head=self.base)
        self.change(3)
        with self.assertRaisesRegex(ValueError, "drift"):
            self.build(first)
        second = self.prepare()
        with self.assertRaisesRegex(ValueError, "stale"):
            self.build(first)
        sha = self.commit()
        with self.assertRaisesRegex(ValueError, "Observed remote"):
            helper.record(self.ctx, token=second["token"], commit=sha, remote_head=self.base)
        self.assertIsNone(self.state()["last_push"])

    def test_success_recording_is_idempotent_and_artifacts_are_hashed(self):
        self.change()
        first = self.prepare(base=self.base)
        self.assertEqual(self.build(first), self.build(first))
        self.assertEqual(self.push(first), helper.record(self.ctx, token=first["token"],
            commit="HEAD", remote_head=self.git("rev-parse", "HEAD")))
        self.assertEqual(self.state()["last_build"]["artifacts"][str(self.artifact.resolve())]["sha256"],
                         helper.hash_file(self.artifact))

    def test_intent_write_failure_leaves_manifests_unchanged(self):
        self.change()
        with mock.patch.object(helper, "save", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                self.prepare(base=self.base)
        self.assertEqual(helper.synchronized(helper.setter.read_versions(self.ctx["paths"])), "1.0.0")
        self.assertFalse(self.ctx["state_path"].exists())

    def test_interrupted_partial_version_write_recovers_same_token(self):
        self.change()
        def interrupted(paths, target):
            original = paths["package"].read_text(encoding="utf-8")
            paths["package"].write_text(helper.setter.replace_json(original, {("version",): target}), encoding="utf-8")
            raise OSError("simulated crash between manifest writes")
        with mock.patch.object(helper.setter, "set_versions", side_effect=interrupted):
            with self.assertRaises(OSError):
                self.prepare(base=self.base)
        token = self.state()["pending"]["prepared"]["token"]
        recovered = self.prepare()
        self.assertEqual((recovered["version"], recovered["token"]), ("1.0.1", token))
        self.assertIsNone(self.state()["pending"])

    def test_interrupted_after_manifest_write_recovers_without_extra_bump(self):
        self.change()
        real_save = helper.save
        count = 0
        def interrupted(path, state):
            nonlocal count
            count += 1
            if count == 2:
                raise OSError("simulated crash before final state save")
            real_save(path, state)
        with mock.patch.object(helper, "save", side_effect=interrupted):
            with self.assertRaises(OSError):
                self.prepare(base=self.base)
        token = self.state()["pending"]["prepared"]["token"]
        self.assertEqual(self.prepare()["token"], token)
        self.assertEqual(helper.synchronized(helper.setter.read_versions(self.ctx["paths"])), "1.0.1")

    def test_pending_recovery_rejects_unrelated_changes(self):
        self.change()
        with mock.patch.object(helper.setter, "set_versions", side_effect=OSError("crash")):
            with self.assertRaises(OSError):
                self.prepare(base=self.base)
        self.change(3)
        with self.assertRaisesRegex(ValueError, "interrupted"):
            self.prepare()

    def test_profile_must_be_explicit_and_repo_contained(self):
        self.prepare(base=self.base)
        for includes in (["../outside"], ["C:/outside"], ["src", "assets"]):
            with self.assertRaises(ValueError):
                self.prepare(includes=includes)
        with self.assertRaises(ValueError):
            helper.context(self.root, self.root / "state")

    def test_custom_profile_retains_managed_fields_and_assets(self):
        self.write("assets/image.bin", "binary asset")
        first = self.prepare(base=self.base, includes=["assets"])
        self.assertEqual(first["version"], "1.0.1")
        self.change()
        self.assertEqual(self.prepare()["fingerprint"], first["fingerprint"])
        self.write("assets/image.bin", "updated asset")
        self.assertNotEqual(self.prepare()["fingerprint"], first["fingerprint"])

    def test_linked_checkout_uses_shared_identity_and_receipt_state(self):
        first = self.prepare(base=self.base)
        linked = Path(self.tmp.name) / "linked checkout"
        self.git("worktree", "add", "--detach", str(linked), "HEAD")
        linked_ctx = helper.context(linked, self.store)
        self.assertEqual(linked_ctx["state_path"], self.ctx["state_path"])
        self.assertEqual(helper.prepare(linked_ctx)["token"], first["token"])
        self.git("worktree", "remove", str(linked))

    def test_concurrent_preparations_serialize_and_bump_once(self):
        self.change()
        argv = [sys.executable, str(HELPERS / "prepare_version.py"), "prepare", "--repo", str(self.root),
                "--state-dir", str(self.store), "--base", self.base]
        processes = [subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE) for _ in range(2)]
        results = []
        for proc in processes:
            stdout, stderr = proc.communicate(timeout=45)
            self.assertEqual(proc.returncode, 0, stderr.decode())
            results.append(json.loads(stdout))
        self.assertEqual(results[0]["token"], results[1]["token"])
        self.assertEqual(results[0]["version"], "1.0.1")

    def test_crlf_checkout_and_commit_match(self):
        self.write("src/app.ts", "export const answer = 2;\r\n")
        first = self.prepare(base=self.base)
        self.push(first)
        self.write("src/app.ts", "export const answer = 2;\n")
        self.assertEqual(self.prepare()["token"], first["token"])

    def test_committed_app_changes_before_bootstrap_and_version_order(self):
        self.change()
        self.commit()
        first = self.prepare(base=self.base, version="1.0.9")
        self.build(first)
        self.change(3)
        self.assertEqual(self.prepare()["version"], "1.0.10")

    def test_explicit_imported_markdown_is_source(self):
        self.write("src/content.md", "imported text\n")
        first = self.prepare(base=self.base, includes=["src", "src/content.md"])
        self.build(first)
        self.write("src/content.md", "changed imported text\n")
        self.assertEqual(self.prepare()["version"], "1.0.2")

    def test_cli_status_does_not_initialize_state_or_change_manifests(self):
        result = subprocess.run([sys.executable, str(HELPERS / "prepare_version.py"), "status",
            "--repo", str(self.root), "--state-dir", str(self.store)], capture_output=True, check=True)
        self.assertIsNone(json.loads(result.stdout)["state"])
        self.assertFalse(self.store.exists())
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_symlink_source_escape_fails(self):
        outside = Path(self.tmp.name) / "outside.txt"
        outside.write_text("outside source")
        link = self.root / "src/escape.ts"
        try:
            link.symlink_to(outside)
        except OSError:
            self.skipTest("Host does not allow creating a symlink")
        with self.assertRaisesRegex(ValueError, "Symlink"):
            self.prepare(base=self.base)

    def test_invalid_pending_version_is_not_used_for_repair(self):
        self.change()
        with mock.patch.object(helper.setter, "set_versions", side_effect=OSError("crash")):
            with self.assertRaises(OSError):
                self.prepare(base=self.base)
        state = self.state()
        state["pending"]["prepared"]["version"] = "0.0.1"
        self.ctx["state_path"].write_text(json.dumps(state))
        with self.assertRaisesRegex(ValueError, "recovery intent"):
            self.prepare()
        self.assertEqual(helper.synchronized(helper.setter.read_versions(self.ctx["paths"])), "1.0.0")

    def test_status_distinguishes_built_pushed_and_changed_source(self):
        self.change()
        first = self.prepare(base=self.base)
        self.build(first)
        def status():
            return json.loads(subprocess.check_output([sys.executable, str(HELPERS / "prepare_version.py"),
                "status", "--repo", str(self.root), "--state-dir", str(self.store)]))
        observed = status()
        self.assertTrue(observed["built_current_source"])
        self.assertFalse(observed["pushed_current_source"])
        self.push(first)
        self.assertTrue(status()["pushed_current_source"])
        self.change(3)
        observed = status()
        self.assertFalse(observed["built_current_source"])
        self.assertFalse(observed["pushed_current_source"])
        self.assertFalse(observed["source_matches"])

    def test_git_symlink_entry_is_rejected_without_host_symlink_support(self):
        oid = subprocess.check_output(["git", "-C", str(self.root), "hash-object", "-w", "--stdin"],
            input=b"../../outside.txt").decode().strip()
        self.git("update-index", "--add", "--cacheinfo", "120000," + oid + ",src/escape.ts")
        self.git("commit", "-qm", "fixture symlink tree")
        with self.assertRaisesRegex(ValueError, "Unsupported source tree"):
            helper.snapshot(self.ctx, helper.profile_for(self.ctx), self.git("rev-parse", "HEAD"))


if __name__ == "__main__":
    unittest.main()
