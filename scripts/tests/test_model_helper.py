import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("model_helper", Path(__file__).parents[1] / "model_helper.py")
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

class HelperTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.path = Path(self.temp.name) / "usage.sqlite3"
        self.config = {"ledger_path": str(self.path), "daily_budget_usd": 1, "per_call_budget_usd": .05}
        self.policy = m.load(m.ROOT / "config/model-routes.json")
        self.catalog = {"data": [{"id": "qwen/qwen3-coder-next", "context_length": 262144, "pricing": {"prompt": "0.00000012", "completion": "0.0000008"}}]}
    def tearDown(self):
        self.temp.cleanup()
    def ledger(self):
        return m.Ledger(self.path)
    def test_concurrent_connections_cannot_overspend(self):
        a, b = self.ledger(), self.ledger()
        route = m.route(self.policy, "coding-advice")
        a.reserve("day", route, "coding-advice", 700000, 1000000)
        with self.assertRaises(m.Refusal):
            b.reserve("day", route, "coding-advice", 400000, 1000000)
        self.assertEqual(b.totals("day")["committed_micros"], 700000)
        a.db.close(); b.db.close()
    def test_unknown_retains_reservation_and_next_day_separate(self):
        ledger = self.ledger()
        identity = ledger.reserve("one", m.route(self.policy, "coding-advice"), "coding-advice", 900000, 1000000)
        self.assertEqual(ledger.settle(identity, None, 10, None, 2), "unknown")
        self.assertEqual(ledger.totals("one")["committed_micros"], 900000)
        self.assertEqual(ledger.totals("two")["committed_micros"], 0)
        ledger.db.close()
    def test_settled_cost_replaces_reservation(self):
        ledger = self.ledger()
        identity = ledger.reserve("one", m.route(self.policy, "coding-advice"), "coding-advice", 900000, 1000000)
        self.assertEqual(ledger.settle(identity, .0000011, 10, 2, 2), "settled")
        self.assertEqual(ledger.totals("one")["committed_micros"], 2)
        ledger.db.close()
    def test_overrun_is_charged_and_blocks_later_dispatch(self):
        ledger = self.ledger()
        identity = ledger.reserve("one", m.route(self.policy, "coding-advice"), "coding-advice", 10, 1000000)
        self.assertEqual(ledger.settle(identity, .001, 10, 2, 2), "overrun")
        ledger.db.close()
        with self.assertRaises(m.Refusal):
            m.run(self.config, self.policy, "draft-local", "Synthetic draft", get=lambda *a, **k: self.fail("must not dispatch"))
    def test_private_cloud_rejected_before_network(self):
        with self.assertRaises(m.Refusal):
            m.run(self.config, self.policy, "coding-advice", "private data", private=True, get=lambda *a, **k: self.fail("network"))
    def test_input_limit_rejects_before_network(self):
        with self.assertRaises(m.Refusal):
            m.run(self.config, self.policy, "draft-local", "a"*2048, get=lambda *a, **k: self.fail("network"))
    def test_local_tokens_and_no_content_persisted(self):
        def fake(url, payload, key, **kwargs):
            self.assertTrue(url.startswith("http://127.0.0.1:11434/"))
            self.assertIsNone(key)
            self.assertEqual(payload["options"]["num_ctx"], 2048)
            return {"response": "sensitive-reply", "done": True, "done_reason": "stop", "prompt_eval_count": 8, "eval_count": 4}
        result = m.run(self.config, self.policy, "draft-local", "sensitive-prompt", True, fake)
        self.assertTrue(result["complete"])
        self.assertEqual(result["usage"]["reported_cost_usd"], 0)
        self.assertEqual(result["usage"]["input_tokens"], 8)
        for path in self.path.parent.glob("usage.sqlite3*"):
            raw = path.read_bytes()
            self.assertNotIn(b"sensitive-prompt", raw); self.assertNotIn(b"sensitive-reply", raw)
    @patch.object(m, "api_key", return_value="unit-test-placeholder")
    def test_cloud_payload_pin_and_usage(self, key):
        def fake(url, payload=None, secret=None, **kwargs):
            if payload is None: return self.catalog
            self.assertEqual(payload["model"], "qwen/qwen3-coder-next")
            self.assertFalse(payload["provider"]["allow_fallbacks"])
            self.assertEqual(payload["provider"]["data_collection"], "deny")
            self.assertEqual(payload["provider"]["max_price"]["request"], 0)
            self.assertNotIn("tools", payload)
            return {"choices": [{"message": {"content": "answer"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 10, "completion_tokens": 3, "cost": .0000036}}
        result = m.run(self.config, self.policy, "coding-advice", "synthetic", get=fake)
        self.assertEqual(result["billing_state"], "settled")
        self.assertEqual(result["daily_totals"]["committed_micros"], 4)
    @patch.object(m, "api_key", return_value="unit-test-placeholder")
    def test_failed_dispatch_not_retried_or_secret_logged(self, key):
        calls = []
        def fake(url, payload=None, secret=None, **kwargs):
            if payload is None: return self.catalog
            calls.append(url)
            raise RuntimeError("unit-test-placeholder sensitive data")
        with self.assertRaises(m.Refusal) as caught:
            m.run(self.config, self.policy, "coding-advice", "synthetic", get=fake)
        self.assertEqual(len(calls), 1)
        self.assertNotIn("unit-test-placeholder", str(caught.exception))
        ledger = self.ledger()
        self.assertEqual(ledger.db.execute("SELECT state FROM calls").fetchone()[0], "unknown")
        self.assertGreater(ledger.db.execute("SELECT reserved_micros FROM calls").fetchone()[0], 0)
        ledger.db.close()
    def test_request_fee_refused(self):
        self.catalog["data"][0]["pricing"]["request"] = ".1"
        with self.assertRaises(m.Refusal):
            m.cloud_quote(m.route(self.policy, "coding-advice"), 100, lambda *a: self.catalog)
    def test_public_repo_ledger_refused(self):
        with self.assertRaises(m.Refusal): m.Ledger(m.ROOT / "private.sqlite3")
    def test_tiered_price_refused(self):
        self.catalog["data"][0]["pricing_overrides"] = [{}]
        with self.assertRaises(m.Refusal):
            m.cloud_quote(m.route(self.policy, "coding-advice"), 100, lambda *a: self.catalog)
    def test_redirect_refused(self):
        with self.assertRaises(m.Refusal): m.NoRedirect().redirect_request(None, None, None, None, None, None)
    def test_truncated_local_reply_not_complete(self):
        result = m.run(self.config, self.policy, "draft-local", "synthetic", get=lambda *a, **k: {"response":"partial", "done":True,"done_reason":"length","prompt_eval_count":8,"eval_count":256})
        self.assertFalse(result["complete"])
    @patch.object(m, "api_key", return_value="unit-test-placeholder")
    def test_cap_refuses_before_inference(self, key):
        ledger = self.ledger()
        ledger.reserve(m.dt.date.today().isoformat(), m.route(self.policy, "coding-advice"), "earlier", 999999, 1000000)
        ledger.db.close()
        def fake(url, payload=None, *args, **kwargs):
            if payload is None: return self.catalog
            self.fail("over-budget inference dispatched")
        with self.assertRaises(m.Refusal): m.run(self.config, self.policy, "coding-advice", "synthetic", get=fake)
    @patch.object(m, "api_key", return_value="unit-test-placeholder")
    def test_near_budget_warning_and_provider_breakdown(self, key):
        ledger = self.ledger()
        identity = ledger.reserve(m.dt.date.today().isoformat(), m.route(self.policy, "coding-advice"), "earlier", 800000, 1000000)
        ledger.settle(identity, .8, 20, 10, 1); ledger.db.close()
        def fake(url, payload=None, *args, **kwargs):
            if payload is None: return self.catalog
            return {"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"cost":.000003}}
        result = m.run(self.config, self.policy, "coding-advice", "synthetic", get=fake)
        self.assertTrue(result["budget_nearing_limit"])
        self.assertEqual(result["daily_totals"]["by_provider"]["openrouter"]["reported_input_tokens"],30)
    @patch.object(m, "api_key", return_value="unit-test-placeholder")
    def test_missing_usage_keeps_cloud_hold(self, key):
        def fake(url, payload=None, *args, **kwargs):
            return self.catalog if payload is None else {"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}
        result=m.run(self.config,self.policy,"coding-advice","synthetic",get=fake)
        self.assertEqual(result["billing_state"],"unknown")
        self.assertGreater(result["daily_totals"]["committed_micros"],0)
    def test_bad_money_refused(self):
        for value in ["NaN", "Infinity", "-1"]:
            with self.assertRaises(m.Refusal): m.micros(value)

if __name__ == "__main__": unittest.main()
