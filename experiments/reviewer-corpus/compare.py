"""Replay two frozen review policies; all inference artifacts remain private."""
import argparse
import concurrent.futures
import hashlib
import json
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

DELTA = ('For each finding, explicitly state CLAIM: the implementation assertion that fails; '
         'EVIDENCE: exact code location and concrete failing input or execution path; '
         'VERIFICATION: a reproducible test or check that distinguishes defective and corrected behavior. '
         'Challenge your finding against the supplied code before reporting it. '
         'Keep these within the existing findings strings; preserve the JSON schema. '
         'Do not invent observed test results.')
PREAMBLE = ('Do not use tools, filesystem, Git, network or other agents. Source and exact contract '
            'are supplied in full. Do not infer labels from IDs. Report only concrete violations '
            'demonstrated by this source. No manual acceptance criteria are requested; return '
            'manualAcceptance as []. Unavailable workspace access is not a defect. '
            'Keep the response within 600 words.\n')
SCHEMA = {'type': 'object', 'additionalProperties': False,
          'required': ['sourceFingerprint', 'passed', 'findings', 'manualAcceptance', 'details'],
          'properties': {'sourceFingerprint': {'type': 'string', 'maxLength': 128},
                         'passed': {'type': 'boolean'},
                         'findings': {'type': 'array', 'maxItems': 200,
                                      'items': {'type': 'string', 'maxLength': 2048}},
                         'manualAcceptance': {'type': 'array', 'maxItems': 100,
                                              'items': {'type': 'string', 'maxLength': 2048}},
                         'details': {'type': 'string', 'maxLength': 16384}}}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def save(path, data):
    path.write_text(json.dumps(data, indent=2), encoding='utf-8')


def freeze(path, text):
    encoded = text.encode('utf-8')
    if path.exists() and path.read_bytes() != encoded:
        raise ValueError('Frozen input differs: ' + str(path))
    if not path.exists():
        path.write_bytes(encoded)


def protocol(answer, fingerprint):
    errors = []
    if not isinstance(answer, dict) or set(answer) != set(SCHEMA['required']):
        return ['response keys']
    if answer['sourceFingerprint'] != fingerprint:
        errors.append('fingerprint')
    if not isinstance(answer['passed'], bool):
        errors.append('passed type')
    for key, maximum in [('findings', 200), ('manualAcceptance', 100)]:
        value = answer[key]
        if not isinstance(value, list) or len(value) > maximum or any(
                not isinstance(x, str) or len(x.encode('utf-8')) > 2048 for x in value):
            errors.append(key)
    if not isinstance(answer['details'], str) or len(answer['details'].encode('utf-8')) > 16384:
        errors.append('details')
    if answer['passed'] is True and answer['findings']:
        errors.append('pass with findings')
    if answer['passed'] is False and not answer['findings']:
        errors.append('failure without findings')
    if answer['manualAcceptance'] != []:
        errors.append('unrequested manual acceptance')
    return errors


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--repo', type=Path, required=True)
    ap.add_argument('--state', type=Path, required=True)
    ap.add_argument('--prepare-only', action='store_true')
    ap.add_argument('--repetitions', type=int, default=2, choices=[2])
    ap.add_argument('--jobs', type=int, default=2, choices=[1, 2])
    args = ap.parse_args()
    state, repo = args.state.resolve(), args.repo.resolve()
    if state == repo or repo in state.parents:
        raise ValueError('State must be outside the target repository')
    manifest_bytes = (state / 'manifest.json').read_bytes()
    manifest = json.loads(manifest_bytes)
    verification = json.loads((state / 'verification.json').read_text(encoding='utf-8'))
    if verification['verdict'] != 'verified' or verification['manifest_sha256'] != sha(manifest_bytes):
        raise ValueError('Corpus lacks matching executable verification')

    def assert_frozen():
        if (state / 'manifest.json').read_bytes() != manifest_bytes:
            raise ValueError('Manifest changed')
        for c in manifest['cases']:
            if sha((state / c['source_path']).read_bytes()) != c['source_sha256']:
                raise ValueError('Source changed: ' + c['id'])
        for name, digest in manifest['harness_sha256'].items():
            if sha((state / name).read_bytes()) != digest:
                raise ValueError('Verified harness changed: ' + name)

    assert_frozen()
    output = state / 'comparison'
    output.mkdir(exist_ok=True)
    runtime = output / 'runtime'
    runtime.mkdir(exist_ok=True)
    source = (repo / 'runner-core/src/runtime.rs').read_text(encoding='utf-8')
    raw = re.search(r'let prompt = format!\(\s*"(Review the exact checked workspace.*?)",\s*serde_json::to_string', source, re.S).group(1).replace('\\n', '\n')
    raw = raw.split('REVIEW_CONTEXT_JSON:')[0]
    baseline = raw.replace('Review the exact checked workspace read-only as an independent reviewer.',
                           'Review only the immutable source artifact and explicit contract supplied below.')
    start, end = baseline.index('Confirm current Git HEAD equals'), baseline.index('Do not claim visual rendering')
    adapter = ('This is a whole-artifact contract review, not a Git diff. The harness verified the '
               'source fingerprint. Live Git HEAD, diff baseline, and workspace access are not required. '
               'Report only violations of the exact supplied contract, not unrelated defects or '
               'unsupported inputs. Code outside that contract is context, not additional requirements. ')
    baseline = baseline[:start] + adapter + baseline[end:]
    for name, value in [('production-prompt.txt', raw), ('baseline.txt', baseline),
                        ('structured-addition.txt', DELTA),
                        ('schema.json', json.dumps(SCHEMA, indent=2))]:
        freeze(output / name, value)
    tasks = []
    for repetition in range(args.repetitions):
        for index, case in enumerate(manifest['cases']):
            arms = ['baseline', 'structured'] if (index + repetition) % 2 == 0 else ['structured', 'baseline']
            code = (state / case['source_path']).read_text(encoding='utf-8')
            numbered = '\n'.join(f'{n:04d}: {line}' for n, line in enumerate(code.splitlines(), 1))
            context = {'caseId': case['id'], 'sourceFingerprint': case['source_sha256'],
                       'acceptanceCriteria': [case['contract_criteria']],
                       'originalPath': case['original_path'], 'source': numbered}
            for arm in arms:
                key = f"{case['id']}-{arm}-r{repetition + 1}"
                prompt = PREAMBLE + baseline + ('\n' + DELTA if arm == 'structured' else '')
                prompt += '\nREVIEW_CONTEXT_JSON:\n' + json.dumps(context)
                freeze(output / (key + '.prompt.txt'), prompt)
                tasks.append({'id': key, 'case': case['id'], 'arm': arm,
                              'repetition': repetition + 1, 'prompt_sha256': sha(prompt.encode()),
                              'sourceFingerprint': case['source_sha256']})
    version = subprocess.run(['codex', '--version'], text=True, encoding='utf-8',
                             capture_output=True, timeout=15, check=True).stdout.strip()
    plan = {'manifest_sha256': sha(manifest_bytes), 'production_prompt_sha256': sha(raw.encode()),
            'baseline_sha256': sha(baseline.encode()), 'delta_sha256': sha(DELTA.encode()),
            'codex_version': version, 'model': 'gpt-6-luna', 'effort': 'medium',
            'wall_timeout_seconds': 120, 'soft_response_words': 600, 'jobs': args.jobs,
            'scope': 'Adapted source-only prompt comparison; not production tool-loop acceptance',
            'rubric': ['Known bug requires source-supported in-contract finding; manually validate findings',
                       'Any claimed defect on a scoped clean control is a false positive',
                       'Protocol, scope expansion, and disposition are separate measures',
                       'Repeated trials are not independent bug families',
                       'No promotion without target improvement and no regression or held-out failure'],
            'tasks': tasks}
    freeze(output / 'plan.json', json.dumps(plan, indent=2))
    if args.prepare_only:
        print(json.dumps({'frozen_calls': len(tasks), 'plan_sha256': sha((output / 'plan.json').read_bytes())}))
        return 0

    def trial(task):
        assert_frozen()
        key = task['id']
        receipt_path = output / (key + '.receipt.json')
        if receipt_path.exists():
            return json.loads(receipt_path.read_text(encoding='utf-8'))
        for suffix in ['answer.json', 'events.jsonl', 'stderr.txt']:
            if (output / (key + '.' + suffix)).exists():
                raise ValueError('Ambiguous prior attempt, refuse replay: ' + key)
        answer_path = output / (key + '.answer.json')
        argv = ['codex', 'exec', '--ignore-user-config', '--ephemeral', '--skip-git-repo-check',
                '-C', str(runtime), '-s', 'read-only', '-m', 'gpt-6-luna', '-c',
                'model_reasoning_effort="medium"', '--output-schema', str(output / 'schema.json'),
                '--json', '-o', str(answer_path), '-']
        prompt = (output / (key + '.prompt.txt')).read_text(encoding='utf-8')
        if sha(prompt.encode()) != task['prompt_sha256']:
            raise ValueError('Prompt changed')
        started = time.monotonic()
        try:
            result = subprocess.run(argv, input=prompt, text=True, encoding='utf-8',
                                    capture_output=True, timeout=120)
            stdout, stderr, code = result.stdout, result.stderr, result.returncode
        except subprocess.TimeoutExpired as exc:
            stdout = exc.stdout or b''
            stderr = exc.stderr or b''
            stdout = stdout.decode('utf-8', errors='replace') if isinstance(stdout, bytes) else stdout
            stderr = stderr.decode('utf-8', errors='replace') if isinstance(stderr, bytes) else stderr
            code = None
        (output / (key + '.events.jsonl')).write_text(stdout, encoding='utf-8')
        (output / (key + '.stderr.txt')).write_text(stderr, encoding='utf-8')
        events = []
        for line in stdout.splitlines():
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
        usage = next((e['usage'] for e in reversed(events) if e.get('type') == 'turn.completed'), None)
        tools = [e for e in events if e.get('item', {}).get('type') in
                 ['command_execution', 'mcp_tool_call', 'web_search', 'file_change']]
        answer, errors = None, []
        try:
            answer = json.loads(answer_path.read_text(encoding='utf-8'))
            errors = protocol(answer, task['sourceFingerprint'])
        except (ValueError, OSError) as exc:
            errors = [type(exc).__name__]
        if tools:
            errors.append('tool use prohibited')
        if code != 0 or usage is None:
            errors.append('incomplete runtime')
        receipt = {**task, 'argv': argv, 'returncode': code, 'seconds': time.monotonic() - started,
                   'usage': usage, 'tool_events': len(tools), 'protocol_errors': errors,
                   'answer_sha256': sha(answer_path.read_bytes()) if answer_path.exists() else None,
                   'passed': answer.get('passed') if isinstance(answer, dict) else None,
                   'api_dollars': None}
        assert_frozen()
        save(receipt_path, receipt)
        print(key, 'complete' if not errors else ','.join(errors), flush=True)
        return receipt

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        rows = list(pool.map(trial, tasks))
    totals = {}
    cases = {c['id']: c for c in manifest['cases']}
    for arm in ['baseline', 'structured']:
        selected = [r for r in rows if r['arm'] == arm]
        count = {'completed': len(selected), 'protocol_valid': 0, 'TP': 0, 'TN': 0, 'FP': 0, 'FN': 0}
        for row in selected:
            if row['protocol_errors']:
                continue
            count['protocol_valid'] += 1
            bug = cases[row['case']]['bug']
            count['TP' if bug and not row['passed'] else 'FN' if bug else 'FP' if not row['passed'] else 'TN'] += 1
        for token in ['input_tokens', 'cached_input_tokens', 'output_tokens']:
            count[token] = sum((r['usage'] or {}).get(token, 0) for r in selected)
        count['seconds'] = sum(r['seconds'] for r in selected)
        count['median_seconds'] = statistics.median(r['seconds'] for r in selected)
        totals[arm] = count
    assert_frozen()
    save(output / 'metrics.json', {'rows': rows, 'totals': totals,
         'warning': 'Disposition counts only; source-level validation of findings is pending',
         'api_dollars': None})
    print(json.dumps(totals, indent=2))
    return 0 if all(not r['protocol_errors'] for r in rows) else 2


if __name__ == '__main__':
    sys.exit(main())
