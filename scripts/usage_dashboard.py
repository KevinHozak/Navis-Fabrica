"""Bounded metadata-only dashboard import/query. No network or inference."""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import sys
import time
from datetime import datetime
from model_usage_stats import read_codex
from usage_metrics import epoch, estimate, number

SCHEMA = 1
MAX_FILES = 10000
MAX_IMPORTS = 400
MAX_NATIVE_ROWS = 20000
MAX_RESPONSE_ROWS = 2000
TOKEN_KEYS = ('input_tokens','cached_input_tokens','cache_write_input_tokens','output_tokens','reasoning_output_tokens','total_tokens')

def label(value, default='unknown'):
    return value if isinstance(value,str) and 0 < len(value) <= 160 and all(ord(c)>=32 for c in value) else default

def digest(value):
    return hashlib.sha256(json.dumps(value,sort_keys=True,separators=(',',':')).encode()).hexdigest()

def clipped(start,end,lower,upper):
    if not number(start) or not number(end) or end < start: return None
    return max(0,min(end,upper)-max(start,lower))

def spans(points,start,end,lower,upper):
    elapsed=clipped(start,end,lower,upper)
    active=None
    if points:
        # Classify original gaps BEFORE clipping; a long idle gap stays idle.
        points=sorted(set(points));active=sum(clipped(a,b,lower,upper) or 0 for a,b in zip(points,points[1:]) if 0 <= b-a <= 300)
    return elapsed,active

class Store:
    def __init__(self,path):
        Path(path).parent.mkdir(parents=True,exist_ok=True)
        self.db=sqlite3.connect(path,timeout=8)
        self.db.execute('PRAGMA journal_mode=WAL')
        self.db.execute('CREATE TABLE IF NOT EXISTS dashboard_records(kind TEXT,id TEXT,rank TEXT,record TEXT,PRIMARY KEY(kind,id))')
        self.db.execute('CREATE TABLE IF NOT EXISTS dashboard_files(path TEXT PRIMARY KEY,stamp TEXT)')
        self.db.execute('CREATE TABLE IF NOT EXISTS dashboard_meta(id TEXT PRIMARY KEY,record TEXT)')
    def put(self,kind,identity,record,rank):
        old=self.db.execute('SELECT rank FROM dashboard_records WHERE kind=? AND id=?',(kind,identity)).fetchone()
        # Do not replace a retained complete snapshot with a shorter/stale copy.
        if old and tuple(json.loads(old[0])) > tuple(rank): return
        self.db.execute('INSERT OR REPLACE INTO dashboard_records VALUES(?,?,?,?)',(kind,identity,json.dumps(rank),json.dumps(record)))
    def records(self,kind):
        return [(identity,json.loads(raw)) for identity,raw in self.db.execute('SELECT id,record FROM dashboard_records WHERE kind=?',(kind,))]
    def close(self): self.db.close()

def clean_codex(raw,path):
    identity=label(raw['session_id'],digest(str(path)))
    parent=label(raw.get('parent_id'),'') or None
    timeline=sorted([{'at':v['at'],'model':label(v['model'])} for v in raw['timeline'] if number(v.get('at'))],key=lambda v:v['at'])
    usage=[]
    for event in raw['usage']:
        stamp=None
        try:
            if datetime.fromisoformat(event['timestamp'].replace('Z','+00:00')).tzinfo is not None:stamp=epoch(event['timestamp'])
        except (ValueError,TypeError,KeyError,AttributeError,OSError):stamp=None
        if stamp is None: continue
        counts={k:event.get(k) if number(event.get(k)) else None for k in TOKEN_KEYS}
        counts['total_tokens']=(counts['input_tokens']+counts['output_tokens']) if counts['input_tokens'] is not None and counts['output_tokens'] is not None else None
        usage.append({'at':stamp,'model':label(event.get('model')),'counts':counts,
                      'event':digest([stamp,event.get('model'),event.get('snapshot')])})
    return identity,{'parent':parent,'timeline':timeline,'usage':usage,'untimed_usage':len(raw['usage'])-len(usage)}

def import_codex(store,root,errors):
    if not root or not Path(root).is_dir(): errors.append('codex_missing');return 0
    files=[];visited=0
    for folder in ('sessions','archived_sessions'):
        base=Path(root)/folder
        for directory,dirs,names in os.walk(base,followlinks=False):
            dirs[:]=[d for d in dirs if not (Path(directory)/d).is_symlink()]
            visited+=1
            if visited>MAX_FILES: errors.append('codex_inventory_limit');break
            for name in names:
                if not name.endswith('.jsonl'):continue
                path=Path(directory)/name
                if path.is_symlink():continue
                files.append(path)
                if len(files)>=MAX_FILES:break
            if len(files)>=MAX_FILES: errors.append('codex_inventory_limit');break
    if not files: errors.append('codex_empty')
    files.sort(key=lambda p:p.stat().st_mtime_ns,reverse=True)
    imported=0;started=time.monotonic()
    for path in files:
        if imported>=MAX_IMPORTS or time.monotonic()-started>18:
            errors.append('codex_import_limit');break
        try:
            stat=path.stat();stamp=f'{SCHEMA}:{stat.st_mtime_ns}:{stat.st_size}'
            key=digest(str(path.resolve()))
            if store.db.execute('SELECT stamp FROM dashboard_files WHERE path=?',(key,)).fetchone()==(stamp,):continue
            raw=read_codex(path,dashboard=True);identity,record=clean_codex(raw,path)
            store.put('codex',identity,record,[len(record['timeline']),len(record['usage'])])
            store.db.execute('INSERT OR REPLACE INTO dashboard_files VALUES(?,?)',(key,stamp));imported+=1
        except (OSError,ValueError,TypeError,KeyError): errors.append('codex_read_error')
    return imported

def native_record(kind,identity,model,provider,at,**extra):
    return {'kind':kind,'id':label(identity,digest(identity)),'model':label(model),'provider':label(provider),'at':at if number(at) else None,**extra}

def import_native(store,paths,errors):
    found=0
    for path in dict.fromkeys(str(Path(p).resolve()) for p in paths):
        if not Path(path).is_file(): errors.append('fabrica_missing');continue
        try:
            with closing(sqlite3.connect(Path(path).as_uri()+'?mode=ro',uri=True,timeout=2)) as db:
                db.execute('PRAGMA trusted_schema=OFF')
                tables={v[0] for v in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
                for table in ('workflow_runs','paid_call_ledger','budget_reservations'):
                    if table in tables and db.execute('SELECT COUNT(*) FROM '+table).fetchone()[0]>MAX_NATIVE_ROWS:
                        errors.append('native_import_limit')
                found+=1
                if 'workflow_runs' in tables:
                    for (raw,) in db.execute('SELECT record_json FROM workflow_runs LIMIT ?',(MAX_NATIVE_ROWS+1,)):
                        if len(raw)>2*1024*1024:errors.append('native_record_limit');continue
                        v=json.loads(raw);events=sorted(set(e['timestampMs']/1000 for e in v.get('events',[]) if number(e.get('timestampMs'))))
                        start=v.get('startedAtMs');finish=v.get('finishedAtMs')
                        start=start/1000 if number(start) else (events[0] if events else None)
                        end=finish/1000 if number(finish) else (events[-1] if events else None)
                        record=native_record('fabrica-workflow',v['id'],v.get('modelId'),v.get('provider'),start,
                            start=start,end=end,points=events,state=label(v.get('state')),partial=not number(finish))
                        store.put('native','workflow:'+record['id'],record,[number(finish),end or 0])
                if {'paid_call_ledger','budget_reservations'}<=tables:
                    query="""SELECT c.call_id,c.state,c.actual_tokens,c.actual_cost_micros,c.settlement_source,
                        c.dispatched_at_ms,r.model_id,r.provider_id FROM paid_call_ledger c
                        JOIN budget_reservations r ON r.reservation_id=c.parent_reservation_id LIMIT ?"""
                    for identity,state,tokens,cost,basis,at,model,provider in db.execute(query,(MAX_NATIVE_ROWS+1,)):
                        record=native_record('fabrica-paid',identity,model,provider,at/1000 if number(at) else None,
                            total_tokens=tokens if number(tokens) else None,actual_usd=cost/1e6 if state=='settled' and number(cost) else None,
                            actual_source=label(basis),state=label(state),partial=state!='settled')
                        store.put('native','paid:'+record['id'],record,[state=='settled',at or 0])
                if 'budget_reservations' in tables:
                    for identity,model,provider,state,at,seconds in db.execute('SELECT reservation_id,model_id,provider_id,state,dispatched_at_ms,actual_seconds FROM budget_reservations LIMIT ?',(MAX_NATIVE_ROWS+1,)):
                        start=at/1000 if number(at) else None
                        end=start+seconds if start is not None and number(seconds) else None
                        record=native_record('fabrica-task',identity,model,provider,start,start=start,end=end,points=[],state=label(state),partial=state!='settled')
                        store.put('native','task:'+record['id'],record,[state=='settled',at or 0])
        except (OSError,sqlite3.Error,ValueError,KeyError,TypeError):errors.append('fabrica_read_error')
    return found

def filtered_report(store,rates,query,collection):
    lower,upper=query['from_ms']/1000,query['to_ms']/1000
    if not 0<=lower<upper or upper-lower>366*86400:raise ValueError('Invalid range')
    source,provider,model=(query.get(k) or '' for k in ('source','provider','model'))
    groups={};details=[];seen=set();seen_intervals=set();all_sources=set();all_providers=set();all_models=set();untimed=0
    def eligible(s,p,m):
        all_sources.add(s);all_providers.add(p);all_models.add(m)
        return (not source or source==s) and (not provider or provider==p) and (not model or model==m)
    def group(s,p,m):
        key=(s,p,m)
        if key not in groups:
            groups[key]={'source':s,'provider':p,'model':m,'records':set(),'tokens':{k:None for k in TOKEN_KEYS},'actual_usd':None,'actual_sources':set(),'estimate_low_usd':None,'estimate_high_usd':None,'events':0,'unknown_cost_events':0,'unpriced_events':0,'cache_write_unknown_events':0,'elapsed_seconds':None,'active_seconds':None,'unknown_active_records':0,'partial_records':set(),'pricing':None}
        return groups[key]
    def add(g,key,value):
        if value is not None:g[key]=(g[key] or 0)+value
    def add_tokens(g,counts):
        for key,value in counts.items():
            if value is not None:g['tokens'][key]=(g['tokens'][key] or 0)+value
    codex=dict(store.records('codex'))
    def family(identity):
        walked=set()
        while identity in codex and codex[identity].get('parent') and identity not in walked:
            walked.add(identity);identity=codex[identity]['parent']
        return identity
    for identity,record in codex.items():
        if not source or source=='codex':untimed+=record.get('untimed_usage',0)
        models={v['model'] for v in record['usage']}|{v['model'] for v in record['timeline']}
        for name in models:
            if not eligible('codex','codex',name):continue
            events=[v for v in record['usage'] if v['model']==name and lower<=v['at']<upper]
            # A copied fork timeline does not identify when independent activity began.
            points=[] if record.get('parent') else record['timeline']
            start=points[0]['at'] if points else None;end=points[-1]['at'] if points else None
            elapsed,active=spans([v['at'] for v in points],start,end,lower,upper)
            # Model-scoped intervals use the model at the left endpoint; never multiply a session span per model.
            intervals=[]
            for a,b in zip(points,points[1:]):
                key=(family(identity),a['at'],b['at'],a['model'])
                if a['model']==name and key not in seen_intervals:
                    seen_intervals.add(key);intervals.append((a['at'],b['at']))
            model_active=sum(clipped(a,b,lower,upper) or 0 for a,b in intervals if 0<=b-a<=300) if points else None
            model_elapsed=sum(clipped(a,b,lower,upper) or 0 for a,b in intervals) if points else None
            if not events and not model_elapsed:continue
            g=group('codex','codex',name);g['records'].add(identity);g['partial_records'].add(identity)
            g['unknown_active_records']+=model_active is None
            add(g,'elapsed_seconds',model_elapsed);add(g,'active_seconds',model_active)
            low=high=None
            for event in events:
                event_key=(family(identity),event['event'])
                if event_key in seen:continue
                seen.add(event_key);g['events']+=1;g['unknown_cost_events']+=1
                inherited_unknown=bool(record.get('parent') and event is record['usage'][0] and event['event'] not in {v['event'] for v in codex.get(record['parent'],{}).get('usage',[])})
                counts=event['counts'] if not inherited_unknown else {k:None for k in TOKEN_KEYS}
                if inherited_unknown and 'fork_baseline_unknown' not in collection['errors']:collection['errors'].append('fork_baseline_unknown')
                add_tokens(g,counts)
                g['cache_write_unknown_events']+=event['counts']['cache_write_input_tokens'] is None
                row={k:v for k,v in counts.items() if v is not None};row['model']=name
                cost=estimate(row,rates.get(name))
                if cost.get('estimated_cost_usd') is None:g['unpriced_events']+=1
                else:
                    add(g,'estimate_low_usd',cost['estimated_cost_usd']);add(g,'estimate_high_usd',cost['estimated_cost_upper_usd'])
                    low=(low or 0)+cost['estimated_cost_usd'];high=(high or 0)+cost['estimated_cost_upper_usd']
                    g['pricing']={'source':cost['pricing_source'],'verified_on':cost['pricing_verified_on']}
            details.append({'id':identity,'source':'codex','provider':'codex','model':name,'state':'observed span','elapsed_seconds':model_elapsed,'active_seconds':model_active,'actual_usd':None,'estimate_low_usd':low,'estimate_high_usd':high,'partial':True})
    for identity,v in store.records('native'):
        if not eligible(v['kind'],v['provider'],v['model']):continue
        if v['at'] is None:untimed+=1;continue
        is_span=v['kind'] in {'fabrica-workflow','fabrica-task'}
        elapsed,active=spans(v.get('points',[]),v.get('start'),v.get('end'),lower,upper) if is_span else (None,None)
        if not (is_span and elapsed is not None and elapsed>0) and not lower<=v['at']<upper:continue
        g=group(v['kind'],v['provider'],v['model']);g['records'].add(v['id']);g['events']+=1
        if v['partial']:g['partial_records'].add(v['id'])
        add(g,'elapsed_seconds',elapsed);add(g,'active_seconds',active)
        g['unknown_active_records']+=active is None
        low=high=None
        if v['kind']=='fabrica-paid':
            add(g,'actual_usd',v['actual_usd']);g['unknown_cost_events']+=v['actual_usd'] is None
            g['actual_sources'].add(v['actual_source'])
            add_tokens(g,{'total_tokens':v['total_tokens']})
            rate=rates.get(v['model'])
            if rate and v['total_tokens'] is not None:
                prices=[p for key in ('short_per_million','long_per_million') for p in rate.get(key,[])]
                low=v['total_tokens']*min(prices)/1e6;high=v['total_tokens']*max(prices)/1e6
                add(g,'estimate_low_usd',low);add(g,'estimate_high_usd',high)
                g['pricing']={'source':rate['source'],'verified_on':rate['verified_on']}
            else:g['unpriced_events']+=1
        details.append({'id':v['id'],'source':v['kind'],'provider':v['provider'],'model':v['model'],'state':v['state'],'elapsed_seconds':elapsed,'active_seconds':active,'actual_usd':v.get('actual_usd'),'estimate_low_usd':low,'estimate_high_usd':high,'partial':v['partial']})
    for g in groups.values():g['records']=len(g['records']);g['partial_records']=len(g['partial_records']);g['actual_sources']=sorted(g['actual_sources'])
    details.sort(key=lambda v:(v['source'],v['id'],v['model']))
    return {'schema_version':SCHEMA,'collection':collection,'query':query,'groups':list(groups.values()),'details':details[:MAX_RESPONSE_ROWS],
        'detail_count':len(details),'details_truncated':len(details)>MAX_RESPONSE_ROWS,'untimed_records':untimed,
        'filters':{'sources':sorted(all_sources),'providers':sorted(all_providers),'models':sorted(all_models)}}

def execute(request):
    if request.get('schema_version')!=SCHEMA:raise ValueError('Unsupported dashboard schema')
    state=Path(request['state_dir']).resolve()
    public=Path(__file__).resolve().parents[1]
    if (public/'.git').exists() and (state==public or public in state.parents):raise ValueError('Dashboard state must be private')
    rates=request['pricing']['models']
    for rate in rates.values():
        for key in ('short_per_million','long_per_million'):
            values=rate.get(key,rate.get('short_per_million'))
            if not isinstance(values,list) or len(values)!=4 or not all(number(v) for v in values):raise ValueError('Invalid pricing snapshot')
    store=Store(state/'dashboard.sqlite3')
    try:
        if request.get('refresh'):
            store.db.execute('BEGIN IMMEDIATE')
            errors=[];imported=import_codex(store,request.get('codex_root'),errors)
            found=import_native(store,request.get('fabrica_databases',[]),errors)
            collection={'at_ms':round(time.time()*1000),'errors':sorted(set(errors)),'imported_sessions':imported,'native_databases_found':found}
            store.db.execute('INSERT OR REPLACE INTO dashboard_meta VALUES(?,?)',('collection',json.dumps(collection)));store.db.commit()
        row=store.db.execute('SELECT record FROM dashboard_meta WHERE id=?',('collection',)).fetchone()
        collection=json.loads(row[0]) if row else {'at_ms':None,'errors':['not_collected'],'imported_sessions':0,'native_databases_found':0}
        return filtered_report(store,rates,request['query'],collection)
    finally:store.close()

if __name__=='__main__':
    try:
        raw=sys.stdin.buffer.read(65537)
        if len(raw)>65536:raise ValueError('Request too large')
        result=execute(json.loads(raw.decode('utf-8-sig')))
        encoded=json.dumps(result,ensure_ascii=True,allow_nan=False)
        if len(encoded)>2*1024*1024:raise ValueError('Dashboard response exceeds bound')
        print(encoded)
    except (OSError,ValueError,TypeError,KeyError,sqlite3.Error):
        print(json.dumps({'error':'Usage collection unavailable; check configured sources and private storage.'}));sys.exit(2)
