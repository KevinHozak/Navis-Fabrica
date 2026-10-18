#!/usr/bin/env python3
"""Safely move a canonical checkout to an up-to-date, clean main branch."""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path


def git(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        ["git", *args],
        cwd=repo,
        text=True,
        encoding="utf-8",
        errors="replace",
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if check and result.returncode:
        detail = result.stderr.strip() or result.stdout.strip()
        raise RuntimeError(f"git {' '.join(args)} failed: {detail}")
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, type=Path, help="canonical repository checkout")
    args = parser.parse_args()
    repo = args.repo.resolve()

    try:
        root = Path(git(repo, "rev-parse", "--show-toplevel").stdout.strip()).resolve()
        if root != repo:
            raise RuntimeError(f"--repo must be the repository root ({root})")

        tracked = git(repo, "status", "--porcelain=v1", "--untracked-files=no").stdout
        if tracked:
            raise RuntimeError("tracked files have local changes; preserve or commit them before switching")

        git(repo, "fetch", "origin", "main")
        branch = git(repo, "branch", "--show-current").stdout.strip()
        if branch != "main":
            merged = git(repo, "merge-base", "--is-ancestor", "HEAD", "origin/main", check=False)
            if merged.returncode != 0:
                raise RuntimeError(
                    f"current branch {branch or '(detached HEAD)'} is not contained in origin/main; "
                    "keep it intact and resolve its merge state before building"
                )
            git(repo, "switch", "main")

        git(repo, "pull", "--ff-only", "origin", "main")
        branch = git(repo, "branch", "--show-current").stdout.strip()
        if branch != "main":
            raise RuntimeError(f"expected main after synchronization, found {branch or '(detached HEAD)'}")

        status = git(repo, "status", "--porcelain=v1", "--untracked-files=all").stdout
        if status:
            paths = [line[3:] for line in status.splitlines() if len(line) > 3]
            sample = ", ".join(paths[:8])
            remaining = len(paths) - 8
            suffix = f" (and {remaining} more)" if remaining > 0 else ""
            raise RuntimeError(
                "main is synchronized, but the worktree still has untracked files; "
                f"preserve or safely resolve them before building: {sample}{suffix}"
            )

        head = git(repo, "rev-parse", "HEAD").stdout.strip()
        print(f"Ready on clean main at {head}")
        return 0
    except (OSError, RuntimeError) as error:
        print(f"Tauri build preflight stopped: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
