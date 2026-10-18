#!/usr/bin/env python3
"""Build a Tauri application without changing its declared version."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path, required=True)
    args = parser.parse_args()
    repo = args.repo.resolve()
    configs = list(repo.glob("**/src-tauri/tauri.conf.json"))
    if not configs:
        raise SystemExit("No src-tauri/tauri.conf.json found")
    config = configs[0]
    frontend = config.parent.parent
    version = json.loads(config.read_text(encoding="utf-8"))["version"]
    cargo = config.parent / "Cargo.toml"
    cargo_version = next((line.split('"')[1] for line in cargo.read_text(encoding="utf-8").splitlines() if line.startswith("version") and '"' in line), version)
    if cargo_version != version:
        raise SystemExit(f"Version mismatch: tauri.conf.json={version}, Cargo.toml={cargo_version}")
    package = frontend / "package.json"
    package_data = json.loads(package.read_text(encoding="utf-8")) if package.exists() else {}
    command = ["npm.cmd", "run", "desktop:build"] if "desktop:build" in package_data.get("scripts", {}) else ["npm.cmd", "run", "tauri", "build"]
    print(f"Building Tauri application version {version} with {' '.join(command)}")
    subprocess.run(command, cwd=frontend, check=True)
    candidates = sorted((config.parent / "target" / "release").glob("*.exe"), key=lambda path: path.stat().st_mtime, reverse=True)
    if os.name == "nt" and candidates:
        command = ["powershell", "-NoProfile", "-Command", f"(Get-Item -LiteralPath '{candidates[0]}').VersionInfo.ProductVersion"]
        embedded = subprocess.check_output(command, text=True).strip()
        if embedded and embedded != version:
            raise SystemExit(f"Executable version mismatch: declared={version}, embedded={embedded}")
    print(f"Build verified: {candidates[0] if candidates else 'bundle output; executable metadata unavailable'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
