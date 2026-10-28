"""Contract and behavioral checks for FAB-S001's synthetic fixtures."""

import importlib.util
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock
from pathlib import Path


MODULE_PATH = Path(__file__).resolve().parents[1] / "economy_trials.py"
SPEC = importlib.util.spec_from_file_location("economy_trials", MODULE_PATH)
trials = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(trials)
TEST_TMP = os.environ.get("FABRICA_TEST_TMP")


class EconomyTrialFixtureTests(unittest.TestCase):
    def canonical_association(self, model_id):
        return {"selected_model_id": model_id,
                "expected_canonical_model_id": "fixture/model",
                "provider_profile": "fixture-provider-profile",
                "at_ms": 1,
                "prerequisites_sha256": hashlib.sha256(b"fixture metadata").hexdigest()}

    def runner_receipt(self, **updates):
        receipt = {"ok": True, "taskId": "small_bugfix",
                   "modelId": trials.TRIAL_MODELS[0], "providerId": "openrouter",
                   "runtimeId": "aider", "runtimeVersion": "0.86.2",
                   "state": "completed", "requestCount": 1, "tokensUsed": 100,
                   "latencyMs": 1000, "billing": "zeroCost",
                   "sourceFingerprint": trials.seed_fingerprint("small_bugfix"),
                   "schemaVersion": 1, "failureCode": None,
                   "observedModelId": trials.TRIAL_MODELS[0]}
        receipt.update(updates)
        return receipt

    def test_allowlist_and_budget_are_exact(self):
        self.assertEqual(trials.TRIAL_MODELS, (
            "poolside/laguna-s-2.1:free",
            "nvidia/nemotron-3-ultra-550b-a55b:free",
            "nvidia/nemotron-3.5-lightning:free",
        ))
        self.assertEqual(trials.TASK_TYPES, ("small_bugfix", "implementation", "multifile_edit"))
        self.assertEqual(trials.MAX_REQUESTS_PER_TASK, 3)
        self.assertEqual(trials.MAX_OUTPUT_TOKENS_PER_REQUEST, 8192)
        self.assertEqual(trials.TASK_DEADLINE_SECONDS, 120)
        spec = json.loads(trials.CONFIG_PATH.read_text(encoding="utf-8"))
        self.assertNotIn("expected", json.dumps(spec).lower())
        self.assertEqual(hashlib.sha256(trials.CONFIG_PATH.read_bytes()).hexdigest(),
                         trials.FIXTURE_SET_HASH)

    def test_canonical_prerequisite_binds_private_settings_and_raw_snapshot_hash(self):
        with tempfile.TemporaryDirectory(dir=TEST_TMP) as temp:
            root = Path(temp) / "private"
            settings_path = root / "settings.json"
            snapshot_path = root / "canonical-model-prerequisites.json"
            profile_id = "openRouter-fixture-7"
            settings_path.parent.mkdir(parents=True)
            settings_path.write_text(json.dumps({
                "providerSelection": {"providerId": profile_id},
                "providers": [{"id": profile_id, "kind": "openRouter"}],
            }), encoding="utf-8")
            now = int(time.time() * 1000)
            snapshot = {
                "at_ms": now - 1000,
                "inference_calls": 0,
                "provider_profile": profile_id,
                "models": [{
                    "id": model,
                    "canonical_slug": "fixture/" + model.rsplit("/", 1)[-1].replace(":free", ""),
                    "context_length": 32768,
                    "pricing": {"prompt": "0", "completion": "0"},
                } for model in trials.TRIAL_MODELS],
                "model_count": len(trials.TRIAL_MODELS),
                "account_limits": {"remaining": "do-not-copy"},
                "aider_version": "aider 0.86.2",
                "aider_exit": 0,
            }
            snapshot_path.write_text(json.dumps(snapshot), encoding="utf-8")
            with mock.patch.object(trials, "PRIVATE_EVIDENCE_ROOT", root):
                association = trials.load_canonical_prerequisite(
                    snapshot_path, trials.TRIAL_MODELS[0], settings_path, now_ms=now)
                self.assertEqual(association["expected_canonical_model_id"],
                                 snapshot["models"][0]["canonical_slug"])
                self.assertEqual(association["prerequisites_sha256"],
                                 hashlib.sha256(snapshot_path.read_bytes()).hexdigest())
                self.assertNotIn("account_limits", association)
                snapshot["at_ms"] = now + 1
                snapshot_path.write_text(json.dumps(snapshot), encoding="utf-8")
                with self.assertRaises(ValueError):
                    trials.load_canonical_prerequisite(
                        snapshot_path, trials.TRIAL_MODELS[0], settings_path, now_ms=now)

    def test_run_invokes_runner_with_new_data_root_uncreated(self):
        with tempfile.TemporaryDirectory(dir=TEST_TMP) as temp:
            root = Path(temp) / "private"
            trial_root = root / "trials"
            profile_id = "openRouter-test-profile"
            settings = root / "settings.json"
            settings.parent.mkdir(parents=True)
            settings.write_text(json.dumps({
                "providerSelection": {"providerId": profile_id},
                "providers": [{"id": profile_id, "kind": "openRouter"}],
            }), encoding="utf-8")
            snapshot = root / "canonical-model-prerequisites.json"
            now = int(time.time() * 1000)
            snapshot.write_text(json.dumps({
                "at_ms": now - 1,
                "inference_calls": 0,
                "provider_profile": profile_id,
                "models": [{
                    "id": model,
                    "canonical_slug": "fixture/" + str(index),
                    "context_length": 32768,
                    "pricing": {"prompt": "0", "completion": "0"},
                } for index, model in enumerate(trials.TRIAL_MODELS)],
                "model_count": 3,
                "aider_version": "aider 0.86.2",
                "aider_exit": 0,
            }), encoding="utf-8")
            runner = root / "bin" / "fabrica-runner.exe"
            runner.parent.mkdir(parents=True)
            runner.write_bytes(b"unit executable placeholder")
            task = "small_bugfix"
            model = trials.TRIAL_MODELS[0]
            data_root = trial_root / "runtime" / "pair"
            receipt = {
                "schemaVersion": 1, "ok": False, "taskId": task, "modelId": model,
                "providerId": "", "runtimeId": "aider", "runtimeVersion": "",
                "state": "failed", "requestCount": 0, "tokensUsed": None,
                "latencyMs": 0, "billing": "zeroCost", "sourceFingerprint": None,
                "failureCode": "PREFLIGHT_REJECTED", "observedModelId": None,
            }

            def fake_runner(argv, timeout_seconds=None):
                self.assertEqual(argv[1], "economy-trial-run")
                self.assertIn("--synthetic-policy-consent", argv)
                self.assertFalse(data_root.exists(), "CLI must create its own fresh data root")
                return json.dumps(receipt).encode(), 1

            with mock.patch.object(trials, "_configured_trial_root", return_value=trial_root), \
                    mock.patch.object(trials, "PRIVATE_EVIDENCE_ROOT", root), \
                    mock.patch.object(trials, "validate_runner_cli"), \
                    mock.patch.object(trials, "_run_bounded", side_effect=fake_runner):
                result = trials.run_trial(
                    runner, settings, trial_root / "journal", data_root, model, task,
                    trial_root / "workspaces" / task,
                    trial_root / "verifiers" / task, snapshot)
            self.assertEqual(result["record"]["failure_code"], "preflight_rejected")
            self.assertFalse(data_root.exists())

    def test_all_baselines_fail_and_expected_outputs_pass_behavioral_checks(self):
        trials.validate_fixture_contract()
        for task_type, fixture in trials.FIXTURES.items():
            with self.subTest(task_type=task_type), tempfile.TemporaryDirectory(dir=TEST_TMP) as temp:
                workspace = Path(temp) / "synthetic"
                verifier = Path(temp) / "verifier"
                metadata = trials.prepare_fixture(task_type, workspace)
                suite = trials.prepare_check_suite(task_type, verifier)
                self.assertEqual(metadata["check_hashes"], suite["hashes"])
                self.assertEqual(metadata["fixture_set_hash"], trials.FIXTURE_SET_HASH)
                self.assertEqual(metadata["seed_hashes"], {
                    name: hashlib.sha256((workspace / name).read_bytes()).hexdigest()
                    for name in fixture["files"]})
                self.assertFalse((workspace / "trial.json").exists())
                self.assertFalse(set(fixture["checks"]) & {p.name for p in workspace.iterdir()})
                env = os.environ.copy()
                env["PYTHONPATH"] = str(workspace)
                env["TRIAL_SOURCE_ROOT"] = str(workspace)
                command = suite["command"]
                baseline = subprocess.run(command, cwd=verifier, env=env, capture_output=True,
                                          text=True, timeout=10)
                self.assertNotEqual(baseline.returncode, 0, baseline.stdout + baseline.stderr)
                for relative, content in fixture["expected"].items():
                    (workspace / relative).write_text(content, encoding="utf-8", newline="\n")
                corrected = subprocess.run(command, cwd=verifier, env=env, capture_output=True,
                                           text=True, timeout=10)
                self.assertEqual(corrected.returncode, 0, corrected.stdout + corrected.stderr)
                self.assertEqual(trials.check_hashes(task_type), suite["hashes"])

    def test_prepare_refuses_nonempty_workspace_and_unknown_type(self):
        with tempfile.TemporaryDirectory(dir=TEST_TMP) as temp:
            occupied = Path(temp) / "occupied"
            occupied.mkdir()
            (occupied / "keep.txt").write_text("preserve", encoding="utf-8")
            with self.assertRaises(FileExistsError):
                trials.prepare_fixture("small_bugfix", occupied)
            with self.assertRaises(ValueError):
                trials.prepare_fixture("unknown", Path(temp) / "new")

    def test_plan_contains_exactly_nine_unique_pairs(self):
        plan = trials.trial_plan()
        self.assertEqual(len(plan), 9)
        self.assertEqual(len({(row["model_id"], row["task_type"]) for row in plan}), 9)
        self.assertEqual({row["model_id"] for row in plan}, set(trials.TRIAL_MODELS))
        self.assertEqual({row["task_type"] for row in plan}, set(trials.TASK_TYPES))

    def test_configured_trial_root_rejects_relative_git_public_and_reparse_paths(self):
        with mock.patch.dict(os.environ, {trials.TRIAL_ROOT_ENV: "relative/trials"}):
            with self.assertRaisesRegex(ValueError, "absolute"):
                trials._configured_trial_root()
        public_source = trials.CONFIG_PATH.resolve().parents[1] / "private-trials"
        with mock.patch.dict(os.environ, {trials.TRIAL_ROOT_ENV: str(public_source)}):
            with self.assertRaisesRegex(ValueError, "public source"):
                trials._configured_trial_root()
        git_descendant = Path(TEST_TMP) / "external-looking-trials"
        with mock.patch.dict(os.environ, {trials.TRIAL_ROOT_ENV: str(git_descendant)}):
            with self.assertRaisesRegex(ValueError, "Git repository"):
                trials._configured_trial_root()
        link_candidate = Path("C:/Temp/Fabrica-Trials-Link")
        with mock.patch.dict(os.environ, {trials.TRIAL_ROOT_ENV: str(link_candidate)}), \
                mock.patch.object(trials, "_is_link_or_reparse",
                                  side_effect=lambda path: Path(path) == link_candidate):
            with self.assertRaisesRegex(ValueError, "symlink or reparse"):
                trials._configured_trial_root()

    def test_runner_receipt_enforces_identity_usage_and_runtime_bounds(self):
        receipt = self.runner_receipt()
        parsed = trials.parse_runner_receipt(json.dumps(receipt).encode(), 0,
                                              trials.TRIAL_MODELS[0], "small_bugfix")
        self.assertEqual(parsed["runtimeVersion"], "0.86.2")
        self.assertEqual(parsed["sourceFingerprint"], trials.seed_fingerprint("small_bugfix"))
        alternate = self.runner_receipt(observedModelId="openrouter/free", ok=False,
                                        state="failed", failureCode="MODEL_IDENTITY_MISMATCH")
        parsed_alternate = trials.parse_runner_receipt(json.dumps(alternate).encode(), 1,
                                                        trials.TRIAL_MODELS[0], "small_bugfix")
        self.assertEqual(parsed_alternate["modelId"], trials.TRIAL_MODELS[0])
        self.assertEqual(parsed_alternate["observedModelId"], "openrouter/free")
        preflight = self.runner_receipt(ok=False, state="failed", requestCount=0,
                                        tokensUsed=None, latencyMs=0, providerId="",
                                        runtimeVersion="", sourceFingerprint=None,
                                        failureCode="INVALID_TRIAL_ARGUMENT", observedModelId=None)
        accepted_preflight = trials.parse_runner_preflight(
            json.dumps(preflight).encode(), 2, trials.TRIAL_MODELS[0], "small_bugfix")
        self.assertEqual(accepted_preflight["requestCount"], 0)
        for invalid in (
            self.runner_receipt(requestCount=4),
            self.runner_receipt(latencyMs=120001),
            self.runner_receipt(billing="paid"),
            self.runner_receipt(runtimeVersion="0.86.3"),
        ):
            with self.subTest(receipt=invalid), self.assertRaises(ValueError):
                trials.parse_runner_receipt(json.dumps(invalid).encode(), 0,
                                            trials.TRIAL_MODELS[0], "small_bugfix")
        with self.assertRaises(ValueError):
            trials.parse_runner_receipt(b"x" * (64 * 1024 + 1), 0,
                                        trials.TRIAL_MODELS[0], "small_bugfix")
        with self.assertRaises(ValueError):
            trials._run_bounded([sys.executable, "-c", "print('x' * 70000)"])

    def test_outer_timeout_terminates_only_its_owned_process_tree(self):
        with tempfile.TemporaryDirectory(dir=TEST_TMP) as temp:
            marker = Path(temp) / "child-survived.txt"
            child = ("import time; time.sleep(2.5); "
                     f"open({str(marker)!r}, 'w', encoding='utf-8').write('alive')")
            parent = ("import subprocess,sys,time; "
                      f"subprocess.Popen([sys.executable, '-c', {child!r}]); "
                      "time.sleep(20)")
            with self.assertRaises(subprocess.TimeoutExpired):
                trials._run_bounded([sys.executable, "-c", parent], timeout_seconds=1)
            time.sleep(2.8)
            self.assertFalse(marker.exists())

    def test_ambiguous_attempt_is_durable_and_cannot_be_retried(self):
        with tempfile.TemporaryDirectory(dir=TEST_TMP) as temp:
            root = Path(temp)
            workspace = root / "workspace"
            trials.prepare_fixture("small_bugfix", workspace)
            journal = root / "journal"
            reserved = trials.reserve_attempt(journal, trials.TRIAL_MODELS[0],
                                               "small_bugfix", workspace,
                                               self.canonical_association(trials.TRIAL_MODELS[0]))
            self.assertEqual(reserved["state"], "attempted")
            finalized = trials.finalize_attempt(
                journal, trials.TRIAL_MODELS[0], "small_bugfix",
                {"state": "ambiguous", "request_count": None,
                 "duration_seconds": None, "checks": {}})
            self.assertEqual(finalized["state"], "ambiguous")
            with self.assertRaises(trials.AttemptAlreadyReserved):
                trials.reserve_attempt(journal, trials.TRIAL_MODELS[0],
                                       "small_bugfix", workspace,
                                       self.canonical_association(trials.TRIAL_MODELS[0]))

    def test_reservation_rejects_modified_or_extra_seed_files(self):
        with tempfile.TemporaryDirectory(dir=TEST_TMP) as temp:
            root = Path(temp)
            workspace = root / "workspace"
            trials.prepare_fixture("small_bugfix", workspace)
            (workspace / "extra.py").write_text("# extra\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                trials.reserve_attempt(root / "journal", trials.TRIAL_MODELS[0],
                                       "small_bugfix", workspace,
                                       self.canonical_association(trials.TRIAL_MODELS[0]))

    def test_finalize_rejects_request_token_and_time_overages(self):
        with tempfile.TemporaryDirectory(dir=TEST_TMP) as temp:
            root = Path(temp)
            workspace = root / "workspace"
            trials.prepare_fixture("small_bugfix", workspace)
            journal = root / "journal"
            trials.reserve_attempt(journal, trials.TRIAL_MODELS[0], "small_bugfix", workspace,
                                   self.canonical_association(trials.TRIAL_MODELS[0]))
            for outcome in (
                {"state": "failed", "request_count": 4},
                {"state": "failed", "output_tokens_per_request": [8193]},
                {"state": "failed", "duration_seconds": 121},
            ):
                with self.subTest(outcome=outcome), self.assertRaises(ValueError):
                    trials.finalize_attempt(journal, trials.TRIAL_MODELS[0],
                                            "small_bugfix", outcome)

    def test_verify_trial_requires_reviewed_inventory_then_executes_host_checks(self):
        with tempfile.TemporaryDirectory(dir=TEST_TMP) as temp:
            root = Path(temp)
            with mock.patch.object(trials, "_configured_trial_root", return_value=root / "private"):
                workspace = root / "private" / "workspace"
                trials.prepare_fixture("small_bugfix", workspace)
                journal = root / "private" / "journal"
                model = trials.TRIAL_MODELS[0]
                trials.reserve_attempt(journal, model, "small_bugfix", workspace,
                                       self.canonical_association(model))
                for name, content in trials.FIXTURES["small_bugfix"]["expected"].items():
                    (workspace / name).write_text(content, encoding="utf-8")
                inventory = trials.candidate_inventory(workspace)
                trials.finalize_attempt(journal, model, "small_bugfix", {
                    "state": "completed", "request_count": 1, "duration_seconds": 2,
                    "observed_model_id": model, "candidate_inventory_after": inventory,
                    "checks": {}, "host_check_status": "not_run",
                    "host_check_hashes": trials.check_hashes("small_bugfix")})
                changed_review = dict(inventory)
                changed_review["name_utils.py"] = "0" * 64
                with self.assertRaises(ValueError):
                    trials.verify_trial(journal, model, "small_bugfix", workspace,
                                        root / "private" / "verifier", changed_review)
                verified = trials.verify_trial(journal, model, "small_bugfix", workspace,
                                               root / "private" / "verifier", inventory)
                self.assertEqual(verified["status"], "passed")
                with self.assertRaises(trials.AttemptAlreadyReserved):
                    trials.verify_trial(journal, model, "small_bugfix", workspace,
                                        root / "private" / "verifier-repeat", inventory)


if __name__ == "__main__":
    unittest.main()
