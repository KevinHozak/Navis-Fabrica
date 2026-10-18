#!/usr/bin/env python3
"""Install the reusable project workflow harness into a target repository."""

from __future__ import annotations

import argparse
from pathlib import Path
from urllib.parse import quote


SCRIPT_DIR = Path(__file__).resolve().parent
TEMPLATE_ROOT = SCRIPT_DIR.parent / "templates" / "project-workflow-harness"


def target_files() -> list[tuple[Path, Path]]:
    return [
        (TEMPLATE_ROOT / "AGENTS.md.template", Path("AGENTS.md")),
        (TEMPLATE_ROOT / "GOALS.md.template", Path("GOALS.md")),
        (TEMPLATE_ROOT / "PLANS.md.template", Path("PLANS.md")),
        (TEMPLATE_ROOT / "PROMPTS.md.template", Path("PROMPTS.md")),
        (TEMPLATE_ROOT / "harness" / "build" / "phase-template.md", Path("harness/build/phase-template.md")),
        (TEMPLATE_ROOT / "harness" / "context" / "README.md", Path("harness/context/README.md")),
        (TEMPLATE_ROOT / "harness" / "code_review" / "README.md", Path("harness/code_review/README.md")),
        (TEMPLATE_ROOT / "harness" / "build-log.md", Path("harness/build-log.md")),
    ]


def install(target: Path, project_name: str, force: bool) -> list[Path]:
    if not TEMPLATE_ROOT.exists():
        raise FileNotFoundError(f"Template root not found: {TEMPLATE_ROOT}")
    target.mkdir(parents=True, exist_ok=True)
    written: list[Path] = []
    for source, relative_target in target_files():
        destination = target / relative_target
        if destination.exists() and not force:
            continue
        destination.parent.mkdir(parents=True, exist_ok=True)
        content = source.read_text(encoding="utf-8-sig")
        content = content.replace("[PROJECT NAME]", project_name)
        content = content.replace("[FABRICA ROOT]", quote(SCRIPT_DIR.parent.as_posix(), safe="/:"))
        destination.write_text(content, encoding="utf-8-sig")
        written.append(destination)
    return written


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", type=Path, required=True, help="Project directory to scaffold")
    parser.add_argument("--project-name", required=True, help="Human-readable project name")
    parser.add_argument("--force", action="store_true", help="Overwrite existing harness files")
    args = parser.parse_args()
    written = install(args.target.resolve(), args.project_name, args.force)
    if written:
        print(f"Created {len(written)} harness file(s) in {args.target.resolve()}")
        for path in written:
            print(f"- {path}")
    else:
        print(f"No files changed in {args.target.resolve()}; existing files were preserved.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
