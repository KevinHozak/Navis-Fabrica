"""Project-document fixtures; no live repository or publication side effects."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent
source = Path(os.environ.get("HARNESS_INSTALLER_MODULE", str(ROOT / "create_project_harness.py")))
spec = importlib.util.spec_from_file_location("harness_installer_under_test", source)
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
# Both baseline and candidate consume the identical real template inputs.
installer.TEMPLATE_ROOT = ROOT.parent / "templates/project-workflow-harness"
if os.environ.get("WORKFLOW_TEST_TMP"):
    Path(os.environ["WORKFLOW_TEST_TMP"]).mkdir(parents=True, exist_ok=True)
    tempfile.tempdir = os.environ["WORKFLOW_TEST_TMP"]
BOM = b"\xef\xbb\xbf"


class ProjectDocuments(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="project-documents-")
        self.addCleanup(self.tmp.cleanup)
        self.target = Path(self.tmp.name) / "project with spaces"

    def test_target_single_bom_in_every_generated_document(self):
        files = installer.install(self.target, "Example Project", False)
        self.assertEqual(len(files), 8)
        for path in files:
            with self.subTest(document=path.name):
                self.assertTrue(path.read_bytes().startswith(BOM))
                self.assertFalse(path.read_bytes().startswith(BOM + BOM))
                self.assertTrue(path.read_text(encoding="utf-8-sig").startswith("# "))

    def test_target_project_name_in_all_planning_headings(self):
        installer.install(self.target, "Example Project", False)
        for name in ("AGENTS.md", "GOALS.md", "PLANS.md", "PROMPTS.md"):
            with self.subTest(document=name):
                text = (self.target / name).read_text(encoding="utf-8-sig")
                self.assertIn("Example Project", text.splitlines()[0])
                self.assertNotIn("[PROJECT NAME]", text)

    def test_existing_guidance_and_unrelated_notes_preserved_by_default(self):
        self.target.mkdir()
        original = {"AGENTS.md": b"user-owned instructions\n", "GOALS.md": BOM + b"user goals\n", "notes.txt": b"unrelated"}
        for name, payload in original.items():
            (self.target / name).write_bytes(payload)
        files = installer.install(self.target, "Example Project", False)
        self.assertEqual(len(files), 6)
        for name, payload in original.items():
            self.assertEqual((self.target / name).read_bytes(), payload)
        self.assertEqual(installer.install(self.target, "Different Project", False), [])

    def test_explicit_force_overwrites_only_declared_harness_paths(self):
        installer.install(self.target, "Example Project", False)
        (self.target / "AGENTS.md").write_bytes(b"old guidance")
        note = self.target / "notes.txt"
        note.write_bytes(b"unrelated")
        files = installer.install(self.target, "Replacement Project", True)
        self.assertEqual(len(files), 8)
        self.assertIn("Replacement Project", (self.target / "AGENTS.md").read_text(encoding="utf-8-sig"))
        self.assertEqual(note.read_bytes(), b"unrelated")

    def test_heldout_unicode_project_name_survives_with_one_bom(self):
        name = "Sage \u96ea \U0001f331"
        installer.install(self.target, name, False)
        for document in ("AGENTS.md", "GOALS.md", "PLANS.md", "PROMPTS.md"):
            payload = (self.target / document).read_bytes()
            self.assertTrue(payload.startswith(BOM))
            self.assertFalse(payload.startswith(BOM + BOM))
            self.assertIn(name, payload.decode("utf-8-sig").splitlines()[0])


if __name__ == "__main__":
    unittest.main(verbosity=2)
