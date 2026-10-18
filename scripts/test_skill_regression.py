#!/usr/bin/env python3
"""Self-tests for Scripts/skill_regression.py."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from skill_regression import check_skill, run_scan


class SkillRegressionTests(unittest.TestCase):
    def write_skill(self, root: Path, body: bytes | str) -> Path:
        skill_dir = root / "demo-skill"
        skill_dir.mkdir(parents=True, exist_ok=True)
        skill_file = skill_dir / "SKILL.md"
        if isinstance(body, bytes):
            skill_file.write_bytes(body)
        else:
            skill_file.write_text(body, encoding="utf-8")
        return skill_file

    def test_valid_skill_and_local_reference(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "demo-skill" / "assets").mkdir(parents=True)
            (root / "demo-skill" / "assets" / "example.txt").write_text("ok", encoding="utf-8")
            skill = self.write_skill(
                root,
                "---\nname: demo-skill\ndescription: A test skill.\n---\n\nSee `assets/example.txt`.\n",
            )
            self.assertEqual(check_skill(skill, root), [])

    def test_commands_prose_and_markdown_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / "demo-skill" / "scripts"
            directory.mkdir(parents=True)
            (directory / "tool.py").write_text("pass", encoding="utf-8")
            (directory / "my tool.py").write_text("pass", encoding="utf-8")
            skill = self.write_skill(root, "---\nname: demo-skill\ndescription: Test\n---\nRun `scripts/tool.py --repo <repo>`. Inspect references/timeline relationships. See [tool](<scripts/my tool.py>).\n")
            self.assertEqual(check_skill(skill, root), [])
            (directory / "my tool.py").unlink()
            self.assertTrue(any("my tool.py" in f.message for f in check_skill(skill, root)))

    def test_missing_metadata_and_reference_are_errors(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            skill = self.write_skill(root, "# Missing metadata\nSee `references/missing.md`.\n")
            findings = check_skill(skill, root)
            messages = " ".join(item.message for item in findings)
            self.assertIn("missing YAML frontmatter", messages)
            self.assertIn("does not exist", messages)

    def test_bom_and_secret_are_errors(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            skill = self.write_skill(
                root,
                b"\xef\xbb\xbf---\nname: demo-skill\ndescription: Test\n---\n\nsk-1234567890123456789012345\n",
            )
            findings = check_skill(skill, root)
            messages = " ".join(item.message for item in findings)
            self.assertIn("without a BOM", messages)
            self.assertIn("hard-coded credential", messages)

    def test_scan_counts_skills_and_errors(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.write_skill(root, "---\nname: demo-skill\ndescription: Test\n---\n")
            report = run_scan([root])
            self.assertEqual(report["skills_checked"], 1)
            self.assertEqual(report["error_count"], 0)


if __name__ == "__main__":
    unittest.main()
