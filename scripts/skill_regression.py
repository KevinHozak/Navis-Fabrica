#!/usr/bin/env python3
"""Run offline regression checks against Codex skill packages.

Examples:
    python Scripts/skill_regression.py
    python Scripts/skill_regression.py --root Backups/hermes-skills --json Logs/skill-regression.json
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import asdict, dataclass
from pathlib import Path
from urllib.parse import unquote, urlsplit


FRONTMATTER_RE = re.compile(r"\A---\r?\n(?P<body>.*?)\r?\n---\r?\n", re.DOTALL)
FRONTMATTER_FIELD_RE = re.compile(r"^(name|description):\s*(.+?)\s*$", re.MULTILINE)
SECRET_RE = re.compile(
    r"(?:sk-[A-Za-z0-9]{20,}|-----BEGIN (?:RSA|OPENSSH|EC|DSA) PRIVATE KEY-----|AIza[0-9A-Za-z_-]{30,})"
)
LOCAL_REFERENCE_RE = re.compile(
    r"(?<![A-Za-z0-9_./-])((?:assets|references|scripts|templates|examples)/[A-Za-z0-9_./-]+\.[A-Za-z0-9]+)"
)
MARKDOWN_LINK_RE = re.compile(r"\[[^\]]*\]\((?:<([^>]+)>|([^\s)]+))\)")


@dataclass(frozen=True)
class Finding:
    severity: str
    skill: str
    path: str
    message: str


def parse_frontmatter(text: str) -> dict[str, str] | None:
    match = FRONTMATTER_RE.match(text)
    if not match:
        return None
    return {
        key: value.strip().strip("\"'")
        for key, value in FRONTMATTER_FIELD_RE.findall(match.group("body"))
    }


def _relative_path(path: Path, root: Path) -> str:
    try:
        return str(path.relative_to(root))
    except ValueError:
        return str(path)


def check_skill(skill_file: Path, scan_root: Path) -> list[Finding]:
    findings: list[Finding] = []
    skill_name = skill_file.parent.name
    display_path = _relative_path(skill_file, scan_root)
    raw = skill_file.read_bytes()

    if raw.startswith(b"\xef\xbb\xbf"):
        findings.append(
            Finding("error", skill_name, display_path, "SKILL.md must be UTF-8 without a BOM")
        )

    try:
        # Decode with utf-8-sig after recording the BOM as a finding so the
        # remaining structural checks still run and report all defects.
        text = raw.decode("utf-8-sig")
    except UnicodeDecodeError as exc:
        findings.append(Finding("error", skill_name, display_path, f"invalid UTF-8: {exc}"))
        return findings

    metadata = parse_frontmatter(text)
    if metadata is None:
        findings.append(Finding("error", skill_name, display_path, "missing YAML frontmatter"))
    else:
        for field in ("name", "description"):
            if not metadata.get(field):
                findings.append(
                    Finding("error", skill_name, display_path, f"frontmatter is missing {field!r}")
                )
        if metadata.get("name") and metadata["name"] != skill_name:
            findings.append(
                Finding(
                    "warning",
                    skill_name,
                    display_path,
                    f"frontmatter name {metadata['name']!r} differs from directory name",
                )
            )

    if SECRET_RE.search(text):
        findings.append(
            Finding("error", skill_name, display_path, "possible hard-coded credential or private key")
        )

    references = set(LOCAL_REFERENCE_RE.findall(text))
    for angled, plain in MARKDOWN_LINK_RE.findall(text):
        link = angled or plain
        if not urlsplit(link).scheme and not link.startswith("#"):
            references.add(unquote(link.split("#", 1)[0]))
    for reference in sorted(references):
        # References containing a sentence-ending period are common in prose.
        reference = reference.rstrip(".,;:)")
        if not (skill_file.parent / reference).exists():
            findings.append(
                Finding(
                    "error",
                    skill_name,
                    display_path,
                    f"referenced local path does not exist: {reference}",
                )
            )

    return findings


def discover_skill_files(root: Path) -> list[Path]:
    if root.is_file():
        return [root] if root.name == "SKILL.md" else []
    return sorted(root.rglob("SKILL.md")) if root.exists() else []


def run_scan(roots: list[Path]) -> dict[str, object]:
    findings: list[Finding] = []
    skills_checked = 0
    for requested_root in roots:
        root = requested_root.resolve()
        files = discover_skill_files(root)
        if not files:
            findings.append(Finding("warning", "<root>", str(root), "no SKILL.md files found"))
            continue
        scan_root = root.parent if root.is_file() else root
        for skill_file in files:
            skills_checked += 1
            findings.extend(check_skill(skill_file, scan_root))

    return {
        "skills_checked": skills_checked,
        "error_count": sum(item.severity == "error" for item in findings),
        "warning_count": sum(item.severity == "warning" for item in findings),
        "findings": [asdict(item) for item in findings],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        action="append",
        type=Path,
        default=None,
        required=True,
        help="Skill directory or SKILL.md path; repeatable. Required caller-owned skill directory.",
    )
    parser.add_argument("--json", dest="json_path", type=Path, help="Write a JSON report")
    args = parser.parse_args()
    report = run_scan(args.root or [Path(__file__).resolve().parents[1] / "skills"])

    if args.json_path:
        args.json_path.parent.mkdir(parents=True, exist_ok=True)
        args.json_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    for item in report["findings"]:
        print(f"{item['severity'].upper()}: {item['path']}: {item['message']}")
    print(
        f"Checked {report['skills_checked']} skill(s); "
        f"{report['error_count']} error(s), {report['warning_count']} warning(s)."
    )
    return 1 if report["error_count"] else 0


if __name__ == "__main__":
    sys.exit(main())
