"""Aggregate human-validated findings; never infer correctness from keywords."""
import argparse
import hashlib
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state', type=Path, required=True)
    parser.add_argument('--assessments', type=Path, required=True)
    args = parser.parse_args()
    state = args.state.resolve()
    manifest = json.loads((state / 'manifest.json').read_text(encoding='utf-8'))
    cases = {c['id']: c for c in manifest['cases']}
    metrics = json.loads((state / 'comparison/metrics.json').read_text(encoding='utf-8'))
    assessments = json.loads(args.assessments.read_text(encoding='utf-8'))
    assert set(assessments['trials']) == {r['id'] for r in metrics['rows']}
    totals = {arm: {'bug_trials': 0, 'supported_detections': 0, 'clean_trials': 0,
                    'clean_false_positives': 0, 'unsupported_findings': 0,
                    'expected_defect_observations': 0, 'covered_defect_observations': 0}
              for arm in ['baseline', 'structured']}
    outcomes = {}
    for row in metrics['rows']:
        assert not row['protocol_errors'], 'Handle invalid trials separately before scoring'
        case = cases[row['case']]
        assessment = assessments['trials'][row['id']]
        answer_bytes = (state / 'comparison' / (row['id'] + '.answer.json')).read_bytes()
        assert hashlib.sha256(answer_bytes).hexdigest() == assessment['answer_sha256']
        answer = json.loads(answer_bytes)
        supported = assessment['supported_finding_indices']
        unsupported = assessment['unsupported_finding_indices']
        assert len(supported + unsupported) == len(set(supported + unsupported))
        assert set(supported + unsupported) == set(range(len(answer['findings'])))
        assert not supported or case['bug'], 'A scoped clean control cannot have a supported defect'
        covered = assessment['covered_contract_tests']
        assert len(covered) == len(set(covered))
        harness_source = (state / 'harness/src' / (case['id'] + '.rs')).read_text(encoding='utf-8')
        for name in covered:
            assert name.startswith('contract_') and ('fn ' + name + '(') in harness_source
        assert bool(covered) == bool(supported)
        total = totals[row['arm']]
        total['unsupported_findings'] += len(unsupported)
        if case['bug']:
            total['bug_trials'] += 1
            total['supported_detections'] += bool(supported) and answer['passed'] is False
            total['expected_defect_observations'] += case['expected_contract_failures']
            total['covered_defect_observations'] += len(covered)
            correct = bool(supported) and answer['passed'] is False
        else:
            total['clean_trials'] += 1
            correct = answer['passed'] is True and not answer['findings']
            total['clean_false_positives'] += not correct
        outcomes[row['id']] = correct
    result = {'human_validated': True, 'independent_blind_judge': False,
              'rubric': 'Supported source-backed defects and executable specimen controls; withdrawn and satisfied claims in findings remain unsupported.',
              'totals': totals, 'trial_correct': outcomes,
              'limitations': ['One evaluator manually validated findings; no independent blinded adjudication.',
                              'Eight cases from three historical fixes; SVG pairs share code.',
                              'Two repetitions are not independent bug families.']}
    (state / 'comparison/validated-scores.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(json.dumps(totals, indent=2))


if __name__ == '__main__':
    main()
