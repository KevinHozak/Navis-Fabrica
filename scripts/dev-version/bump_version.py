#!/usr/bin/env python3
"""Bump once per Tauri PR branch, then verify every managed version field."""
from __future__ import annotations
import argparse
from pathlib import Path
import re
import subprocess
from set_version import discover, read_versions, set_versions


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True, encoding="utf-8", stderr=subprocess.STDOUT).strip()


def classify(commits):
    if "BREAKING CHANGE" in commits.upper() or re.search(r"\b(?:feat|fix|refactor)(?:\([^)]*\))?!:", commits):
        return "major"
    return "minor" if re.search(r"\bfeat(?:\([^)]*\))?:", commits) else "patch"


def version_tuple(value):
    if not re.fullmatch(r"\d+\.\d+\.\d+", value):
        raise ValueError(f"Automatic bump supports stable SemVer only: {value}; use set_version for prereleases")
    return tuple(map(int, value.split(".")))


def next_version(value, level):
    numbers = list(version_tuple(value))
    pos = {"major": 0, "minor": 1, "patch": 2}[level]
    numbers[pos] += 1
    numbers[pos + 1:] = [0] * (2 - pos)
    return ".".join(map(str, numbers))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--base", default="origin/main")
    parser.add_argument("--level", choices=("major", "minor", "patch"))
    parser.add_argument("--allow-major", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    repo = args.repo.resolve()
    paths = discover(repo)
    current = read_versions(paths)
    # Use the Git root for paths even when caller selected a frontend in a monorepo.
    root = Path(git(repo, "rev-parse", "--show-toplevel")).resolve()
    texts = {key: git(repo, "show", f"{args.base}:{path.relative_to(root).as_posix()}") for key, path in paths.items()}
    base = read_versions(paths, texts)
    for label, values in (("Branch", current), ("Base", base)):
        if len(set(values.values())) != 1:
            raise SystemExit(f"{label} managed versions disagree: {values}. Use an explicit set_version target to repair.")
    if version_tuple(current["tauri"]) < version_tuple(base["tauri"]):
        raise SystemExit("Branch version is older than base; synchronize first")
    if current["tauri"] != base["tauri"]:
        print(f"Version {current['tauri']} already newer than base; verified, no additional bump")
        return 0
    level = args.level or classify(git(repo, "log", "--format=%s%n%b", f"{args.base}..HEAD"))
    if level == "major" and not args.allow_major:
        raise SystemExit("Major bump needs explicit authorization and --allow-major")
    target = next_version(current["tauri"], level)
    if not args.dry_run:
        set_versions(paths, target)
    print(f"{current['tauri']} -> {target} ({level})" + (" dry-run" if args.dry_run else " verified"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
