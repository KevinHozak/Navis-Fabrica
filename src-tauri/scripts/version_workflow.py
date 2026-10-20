"""Shared version entry point for commit review and deterministic Push."""
import argparse
import json
import subprocess
from pathlib import Path
import prepare_version as versions


def native_path(value):
    value = str(value)
    if value.startswith('\\\\?\\UNC\\'):
        return '\\\\' + value[8:]
    if value.startswith('\\\\?\\'):
        return value[4:]
    return value


def context(repo, state_dir):
    return versions.context(native_path(repo), native_path(state_dir))


def initial_version(ctx):
    try:
        versions.git(ctx['app'], 'rev-parse', '--verify', 'HEAD')
        return None
    except subprocess.CalledProcessError:
        # A named unborn branch keeps its declared initial version. There is
        # no prior app content to compare and no publication receipt to invent.
        versions.git(ctx['app'], 'symbolic-ref', '--quiet', 'HEAD')
        return versions.synchronized(versions.setter.read_versions(ctx['paths']))


def prepare(repo, state_dir):
    repo = Path(native_path(repo)).resolve()
    if not (repo / 'src-tauri' / 'tauri.conf.json').is_file():
        return {'supported': False}
    ctx = context(repo, state_dir)
    initial = initial_version(ctx)
    if initial is not None:
        return dict(supported=True, version=initial, action='initial-version')
    result = versions.prepare(ctx, base='HEAD')
    return dict(supported=True, version=result['version'], action=result['action'])


def inspect(repo, state_dir, push_only=False):
    ctx = context(repo, state_dir)
    initial = initial_version(ctx)
    if initial is not None:
        return dict(ready=False, token=None, version=initial)
    state = versions.load(ctx)
    if state and state['pending'] is None:
        fingerprint, declared = versions.snapshot(ctx, state['profile'])
        prepared = state['prepared']
        if fingerprint == prepared['fingerprint'] and versions.synchronized(declared) == prepared['version']:
            return dict(ready=True, token=prepared['token'], version=prepared['version'])
    if push_only:
        # Publishing existing commits never creates a version-changing commit.
        return dict(ready=False, token=None, version=json.loads(ctx['paths']['tauri'].read_text(encoding='utf-8'))['version'])
    raise ValueError('Version preparation is stale or missing. Refresh the commit review before confirming.')


def record_push(repo, state_dir, commit, remote_head):
    ctx = context(repo, state_dir)
    result = inspect(repo, state_dir)
    versions.record(ctx, token=result['token'], commit=commit, remote_head=remote_head)
    return dict(version=result['version'], recorded=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--repo', required=True)
    parser.add_argument('--state-dir', required=True)
    parser.add_argument('--action', choices=('prepare', 'inspect', 'record-push'), required=True)
    parser.add_argument('--push-only', action='store_true')
    parser.add_argument('--commit')
    parser.add_argument('--remote-head')
    args = parser.parse_args()
    try:
        if args.action == 'prepare':
            result = prepare(args.repo, args.state_dir)
        elif args.action == 'inspect':
            result = inspect(args.repo, args.state_dir, args.push_only)
        else:
            result = record_push(args.repo, args.state_dir, args.commit, args.remote_head)
        print(json.dumps(result), flush=True)
    except Exception as error:
        print(json.dumps({'error': str(error)}), flush=True)
        raise SystemExit(1)
