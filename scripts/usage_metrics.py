"""Cost provenance and timestamp-only activity metrics; never infer billing."""
from datetime import datetime
from decimal import Decimal
import math

VERSION = 4
IDLE_SECONDS = 300


def number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) and value >= 0


def epoch(value):
    try:
        return datetime.fromisoformat(value.replace('Z', '+00:00')).timestamp()
    except (ValueError, TypeError, AttributeError, OverflowError):
        return None


def timing(timestamps, start=None, finish=None, idle_seconds=IDLE_SECONDS):
    points = sorted(set(t for t in timestamps if number(t)))
    activity_observed = bool(points)
    start = start if number(start) else (points[0] if points else None)
    finish = finish if number(finish) else (points[-1] if points else None)
    if start is None or finish is None or finish < start:
        return {'elapsed_seconds': None, 'active_seconds_estimate': None, 'idle_gap_seconds': idle_seconds}
    points = sorted(set([start, finish] + [t for t in points if start <= t <= finish]))
    return {'elapsed_seconds': round(finish - start, 3),
            'active_seconds_estimate': round(sum(b-a for a, b in zip(points, points[1:]) if b-a <= idle_seconds), 3) if activity_observed else None,
            'idle_gap_seconds': idle_seconds,
            'method': 'Span between observed endpoints; active sums adjacent event gaps <= idle threshold. Not human work time.'}


def estimate(row, rate):
    if not rate:
        return {'estimated_cost_usd': None, 'reason': 'No verified exact-model pricing'}
    required = ('input_tokens', 'output_tokens')
    if not all(number(row.get(k)) for k in required):
        return {'estimated_cost_usd': None, 'reason': 'Input/output token breakdown unavailable'}
    inp, out = row['input_tokens'], row['output_tokens']
    cached, writes = row.get('cached_input_tokens', 0), row.get('cache_write_input_tokens', 0)
    if not number(cached) or not number(writes) or cached + writes > inp:
        return {'estimated_cost_usd': None, 'reason': 'Invalid token breakdown'}
    def price(values):
        counts = [inp-cached-writes, cached, writes, out]
        return float(sum(Decimal(str(n)) * Decimal(str(r)) for n, r in zip(counts, values)) / Decimal(1000000))
    low = price(rate['short_per_million'])
    high = price(rate.get('long_per_million', rate['short_per_million']))
    return {'estimated_cost_usd': round(low, 9), 'estimated_cost_upper_usd': round(high, 9),
            'basis': 'Current-rate reconstruction; short/long context range; excludes tool fees and service-tier uplifts',
            'pricing_source': rate['source'], 'pricing_verified_on': rate['verified_on'],
            'cache_write_count_known': 'cache_write_input_tokens' in row}


def cost_summary(rows, rates):
    priced = [estimate(r, rates.get(r.get('model'))) for r in rows]
    known = [r for r in priced if r.get('estimated_cost_usd') is not None]
    return {'actual_cost_usd': None, 'actual_cost_source': 'Not reported by Codex subscription logs',
            'estimated_cost_usd': round(sum(r['estimated_cost_usd'] for r in known), 9) if known else None,
            'estimated_cost_upper_usd': round(sum(r['estimated_cost_upper_usd'] for r in known), 9) if known else None,
            'priced_events': len(known), 'unpriced_events': len(rows)-len(known),
            'basis': 'API-equivalent estimate, not a Codex subscription bill',
            'pricing_sources': sorted(set(r['pricing_source'] for r in known))}
