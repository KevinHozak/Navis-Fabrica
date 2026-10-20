"""Provider-independent Tauri build using Fabrica's shared version protocol."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
import tomllib

import prepare_version as versions
from version_workflow import native_path


def build_command(package, profile):
    manager = package.get('packageManager', 'npm').split('@')[0]
    if manager not in ('npm', 'pnpm', 'yarn', 'bun'):
        raise ValueError('Unsupported package manager; use npm, pnpm, yarn, or bun.')
    executable = manager + ('.cmd' if os.name == 'nt' else '')
    scripts = package.get('scripts', {})
    if 'desktop:build' in scripts:
        args = ['run', 'desktop:build']
        if profile == 'debug':
            args += (['--'] if manager == 'npm' else []) + ['--debug']
    elif 'tauri' in scripts:
        args = ['run', 'tauri'] + (['--'] if manager == 'npm' else []) + ['build']
        if profile == 'debug':
            args.append('--debug')
    else:
        raise ValueError('The project must declare desktop:build or tauri in package.json.')
    return [executable, *args]


def metadata(path):
    env = os.environ.copy()
    env['FABRICA_BUILD_ARTIFACT'] = str(path)
    command = "$v=(Get-Item -LiteralPath $env:FABRICA_BUILD_ARTIFACT).VersionInfo; @{fileVersion=$v.FileVersion;productVersion=$v.ProductVersion}|ConvertTo-Json -Compress"
    output = subprocess.check_output(['powershell.exe', '-NoProfile', '-NonInteractive', '-Command', command], env=env, timeout=15)
    return json.loads(output)


def run_build(command, repo):
    subprocess.run(command, cwd=repo, check=True)


def smoke(path):
    with tempfile.TemporaryDirectory(prefix='fabrica-build-smoke-', ignore_cleanup_errors=True) as folder:
        root = Path(folder).resolve()
        if not root.is_relative_to(Path(tempfile.gettempdir()).resolve()) or not root.name.startswith('fabrica-build-smoke-'):
            raise ValueError('Unsafe isolated startup directory.')
        env = os.environ.copy()
        for name in ('APPDATA', 'LOCALAPPDATA'):
            env[name] = str(root / name)
            Path(env[name]).mkdir()
        startup = subprocess.STARTUPINFO()
        startup.dwFlags |= subprocess.STARTF_USESHOWWINDOW
        startup.wShowWindow = 0
        process = subprocess.Popen([str(path)], cwd=root, env=env, startupinfo=startup)
        try:
            time.sleep(8)
            if process.poll() is not None:
                raise ValueError('The built app exited during its isolated startup check.')
        finally:
            if process.poll() is None:
                subprocess.run(['taskkill.exe', '/PID', str(process.pid), '/T', '/F'], capture_output=True, timeout=15)
                process.wait(timeout=10)


def phase(step, label):
    print(f'BUILD_STEP:{step}/6:{label}', flush=True)


def build(repo, profile, state_dir):
    repo = Path(native_path(repo)).resolve(strict=True)
    if profile not in ('release', 'debug'):
        raise ValueError('Choose release or debug.')
    config_path = repo / 'src-tauri' / 'tauri.conf.json'
    config = json.loads(config_path.read_text(encoding='utf-8'))
    package = json.loads((repo / 'package.json').read_text(encoding='utf-8'))
    command = build_command(package, profile)
    cargo = tomllib.loads((repo / 'src-tauri' / 'Cargo.toml').read_text(encoding='utf-8'))
    binary = cargo['package'].get('default-run', cargo['package']['name'])
    if not re.fullmatch(r'[A-Za-z0-9_-]+', binary):
        raise ValueError('The Cargo binary name is unsupported.')
    # HEAD is inspected locally by the shared protocol on first use; existing
    # preparations and source profiles always take precedence. No Git sync.
    phase(1, 'Preparing version')
    ctx = versions.context(repo, native_path(state_dir))
    prepared = versions.prepare(ctx, base='HEAD')
    version = prepared['version']
    print(f'Version {version}: {prepared["action"]}.', flush=True)
    phase(2, f'Building {profile} artifacts')
    print(f'Building {profile.upper()} v{version}: {" ".join(command)}', flush=True)
    run_build(command, repo)
    phase(3, 'Verifying executable and installer versions')
    # Only the expected profile/binary is accepted, never the newest arbitrary exe.
    config = json.loads(config_path.read_text(encoding='utf-8'))
    target = config.get('build', {}).get('target')
    output = repo / 'src-tauri' / 'target'
    if target:
        output /= target
    output /= profile
    artifact = output / (binary + '.exe')
    if not artifact.is_file() or not artifact.stat().st_size:
        raise ValueError(f'The build did not produce the expected executable: {artifact}')
    artifacts = [artifact]
    bundle = config.get('bundle', {})
    targets = bundle.get('targets', [])
    if isinstance(targets, str):
        targets = [targets]
    if bundle.get('active', True) and ('nsis' in targets or 'all' in targets):
        installers = list((output / 'bundle' / 'nsis').glob(f'*_{version}_*-setup.exe'))
        if len(installers) != 1:
            raise ValueError('Expected exactly one NSIS installer for the declared version.')
        artifacts += installers
    for item in artifacts:
        embedded = metadata(item)
        if embedded.get('fileVersion') != version or embedded.get('productVersion') != version:
            raise ValueError(f'Embedded version mismatch: {item}')
    phase(4, 'Checking isolated startup')
    smoke(artifact)
    phase(5, 'Saving build receipt')
    versions.record(ctx, token=prepared['token'], artifacts=artifacts)
    print('NAVIS_BUILD_ARTIFACT:' + json.dumps(dict(profile=profile, path=str(artifact), fileVersion=version, productVersion=version)), flush=True)
    phase(6, 'Build completed and verified')
    print(f'{profile.upper()} build verified; version receipt saved.', flush=True)
    return artifact


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--repo', required=True)
    parser.add_argument('--profile', choices=('release', 'debug'), default='release')
    parser.add_argument('--state-dir', required=True)
    args = parser.parse_args()
    try:
        build(args.repo, args.profile, args.state_dir)
    except Exception as error:
        print(f'Build failed: {error}', flush=True)
        raise SystemExit(1)
