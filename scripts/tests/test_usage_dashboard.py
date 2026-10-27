import json,sqlite3,tempfile,sys,unittest
from pathlib import Path
from contextlib import contextmanager
@contextmanager
def database(path):
 db=sqlite3.connect(path)
 try:
  yield db
  db.commit()
 finally:db.close()
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
import usage_dashboard as d
RATE={'short_per_million':[2,.1,2.5,10],'long_per_million':[4,.2,5,15],'source':'fixture','verified_on':'2026-10-16'}
class DashboardTests(unittest.TestCase):
 def setUp(self):
  self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup);self.root=Path(self.temp.name);(self.root/'sessions').mkdir()
  self.request={'schema_version':1,'state_dir':str(self.root/'state'),'codex_root':str(self.root),'fabrica_databases':[],'pricing':{'models':{'fixture':RATE}},'refresh':True,'query':{'from_ms':0,'to_ms':1000000,'source':None,'provider':None,'model':None}}
 def session(self,name='a',parent=None,model='fixture'):
  rows=[{'type':'session_meta','payload':{'id':name,**({'forked_from_id':parent} if parent else {})}}, {'type':'turn_context','payload':{'model':model}}]
  for at,total,cached in [(10,100,20),(60,200,40),(600,300,60),(630,400,80)]:
   rows.append({'type':'event_msg','timestamp':f'1970-01-01T00:{at//60:02}:{at%60:02}Z','payload':{'type':'token_count','info':{'total_token_usage':{'input_tokens':total,'output_tokens':total//10,'cached_input_tokens':cached,'reasoning_output_tokens':total//20,'total_tokens':total+total//10}},'text':'PRIVATE-PROMPT'}})
  rows.append({'type':'response_item','timestamp':'1970-01-01T00:11:00Z','payload':{'type':'function_call','arguments':'PRIVATE-TOOL-ARGS'}})
  path=self.root/'sessions'/(name+'.jsonl');path.write_text('\n'.join(map(json.dumps,rows)),encoding='utf-8');return path
 def run_report(self,**filters):
  self.request['query'].update(filters);return d.execute(self.request)
 def test_idempotent_retention_and_privacy(self):
  path=self.session();first=self.run_report();second=self.run_report();self.assertEqual(first['groups'],second['groups']);path.unlink();third=self.run_report();self.assertEqual(first['groups'],third['groups'])
  data=(self.root/'state/dashboard.sqlite3').read_bytes();self.assertNotIn(b'PRIVATE-PROMPT',data);self.assertNotIn(b'PRIVATE-TOOL-ARGS',data)
  g=first['groups'][0];self.assertEqual(g['tokens']['input_tokens'],400);self.assertEqual(g['tokens']['output_tokens'],40);self.assertEqual(g['tokens']['reasoning_output_tokens'],20);self.assertEqual(g['tokens']['total_tokens'],440);self.assertIsNone(g['actual_usd']);self.assertEqual(g['cache_write_unknown_events'],4);self.assertAlmostEqual(g['estimate_low_usd'],.001048);self.assertAlmostEqual(g['estimate_high_usd'],.001896)
 def test_timestamp_range_and_idle_gap_clipping(self):
  self.session();v=self.run_report(from_ms=50000,to_ms=620000);g=v['groups'][0]
  self.assertEqual(g['tokens']['input_tokens'],200)
  self.assertEqual(g['elapsed_seconds'],570);self.assertEqual(g['active_seconds'],30)
  v=self.run_report(from_ms=100000,to_ms=200000);self.assertEqual(v['groups'][0]['active_seconds'],0);self.assertEqual(v['groups'][0]['events'],0)
 def test_model_filter_unknown_pricing_and_missing_sources(self):
  self.session(model='unpriced');v=self.run_report(model='unpriced');g=v['groups'][0];self.assertIsNone(g['estimate_low_usd']);self.assertEqual(g['unpriced_events'],4)
  self.assertFalse(self.run_report(model='different')['groups']);self.request['codex_root']=str(self.root/'missing');self.assertIn('codex_missing',self.run_report()['collection']['errors'])
 def test_forks_and_duplicate_copies_do_not_double_token_totals(self):
  path=self.session();(self.root/'sessions/copy.jsonl').write_bytes(path.read_bytes());self.session('child',parent='a')
  v=self.run_report();self.assertEqual(sum(g['tokens']['input_tokens'] or 0 for g in v['groups']),400)
  self.assertEqual(sum(g['active_seconds'] or 0 for g in v['groups']),110)
 def native(self):
  path=self.root/'native.sqlite3'
  with database(path) as db:
   db.execute('CREATE TABLE workflow_runs(record_json TEXT)');db.execute('INSERT INTO workflow_runs VALUES(?)',(json.dumps({'id':'run','provider':'codex','modelId':'fixture','startedAtMs':10000,'finishedAtMs':630000,'events':[{'timestampMs':10000},{'timestampMs':60000},{'timestampMs':600000},{'timestampMs':630000}],'prompt':'PRIVATE-PROMPT'}),))
   db.execute('CREATE TABLE budget_reservations(reservation_id TEXT,model_id TEXT,provider_id TEXT,state TEXT,dispatched_at_ms INTEGER,actual_seconds INTEGER)');db.execute("INSERT INTO budget_reservations VALUES('parent','fixture','openrouter','settled',10000,620)")
   db.execute('CREATE TABLE paid_call_ledger(call_id TEXT,parent_reservation_id TEXT,state TEXT,actual_tokens INTEGER,actual_cost_micros INTEGER,settlement_source TEXT,dispatched_at_ms INTEGER)')
   db.executemany('INSERT INTO paid_call_ledger VALUES(?,?,?,?,?,?,?)',[('zero','parent','settled',100,0,'provider',10000),('unknown','parent','unresolved',None,None,'unknown',60000)])
  self.request['fabrica_databases']=[str(path),str(path)];return path
 def test_actual_zero_unknown_parent_exclusion_and_native_retention(self):
  path=self.native();v=self.run_report();g=next(g for g in v['groups'] if g['source']=='fabrica-paid');self.assertEqual(g['records'],2);self.assertEqual(g['actual_usd'],0);self.assertEqual(g['unknown_cost_events'],1)
  self.assertEqual(len([v for v in v['details'] if v['source']=='fabrica-task']),1)
  path.unlink();again=self.run_report();self.assertEqual(v['groups'],again['groups']);self.assertNotIn('PRIVATE-PROMPT',json.dumps(again))
 def test_native_partial_missing_timeline_and_date_clipping(self):
  path=self.native()
  with database(path) as db:db.execute('INSERT INTO workflow_runs VALUES(?)',(json.dumps({'id':'partial','provider':'codex','modelId':'fixture','startedAtMs':10000,'events':[]}),))
  v=self.run_report(from_ms=50000,to_ms=620000);rows=[v for v in v['details'] if v['source']=='fabrica-workflow'];self.assertEqual(len(rows),1);self.assertEqual(rows[0]['elapsed_seconds'],570);self.assertEqual(rows[0]['active_seconds'],30)
  v=self.run_report(from_ms=0,to_ms=1000000);partial=next(v for v in v['details'] if v['id']=='partial');self.assertTrue(partial['partial']);self.assertIsNone(partial['active_seconds']);self.assertIsNone(partial['elapsed_seconds'])
 def test_invalid_range_refuses(self):
  self.session()
  with self.assertRaises(ValueError):self.run_report(from_ms=100,to_ms=100)
 def test_running_span_has_no_synthetic_finish(self):
  self.native()
  with database(self.root/'native.sqlite3') as db:db.execute('INSERT INTO workflow_runs VALUES(?)',(json.dumps({'id':'running','startedAtMs':10000,'events':[{'timestampMs':10000},{'timestampMs':60000}]}),))
  v=self.run_report();row=next(r for r in v['details'] if r['id']=='running');self.assertEqual(row['elapsed_seconds'],50);self.assertTrue(row['partial'])

 def test_concurrent_refresh_serializes_without_double_counting(self):
  from concurrent.futures import ThreadPoolExecutor
  self.session();self.run_report()
  with ThreadPoolExecutor(max_workers=2) as pool:results=list(pool.map(lambda _:d.execute(self.request),range(2)))
  self.assertTrue(all(v['groups'][0]['tokens']['input_tokens']==400 for v in results))
 def test_concurrent_sessions_are_spans_not_unique_wall_time(self):
  self.session();self.session('independent');v=self.run_report();self.assertEqual(v['groups'][0]['records'],2);self.assertEqual(v['groups'][0]['active_seconds'],220)
 def test_naive_timestamps_are_unattributed(self):
  p=self.session();p.write_text(p.read_text(encoding='utf-8').replace('Z',''),encoding='utf-8');v=self.run_report();self.assertFalse(v['groups']);self.assertEqual(v['untimed_records'],4)
 def test_pricing_snapshot_validation_refuses_negative_rates(self):
  self.request['pricing']={'models':{'bad':{**RATE,'short_per_million':[-1,0,0,0]}}}
  with self.assertRaises(ValueError):self.run_report()
if __name__=='__main__':unittest.main()
