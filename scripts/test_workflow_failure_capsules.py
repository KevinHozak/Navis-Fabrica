"""Failure capture exercises synthetic responses and disposable repositories only."""
import copy
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import unittest

import test_workflow_evidence as existing
import workflow_evidence as w


class FailureCapsules(unittest.TestCase):
    setUp = existing.Fixture.setUp
    record = existing.Fixture.record
    check = existing.Fixture.check
    ready = existing.Fixture.ready

    def payload(self, state="malformed"):
        return {
            "id": "review-1", "attempt_id": "attempt-1", "stage": "review",
            "diagnostic_code": "invalid_json" if state == "malformed" else state,
            "parser": {"name": "strict-review", "version": "fixture-1", "schema": "review-1"},
            "response": {"state": state},
            "usage_reservations": [{"id": "usage-1", "state": "unknown"}],
            "next_action": "inspect-protocol",
        }

    def test_malformed_response_persists_diagnosis_without_raw_body(self):
        response = '{"passed":'  # A real JSON parse failure on synthetic input.
        with self.assertRaises(json.JSONDecodeError):
            json.loads(response)
        data = self.record()
        self.check(data)
        fixture = self.root / "malformed-review.txt"
        fixture.write_text(response, encoding="utf-8")
        payload = self.payload()
        payload["response"].update(sha256=hashlib.sha256(response.encode()).hexdigest(), size_bytes=len(response))
        payload["synthetic_reproduction"] = str(fixture)
        source = (self.repo / "source.txt").read_bytes()
        checks = copy.deepcopy(data["checks"])
        w.checkpoint(data, self.repo, "failure-capsule", payload)
        record = self.root / "run.json"
        w.save(record, data)
        capsule = w.load(record)["failure_capsules"]["review-1"]
        self.assertEqual(capsule["diagnostic_code"], "invalid_json")
        self.assertEqual(capsule["checks"]["build"]["status"], "passed")
        self.assertEqual(capsule["source"]["fingerprint"], w.fingerprint(self.repo))
        self.assertEqual(capsule["synthetic_reproduction"]["sha256"], hashlib.sha256(fixture.read_bytes()).hexdigest())
        self.assertFalse(capsule["response_content_retained"])
        self.assertNotIn(response, record.read_text())
        self.assertEqual(checks, data["checks"])
        self.assertEqual(source, (self.repo / "source.txt").read_bytes())
        self.assertFalse(w.readiness(data, self.repo)["ready"])

    def test_valid_json_review_still_passes_without_capsule(self):
        review = json.loads('{"status":"complete","actionable":0}')
        data = self.ready()
        review["head"] = w.identity(self.repo)["head"]
        w.checkpoint(data, self.repo, "review", review)
        self.assertTrue(w.readiness(data, self.repo)["ready"])
        self.assertNotIn("failure_capsules", data)

    def test_duplicate_output_is_diagnosable_without_saving_content(self):
        response = '{"passed":true}{"passed":true}'
        with self.assertRaises(json.JSONDecodeError):
            json.loads(response)
        data = self.record()
        payload = self.payload()
        payload["diagnostic_code"] = "duplicate_output"
        payload["response"].update(sha256=hashlib.sha256(response.encode()).hexdigest(), size_bytes=len(response))
        w.checkpoint(data, self.repo, "failure-capsule", payload)
        self.assertEqual(data["failure_capsules"]["review-1"]["diagnostic_code"], "duplicate_output")
        self.assertNotIn(response, json.dumps(data))

    def test_previous_review_text_is_not_copied_and_complete_metadata_is_bounded(self):
        data = self.ready()
        data["review"]["details"] = "private-review-content-must-not-copy"
        w.checkpoint(data, self.repo, "failure-capsule", self.payload())
        self.assertNotIn("private-review-content-must-not-copy", json.dumps(data["failure_capsules"]))
        data = self.record()
        data["checks"]["x" * 33000] = {"status": "passed"}
        before = copy.deepcopy(data)
        with self.assertRaisesRegex(ValueError, "32768"):
            w.checkpoint(data, self.repo, "failure-capsule", self.payload())
        self.assertEqual(data, before)

    def test_failed_new_review_supersedes_pass_but_keeps_original_observation(self):
        data = self.ready()
        original = copy.deepcopy(data["review"])
        w.checkpoint(data, self.repo, "failure-capsule", self.payload())
        self.assertEqual(data["failure_capsules"]["review-1"]["previous_review"], original)
        self.assertEqual(data["review"]["status"], "unavailable")
        self.assertFalse(w.readiness(data, self.repo)["ready"])
        # A new explicit observed successful review can recover readiness.
        w.checkpoint(data, self.repo, "review", original)
        self.assertTrue(w.readiness(data, self.repo)["ready"])
        self.assertIn("review-1", data["failure_capsules"])

    def test_rejects_raw_oversize_invalid_and_release_fields_atomically(self):
        variations = []
        raw = self.payload(); raw["raw_response"] = "token=must-not-save"; variations.append(raw)
        raw = self.payload(); raw["response"]["body"] = "private text"; variations.append(raw)
        raw = self.payload(); raw["id"] = "x" * 17000; variations.append(raw)
        raw = self.payload(); raw["diagnostic_code"] = {}; variations.append(raw)
        raw = self.payload(); raw["response"]["size_bytes"] = True; variations.append(raw)
        raw = self.payload(); raw["usage_reservations"][0]["state"] = "released"; variations.append(raw)
        raw = self.payload(); raw["parser"] = None; variations.append(raw)
        raw = self.payload(); raw["synthetic_reproduction"] = {}; variations.append(raw)
        for payload in variations:
            with self.subTest(payload_keys=list(payload)):
                data = self.record()
                before = copy.deepcopy(data)
                with self.assertRaises(ValueError):
                    w.checkpoint(data, self.repo, "failure-capsule", payload)
                self.assertEqual(data, before)

    def test_terminal_attempts_holds_checks_effects_and_duplicate_capsule_preserved(self):
        data = self.record()
        self.check(data)
        data["terminal_attempts"] = {"attempt-1": "failed_review_protocol"}
        data["usage_holds"] = {"usage-1": {"state": "unknown", "tokens": 24576}}
        data["effects"] = {"local-1": {"kind": "fixture"}}
        preserved = {name: copy.deepcopy(data[name]) for name in ("terminal_attempts", "usage_holds", "effects", "checks")}
        w.checkpoint(data, self.repo, "failure-capsule", self.payload())
        for name, original in preserved.items():
            self.assertEqual(data[name], original)
        before = copy.deepcopy(data)
        with self.assertRaisesRegex(ValueError, "already recorded"):
            w.checkpoint(data, self.repo, "failure-capsule", self.payload())
        self.assertEqual(before, data)

    def test_cli_roundtrip_and_invalid_input_leave_record_unchanged(self):
        record = self.root / "run.json"
        w.save(record, self.record())
        payload = self.root / "capsule.json"
        payload.write_text(json.dumps(self.payload()), encoding="utf-8")
        command = [sys.executable, str(Path(w.__file__)), "checkpoint", "--repo", str(self.repo),
                   "--record", str(record), "--action", "failure-capsule", "--input", str(payload)]
        result = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(w.load(record)["failure_capsules"]["review-1"]["attempt_id"], "attempt-1")
        before = record.read_bytes()
        payload.write_text('{"raw_response":"password=must-not-save"}', encoding="utf-8")
        result = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("must-not-save", result.stdout + result.stderr)
        self.assertEqual(before, record.read_bytes())

    def test_heldout_timeout_and_truncated_response_keep_reservations_and_no_pass(self):
        for state in ("timeout", "truncated"):
            with self.subTest(state=state):
                data = self.ready()
                data["usage_holds"] = {"usage-1": "unknown"}
                w.checkpoint(data, self.repo, "failure-capsule", self.payload(state))
                capsule = data["failure_capsules"]["review-1"]
                self.assertEqual(capsule["response"]["state"], state)
                self.assertEqual(capsule["usage_reservations"], [{"id": "usage-1", "state": "unknown"}])
                self.assertEqual(data["usage_holds"], {"usage-1": "unknown"})
                self.assertEqual(data["review"]["status"], "unavailable")
                self.assertFalse(w.readiness(data, self.repo)["ready"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
