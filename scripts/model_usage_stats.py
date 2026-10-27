"""Local, metadata-only usage ledger for Codex and Fabrica. No network calls."""
import argparse
import json
import os
from pathlib import Path
import re
import sqlite3
import time
import uuid
from datetime import datetime
from contextlib import closing
from usage_metrics import VERSION, cost_summary, epoch, estimate, number, timing

ROOT = Path(__file__).resolve().parents[1]
DATA = Path(os.environ.get('FABRICA_USAGE_DATA', str(Path(os.environ.get('LOCALAPPDATA', str(Path.home() / '.local/state'))) / 'navis-fabrica/usage')))


def read_codex(path, dashboard=False):
    model = 'unknown'
    previous = {}
    rows = []
    skills = set()
    timestamps = []
    timeline = []
    identity, parent = path.stem, None
    if path.stat().st_size > 64 * 1024 * 1024:
        raise ValueError("Session exceeds import bound")
    with path.open(encoding='utf-8-sig') as stream:
        while line := stream.readline(2 * 1024 * 1024 + 1):
            if len(line.encode('utf-8')) > 2 * 1024 * 1024:
                raise ValueError("Session event exceeds import bound")
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue  # A live log can end with an incomplete line.
            payload = event.get('payload', {})
            if event.get('type') == 'session_meta':
                identity = payload.get('id') or identity
                parent = payload.get('forked_from_id') or payload.get('parent_session_id')
            if event.get('type') in ('event_msg', 'response_item'):
                timestamp = None
                if dashboard:
                    try:
                        if datetime.fromisoformat(event['timestamp'].replace('Z', '+00:00')).tzinfo is not None:timestamp=epoch(event['timestamp'])
                    except (ValueError, TypeError, AttributeError, KeyError, OSError):timestamp=None
                else:timestamp=epoch(event.get('timestamp'))
                if timestamp is not None:
                    timestamps.append(timestamp)
                    timeline.append({'at': timestamp, 'model': model})
            if event.get('type') == 'turn_context':
                model = payload.get('model') or 'unknown'
            if not dashboard and event.get('type') == 'response_item' and payload.get('type') == 'function_call':
                # A skill file read is evidence of loading, not proof of invocation.
                skills.update(re.findall(r'[/\\]([^/\\"\s]+)[/\\]SKILL\.md', payload.get('arguments', '')))
            if event.get('type') != 'event_msg' or payload.get('type') != 'token_count':
                continue
            total = (payload.get('info') or {}).get('total_token_usage')
            if not total:
                continue
            if not isinstance(total, dict) or any(not number(v) for k,v in total.items() if k.endswith('_tokens')):
                raise ValueError("Invalid usage counters")
            # Cumulative counters are snapshots, never sum snapshots directly.
            delta = {k: max(0, total.get(k, 0) - previous.get(k, 0)) for k in
                     ('input_tokens', 'cached_input_tokens', 'cache_write_input_tokens', 'output_tokens', 'reasoning_output_tokens')}
            if total.get('total_tokens', 0) < previous.get('total_tokens', 0):
                delta = {k: total.get(k, 0) for k in delta}
            previous = total
            if any(delta.values()):
                row = {'source': 'codex', 'model': model, 'timestamp': event.get('timestamp'), **delta}
                if dashboard:
                    row['snapshot'] = {k: total.get(k) for k in delta}
                if 'cache_write_input_tokens' not in total:
                    row.pop('cache_write_input_tokens', None)
                rows.append(row)
    result = {'usage': rows, 'skill_files_observed': sorted(skills), 'timing': timing(timestamps)}
    if dashboard:
        result.update(session_id=identity, parent_id=parent, timeline=timeline)
    return result


def collect(days=7, codex_home=None, fabrica_db=None, output=None, pricing=None):
    destination = Path(output) if output else DATA
    if destination.resolve() == ROOT or ROOT in destination.resolve().parents:
        raise ValueError("Usage data must stay outside the public Fabrica checkout")
    destination.mkdir(parents=True, exist_ok=True)
    home = Path(codex_home) if codex_home else Path.home() / '.codex'
    db = sqlite3.connect(destination / 'usage.sqlite3', timeout=15)
    db.execute('CREATE TABLE IF NOT EXISTS sources (path TEXT PRIMARY KEY, stamp TEXT, record TEXT)')
    cutoff = time.time() - days * 86400
    errors = []
    rates = json.loads(Path(pricing or os.environ.get('FABRICA_USAGE_PRICING') or ROOT / 'config' / 'model-pricing.json').read_text(encoding='utf-8-sig'))['models']
    for folder in ('sessions', 'archived_sessions'):
        for path in (home / folder).rglob('*.jsonl'):
            stat = path.stat()
            if stat.st_mtime < cutoff:
                continue
            stamp = f'{VERSION}:{stat.st_mtime_ns}:{stat.st_size}'
            old = db.execute('SELECT stamp FROM sources WHERE path=?', (str(path),)).fetchone()
            if old and old[0] == stamp:
                continue
            try:
                record = read_codex(path)
                db.execute('INSERT OR REPLACE INTO sources VALUES (?,?,?)', (str(path), stamp, json.dumps(record)))
            except (OSError, ValueError, TypeError) as exc:
                errors.append(f'{path.name}: {type(exc).__name__}')
    db.commit()
    groups = {}
    skill_sessions = {}
    sessions = []
    all_usage = []
    for path, raw in db.execute('SELECT path, record FROM sources'):
        record = json.loads(raw)
        all_usage.extend(record['usage'])
        sessions.append({'session': Path(path).stem, 'timing': record.get('timing'),
                         'cost': cost_summary(record['usage'], rates)})
        for skill in record['skill_files_observed']:
            skill_sessions[skill] = skill_sessions.get(skill, 0) + 1
        seen = set()
        for row in record['usage']:
            model = row['model']
            g = groups.setdefault(model, dict(sessions=0, usage_events=0, input_tokens=0,
                cached_input_tokens=0, output_tokens=0, reasoning_output_tokens=0))
            if model not in seen:
                g['sessions'] += 1
                seen.add(model)
            g['usage_events'] += 1
            for key in ('input_tokens', 'cached_input_tokens', 'output_tokens', 'reasoning_output_tokens'):
                g[key] += row[key]
    db.close()
    for model, group in groups.items():
        group['cost'] = cost_summary([r for r in all_usage if r['model'] == model], rates)
    candidates = [Path(fabrica_db)] if fabrica_db else [
        Path(os.environ.get(base, str(Path.home()))) / 'com.navis.fabrica' / 'workflow-runs.sqlite3'
        for base in ('APPDATA', 'LOCALAPPDATA')]
    if not fabrica_db:
        candidates.extend(Path(os.environ.get(base, str(Path.home()))) / 'com.navis.fabrica' / 'runner' / 'jobs.sqlite3'
                          for base in ('APPDATA', 'LOCALAPPDATA'))
    fabrica = {'databases_found': [], 'runs': [], 'usage_signals': [], 'paid_calls': [], 'routing_estimates': [], 'runner_tasks': []}
    for path in candidates:
        if not path.is_file():
            continue
        try:
            with closing(sqlite3.connect(path.resolve().as_uri() + '?mode=ro', uri=True)) as source:
                tables = {r[0] for r in source.execute("SELECT name FROM sqlite_master WHERE type='table'")}
                for table, key in [('workflow_runs', 'runs'), ('usage_signals', 'usage_signals')]:
                    if table not in tables:
                        continue
                    for (raw,) in source.execute(f'SELECT record_json FROM {table}'):
                        value = json.loads(raw)
                        allowed = ('id', 'provider', 'modelId', 'state', 'startedAtMs', 'finishedAtMs', 'latencyMs') if key == 'runs' else ('id', 'providerId', 'modelId', 'category', 'metric', 'value', 'unit', 'source', 'confidence', 'collectedAtMs')
                        item = {k: value.get(k) for k in allowed}
                        if key == 'runs':
                            start, finish = value.get('startedAtMs'), value.get('finishedAtMs')
                            timestamps = [e.get('timestampMs') / 1000 for e in value.get('events', []) if number(e.get('timestampMs'))]
                            item['timing'] = timing(timestamps, start / 1000 if number(start) else None,
                                finish / 1000 if number(finish) else None)
                            item['timing']['finished'] = number(finish)
                        fabrica[key].append(item)
                if {'paid_call_ledger', 'budget_reservations'} <= tables:
                    query = '''SELECT c.call_id,c.state,c.actual_tokens,c.actual_cost_micros,
                        c.settlement_source,c.dispatched_at_ms,r.model_id,r.provider_id
                        FROM paid_call_ledger c JOIN budget_reservations r
                        ON r.reservation_id=c.parent_reservation_id'''
                    for call_id, state, tokens, micros, settlement, started, model, provider in source.execute(query):
                        actual = micros / 1000000 if state == 'settled' and number(micros) else None
                        rate = rates.get(model)
                        # Runner ledger has total tokens only: provide bounds, never invent an input/output split.
                        bounds = [v for k in ('short_per_million', 'long_per_million') for v in rate.get(k, [])] if rate else []
                        fabrica['paid_calls'].append({'id': call_id, 'model': model, 'provider': provider,
                            'state': state, 'actual_cost_usd': actual, 'actual_cost_source': settlement,
                            'estimated_cost_usd': None,
                            'estimated_cost_lower_usd': tokens * min(bounds) / 1000000 if bounds and number(tokens) else None,
                            'estimated_cost_upper_usd': tokens * max(bounds) / 1000000 if bounds and number(tokens) else None,
                            'estimate_basis': 'Token-only range; input/output/cache split unavailable; excludes fees',
                            'pricing_source': rate['source'] if rate else None})
                if 'budget_reservations' in tables:
                    for task_id, model, state, started, seconds in source.execute(
                            'SELECT reservation_id,model_id,state,dispatched_at_ms,actual_seconds FROM budget_reservations'):
                        fabrica['runner_tasks'].append({'id': task_id, 'model': model, 'state': state,
                            'started_at_ms': started, 'elapsed_seconds': seconds if number(seconds) else None,
                            'active_seconds_estimate': None,
                            'time_source': 'Runner settlement elapsed seconds; no event timeline for active time'})
                if 'jev_routing_history' in tables:
                    for (raw,) in source.execute('SELECT record_json FROM jev_routing_history'):
                        value = json.loads(raw)
                        item = {k: value.get(k) for k in
                            ('evaluatedAtMs', 'source', 'selectedModelId', 'inputTokens', 'outputTokens', 'estimatedCostUsd')}
                        item['id'] = str(value.get('evaluatedAtMs')) + ':' + str(value.get('source'))
                        fabrica['routing_estimates'].append(item)
                fabrica['databases_found'].append(str(path))
        except (sqlite3.Error, ValueError) as exc:
            errors.append(f'Fabrica: {type(exc).__name__}')
    # Retain observed Fabrica records after its own bounded history is pruned.
    with closing(sqlite3.connect(destination / 'usage.sqlite3', timeout=15)) as ledger:
        ledger.execute('CREATE TABLE IF NOT EXISTS fabrica_records (kind TEXT, id TEXT, record TEXT, PRIMARY KEY(kind,id))')
        for kind in ('runs', 'usage_signals', 'paid_calls', 'routing_estimates', 'runner_tasks'):
            for item in fabrica[kind]:
                ledger.execute('INSERT OR REPLACE INTO fabrica_records VALUES (?,?,?)', (kind, str(item['id']), json.dumps(item)))
            fabrica[kind] = [json.loads(r[0]) for r in ledger.execute('SELECT record FROM fabrica_records WHERE kind=?', (kind,))]
        ledger.commit()
    settled_costs = [c['actual_cost_usd'] for c in fabrica['paid_calls'] if c['actual_cost_usd'] is not None]
    fabrica['cost'] = {'actual_cost_usd': sum(settled_costs) if settled_costs else None,
                       'actual_cost_calls': len(settled_costs),
                       'unknown_cost_calls': len(fabrica['paid_calls']) - len(settled_costs),
                       'basis': 'Paid-call settlements only; excludes parent reservations to prevent double counting',
                       'estimated_cost_usd': None,
                       'estimate_basis': 'See per-call ranges and separate routing estimates; different coverage, never added to actual cost'}
    active_runs = [r['timing']['active_seconds_estimate'] for r in fabrica['runs'] if r['timing']['active_seconds_estimate'] is not None]
    fabrica['time'] = {'elapsed_run_seconds': sum(r['timing']['elapsed_seconds'] or 0 for r in fabrica['runs']) if fabrica['runs'] else None,
                       'active_run_seconds_estimate': sum(active_runs) if active_runs else None,
                       'runs_missing_active_time': len(fabrica['runs']) - len(active_runs),
                       'basis': 'Sum of workflow spans; concurrent runs overlap; runner_tasks are separate and not added'}
    result = {'collected_at': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
        'schema_version': VERSION, 'codex_sessions': sessions,
        'pricing_snapshot': rates,
        'codex_cost': cost_summary(all_usage, rates),
        'codex_time': {'elapsed_session_seconds': sum((s['timing'] or {}).get('elapsed_seconds') or 0 for s in sessions),
                       'active_session_seconds_estimate': sum((s['timing'] or {}).get('active_seconds_estimate') or 0 for s in sessions),
                       'basis': 'Sum of session spans, including concurrent sessions; not unique wall-clock or human time'},
        'codex_models': groups, 'skill_file_observed_sessions': skill_sessions,
        'fabrica': fabrica, 'errors': errors,
        'notes': ['Initial import scans logs modified within the requested days; ledger retains imported history.',
                  'Skill file reads indicate loading, not invocation or per-skill token attribution.',
                  'Cached input is included in input; reasoning output is included in output.',
                  'Estimated costs use a dated current-price snapshot, not historical invoices. Unknown model rates stay unknown.',
                  'Codex and Fabrica sources are separate; do not sum overlapping runs.']}
    temporary = destination / ('latest.' + uuid.uuid4().hex + '.tmp')
    temporary.write_text(json.dumps(result, indent=2), encoding='utf-8-sig')
    temporary.replace(destination / 'latest.json')
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--days', type=int, default=7)
    parser.add_argument('--codex-home')
    parser.add_argument('--fabrica-db')
    parser.add_argument('--output')
    parser.add_argument('--pricing', help='JSON exact-model pricing snapshot')
    args = parser.parse_args()
    result = collect(args.days, args.codex_home, args.fabrica_db, args.output, args.pricing)
    print(json.dumps({'models': len(result['codex_models']), 'fabrica_runs': len(result['fabrica']['runs']), 'errors': result['errors']}))
