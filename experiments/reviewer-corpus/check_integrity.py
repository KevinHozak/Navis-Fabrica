"""Check immutable-input refusal paths without inference or recompiling Rust."""
import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from compare import protocol


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--state', type=Path, required=True)
    args = parser.parse_args()
    state = args.state.resolve()
    manifest_path = state / 'manifest.json'
    original = manifest_path.read_bytes()
    manifest = json.loads(original)
    command = [sys.executable, str(Path(__file__).with_name('corpus.py')),
               '--repo', str(args.repo.resolve()), '--state', str(state)]
    result = subprocess.run(command, capture_output=True, text=True, encoding='utf-8', timeout=15)
    assert result.returncode == 2 and 'overwrite' in result.stderr
    assert manifest_path.read_bytes() == original

    with tempfile.TemporaryDirectory(prefix='integrity-', dir=state.parent) as temporary:
        target = Path(temporary).resolve()
        assert target.parent == state.parent
        paths = set(manifest['harness_sha256']) | {'manifest.json'}
        paths |= {case['source_path'] for case in manifest['cases']}
        for relative in paths:
            destination = target / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(state / relative, destination)
        changed = target / manifest['cases'][0]['source_path']
        changed.write_bytes(changed.read_bytes() + b'\n// changed after freeze\n')
        result = subprocess.run(command[:-1] + [str(target), '--verify-only'],
                                capture_output=True, text=True, encoding='utf-8', timeout=15)
        assert result.returncode == 2 and 'Frozen source changed' in result.stderr
        assert not (target / 'logs').exists(), 'Tampered source reached compilation'

    valid = {'sourceFingerprint': 'a' * 64, 'passed': True, 'findings': [],
             'manualAcceptance': [], 'details': 'Reviewed the scoped contract.'}
    assert protocol(valid, 'a' * 64) == []
    assert protocol({**valid, 'sourceFingerprint': 'b' * 64}, 'a' * 64)
    assert protocol({**valid, 'findings': ['A defect despite passed=true']}, 'a' * 64)
    assert protocol({**valid, 'manualAcceptance': ['Unrequested gate']}, 'a' * 64)
    assert protocol({**valid, 'passed': False}, 'a' * 64)
    assert protocol({**valid, 'extra': True}, 'a' * 64)
    print(json.dumps({'overwrite_refused': True, 'tampering_refused_before_compile': True,
                      'protocol_cases_passed': 6,
                      'manifest_sha256_unchanged': hashlib.sha256(original).hexdigest()}))


if __name__ == '__main__':
    main()
