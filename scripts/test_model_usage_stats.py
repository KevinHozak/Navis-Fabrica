import json
from pathlib import Path
import tempfile
import os
import unittest
import sqlite3

from model_usage_stats import collect, read_codex
from usage_metrics import cost_summary, estimate, timing


def temporary_directory():
    scratch = Path(os.environ.get('WORKFLOW_TEST_TMP', tempfile.gettempdir()))
    scratch.mkdir(parents=True, exist_ok=True)
    return tempfile.TemporaryDirectory(dir=scratch)


class UsageTests(unittest.TestCase):
    def test_public_checkout_output_refused(self):
        with self.assertRaises(ValueError):
            collect(output=Path(__file__).resolve().parents[1] / 'private-fixture')

    def test_cost_cache_reasoning_and_unknown_models(self):
        rate = {'short_per_million': [2, .1, 2.5, 10], 'long_per_million': [4, .2, 5, 15],
                'source': 'fixture', 'verified_on': '2026-10-16'}
        row = dict(model='fixture', input_tokens=1000000, cached_input_tokens=500000,
                   cache_write_input_tokens=100000, output_tokens=100000, reasoning_output_tokens=40000)
        self.assertAlmostEqual(estimate(row, rate)['estimated_cost_usd'], 2.1)
        self.assertAlmostEqual(estimate(row, rate)['estimated_cost_upper_usd'], 3.7)
        summary = cost_summary([row, {**row, 'model': 'unknown'}], {'fixture': rate})
        self.assertEqual(summary['unpriced_events'], 1)
        self.assertIsNone(summary['actual_cost_usd'])
        self.assertIsNone(estimate({**row, 'cached_input_tokens': 2000000}, rate)['estimated_cost_usd'])

    def test_idle_time_and_missing_timestamps(self):
        result = timing([0, 60, 60, 600, 630])
        self.assertEqual(result['elapsed_seconds'], 630)
        self.assertEqual(result['active_seconds_estimate'], 90)
        self.assertIsNone(timing([])['elapsed_seconds'])

    def test_fabrica_actual_estimates_time_and_retention(self):
        with temporary_directory() as tmp:
            root = Path(tmp)
            source = root / 'fabrica.sqlite3'
            with sqlite3.connect(source) as db:
                db.execute('CREATE TABLE workflow_runs(record_json TEXT)')
                run = dict(id='run', modelId='meta/muse-spark-open', startedAtMs=1000,
                           finishedAtMs=631000, events=[{'timestampMs': 61000}, {'timestampMs': 601000}],
                           secret='should not export')
                db.execute('INSERT INTO workflow_runs VALUES (?)', (json.dumps(run),))
                db.execute('CREATE TABLE budget_reservations(reservation_id TEXT, model_id TEXT, provider_id TEXT, state TEXT, dispatched_at_ms INTEGER, actual_seconds INTEGER)')
                db.execute("INSERT INTO budget_reservations VALUES ('parent','meta/muse-spark-open','openrouter','settled',1000,630)")
                db.execute('CREATE TABLE paid_call_ledger(call_id TEXT,parent_reservation_id TEXT,state TEXT,actual_tokens INTEGER,actual_cost_micros INTEGER,settlement_source TEXT,dispatched_at_ms INTEGER)')
                db.execute("INSERT INTO paid_call_ledger VALUES ('call','parent','settled',1000000,1250000,'openrouter-response-usage',1000)")
                db.execute("INSERT INTO paid_call_ledger VALUES ('unknown','parent','unresolved',NULL,NULL,'unknown',1000)")
            db.close()
            args = dict(codex_home=root, fabrica_db=source, output=root / 'out')
            result = collect(**args)
            self.assertEqual(result['fabrica']['cost']['actual_cost_usd'], 1.25)
            self.assertEqual(result['fabrica']['cost']['unknown_cost_calls'], 1)
            self.assertEqual(result['fabrica']['runs'][0]['timing']['elapsed_seconds'], 630)
            self.assertEqual(result['fabrica']['runs'][0]['timing']['active_seconds_estimate'], 90)
            self.assertEqual(result['fabrica']['paid_calls'][0]['estimated_cost_lower_usd'], .15)
            self.assertNotIn('should not export', json.dumps(result))
            with sqlite3.connect(source) as db:
                db.execute('DELETE FROM workflow_runs')
                db.execute('DELETE FROM paid_call_ledger')
            db.close()
            repeated = collect(**args)
            self.assertEqual(repeated['fabrica']['cost']['actual_cost_usd'], 1.25)
            self.assertEqual(len(repeated['fabrica']['runs']), 1)

    def test_snapshots_models_and_reimport(self):
        with temporary_directory() as tmp:
            root = Path(tmp)
            folder = root / 'sessions'
            folder.mkdir()
            events = [{'type': 'turn_context', 'payload': {'model': 'first'}}]
            for count in (10, 10, 15):
                events.append({'type': 'event_msg', 'payload': {'type': 'token_count',
                    'info': {'total_token_usage': {'input_tokens': count, 'total_tokens': count}}}})
            events.append({'type': 'turn_context', 'payload': {'model': 'second'}})
            events.append({'type': 'event_msg', 'payload': {'type': 'token_count',
                'info': {'total_token_usage': {'input_tokens': 20, 'total_tokens': 20}}}})
            path = folder / 'test.jsonl'
            path.write_text('\n'.join(map(json.dumps, events)) + '\n{"partial":', encoding='utf-8')
            self.assertEqual([r['input_tokens'] for r in read_codex(path)['usage']], [10, 5, 5])
            first = collect(codex_home=root, fabrica_db=root / 'missing', output=root / 'out')
            second = collect(codex_home=root, fabrica_db=root / 'missing', output=root / 'out')
            self.assertEqual(first['codex_models'], second['codex_models'])
            self.assertEqual(second['codex_models']['first']['input_tokens'], 15)
            self.assertEqual(second['codex_models']['second']['input_tokens'], 5)
            self.assertEqual(second['fabrica']['databases_found'], [])


if __name__ == '__main__':
    unittest.main()
